//! The progression engine: the targets of an exercise's next session, and what changed.
//!
//! [`next_targets`] takes one exercise of a program, the lifter's training max (if any), the
//! [`ProgressionSettings`] and the exercise's recent history ([`PastSession`]s, oldest first), and
//! returns [`NextTargets`]: the working and warm-up sets to prefill, where they come from
//! ([`TargetSource`]), and the [`ProgressionChange`] that the last session caused, for the
//! end-of-session summary ("Squat: 100 → 102.5 kg"). It is pure and deterministic: no clock, no
//! storage, and the same input always gives the same output.
//!
//! # History
//!
//! The caller passes the history of **one exercise**, oldest session first. Build it with
//! [`exercise_history`] from the stored [`SessionLog`]s, after filtering them:
//!
//! - Only sessions of the **active program** (every version of it, no other program), as for
//!   [`next_day`]. Progression does not carry over from one program to another: another program
//!   may use the same exercise with a different rep scheme (5 × 5 against 3 × 8–12), so its loads
//!   say little about this one. A new program starts from its own loads.
//! - For an exercise loaded as a percentage of its training max, only sessions **completed after
//!   the training max was last entered** (see [Training max](#training-max)).
//!
//! [`exercise_history`] keeps completed sessions only (abandoned ones would count as failures),
//! and only their working sets of the exercise, in set order. Sessions where the exercise has no
//! working set (it was skipped) are ignored by the engine: they are neither a success nor a failure.
//!
//! Each [`PastSession`] carries its own [`Prescription`]: the exercise's work, load and progression
//! rule **on the day and program version that session was run from**. A program may prescribe the
//! same exercise differently on different days (5 × 5 at 80 % on day A, 3 × 3 at 90 % on day B),
//! and a new version may change it or its rule, so a past session is always judged against what it
//! asked for, never against the day or version being planned.
//!
//! **Which rule applies a session's outcome.** The step from session `N` to the next judged
//! session `N + 1` uses the rule of the version that **produced the target shown for `N + 1`**:
//! `N + 1`'s own prescription. The step from the last judged session to the next target uses the
//! planned `exercise`'s rule. So a step is always replayed with the rule the lifter's next targets
//! were computed with, and never changes once a later session is logged: a session done exactly
//! as shown stays a success whatever versions follow, and a new version's increment or deload
//! applies from the step it planned on, never to the past. A session done under a version whose
//! rule is of another kind (a fixed-load version in a training max history, or the reverse, or no
//! rule) moves nothing: no increment, no deload. The planned `exercise` also decides the sets and
//! reps of the result.
//!
//! To build the history (the stored-history ticket, #18): for each session, load the program
//! version it was run from (`Session::program_version_id`), and take
//! [`Prescription::in_program`] for its day (`Session::day`); [`exercise_history`] takes this
//! lookup as a closure. A session that cannot be judged (its prescription is not found, or it is
//! timed work) is not judged but **ends a failure streak**, so two streaks on either side of it
//! never merge into one.
//!
//! The working sets carry their logged `set_index`. The sets with an index below the prescribed
//! number of sets are the **prescribed working sets**; the others are extras (a top single, a
//! failed heavier attempt, back-off sets, extra AMRAP sets). The session screen (#28) must log
//! extras after the prescribed sets, with indices from the prescribed count up, and leave a gap
//! for a skipped working set, so that an extra is never taken for a working set. When an index
//! appears more than once (a set retried or logged twice), only its first occurrence in set order
//! counts; `exercise_history` keeps the logging order within an index.
//!
//! # Judging a session
//!
//! Each past session gets a [`SessionVerdict`] against **its own** prescription, on its
//! prescribed working sets only; extras never change the verdict nor the base. With `n` its
//! number of working sets and its rep target (a fixed count, where the bottom and top are the
//! same, or a range), and the counted sets defined below:
//!
//! - **Success**: at least `n` counted sets reached the **top** of the target (the fixed count, or
//!   the top of the range).
//! - **Hold** (ranges only): at least `n` counted sets reached the **bottom** of the range, but
//!   fewer than `n` reached the top. Not a failure: it neither progresses the weight nor counts
//!   towards a deload, and it ends a failure streak.
//! - **Failure**: fewer than `n` counted sets reached the bottom of the target. A missing set, a
//!   set with fewer reps than the target and a failed attempt (0 reps) all count as missed sets.
//!
//! Which sets count:
//!
//! - **Weight rules** (`add_when_top_of_range`, `double_progression`): every prescribed working
//!   set counts. The session's weight (the *base*) is the lightest of them that has a weight; if
//!   none has (a slip in the log), the session's prescribed fixed load, else the planned load.
//!   So 1 × 5 at 100 kg followed by a 110 kg single is a success at 100 kg, 3 × 5 at 100 kg
//!   followed by 1 × 10 at 60 kg is a success at 100 kg, and 2 × 5 at 100 kg (third set
//!   skipped) followed by a 60 kg back-off set is a failure at 100 kg.
//! - **Training max rule**: sets at least as heavy as what the session was prescribed. A set
//!   logged with its target ([`WorkingSet::target`], the prefill the lifter saw, stored since #60)
//!   counts when it is at least that target's weight, **exactly**: "lifted at least what was
//!   prescribed then", whatever the step was. A set logged without one (before #60), or with a
//!   target that has no weight (never a training max prescription), is compared
//!   with its prescription's exact weight (its percentage of the training max at that point of
//!   the replay, or its fixed weight, unrounded) less a fixed tolerance of 1.25 kg, half of the
//!   2.5 kg the step was capped at then, so that any target the app could have shown counts. Near
//!   [`Weight::MAX`], where targets are rounded down, that threshold is lowered so that the target
//!   shown still counts. Only the prescribed working sets are considered. Either way the verdict
//!   never depends on the current settings, so the step has no upper bound any more.
//!
//! # Rules
//!
//! Every computed weight is rounded to the settings' loadable [step](ProgressionSettings::step)
//! (2.5 kg or 5 lb by default); see [Rounding](#rounding).
//!
//! - **None** (and every timed exercise, which cannot have a rule): no progression. The targets
//!   are the last performance, else the program default.
//! - **`add_when_top_of_range`**: from the base of the last session (see above). After a success,
//!   the next weight is base + increment; after a hold or a failure, it stays the base. Reps aim
//!   for the top of the target.
//! - **`double_progression`**: the weight stays the base while the reps climb. After a hold, the
//!   rep target becomes the lowest reps of the prescribed sets plus one ("reps 8 → 9"). After a
//!   success, the weight goes up by the increment and the rep target goes back to the bottom of
//!   the range (at [`Weight::MAX`], where it cannot go up, the reps stay at the top). After a
//!   failure, the rep target is the bottom of the range. The change is described in the terms of
//!   the session's own range; the planned sets clamp the rep target into the planned day's range.
//! - **`training_max`**: the working weight is the load's percentage of the training max. A
//!   success adds the increment to the training max; a failure streak cuts it. Both are exact
//!   and unrounded, so the replayed training max never depends on the settings.
//! - **`deload_after_failures`** (any rule): after `failures` consecutive failed sessions, the
//!   weight is cut to `× (1 − percent)`, rounded to a lighter step (a training max: exactly), and
//!   the failure count starts again from zero, so the session after a deload needs a fresh streak
//!   before the next one. A success, a hold, or a session that cannot be judged also resets the
//!   count. Each step's deload setting comes from the rule that applies it (see above).
//!
//! For weight rules, the base is what was actually lifted, not what was prescribed: going heavier
//! than the target moves the base up, and going lighter moves it down.
//!
//! # Training max
//!
//! Training maxes are kept by the app per user and per exercise, outside the program. The value to
//! pass is the one **the lifter last entered**, together with the history since then. The engine
//! replays that history to get the current, effective training max (returned in
//! [`ExerciseTargets::training_max`]), so calling it twice with the same input never applies an
//! increase twice. The app must not store the effective training max back unless it also starts
//! the history again from that point.
//!
//! A load that is a percentage of the training max, with no training max given, returns
//! [`NextTargets::NeedsTrainingMax`], whatever the rule, so the app can ask for it.
//!
//! # Prefill order
//!
//! The working sets come from the first of these that applies ([`TargetSource`]):
//!
//! 1. **Progression**: the exercise has a rule and a history. One weight and one rep target for
//!    every set.
//! 2. **Last performance**: no rule (or timed work) and a history. Set `i` repeats the weight of
//!    the last session's working set with index `i`; a set not done repeats the one done before
//!    it (or the first one done). Extras (indices at or above the planned count) are never used;
//!    with no working set at all, the program's load. Reps stay the
//!    program's fixed count; for a range, the last reps clamped into the range. Timed work keeps
//!    the program's seconds and rounds: a short hold last time does not shorten the target.
//! 3. **Program default**: no history. The program's load (a fixed weight, or a percentage of the
//!    training max) as described in [Rounding](#rounding), and the fixed count, or for a range the top of it (the bottom for
//!    double progression).
//!
//! Warm-up sets follow the program's warm-up lines, computed from the heaviest working weight:
//! fixed warm-up weights and percentages of the working weight (see [Rounding](#rounding)).
//!
//! # Rounding
//!
//! Weights the lifter actually lifted (the base of a hold or a failure, a copy of the last
//! performance) are kept as they are: they were loadable. The others are rounded:
//!
//! - the program's fixed load and fixed warm-up weights, when they are written in the other unit
//!   than the lifter's ([`ProgressionSettings::unit`]), to the nearest step: a program written in
//!   kg gives loadable weights to a lifter in lb (the 20 kg bar becomes 45 lb). Written in the
//!   lifter's unit, they are kept as written (a 24 kg kettlebell stays 24 kg). A fixed warm-up
//!   lighter than the working weight stays lighter: if rounding would reach the working weight, it
//!   is kept exact;
//! - an increase to the nearest step, or the next step up when the nearest would not move it, so
//!   an increment smaller than the step still progresses (100 kg + 1 kg with a 2.5 kg step is
//!   102.5 kg);
//! - a percentage of the training max to the nearest step (the training max itself is never
//!   rounded);
//! - a weight deload, and a warm-up percentage of the working weight, to the nearest step, or down when
//!   the nearest would not be lighter: a deload never increases the weight, and a warm-up is never
//!   as heavy as the working weight.
//!
//! A computed weight that would round to zero keeps its exact value instead. Nothing goes above
//! [`Weight::MAX`]: an increase stops at the cap (off the step if no step fits below it), and at
//! the cap it leaves the weight where it is.
//!
//! [`SessionLog`]: crate::SessionLog
//! [`next_day`]: crate::next_day
//! [`Weight::MAX`]: crate::Weight::MAX

mod change;
mod engine;
mod history;
mod settings;
mod target;

pub use change::{ChangeKind, ProgressionChange, ProgressionChangeDisplay};
pub use engine::{SessionVerdict, next_targets};
pub use history::{PastSession, Prescription, WorkingSet, exercise_history};
pub use settings::ProgressionSettings;
pub use target::{ExerciseTargets, NextTargets, SetGoal, SetTarget, TargetSource};
