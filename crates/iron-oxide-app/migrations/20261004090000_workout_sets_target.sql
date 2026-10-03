-- What the app prescribed for each set when it was logged (#60): the target the lifter saw (the
-- progression's prefill, or last session's set), snapshot with the set. The progression engine
-- judges a training max session's set against it exactly ("lifted at least what was prescribed
-- then"), so the verdict no longer needs a tolerance and the rounding step no longer needs the
-- 2.5 kg cap that tolerance relied on.
--
-- - `target_goal`: the domain `SetGoal` JSON (reps with the program's range, a hold, intervals),
--   checked by the domain when it is written. NULL means **no target was recorded**: every set
--   logged before #60, and sets logged without a prescription (an extra set, an added exercise,
--   a client that does not send one). Such sets keep the legacy rule: compared with the exact
--   prescribed weight less 1.25 kg.
-- - `target_weight_ng`: the target's load (a `Weight`, exact nanograms like `weight_ng`), NULL for
--   a body-weight target, and always NULL when there is no target.
--
-- No backfill: the target a past set was shown depended on the settings and the training max of
-- that day, which are not stored; recomputing it now could judge an old session differently from
-- what the lifter was told.
ALTER TABLE workout_sets
    ADD COLUMN target_weight_ng bigint CHECK (is_weight_ng(target_weight_ng)),
    ADD COLUMN target_goal jsonb CHECK (
        jsonb_typeof(target_goal) = 'object' AND octet_length(target_goal::text) <= 1024
    ),
    ADD CONSTRAINT workout_sets_target_weight_needs_a_target
        CHECK (target_weight_ng IS NULL OR target_goal IS NOT NULL);
