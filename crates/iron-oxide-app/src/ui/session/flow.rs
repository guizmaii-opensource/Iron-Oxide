//! The session's view model (#28): which set comes next, what to prefill, and how to show it.
//!
//! Pure functions over the [`SessionPlan`] and the sets logged so far, so the screen stays thin
//! and everything here is unit-tested. The targets come from the progression engine (through the
//! plan); nothing here recomputes a load.
//!
//! - [`steps`] lays the day out as one [`Step`] per set, in the order they are done: an exercise's
//!   warm-ups then its working sets; a superset's warm-ups first, then its members in turn
//!   (A1, A2, A1, A2, …).
//! - [`current_step`] is the first step neither logged nor skipped, so a reloaded session resumes
//!   where it stopped.
//! - `set_index` numbers an exercise's sets of one kind (warm-up or working) from 0, in program
//!   order: working sets `0..n`, and a skipped one leaves its index unused, as the progression
//!   engine expects.

use std::collections::BTreeSet;
use std::time::Duration;

use iron_oxide_domain::program::{Exercise, Work};
use iron_oxide_domain::progression::{NextTargets, SetGoal, SetTarget};
use iron_oxide_domain::time::Timestamp;
use iron_oxide_domain::timer::IntervalPlan;
use iron_oxide_domain::{ExerciseId, LoggedSet, Reps, Seconds, SetId, Unit, Weight};

use crate::api::sessions::{PlannedExercise, SessionPlan};
use crate::ui::weight::weight_text;

/// One set of the day, in the order it is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// The exercise's position in the plan.
    pub exercise: usize,
    pub warm_up: bool,
    /// Position among the exercise's sets of the same kind, from 0.
    pub set_index: u16,
    /// How many sets of this kind the exercise has.
    pub of: u16,
    /// What to prefill.
    pub target: SetTarget,
    /// Whether `target` is what the program prescribes (the progression's target, or last
    /// session's set), as opposed to a stand-in: the empty bar when the plan needs a training max
    /// the lifter has not entered. Only a prescribed target is saved with the set (#60): the
    /// progression judges a set against its saved target exactly, so a stand-in would make any
    /// lift count once the training max is entered.
    pub prescribed: bool,
}

impl Step {
    /// Whether `set` is the logged result of this step.
    #[must_use]
    pub fn is_logged_by(&self, exercise: &ExerciseId, set: &LoggedSet<Timestamp>) -> bool {
        set.exercise == *exercise && set.warm_up == self.warm_up && set.set_index == self.set_index
    }
}

/// The warm-up and working sets of one planned exercise. When the plan needs a training max the
/// lifter has not entered, the working sets fall back to the program's sets and reps, loaded with
/// `fallback_weight` (the empty bar) so the lifter can still log what they lift.
#[must_use]
pub fn exercise_sets(
    planned: &PlannedExercise,
    fallback_weight: Weight,
) -> (Vec<SetTarget>, Vec<SetTarget>) {
    match &planned.targets {
        NextTargets::Ready(targets) => (targets.warmup.clone(), targets.working.clone()),
        NextTargets::NeedsTrainingMax { .. } => {
            let exercise = &planned.exercise;
            let target = SetTarget {
                weight: exercise.load.map(|_| fallback_weight),
                goal: goal_of(exercise.work),
            };
            (Vec::new(), vec![target; usize::from(exercise.work.sets())])
        }
    }
}

/// The goal of one working set of `work`, as the program writes it.
#[must_use]
pub const fn goal_of(work: Work) -> SetGoal {
    match work {
        Work::Reps { reps, .. } => SetGoal::Reps {
            reps: reps.min(),
            range: None,
        },
        Work::Hold { seconds, .. } => SetGoal::Hold { seconds },
        Work::Intervals { work, rest, rounds } => SetGoal::Intervals { work, rest, rounds },
    }
}

/// Every set of the day, in the order they are done. See the [module documentation](self).
#[must_use]
pub fn steps(plan: &SessionPlan, fallback_weight: Weight) -> Vec<Step> {
    let sets: Vec<_> = plan
        .exercises
        .iter()
        .map(|planned| {
            let prescribed = matches!(planned.targets, NextTargets::Ready(_));
            (exercise_sets(planned, fallback_weight), prescribed)
        })
        .collect();
    let mut steps = Vec::new();
    let mut start = 0;
    while start < plan.exercises.len() {
        let end = superset_end(&plan.exercises, start);
        let group = &sets[start..end];
        for (offset, ((warmup, _), prescribed)) in group.iter().enumerate() {
            push_kind(&mut steps, start + offset, true, warmup, None, *prescribed);
        }
        let rounds = group
            .iter()
            .map(|((_, working), _)| working.len())
            .max()
            .unwrap_or(0);
        for round in 0..rounds {
            for (offset, ((_, working), prescribed)) in group.iter().enumerate() {
                push_kind(
                    &mut steps,
                    start + offset,
                    false,
                    working,
                    Some(round),
                    *prescribed,
                );
            }
        }
        start = end;
    }
    steps
}

/// The end (exclusive) of the group starting at `start`: the run of exercises sharing its superset
/// label, or the exercise alone.
fn superset_end(exercises: &[PlannedExercise], start: usize) -> usize {
    let Some(label) = &exercises[start].exercise.superset else {
        return start + 1;
    };
    let mut end = start + 1;
    while exercises
        .get(end)
        .is_some_and(|next| next.exercise.superset.as_ref() == Some(label))
    {
        end += 1;
    }
    end
}

/// Pushes the sets of one kind: all of them, or only the one of `round`.
fn push_kind(
    steps: &mut Vec<Step>,
    exercise: usize,
    warm_up: bool,
    targets: &[SetTarget],
    round: Option<usize>,
    prescribed: bool,
) {
    let of = u16::try_from(targets.len()).unwrap_or(u16::MAX);
    for (index, target) in targets.iter().enumerate() {
        if round.is_some_and(|round| round != index) {
            continue;
        }
        steps.push(Step {
            exercise,
            warm_up,
            set_index: u16::try_from(index).unwrap_or(u16::MAX),
            of,
            target: *target,
            prescribed,
        });
    }
}

/// Whether `step` is done: logged, or its exercise skipped.
#[must_use]
pub fn is_done(
    step: &Step,
    plan: &SessionPlan,
    logged: &[LoggedSet<Timestamp>],
    skipped: &BTreeSet<ExerciseId>,
) -> bool {
    let id = &plan.exercises[step.exercise].exercise.id;
    skipped.contains(id) || logged.iter().any(|set| step.is_logged_by(id, set))
}

/// The position of the first step not done yet, or `None` when the day is over.
#[must_use]
pub fn current_step(
    steps: &[Step],
    plan: &SessionPlan,
    logged: &[LoggedSet<Timestamp>],
    skipped: &BTreeSet<ExerciseId>,
) -> Option<usize> {
    steps
        .iter()
        .position(|step| !is_done(step, plan, logged, skipped))
}

/// The step after `current` that is not done yet.
#[must_use]
pub fn next_step(
    steps: &[Step],
    current: usize,
    plan: &SessionPlan,
    logged: &[LoggedSet<Timestamp>],
    skipped: &BTreeSet<ExerciseId>,
) -> Option<usize> {
    (current + 1..steps.len()).find(|&index| !is_done(&steps[index], plan, logged, skipped))
}

/// The steps not done yet.
#[must_use]
pub fn remaining(
    steps: &[Step],
    plan: &SessionPlan,
    logged: &[LoggedSet<Timestamp>],
    skipped: &BTreeSet<ExerciseId>,
) -> usize {
    steps
        .iter()
        .filter(|step| !is_done(step, plan, logged, skipped))
        .count()
}

/// The values the steppers start from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefill {
    pub reps: Reps,
    pub weight: Option<Weight>,
}

/// What the steppers start from: each set's own target (the progression's, or last session's set
/// by set). The one exception is an explicit override: when the latest working set logged before
/// this one, for the same exercise, was lifted at another weight than its own target, that weight
/// carries to the following working sets of the exercise. Warm-ups always keep their target.
#[must_use]
pub fn prefill(
    steps: &[Step],
    step: &Step,
    exercise: &ExerciseId,
    logged: &[LoggedSet<Timestamp>],
) -> Prefill {
    let reps = match step.target.goal {
        SetGoal::Reps { reps, .. } => reps,
        SetGoal::Hold { .. } | SetGoal::Intervals { .. } => Reps::new(1),
    };
    let overridden = (!step.warm_up)
        .then(|| {
            let last = logged.iter().rev().find(|set| {
                set.exercise == *exercise && !set.warm_up && set.set_index < step.set_index
            })?;
            let own_target = steps
                .iter()
                .find(|other| {
                    other.exercise == step.exercise
                        && !other.warm_up
                        && other.set_index == last.set_index
                })
                .and_then(|other| other.target.weight);
            last.weight.filter(|weight| Some(*weight) != own_target)
        })
        .flatten();
    let weight = step
        .target
        .weight
        .map(|target| overridden.unwrap_or(target));
    Prefill { reps, weight }
}

/// The set to save for a step. The completion time is never before the session's start, so a
/// clock that stepped back (or another device's) does not make the server refuse it.
#[must_use]
pub fn logged_set(
    id: SetId,
    step: &Step,
    exercise: &Exercise,
    entry: Entry,
    now: Timestamp,
    started_at: Timestamp,
) -> LoggedSet<Timestamp> {
    LoggedSet {
        id,
        exercise: exercise.id.clone(),
        set_index: step.set_index,
        reps: entry.reps,
        weight: entry.weight,
        duration: entry.duration,
        warm_up: step.warm_up,
        completed_at: now.max(started_at),
        // What this set was prescribed, so that the progression judges it exactly (#60); nothing
        // when the prefill was only a stand-in (the bar, for a missing training max).
        target: step.prescribed.then_some(step.target),
    }
}

/// What the lifter entered for a set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub reps: Reps,
    pub weight: Option<Weight>,
    pub duration: Option<Seconds>,
}

/// When to finish the session: now, but never before its start or a logged set (the server
/// refuses those with a 422).
#[must_use]
pub fn finish_time(
    now: Timestamp,
    started_at: Timestamp,
    logged: &[LoggedSet<Timestamp>],
) -> Timestamp {
    logged
        .iter()
        .map(|set| set.completed_at)
        .fold(now.max(started_at), Timestamp::max)
}

/// The interval plan of an intervals goal, or `None` for another goal (or a plan the domain
/// refuses, which a validated program never has).
#[must_use]
pub fn interval_plan(goal: SetGoal) -> Option<IntervalPlan> {
    match goal {
        SetGoal::Intervals { work, rest, rounds } => {
            IntervalPlan::new(work.as_duration(), rest.as_duration(), u32::from(rounds)).ok()
        }
        SetGoal::Reps { .. } | SetGoal::Hold { .. } => None,
    }
}

/// The time to log for a timed set: the time since its timer `started`, capped at the whole plan
/// for intervals; the goal's own time when the timer was not used. `None` for sets of reps.
#[must_use]
pub fn timed_duration(
    goal: SetGoal,
    started: Option<Timestamp>,
    now: Timestamp,
) -> Option<Seconds> {
    let planned = match goal {
        SetGoal::Reps { .. } => return None,
        SetGoal::Hold { seconds } => seconds.as_duration(),
        SetGoal::Intervals { .. } => interval_plan(goal)?.total(),
    };
    let elapsed = started.map_or(planned, |started| {
        let elapsed = now.saturating_duration_since(started);
        match goal {
            SetGoal::Intervals { .. } => elapsed.min(planned),
            SetGoal::Reps { .. } | SetGoal::Hold { .. } => elapsed,
        }
    });
    let millis = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    Seconds::from_millis_ceil(millis).ok()
}

/// A time left on a countdown, `m:ss`, rounded up so the last second shows `0:01`, not `0:00`.
#[must_use]
pub fn clock_text(left: Duration) -> String {
    let millis = u64::try_from(left.as_millis()).unwrap_or(u64::MAX);
    Seconds::from_millis_ceil(millis).map_or_else(|_| "–".to_owned(), |seconds| seconds.to_string())
}

/// The rest after the working or warm-up set `steps[done]`, before `next` (the next step not done
/// yet, skipped exercises left out), or `None` for no rest timer:
/// - none after the last set of the day, nor after a warm-up (warm-ups lead straight into the
///   next set);
/// - in a superset, a round ends after its last member not skipped. Inside a round (`next` is a
///   later member's working set) there is no rest, unless the program gives the member an
///   explicit rest above 0. Between rounds, and after the superset, the superset's rest: its last
///   member's when above 0, else the user's default. Skipped members never move the round
///   boundaries: they are only left out of `next`;
/// - outside a superset, the exercise's rest, or the user's default when the program says 0.
#[must_use]
pub fn rest_after(
    plan: &SessionPlan,
    steps: &[Step],
    done: usize,
    next: Option<usize>,
    default_rest: Seconds,
) -> Option<Seconds> {
    let step = steps.get(done)?;
    let next = steps.get(next?)?;
    if step.warm_up {
        return None;
    }
    let exercise = &plan.exercises[step.exercise].exercise;
    let rest = if exercise.superset.is_some() {
        let group = group_of(plan, step.exercise);
        // A later member of the same group: the round goes on. The same or an earlier member: a
        // new round. Anything else: the superset is over.
        let within_round =
            !next.warm_up && next.exercise > step.exercise && group.contains(&next.exercise);
        if within_round {
            return (!exercise.rest.is_zero()).then_some(exercise.rest);
        }
        plan.exercises[group.end - 1].exercise.rest
    } else {
        exercise.rest
    };
    Some(if rest.is_zero() { default_rest } else { rest }).filter(|rest| !rest.is_zero())
}

/// The plan positions of the superset `exercise` belongs to (the exercise alone outside one).
fn group_of(plan: &SessionPlan, exercise: usize) -> std::ops::Range<usize> {
    let label = &plan.exercises[exercise].exercise.superset;
    if label.is_none() {
        return exercise..exercise + 1;
    }
    let same = |index: usize| &plan.exercises[index].exercise.superset == label;
    let start = (0..=exercise)
        .rev()
        .take_while(|&index| same(index))
        .last()
        .unwrap_or(exercise);
    let end = (exercise..plan.exercises.len())
        .take_while(|&index| same(index))
        .last()
        .map_or(exercise + 1, |last| last + 1);
    start..end
}

/// The rest screen's header: `REST · BACK SQUAT` and `Set 2 logged ✓` for the set it follows.
#[must_use]
pub fn rest_header(plan: &SessionPlan, set: &LoggedSet<Timestamp>) -> (String, String) {
    let name = plan
        .exercises
        .iter()
        .find(|planned| planned.exercise.id == set.exercise)
        .map_or_else(
            || set.exercise.to_string(),
            |planned| planned.exercise.name.clone(),
        );
    let kind = if set.warm_up { "Warm-up" } else { "Set" };
    (
        format!("REST · {}", name.to_uppercase()),
        format!("{kind} {} logged ✓", set.set_index + 1),
    )
}

/// The "up next" card of the rest screen: `UP NEXT · SET 3 / 5` and `5 × 100 kg`. The exercise
/// is named when it differs from the one just done (`after`).
#[must_use]
pub fn up_next(
    steps: &[Step],
    next: usize,
    after: usize,
    plan: &SessionPlan,
    unit: Unit,
) -> (String, String) {
    let step = &steps[next];
    let kind = if step.warm_up { "WARM-UP" } else { "SET" };
    let position = format!("{kind} {} / {}", step.set_index + 1, step.of);
    let label = if step.exercise == after {
        format!("UP NEXT · {position}")
    } else {
        let name = plan.exercises[step.exercise].exercise.name.to_uppercase();
        format!("UP NEXT · {name} · {position}")
    };
    let value = match (step.target.goal, step.target.weight) {
        (SetGoal::Reps { reps, range }, Some(weight)) => {
            let reps = range.map_or_else(
                || reps.get().to_string(),
                |range| format!("{}–{}", range.min.get(), range.max.get()),
            );
            format!("{reps} × {}", weight_text(weight, unit))
        }
        (goal, Some(weight)) => format!("{} · {}", goal_text(goal), weight_text(weight, unit)),
        (goal, None) => goal_text(goal),
    };
    (label, value)
}

/// How much of a rest is left, in percent: what fills its bar (full at the start, empty at the
/// end, as on the board).
#[must_use]
pub fn rest_left_percent(left: Duration, total: Duration) -> u32 {
    if total.is_zero() {
        return 0;
    }
    let share = left.min(total).as_millis().saturating_mul(100) / total.as_millis();
    u32::try_from(share).unwrap_or(100)
}

/// `5 reps`, `8–12 reps`, `0:45 hold`, `8 rounds · 0:30 on, 1:30 off`.
#[must_use]
pub fn goal_text(goal: SetGoal) -> String {
    match goal {
        SetGoal::Reps {
            range: Some(range), ..
        } => format!("{}–{} reps", range.min.get(), range.max.get()),
        SetGoal::Reps { reps, range: None } => plural(u32::from(reps.get()), "rep"),
        SetGoal::Hold { seconds } => format!("{seconds} hold"),
        SetGoal::Intervals { work, rest, rounds } => {
            format!(
                "{} · {work} on, {rest} off",
                plural(u32::from(rounds), "round")
            )
        }
    }
}

/// `1 rep`, `5 reps`.
#[must_use]
pub fn plural(count: u32, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// The header label: `DAY A · SET 2 / 5`, `DAY A · WARM-UP 1 / 3`.
#[must_use]
pub fn header_label(day_name: &str, step: &Step) -> String {
    let kind = if step.warm_up { "WARM-UP" } else { "SET" };
    format!(
        "{} · {kind} {} / {}",
        day_name.to_uppercase(),
        step.set_index + 1,
        step.of
    )
}

/// The line under the exercise name: `Target 5 reps · 100 kg · rest 3:00`.
#[must_use]
pub fn target_line(step: &Step, exercise: &Exercise, unit: Unit) -> String {
    let mut parts = vec![format!("Target {}", goal_text(step.target.goal))];
    if let Some(weight) = step.target.weight {
        parts.push(weight_text(weight, unit));
    }
    if !step.warm_up && !exercise.rest.is_zero() {
        parts.push(format!("rest {}", exercise.rest));
    }
    parts.join(" · ")
}

/// The line under Done: what comes after the current step.
#[must_use]
pub fn next_line(
    steps: &[Step],
    current: usize,
    next: Option<usize>,
    plan: &SessionPlan,
    unit: Unit,
) -> String {
    let Some(next) = next else {
        return "Last set of the day".to_owned();
    };
    let step = &steps[next];
    let exercise = &plan.exercises[step.exercise].exercise;
    let load = step
        .target
        .weight
        .map(|weight| format!(" · {}", weight_text(weight, unit)))
        .unwrap_or_default();
    if step.exercise == steps[current].exercise {
        let kind = if step.warm_up { "warm-up" } else { "set" };
        return format!(
            "Next: {kind} {} / {} · {}{load}",
            step.set_index + 1,
            step.of,
            goal_text(step.target.goal)
        );
    }
    let name = exercise.name.to_lowercase();
    if step.warm_up {
        return format!(
            "Next: {name} · warm-up · {}{load}",
            goal_text(step.target.goal)
        );
    }
    format!("Next: {name} · {}", sets_text(step.of, step.target, unit))
}

/// `3 × 5 reps · 40 kg`, `3 × 0:30 hold`, `8 rounds · 0:30 on, 1:30 off` (intervals are one set).
fn sets_text(sets: u16, target: SetTarget, unit: Unit) -> String {
    let count = match target.goal {
        SetGoal::Intervals { .. } => String::new(),
        SetGoal::Reps { .. } | SetGoal::Hold { .. } => format!("{sets} × "),
    };
    let load = target
        .weight
        .map(|weight| format!(" · {}", weight_text(weight, unit)))
        .unwrap_or_default();
    format!("{count}{}{load}", goal_text(target.goal))
}

/// An exercise's working sets in one line, for the preview of the next workout:
/// `3 × 5 reps · 40 kg`. Without a training max, it says one is needed.
#[must_use]
pub fn exercise_summary(planned: &PlannedExercise, fallback_weight: Weight, unit: Unit) -> String {
    if matches!(planned.targets, NextTargets::NeedsTrainingMax { .. }) {
        let target = SetTarget {
            weight: None,
            goal: goal_of(planned.exercise.work),
        };
        return format!(
            "{} · needs a training max",
            sets_text(planned.exercise.work.sets(), target, unit)
        );
    }
    let (_, working) = exercise_sets(planned, fallback_weight);
    working.first().map_or_else(String::new, |first| {
        sets_text(
            u16::try_from(working.len()).unwrap_or(u16::MAX),
            *first,
            unit,
        )
    })
}

/// The superset position of an exercise: `A1`, `A2` for a one-letter label, `UPPER-1 · 2` for a
/// longer one (so its digits never run into the position), `None` outside a superset.
#[must_use]
pub fn superset_tag(plan: &SessionPlan, exercise: usize) -> Option<String> {
    let label = plan.exercises[exercise].exercise.superset.as_ref()?;
    let first = (0..=exercise)
        .rev()
        .take_while(|&index| plan.exercises[index].exercise.superset.as_ref() == Some(label))
        .last()
        .unwrap_or(exercise);
    let label = label.as_str().to_uppercase();
    let position = exercise - first + 1;
    Some(if label.chars().count() == 1 {
        format!("{label}{position}")
    } else {
        format!("{label} · {position}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::sessions::SessionView;
    use iron_oxide_domain::program::{RepTarget, SupersetId};
    use iron_oxide_domain::progression::{ExerciseTargets, TargetSource};
    use iron_oxide_domain::{DayId, ProgramId, ProgramVersionId, SessionId, SessionStatus};
    use uuid::Uuid;

    fn kg(value: f64) -> Weight {
        Weight::from_kg(value).unwrap()
    }

    fn reps_goal(reps: u16) -> SetGoal {
        SetGoal::Reps {
            reps: Reps::new(reps),
            range: None,
        }
    }

    fn target(weight: f64, reps: u16) -> SetTarget {
        SetTarget {
            weight: Some(kg(weight)),
            goal: reps_goal(reps),
        }
    }

    fn exercise(id: &str, sets: u16, superset: Option<&str>) -> Exercise {
        Exercise {
            id: ExerciseId::new(id).unwrap(),
            name: id.replace('-', " "),
            work: Work::Reps {
                sets,
                reps: RepTarget::Fixed(Reps::new(5)),
            },
            load: None,
            rest: Seconds::new(180),
            tempo: None,
            notes: None,
            demo_url: None,
            warmup: Vec::new(),
            superset: superset
                .map(|label| serde_json::from_value::<SupersetId>(label.into()).unwrap()),
            progression: Default::default(),
        }
    }

    fn planned(id: &str, warmups: u16, sets: u16, superset: Option<&str>) -> PlannedExercise {
        let exercise = exercise(id, sets, superset);
        PlannedExercise {
            targets: NextTargets::Ready(ExerciseTargets {
                exercise: exercise.id.clone(),
                source: TargetSource::Progression,
                warmup: (0..warmups).map(|_| target(20.0, 5)).collect(),
                working: (0..sets).map(|_| target(100.0, 5)).collect(),
                training_max: None,
                failed_sessions: 0,
                last_verdict: None,
                change: None,
            }),
            exercise,
        }
    }

    fn plan(exercises: Vec<PlannedExercise>) -> SessionPlan {
        SessionPlan {
            session: SessionView {
                id: SessionId::from_uuid(Uuid::from_u128(1)),
                program_id: ProgramId::from_uuid(Uuid::from_u128(2)),
                program_version_id: ProgramVersionId::from_uuid(Uuid::from_u128(3)),
                day: DayId::new("a").unwrap(),
                status: SessionStatus::InProgress,
                started_at: Timestamp::from_epoch_millis(1_000),
                finished_at: None,
            },
            day_name: "Day A".to_owned(),
            exercises,
        }
    }

    fn logged(
        id: &str,
        warm_up: bool,
        set_index: u16,
        weight: f64,
        at: i64,
    ) -> LoggedSet<Timestamp> {
        LoggedSet {
            id: SetId::from_uuid(Uuid::from_u128(
                u128::from(set_index) + 100 + u128::try_from(at).unwrap(),
            )),
            exercise: ExerciseId::new(id).unwrap(),
            set_index,
            reps: Reps::new(5),
            weight: Some(kg(weight)),
            duration: None,
            warm_up,
            completed_at: Timestamp::from_epoch_millis(at),
            target: None,
        }
    }

    /// `(exercise position, warm-up, set index)` of each step.
    fn order(steps: &[Step]) -> Vec<(usize, bool, u16)> {
        steps
            .iter()
            .map(|step| (step.exercise, step.warm_up, step.set_index))
            .collect()
    }

    #[test]
    fn an_exercise_does_its_warm_ups_then_its_working_sets() {
        let plan = plan(vec![
            planned("squat", 2, 3, None),
            planned("bench", 0, 2, None),
        ]);
        let steps = steps(&plan, kg(20.0));
        assert_eq!(
            order(&steps),
            [
                (0, true, 0),
                (0, true, 1),
                (0, false, 0),
                (0, false, 1),
                (0, false, 2),
                (1, false, 0),
                (1, false, 1),
            ]
        );
        assert_eq!(steps[0].of, 2);
        assert_eq!(steps[2].of, 3);
        assert_eq!(steps[2].target, target(100.0, 5));
    }

    #[test]
    fn a_superset_alternates_its_members_after_their_warm_ups() {
        let plan = plan(vec![
            planned("press", 1, 2, Some("a")),
            planned("chin-up", 1, 2, Some("a")),
            planned("curl", 0, 1, None),
        ]);
        assert_eq!(
            order(&steps(&plan, kg(20.0))),
            [
                (0, true, 0),
                (1, true, 0),
                (0, false, 0),
                (1, false, 0),
                (0, false, 1),
                (1, false, 1),
                (2, false, 0),
            ]
        );
    }

    #[test]
    fn a_missing_training_max_falls_back_to_the_program_sets_on_the_bar() {
        let mut exercise = exercise("bench", 3, None);
        exercise.load = Some(
            serde_json::from_value(serde_json::json!({"percent_of_training_max": 80})).unwrap(),
        );
        let plan = plan(vec![PlannedExercise {
            targets: NextTargets::NeedsTrainingMax {
                exercise: exercise.id.clone(),
            },
            exercise,
        }]);
        let steps = steps(&plan, kg(20.0));
        assert_eq!(steps.len(), 3);
        assert!(steps.iter().all(|step| step.target == target(20.0, 5)));
        // The bar is a stand-in, not a prescription: the set is saved without a target (#60),
        // or a training max entered while the workout is open would judge it against 20 kg.
        assert!(steps.iter().all(|step| !step.prescribed));
        let entry = Entry {
            reps: Reps::new(5),
            weight: Some(kg(60.0)),
            duration: None,
        };
        let start = Timestamp::from_epoch_millis(1_000);
        let saved = logged_set(
            SetId::from_uuid(Uuid::from_u128(9)),
            &steps[0],
            &plan.exercises[0].exercise,
            entry,
            start,
            start,
        );
        assert_eq!(saved.target, None);
    }

    #[test]
    fn the_current_step_resumes_after_the_logged_sets() {
        let plan = plan(vec![
            planned("squat", 1, 2, None),
            planned("bench", 0, 1, None),
        ]);
        let steps = steps(&plan, kg(20.0));
        let none = BTreeSet::new();
        assert_eq!(current_step(&steps, &plan, &[], &none), Some(0));
        let sets = vec![
            logged("squat", true, 0, 20.0, 2_000),
            logged("squat", false, 0, 100.0, 3_000),
        ];
        assert_eq!(current_step(&steps, &plan, &sets, &none), Some(2));
        assert_eq!(next_step(&steps, 2, &plan, &sets, &none), Some(3));
        assert_eq!(remaining(&steps, &plan, &sets, &none), 2);
        // A warm-up and a working set with the same index are different steps.
        let only_warm = vec![logged("squat", true, 0, 20.0, 2_000)];
        assert_eq!(current_step(&steps, &plan, &only_warm, &none), Some(1));
    }

    #[test]
    fn skipping_an_exercise_moves_to_the_next_one_and_leaves_its_indices_unused() {
        let plan = plan(vec![
            planned("squat", 0, 3, None),
            planned("bench", 0, 1, None),
        ]);
        let steps = steps(&plan, kg(20.0));
        let sets = vec![logged("squat", false, 0, 100.0, 2_000)];
        let skipped = BTreeSet::from([ExerciseId::new("squat").unwrap()]);
        assert_eq!(current_step(&steps, &plan, &sets, &skipped), Some(3));
        assert_eq!(next_step(&steps, 0, &plan, &sets, &skipped), Some(3));
        let all = BTreeSet::from([
            ExerciseId::new("squat").unwrap(),
            ExerciseId::new("bench").unwrap(),
        ]);
        assert_eq!(current_step(&steps, &plan, &sets, &all), None);
    }

    #[test]
    fn the_prefill_carries_a_changed_working_weight_but_not_into_warm_ups() {
        let plan = plan(vec![planned("squat", 1, 3, None)]);
        let steps = steps(&plan, kg(20.0));
        let squat = ExerciseId::new("squat").unwrap();
        assert_eq!(
            prefill(&steps, &steps[1], &squat, &[]),
            Prefill {
                reps: Reps::new(5),
                weight: Some(kg(100.0))
            }
        );
        let sets = vec![
            logged("squat", true, 0, 20.0, 2_000),
            logged("squat", false, 0, 102.5, 3_000),
        ];
        assert_eq!(
            prefill(&steps, &steps[2], &squat, &sets).weight,
            Some(kg(102.5))
        );
        assert_eq!(
            prefill(&steps, &steps[3], &squat, &sets).weight,
            Some(kg(102.5))
        );
        assert_eq!(
            prefill(&steps, &steps[0], &squat, &sets).weight,
            Some(kg(20.0))
        );
    }

    /// Review of #97: last session's per-set values (or a ramp) come back set by set.
    #[test]
    fn prefill_keeps_per_set_targets_when_unchanged() {
        let mut squat = planned("squat", 0, 3, None);
        if let NextTargets::Ready(targets) = &mut squat.targets {
            targets.source = TargetSource::LastPerformance;
            targets.working = vec![target(60.0, 5), target(70.0, 5), target(80.0, 5)];
        }
        let plan = plan(vec![squat]);
        let steps = steps(&plan, kg(20.0));
        let id = ExerciseId::new("squat").unwrap();
        // Set 1 logged at its target: set 2 keeps its own.
        let sets = vec![logged("squat", false, 0, 60.0, 2_000)];
        assert_eq!(
            prefill(&steps, &steps[1], &id, &sets).weight,
            Some(kg(70.0))
        );
        // Set 2 overridden to 72.5: it carries to set 3, but not back to set 1 or 2.
        let sets = vec![
            logged("squat", false, 0, 60.0, 2_000),
            logged("squat", false, 1, 72.5, 3_000),
        ];
        assert_eq!(
            prefill(&steps, &steps[2], &id, &sets).weight,
            Some(kg(72.5))
        );
        assert_eq!(
            prefill(&steps, &steps[0], &id, &sets).weight,
            Some(kg(60.0))
        );
        assert_eq!(
            prefill(&steps, &steps[1], &id, &sets).weight,
            Some(kg(70.0))
        );
    }

    #[test]
    fn body_weight_and_timed_sets_have_no_weight_to_prefill() {
        let step = Step {
            exercise: 0,
            warm_up: false,
            set_index: 0,
            of: 3,
            target: SetTarget {
                weight: None,
                goal: SetGoal::Hold {
                    seconds: Seconds::new(45),
                },
            },
            prescribed: true,
        };
        let plank = ExerciseId::new("plank").unwrap();
        let sets = vec![logged("plank", false, 0, 10.0, 2_000)];
        assert_eq!(
            prefill(&[step], &step, &plank, &sets),
            Prefill {
                reps: Reps::new(1),
                weight: None
            }
        );
    }

    #[test]
    fn times_never_go_before_the_start_or_a_logged_set() {
        let start = Timestamp::from_epoch_millis(1_000);
        let plan = plan(vec![planned("squat", 0, 1, None)]);
        let steps = steps(&plan, kg(20.0));
        let entry = Entry {
            reps: Reps::new(5),
            weight: Some(kg(100.0)),
            duration: None,
        };
        let id = SetId::from_uuid(Uuid::from_u128(9));
        let early = logged_set(
            id,
            &steps[0],
            &plan.exercises[0].exercise,
            entry,
            Timestamp::from_epoch_millis(500),
            start,
        );
        assert_eq!(early.completed_at, start);
        assert_eq!(early.set_index, 0);
        assert!(!early.warm_up);
        let on_time = logged_set(
            id,
            &steps[0],
            &plan.exercises[0].exercise,
            entry,
            Timestamp::from_epoch_millis(5_000),
            start,
        );
        assert_eq!(on_time.completed_at, Timestamp::from_epoch_millis(5_000));
        // A planned set is saved with the target it was shown (#60).
        assert!(steps[0].prescribed);
        assert_eq!(on_time.target, Some(steps[0].target));

        let sets = vec![logged("squat", false, 0, 100.0, 9_000)];
        assert_eq!(
            finish_time(Timestamp::from_epoch_millis(4_000), start, &sets),
            Timestamp::from_epoch_millis(9_000)
        );
        assert_eq!(
            finish_time(Timestamp::from_epoch_millis(500), start, &[]),
            start
        );
        assert_eq!(
            finish_time(Timestamp::from_epoch_millis(20_000), start, &sets),
            Timestamp::from_epoch_millis(20_000)
        );
    }

    #[test]
    fn goals_read_naturally() {
        assert_eq!(goal_text(reps_goal(5)), "5 reps");
        assert_eq!(goal_text(reps_goal(1)), "1 rep");
        let range = serde_json::from_value(serde_json::json!({"min": 8, "max": 12})).unwrap();
        assert_eq!(
            goal_text(SetGoal::Reps {
                reps: Reps::new(8),
                range: Some(range)
            }),
            "8–12 reps"
        );
        assert_eq!(
            goal_text(SetGoal::Hold {
                seconds: Seconds::new(45)
            }),
            "0:45 hold"
        );
        assert_eq!(
            goal_text(SetGoal::Intervals {
                work: Seconds::new(30),
                rest: Seconds::new(90),
                rounds: 8
            }),
            "8 rounds · 0:30 on, 1:30 off"
        );
    }

    #[test]
    fn labels_follow_the_board() {
        let plan = plan(vec![
            planned("back-squat", 1, 5, None),
            planned("bench-press", 0, 5, None),
        ]);
        let steps = steps(&plan, kg(20.0));
        assert_eq!(header_label("Day A", &steps[2]), "DAY A · SET 2 / 5");
        assert_eq!(header_label("Day A", &steps[0]), "DAY A · WARM-UP 1 / 1");
        let squat = &plan.exercises[0].exercise;
        assert_eq!(
            target_line(&steps[1], squat, Unit::Kg),
            "Target 5 reps · 100 kg · rest 3:00"
        );
        assert_eq!(
            target_line(&steps[0], squat, Unit::Kg),
            "Target 5 reps · 20 kg"
        );
        assert_eq!(
            next_line(&steps, 1, Some(2), &plan, Unit::Kg),
            "Next: set 2 / 5 · 5 reps · 100 kg"
        );
        assert_eq!(
            next_line(&steps, 5, Some(6), &plan, Unit::Kg),
            "Next: bench press · 5 × 5 reps · 100 kg"
        );
        assert_eq!(
            next_line(&steps, 10, None, &plan, Unit::Kg),
            "Last set of the day"
        );
    }

    #[test]
    fn a_workout_preview_sums_up_each_exercise() {
        let plan = plan(vec![planned("squat", 2, 3, None)]);
        assert_eq!(
            exercise_summary(&plan.exercises[0], kg(20.0), Unit::Kg),
            "3 × 5 reps · 100 kg"
        );
        let bench = exercise("bench", 5, None);
        let needs = PlannedExercise {
            targets: NextTargets::NeedsTrainingMax {
                exercise: bench.id.clone(),
            },
            exercise: bench,
        };
        assert_eq!(
            exercise_summary(&needs, kg(20.0), Unit::Kg),
            "5 × 5 reps · needs a training max"
        );
    }

    #[test]
    fn superset_members_are_tagged_by_position() {
        let plan = plan(vec![
            planned("curl", 0, 1, None),
            planned("press", 0, 2, Some("a")),
            planned("chin-up", 0, 2, Some("a")),
            planned("dip", 0, 2, Some("b")),
        ]);
        assert_eq!(superset_tag(&plan, 0), None);
        assert_eq!(superset_tag(&plan, 1).as_deref(), Some("A1"));
        assert_eq!(superset_tag(&plan, 2).as_deref(), Some("A2"));
        assert_eq!(superset_tag(&plan, 3).as_deref(), Some("B1"));
        let mut long = plan.clone();
        long.exercises[1].exercise.superset =
            Some(serde_json::from_value("upper-1".into()).unwrap());
        long.exercises[2].exercise.superset = long.exercises[1].exercise.superset.clone();
        assert_eq!(superset_tag(&long, 1).as_deref(), Some("UPPER-1 · 1"));
        assert_eq!(superset_tag(&long, 2).as_deref(), Some("UPPER-1 · 2"));
    }

    #[test]
    fn timed_sets_log_the_time_since_their_timer_started() {
        let at = Timestamp::from_epoch_millis;
        let hold = SetGoal::Hold {
            seconds: Seconds::new(45),
        };
        assert_eq!(timed_duration(reps_goal(5), Some(at(0)), at(9_000)), None);
        // Without the timer, the goal's own time.
        assert_eq!(
            timed_duration(hold, None, at(9_000)),
            Some(Seconds::new(45))
        );
        // Rounded up to the second, and a hold may run past its target.
        assert_eq!(
            timed_duration(hold, Some(at(1_000)), at(31_200)),
            Some(Seconds::new(31))
        );
        assert_eq!(
            timed_duration(hold, Some(at(0)), at(60_000)),
            Some(Seconds::new(60))
        );
        // Intervals stop at the end of the plan: 3 × 30 s with 2 × 60 s of rest.
        let intervals = SetGoal::Intervals {
            work: Seconds::new(30),
            rest: Seconds::new(60),
            rounds: 3,
        };
        assert_eq!(
            timed_duration(intervals, None, at(0)),
            Some(Seconds::new(210))
        );
        assert_eq!(
            timed_duration(intervals, Some(at(0)), at(100_000)),
            Some(Seconds::new(100))
        );
        assert_eq!(
            timed_duration(intervals, Some(at(0)), at(900_000)),
            Some(Seconds::new(210))
        );
        // A clock that stepped back logs zero rather than failing.
        assert_eq!(
            timed_duration(hold, Some(at(5_000)), at(1_000)),
            Some(Seconds::new(0))
        );
    }

    #[test]
    fn countdowns_round_up_to_the_second() {
        assert_eq!(clock_text(Duration::from_millis(134_200)), "2:15");
        assert_eq!(clock_text(Duration::from_millis(200)), "0:01");
        assert_eq!(clock_text(Duration::ZERO), "0:00");
        assert_eq!(clock_text(Duration::from_secs(180)), "3:00");
    }

    fn superset_day() -> SessionPlan {
        let mut press = planned("press", 1, 2, Some("a"));
        press.exercise.rest = Seconds::new(0);
        let mut chin = planned("chin-up", 0, 2, Some("a"));
        chin.exercise.rest = Seconds::new(90);
        let mut curl = planned("curl", 0, 1, None);
        curl.exercise.rest = Seconds::new(0);
        plan(vec![press, chin, curl])
    }

    #[test]
    fn rests_follow_the_program_and_supersets() {
        let plan = superset_day();
        let steps = steps(&plan, kg(20.0));
        let default = Seconds::new(120);
        // [press warm-up, press 1, chin 1, press 2, chin 2, curl 1]
        let rest = |plan: &SessionPlan, steps: &[Step], done: usize| {
            rest_after(
                plan,
                steps,
                done,
                (done + 1 < steps.len()).then_some(done + 1),
                default,
            )
        };
        assert_eq!(rest(&plan, &steps, 0), None, "after a warm-up");
        assert_eq!(rest(&plan, &steps, 1), None, "A1 → A2 transition of 0");
        assert_eq!(
            rest(&plan, &steps, 2),
            Some(Seconds::new(90)),
            "the last member rests for the group"
        );
        assert_eq!(rest(&plan, &steps, 4), Some(Seconds::new(90)));
        // A rest of 0 outside a superset takes the user's default; the last set rests not at all.
        let mut single = planned("curl", 0, 2, None);
        single.exercise.rest = Seconds::new(0);
        let alone = super::tests::plan(vec![single]);
        let alone_steps = super::steps(&alone, kg(20.0));
        assert_eq!(rest(&alone, &alone_steps, 0), Some(default));
        assert_eq!(rest(&alone, &alone_steps, 1), None);
        // A transition with its own rest keeps it.
        let mut timed = superset_day();
        timed.exercises[0].exercise.rest = Seconds::new(15);
        let timed_steps = super::steps(&timed, kg(20.0));
        assert_eq!(rest(&timed, &timed_steps, 1), Some(Seconds::new(15)));
    }

    /// Review of #99: skipping one member keeps the superset's shared rest for the other.
    #[test]
    fn superset_rest_when_the_other_member_is_skipped() {
        let plan = superset_day();
        let steps = steps(&plan, kg(20.0));
        let skipped: BTreeSet<ExerciseId> = [ExerciseId::new("chin-up").unwrap()].into();
        let mut sets = vec![
            logged("press", true, 0, 20.0, 2_000),
            logged("press", false, 0, 100.0, 3_000),
        ];
        let rest_length = |sets: &[LoggedSet<Timestamp>], done: usize| {
            let next = current_step(&steps, &plan, sets, &skipped);
            rest_after(&plan, &steps, done, next, Seconds::new(120))
        };
        assert_eq!(current_step(&steps, &plan, &sets, &skipped), Some(3));
        assert_eq!(rest_length(&sets, 1), Some(Seconds::new(90)));
        sets.push(logged("press", false, 1, 100.0, 4_000));
        assert_eq!(rest_length(&sets, 3), Some(Seconds::new(90)));
        // The last member skipped the other way round: chin-up rests with its own 90 s.
        let press: BTreeSet<ExerciseId> = [ExerciseId::new("press").unwrap()].into();
        let chin = vec![logged("chin-up", false, 0, 0.0, 3_000)];
        let next = current_step(&steps, &plan, &chin, &press);
        assert_eq!(
            rest_after(&plan, &steps, 2, next, Seconds::new(120)),
            Some(Seconds::new(90))
        );
    }

    /// Re-check of #99: a last member with rest 0 still rests between rounds (the default).
    #[test]
    fn last_member_rest_zero_takes_default() {
        let mut plan = superset_day();
        plan.exercises[1].exercise.rest = Seconds::new(0); // chin-up, the last member
        let steps = steps(&plan, kg(20.0));
        // [press wu, press 1, chin 1, press 2, chin 2, curl 1]: after chin 1, before press 2.
        assert_eq!(
            rest_after(&plan, &steps, 2, Some(3), Seconds::new(120)),
            Some(Seconds::new(120))
        );
        // And after the superset, before curl.
        assert_eq!(
            rest_after(&plan, &steps, 4, Some(5), Seconds::new(120)),
            Some(Seconds::new(120))
        );
        // Inside the round, still none.
        assert_eq!(
            rest_after(&plan, &steps, 1, Some(2), Seconds::new(120)),
            None
        );
    }

    fn three_member_day() -> SessionPlan {
        let mut press = planned("press", 0, 2, Some("a"));
        press.exercise.rest = Seconds::new(0);
        let mut row = planned("row", 0, 2, Some("a"));
        row.exercise.rest = Seconds::new(0);
        let mut chin = planned("chin-up", 0, 2, Some("a"));
        chin.exercise.rest = Seconds::new(90);
        plan(vec![press, row, chin])
    }

    /// Re-check of #99: with the last member skipped, the round ends after the one before it.
    #[test]
    fn three_members_last_skipped() {
        let plan = three_member_day();
        let steps = steps(&plan, kg(20.0));
        let skipped: BTreeSet<ExerciseId> = [ExerciseId::new("chin-up").unwrap()].into();
        let sets = vec![
            logged("press", false, 0, 100.0, 2_000),
            logged("row", false, 0, 100.0, 3_000),
        ];
        let next = current_step(&steps, &plan, &sets, &skipped); // press 2
        assert_eq!(next, Some(3));
        assert_eq!(
            rest_after(&plan, &steps, 1, next, Seconds::new(120)),
            Some(Seconds::new(90))
        );
        // press 1 → row 1 stays a transition.
        let first = vec![logged("press", false, 0, 100.0, 2_000)];
        let next = current_step(&steps, &plan, &first, &skipped);
        assert_eq!(rest_after(&plan, &steps, 0, next, Seconds::new(120)), None);
    }

    /// A skipped middle member does not move the round: press → chin-up inside, rest after chin-up.
    #[test]
    fn three_members_middle_skipped() {
        let plan = three_member_day();
        let steps = steps(&plan, kg(20.0));
        // [press 1, row 1, chin 1, press 2, row 2, chin 2]
        let skipped: BTreeSet<ExerciseId> = [ExerciseId::new("row").unwrap()].into();
        let press = vec![logged("press", false, 0, 100.0, 2_000)];
        let next = current_step(&steps, &plan, &press, &skipped);
        assert_eq!(next, Some(2));
        assert_eq!(rest_after(&plan, &steps, 0, next, Seconds::new(120)), None);
        let chin = vec![
            logged("press", false, 0, 100.0, 2_000),
            logged("chin-up", false, 0, 0.0, 3_000),
        ];
        let next = current_step(&steps, &plan, &chin, &skipped);
        assert_eq!(next, Some(3));
        assert_eq!(
            rest_after(&plan, &steps, 2, next, Seconds::new(120)),
            Some(Seconds::new(90))
        );
    }

    #[test]
    fn the_rest_screen_reads_like_the_board() {
        let plan = plan(vec![
            planned("back-squat", 0, 5, None),
            planned("bench-press", 0, 3, None),
        ]);
        let steps = steps(&plan, kg(20.0));
        let set = logged("back-squat", false, 1, 100.0, 2_000);
        assert_eq!(
            rest_header(&plan, &set),
            ("REST · BACK SQUAT".to_owned(), "Set 2 logged ✓".to_owned())
        );
        assert_eq!(
            up_next(&steps, 2, 0, &plan, Unit::Kg),
            ("UP NEXT · SET 3 / 5".to_owned(), "5 × 100 kg".to_owned())
        );
        assert_eq!(
            up_next(&steps, 5, 0, &plan, Unit::Kg),
            (
                "UP NEXT · BENCH PRESS · SET 1 / 3".to_owned(),
                "5 × 100 kg".to_owned()
            )
        );
        // 2:14 of 3:00, as on the board.
        assert_eq!(
            rest_left_percent(Duration::from_secs(134), Duration::from_secs(180)),
            74
        );
        assert_eq!(
            rest_left_percent(Duration::ZERO, Duration::from_secs(180)),
            0
        );
        assert_eq!(
            rest_left_percent(Duration::from_secs(180), Duration::from_secs(180)),
            100
        );
        assert_eq!(rest_left_percent(Duration::ZERO, Duration::ZERO), 0);
    }
}
