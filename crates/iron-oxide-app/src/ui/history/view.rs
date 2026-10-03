//! What the history screens show, as plain data (#33): dates, durations, set lines, names, the
//! chart series and their table fallback. Pure, so it is unit-tested; the components only lay it
//! out.

use std::collections::{HashMap, HashSet};

use iron_oxide_domain::{
    DayId, ExerciseId, LoggedSet, ProgramId, SessionId, SessionStatus, Unit, Volume, Weight,
    entitlements::{Entitlements, Feature},
    program::Program,
    time::Timestamp,
};

use super::chart::WeightPoint;
use crate::api::error::FailureKind;
use crate::api::history::{ExerciseLog, ExerciseSeries, SessionSummary};
use crate::ui::weight::{estimate_number, estimate_text, weight_number, weight_text};

const MS_PER_MINUTE: i64 = 60_000;
const MS_PER_DAY: i64 = 86_400_000;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
/// Weekdays from Monday.
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// A calendar date in the user's time zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalDate {
    pub year: i64,
    /// 1 to 12.
    pub month: u32,
    /// 1 to 31.
    pub day: u32,
    /// 0 (Monday) to 6 (Sunday).
    pub weekday: usize,
}

impl LocalDate {
    /// The date of `at_ms` (ms since the epoch, UTC) at `offset_minutes` east of UTC.
    #[must_use]
    pub fn of(at_ms: i64, offset_minutes: i32) -> Self {
        let local = at_ms.saturating_add(i64::from(offset_minutes) * MS_PER_MINUTE);
        let days = local.div_euclid(MS_PER_DAY);
        // 1970-01-01 was a Thursday (3, counting from Monday).
        let weekday = usize::try_from((days + 3).rem_euclid(7)).unwrap_or(0);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            weekday,
        }
    }

    /// `Sat 3 Oct 2026`.
    #[must_use]
    pub fn long(self) -> String {
        format!(
            "{} {} {} {}",
            WEEKDAYS[self.weekday % 7],
            self.day,
            self.month_name(),
            self.year
        )
    }

    /// `3 Oct 2026`.
    #[must_use]
    pub fn short(self) -> String {
        format!("{} {} {}", self.day, self.month_name(), self.year)
    }

    fn month_name(self) -> &'static str {
        MONTHS[usize::try_from(self.month.clamp(1, 12) - 1).unwrap_or(0)]
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day). Howard Hinnant's
/// `civil_from_days`, exact for every `i64` day count we can meet.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// How long a session took: `48 min`, `1 h 05 min`. `None` while it has no end.
#[must_use]
pub fn duration_text(started: Timestamp, finished: Option<Timestamp>) -> Option<String> {
    let finished = finished?;
    let minutes = finished.saturating_duration_since(started).as_secs() / 60;
    Some(match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes} min"),
        (hours, minutes) => format!("{hours} h {minutes:02} min"),
    })
}

/// How a session ended, when it did not end normally.
#[must_use]
pub const fn status_label(status: SessionStatus) -> Option<&'static str> {
    match status {
        SessionStatus::Completed => None,
        SessionStatus::InProgress => Some("In progress"),
        SessionStatus::Skipped => Some("Skipped"),
        SessionStatus::Abandoned => Some("Abandoned"),
    }
}

/// A slug as a name, when the program does not give one: `back-squat` → `Back squat`.
#[must_use]
pub fn humanise(slug: &str) -> String {
    let words = slug.replace(['-', '_'], " ");
    let mut chars = words.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// Day and exercise names from the programs' documents, with the slug as a fallback.
///
/// A session keeps its program version's day id; the name comes from the program's current
/// version (renaming a day in a later version renames it in the history too).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    days: HashMap<(ProgramId, DayId), String>,
    exercises: HashMap<ExerciseId, String>,
    /// Programs whose names are loaded or being loaded.
    requested: HashSet<ProgramId>,
}

impl Names {
    /// Adds the names of `program`'s document.
    pub fn learn(&mut self, program_id: ProgramId, document: &Program) {
        self.requested.insert(program_id);
        for day in &document.days {
            self.days
                .insert((program_id, day.id.clone()), day.name.clone());
            for exercise in &day.exercises {
                self.exercises
                    .entry(exercise.id.clone())
                    .or_insert_with(|| exercise.name.clone());
            }
        }
    }

    /// The programs among `ids` not loaded nor being loaded yet, each once; marks them requested.
    pub fn claim_unrequested(
        &mut self,
        ids: impl IntoIterator<Item = ProgramId>,
    ) -> Vec<ProgramId> {
        ids.into_iter()
            .filter(|id| self.requested.insert(*id))
            .collect()
    }

    #[must_use]
    pub fn day(&self, program_id: ProgramId, day_id: &DayId) -> String {
        self.days
            .get(&(program_id, day_id.clone()))
            .cloned()
            .unwrap_or_else(|| humanise(day_id.as_str()))
    }

    #[must_use]
    pub fn exercise(&self, id: &ExerciseId) -> String {
        self.exercises
            .get(id)
            .cloned()
            .unwrap_or_else(|| humanise(id.as_str()))
    }
}

/// Appends a page of sessions to those already shown, skipping any already there (a page fetched
/// twice adds nothing).
pub fn append_page(shown: &mut Vec<SessionSummary>, page: Vec<SessionSummary>) {
    let known: HashSet<SessionId> = shown.iter().map(|session| session.id).collect();
    shown.extend(
        page.into_iter()
            .filter(|session| !known.contains(&session.id)),
    );
}

/// One set as the details screen writes it: `100 kg × 5`, `BW × 12`, `60 s`, `20 kg · 45 s`.
#[must_use]
pub fn set_text<T>(set: &LoggedSet<T>, unit: Unit) -> String {
    let reps = set.reps.get();
    match (set.weight, set.duration) {
        (Some(weight), None) => format!("{} × {reps}", weight_text(weight, unit)),
        (None, None) => format!("BW × {reps}"),
        (None, Some(duration)) => format!("{} s", duration.get()),
        (Some(weight), Some(duration)) => {
            format!("{} · {} s", weight_text(weight, unit), duration.get())
        }
    }
}

/// The sets of an exercise as numbered rows: warm-ups first (`W1`, `W2`), then working sets
/// (`1`, `2`), each in completion order.
#[must_use]
pub fn set_rows<T>(sets: &[LoggedSet<T>], unit: Unit) -> Vec<SetRow> {
    let mut warm_ups = 0;
    let mut working = 0;
    let mut rows: Vec<SetRow> = sets
        .iter()
        .map(|set| {
            let label = if set.warm_up {
                warm_ups += 1;
                format!("W{warm_ups}")
            } else {
                working += 1;
                working.to_string()
            };
            SetRow {
                label,
                text: set_text(set, unit),
                warm_up: set.warm_up,
                failed: set.reps.get() == 0,
            }
        })
        .collect();
    // Stable: each group keeps its order.
    rows.sort_by_key(|row| !row.warm_up);
    rows
}

/// Whether a failed chart request means the charts are locked. Only the plan says so: a `403`
/// is a plan refusal only if entitlements fetched *after* it (`refreshed`) leave the charts out.
/// Any other `403` (the cross-site request guard, say) is an ordinary error, shown as such.
#[must_use]
pub fn charts_locked_after(failure: FailureKind, refreshed: Option<&Entitlements>) -> bool {
    failure == FailureKind::Forbidden
        && refreshed.is_some_and(|entitlements| !entitlements.allows(Feature::ExerciseCharts))
}

/// A row of the sets of one exercise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetRow {
    /// `W1` for a warm-up, `1` for a working set.
    pub label: String,
    pub text: String,
    pub warm_up: bool,
    /// No rep done.
    pub failed: bool,
}

/// The session's total volume: the sum of its exercises' (working sets only).
#[must_use]
pub fn session_volume(exercises: &[ExerciseLog]) -> Volume {
    exercises.iter().map(|log| log.volume).sum()
}

/// `12 450 kg`: a volume with thin grouping, readable at a glance.
#[must_use]
pub fn volume_text(volume: Volume, unit: Unit) -> String {
    format!(
        "{} {}",
        group_thousands(&volume.format_value(unit, 0)),
        unit.symbol()
    )
}

/// Groups the digits of a whole number by three with a narrow no-break space: `12 450`.
fn group_thousands(digits: &str) -> String {
    let count = digits.chars().count();
    let mut out = String::with_capacity(digits.len() + count / 3 * 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (count - index).is_multiple_of(3) {
            out.push('\u{202f}');
        }
        out.push(digit);
    }
    out
}

/// The two lines of an exercise's charts, oldest first: the top set's weight, and the best
/// estimated one-rep max (sessions without an estimate, such as 20-rep sets, have no e1RM point).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChartSeries {
    pub top_set: Vec<WeightPoint>,
    pub e1rm: Vec<WeightPoint>,
}

/// The chart lines of `series` (the server sends it oldest first, one point per session).
#[must_use]
pub fn chart_series(series: &ExerciseSeries) -> ChartSeries {
    let mut out = ChartSeries::default();
    for point in &series.points {
        let at_ms = point.key.started_at.epoch_millis();
        out.top_set.push(WeightPoint {
            at_ms,
            weight: point.top_set.weight,
        });
        if let Some(weight) = point.best_e1rm {
            out.e1rm.push(WeightPoint { at_ms, weight });
        }
    }
    out
}

/// What a chart line measures, which decides how its values are written: a load lifted is exact
/// (`102.25 kg`), an estimate is rounded to 0.5 kg or 1 lb (`169 kg`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    Load,
    Estimate,
}

impl Measure {
    #[must_use]
    pub fn text(self, weight: Weight, unit: Unit) -> String {
        match self {
            Self::Load => weight_text(weight, unit),
            Self::Estimate => estimate_text(weight, unit),
        }
    }

    #[must_use]
    pub fn number(self, weight: Weight, unit: Unit) -> String {
        match self {
            Self::Load => weight_number(weight, unit),
            Self::Estimate => estimate_number(weight, unit),
        }
    }
}

/// A row of the data table under the charts (their accessible fallback), newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesRow {
    /// The session, the row's key.
    pub session_id: SessionId,
    pub at_ms: i64,
    /// `100 kg × 5`.
    pub top_set: String,
    /// `116.5 kg` (rounded like every estimate), or `—` without an estimate.
    pub e1rm: String,
}

#[must_use]
pub fn series_rows(series: &ExerciseSeries, unit: Unit) -> Vec<SeriesRow> {
    series
        .points
        .iter()
        .rev()
        .map(|point| SeriesRow {
            session_id: point.key.session_id,
            at_ms: point.key.started_at.epoch_millis(),
            top_set: format!(
                "{} × {}",
                weight_text(point.top_set.weight, unit),
                point.top_set.reps.get()
            ),
            e1rm: point
                .best_e1rm
                .map_or_else(|| "—".to_owned(), |weight| estimate_text(weight, unit)),
        })
        .collect()
}

/// A sentence for screen readers summing up a line: where it started, where it is, and its best.
/// `date` writes a time (ms) as a date.
#[must_use]
pub fn chart_summary(
    what: &str,
    points: &[WeightPoint],
    measure: Measure,
    unit: Unit,
    date: impl Fn(i64) -> String,
) -> String {
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        return format!("{what}: no data yet.");
    };
    let best = points
        .iter()
        .max_by_key(|point| point.weight)
        .unwrap_or(last);
    let sessions = match points.len() {
        1 => "1 session".to_owned(),
        count => format!("{count} sessions"),
    };
    if points.len() == 1 {
        return format!(
            "{what}: {} on {}, {sessions}.",
            measure.text(first.weight, unit),
            date(first.at_ms)
        );
    }
    format!(
        "{what} over {sessions}, from {} on {} to {} on {}. Best {} on {}.",
        measure.text(first.weight, unit),
        date(first.at_ms),
        measure.text(last.weight, unit),
        date(last.at_ms),
        measure.text(best.weight, unit),
        date(best.at_ms),
    )
}

/// The latest value of a line, for the big number above its chart.
#[must_use]
pub fn latest_number(points: &[WeightPoint], measure: Measure, unit: Unit) -> Option<String> {
    points
        .last()
        .map(|point| measure.number(point.weight, unit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iron_oxide_domain::entitlements::{FeatureAccess, Plan};
    use iron_oxide_domain::{Lift, ProgramVersionId, Reps, Seconds, SeriesPoint, SetId};

    use crate::api::history::SeriesKey;

    fn kg(value: f64) -> Weight {
        Weight::from_kg(value).unwrap()
    }

    fn ts(ms: i64) -> Timestamp {
        Timestamp::from_epoch_millis(ms)
    }

    fn uuid(n: u128) -> uuid::Uuid {
        uuid::Uuid::from_u128(n)
    }

    #[test]
    fn dates_in_utc() {
        // 2026-10-03T12:00:00Z, a Saturday.
        let at = 1_791_028_800_000;
        let date = LocalDate::of(at, 0);
        assert_eq!((date.year, date.month, date.day), (2026, 10, 3));
        assert_eq!(date.long(), "Sat 3 Oct 2026");
        assert_eq!(date.short(), "3 Oct 2026");
        assert_eq!(LocalDate::of(0, 0).long(), "Thu 1 Jan 1970");
        // Leap day.
        assert_eq!(
            LocalDate::of(1_709_164_800_000, 0).long(),
            "Thu 29 Feb 2024"
        );
        // Before the epoch.
        assert_eq!(LocalDate::of(-1, 0).long(), "Wed 31 Dec 1969");
    }

    #[test]
    fn dates_follow_the_offset() {
        // 2026-10-03T23:30Z is already Sunday in Paris (+120) and still Saturday in New York.
        let at = 1_791_070_200_000;
        assert_eq!(LocalDate::of(at, 0).long(), "Sat 3 Oct 2026");
        assert_eq!(LocalDate::of(at, 120).long(), "Sun 4 Oct 2026");
        assert_eq!(LocalDate::of(at, -240).long(), "Sat 3 Oct 2026");
        // 2026-10-03T02:00Z is still Friday in New York.
        assert_eq!(
            LocalDate::of(1_790_992_800_000, -240).long(),
            "Fri 2 Oct 2026"
        );
    }

    #[test]
    fn durations_in_minutes_and_hours() {
        let start = ts(0);
        assert_eq!(duration_text(start, None), None);
        assert_eq!(duration_text(start, Some(ts(59_999))).unwrap(), "0 min");
        assert_eq!(
            duration_text(start, Some(ts(48 * 60_000))).unwrap(),
            "48 min"
        );
        assert_eq!(
            duration_text(start, Some(ts(65 * 60_000))).unwrap(),
            "1 h 05 min"
        );
        assert_eq!(
            duration_text(start, Some(ts(120 * 60_000))).unwrap(),
            "2 h 00 min"
        );
        // A clock that went backwards reads 0, not a panic.
        assert_eq!(duration_text(ts(10_000), Some(ts(0))).unwrap(), "0 min");
    }

    #[test]
    fn statuses_other_than_completed_are_labelled() {
        assert_eq!(status_label(SessionStatus::Completed), None);
        assert_eq!(status_label(SessionStatus::Skipped), Some("Skipped"));
        assert_eq!(status_label(SessionStatus::Abandoned), Some("Abandoned"));
    }

    #[test]
    fn slugs_read_as_names() {
        assert_eq!(humanise("back-squat"), "Back squat");
        assert_eq!(humanise("day_a"), "Day a");
        assert_eq!(humanise("a"), "A");
        assert_eq!(humanise(""), "");
    }

    fn program() -> Program {
        iron_oxide_domain::program::builtin_programs().unwrap()[0]
            .program()
            .clone()
    }

    #[test]
    fn names_come_from_the_program_then_the_slug() {
        let document = program();
        let day = &document.days[0];
        let exercise = &day.exercises[0];
        let id = ProgramId::from_uuid(uuid(1));
        let mut names = Names::default();
        assert_eq!(names.day(id, &day.id), humanise(day.id.as_str()));
        names.learn(id, &document);
        assert_eq!(names.day(id, &day.id), day.name);
        assert_eq!(names.exercise(&exercise.id), exercise.name);
        // Another program's day of the same id is not named by this one.
        let other = ProgramId::from_uuid(uuid(2));
        assert_eq!(names.day(other, &day.id), humanise(day.id.as_str()));
        let unknown = ExerciseId::new("zercher-squat").unwrap();
        assert_eq!(names.exercise(&unknown), "Zercher squat");
    }

    #[test]
    fn programs_are_requested_once() {
        let (a, b) = (ProgramId::from_uuid(uuid(1)), ProgramId::from_uuid(uuid(2)));
        let mut names = Names::default();
        assert_eq!(names.claim_unrequested([a, a, b]), [a, b]);
        assert!(names.claim_unrequested([a, b]).is_empty());
        let c = ProgramId::from_uuid(uuid(3));
        names.learn(c, &program());
        assert!(names.claim_unrequested([c]).is_empty());
    }

    fn summary(n: u128) -> SessionSummary {
        SessionSummary {
            id: SessionId::from_uuid(uuid(n)),
            program_id: ProgramId::from_uuid(uuid(100)),
            program_name: "P".to_owned(),
            program_version_id: ProgramVersionId::from_uuid(uuid(200)),
            program_version: 1,
            day_id: DayId::new("a").unwrap(),
            status: SessionStatus::Completed,
            started_at: ts(0),
            finished_at: Some(ts(1)),
            working_sets: 0,
        }
    }

    #[test]
    fn pages_append_without_duplicates() {
        let mut shown = vec![summary(1), summary(2)];
        append_page(&mut shown, vec![summary(2), summary(3)]);
        let ids: Vec<_> = shown.iter().map(|s| s.id).collect();
        assert_eq!(ids, [1, 2, 3].map(|n| SessionId::from_uuid(uuid(n))));
        append_page(&mut shown, Vec::new());
        assert_eq!(shown.len(), 3);
    }

    fn set(
        index: u128,
        reps: u16,
        weight: Option<f64>,
        duration: Option<u32>,
        warm_up: bool,
    ) -> LoggedSet<Timestamp> {
        LoggedSet {
            id: SetId::from_uuid(uuid(index)),
            exercise: ExerciseId::new("back-squat").unwrap(),
            set_index: 0,
            reps: Reps::new(reps),
            weight: weight.map(kg),
            duration: duration.map(Seconds::new),
            warm_up,
            completed_at: ts(0),
            target: None,
        }
    }

    #[test]
    fn sets_read_in_the_users_unit() {
        assert_eq!(
            set_text(&set(1, 5, Some(100.0), None, false), Unit::Kg),
            "100 kg × 5"
        );
        assert_eq!(
            set_text(&set(1, 5, Some(100.0), None, false), Unit::Lb),
            "220.46 lb × 5"
        );
        assert_eq!(
            set_text(&set(1, 12, None, None, false), Unit::Kg),
            "BW × 12"
        );
        assert_eq!(
            set_text(&set(1, 1, None, Some(60), false), Unit::Kg),
            "60 s"
        );
        assert_eq!(
            set_text(&set(1, 1, Some(20.0), Some(45), false), Unit::Kg),
            "20 kg · 45 s"
        );
    }

    fn log(sets: Vec<LoggedSet<Timestamp>>) -> ExerciseLog {
        ExerciseLog {
            exercise_id: ExerciseId::new("back-squat").unwrap(),
            sets,
            top_set: None,
            best_e1rm: None,
            volume: Volume::ZERO,
        }
    }

    #[test]
    fn warm_ups_come_first_and_are_numbered_apart() {
        let rows = set_rows(
            &[
                set(1, 5, Some(60.0), None, true),
                set(2, 5, Some(100.0), None, false),
                set(3, 3, Some(80.0), None, true),
                set(4, 0, Some(100.0), None, false),
            ],
            Unit::Kg,
        );
        let labels: Vec<_> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels, ["W1", "W2", "1", "2"]);
        assert_eq!(rows[1].text, "80 kg × 3");
        assert!(rows[3].failed && !rows[2].failed);
    }

    #[test]
    fn session_volume_sums_the_exercises() {
        let mut a = log(Vec::new());
        a.volume = Volume::of(kg(100.0), Reps::new(5));
        let mut b = log(Vec::new());
        b.volume = Volume::of(kg(50.0), Reps::new(10));
        assert_eq!(
            session_volume(&[a, b]),
            Volume::of(kg(1000.0), Reps::new(1))
        );
        assert_eq!(session_volume(&[]), Volume::ZERO);
    }

    #[test]
    fn volumes_are_grouped_by_thousands() {
        let volume = Volume::of(kg(249.0), Reps::new(50));
        assert_eq!(volume_text(volume, Unit::Kg), "12\u{202f}450 kg");
        assert_eq!(
            volume_text(Volume::of(kg(100.0), Reps::new(5)), Unit::Kg),
            "500 kg"
        );
        assert_eq!(volume_text(Volume::ZERO, Unit::Kg), "0 kg");
        // 500 kg is 1102.31 lb.
        let lb = volume_text(Volume::of(kg(100.0), Reps::new(5)), Unit::Lb);
        assert_eq!(lb, "1\u{202f}102 lb");
        assert_eq!(group_thousands("1234567"), "1\u{202f}234\u{202f}567");
    }

    fn series() -> ExerciseSeries {
        let point = |n: u128, ms: i64, weight: f64, reps: u16, e1rm: Option<f64>| SeriesPoint {
            key: SeriesKey {
                started_at: ts(ms),
                session_id: SessionId::from_uuid(uuid(n)),
            },
            top_set: Lift {
                weight: kg(weight),
                reps: Reps::new(reps),
            },
            best_e1rm: e1rm.map(kg),
        };
        ExerciseSeries {
            exercise_id: ExerciseId::new("back-squat").unwrap(),
            points: vec![
                point(1, 1_000, 100.0, 5, Some(116.67)),
                point(2, 2_000, 60.0, 20, None),
                point(3, 3_000, 105.0, 5, Some(122.5)),
            ],
        }
    }

    #[test]
    fn the_series_has_a_top_set_line_and_an_e1rm_line() {
        let lines = chart_series(&series());
        let top: Vec<_> = lines.top_set.iter().map(|p| (p.at_ms, p.weight)).collect();
        assert_eq!(
            top,
            [(1_000, kg(100.0)), (2_000, kg(60.0)), (3_000, kg(105.0))]
        );
        // The 20-rep session has no estimate, so no e1RM point.
        let e1rm: Vec<_> = lines.e1rm.iter().map(|p| (p.at_ms, p.weight)).collect();
        assert_eq!(e1rm, [(1_000, kg(116.67)), (3_000, kg(122.5))]);
        let empty = ExerciseSeries {
            exercise_id: ExerciseId::new("back-squat").unwrap(),
            points: Vec::new(),
        };
        assert_eq!(chart_series(&empty), ChartSeries::default());
    }

    #[test]
    fn the_table_is_newest_first_in_the_users_unit() {
        let rows = series_rows(&series(), Unit::Lb);
        assert_eq!(rows[0].at_ms, 3_000);
        assert_eq!(rows[0].top_set, "231.49 lb × 5");
        assert_eq!(rows[0].session_id, SessionId::from_uuid(uuid(3)));
        // Estimates are rounded: 122.5 kg = 270.07 lb → 270 lb.
        assert_eq!(rows[0].e1rm, "270 lb");
        assert_eq!(rows[1].e1rm, "—");
        assert_eq!(rows[2].top_set, "220.46 lb × 5");
    }

    #[test]
    fn summaries_say_where_the_line_goes() {
        let date = |ms: i64| format!("t{ms}");
        let lines = chart_series(&series());
        assert_eq!(
            chart_summary("Top set", &lines.top_set, Measure::Load, Unit::Kg, date),
            "Top set over 3 sessions, from 100 kg on t1000 to 105 kg on t3000. \
             Best 105 kg on t3000."
        );
        assert_eq!(
            chart_summary(
                "Top set",
                &lines.top_set[..1],
                Measure::Load,
                Unit::Kg,
                date
            ),
            "Top set: 100 kg on t1000, 1 session."
        );
        assert_eq!(
            chart_summary("e1RM", &[], Measure::Estimate, Unit::Kg, date),
            "e1RM: no data yet."
        );
        assert_eq!(
            chart_summary("e1RM", &lines.e1rm, Measure::Estimate, Unit::Kg, date),
            "e1RM over 2 sessions, from 116.5 kg on t1000 to 122.5 kg on t3000. \
             Best 122.5 kg on t3000."
        );
        assert_eq!(
            latest_number(&lines.e1rm[..1], Measure::Estimate, Unit::Kg).unwrap(),
            "116.5"
        );
        assert_eq!(
            latest_number(&lines.e1rm[..1], Measure::Load, Unit::Kg).unwrap(),
            "116.67"
        );
        assert_eq!(latest_number(&[], Measure::Load, Unit::Kg), None);
    }

    fn entitlements(charts: bool) -> Entitlements {
        let mut entitlements = Entitlements::of(Plan::Free);
        entitlements.features = vec![FeatureAccess {
            feature: Feature::ExerciseCharts,
            allowed: charts,
        }];
        entitlements
    }

    #[test]
    fn only_the_plan_locks_the_charts() {
        // The plan changed meanwhile: the refreshed entitlements leave the charts out.
        assert!(charts_locked_after(
            FailureKind::Forbidden,
            Some(&entitlements(false))
        ));
        // A 403 while the plan still includes the charts (the CSRF guard): an ordinary error.
        assert!(!charts_locked_after(
            FailureKind::Forbidden,
            Some(&entitlements(true))
        ));
        // The plan could not be read again: not locked, the error is shown.
        assert!(!charts_locked_after(FailureKind::Forbidden, None));
        // Other failures never lock, whatever the plan.
        for kind in [
            FailureKind::Unauthorized,
            FailureKind::NotFound,
            FailureKind::Transient,
            FailureKind::Network,
            FailureKind::Other,
        ] {
            assert!(
                !charts_locked_after(kind, Some(&entitlements(false))),
                "{kind:?}"
            );
        }
    }
}
