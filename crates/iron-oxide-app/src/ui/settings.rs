//! The Settings page (#34). The settings themselves, shared by every screen, are loaded and saved
//! by [`crate::ui::user_settings`]; the page shows them and changes them through
//! [`UserSettings::change`].

use dioxus::prelude::*;
use iron_oxide_domain::entitlements::{Entitlements, Limit, Quota};
use iron_oxide_domain::program::{Load, Program};
use iron_oxide_domain::{
    ExerciseId, PlateInventory, PlateInventoryError, PlateStock, Seconds, Unit, Weight,
};

use super::account::Account;
use super::appearance::AppearanceCard;
use super::components::{Button, ButtonVariant, Card, Chip, LoadingState, Stepper, WeightStepper};
use super::errors::{BannerKind, use_errors};
use super::prefs::step_choices;
use super::shell::{SessionStatus, use_session};
use super::user_settings::{UserSettings, use_user_settings};
use super::weight::weight_text;
use crate::api::billing::my_entitlements;
use crate::api::programs::get_active_program;
use crate::api::settings::{
    MAX_DEFAULT_REST, Settings, TrainingMax, delete_training_max, set_training_max, training_maxes,
};

/// The default rest moves by this many seconds.
pub const REST_STEP: i64 = 15;

/// The shortest default rest the page offers.
pub const MIN_REST: i64 = 15;

/// `2:00`, `0:45`, `60:00`: a rest as minutes and seconds.
#[must_use]
pub fn rest_text(rest: Seconds) -> String {
    let seconds = rest.get();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The bar most gyms have in `unit`: 20 kg or 45 lb.
#[must_use]
pub fn standard_bar(unit: Unit) -> Weight {
    bar_choices(unit)
        .first()
        .and_then(|&value| Weight::new(value, unit).ok())
        .unwrap_or(Weight::ZERO)
}

/// The common bars, offered as one-tap choices: the standard one first.
#[must_use]
pub const fn bar_choices(unit: Unit) -> &'static [f64] {
    match unit {
        Unit::Kg => &[20.0, 15.0, 10.0],
        Unit::Lb => &[45.0, 35.0, 15.0],
    }
}

/// How far the bar stepper moves: 0.5 kg or 1 lb (bars come in odd sizes).
#[must_use]
pub fn bar_step(unit: Unit) -> Weight {
    let value = match unit {
        Unit::Kg => 0.5,
        Unit::Lb => 1.0,
    };
    Weight::new(value, unit).unwrap_or(Weight::ZERO)
}

/// The heaviest bar the stepper goes to.
#[must_use]
pub fn max_bar() -> Weight {
    Weight::from_kg(100.0).unwrap_or(Weight::MAX)
}

/// `settings` shown in `unit`. Weights stay as they are: a 20 kg bar is still 20 kg.
#[must_use]
pub fn with_unit(settings: &Settings, unit: Unit) -> Settings {
    Settings {
        unit,
        ..settings.clone()
    }
}

/// `settings` with the standard bar and plates of their unit.
#[must_use]
pub fn with_standard_equipment(settings: &Settings) -> Settings {
    Settings {
        bar_weight: standard_bar(settings.unit),
        plate_inventory: PlateInventory::default_for(settings.unit),
        ..settings.clone()
    }
}

/// Whether the bar and the plates are exactly the standard ones of `unit`.
#[must_use]
pub fn has_standard_equipment(settings: &Settings, unit: Unit) -> bool {
    settings.bar_weight == standard_bar(unit)
        && settings.plate_inventory == PlateInventory::default_for(unit)
}

/// Reads a weight typed in `unit` (`2.5`, `2,5`, `102.5`).
///
/// # Errors
/// A message for the user when it is not a positive number or not a valid weight.
pub fn parse_weight(input: &str, unit: Unit) -> Result<Weight, String> {
    let text = input.trim().replace(',', ".");
    if text.is_empty() {
        return Err("Enter a weight.".to_owned());
    }
    let value: f64 = text
        .parse()
        .map_err(|_| format!("\u{201c}{}\u{201d} is not a number.", input.trim()))?;
    if !value.is_finite() || value <= 0.0 {
        return Err("The weight must be more than zero.".to_owned());
    }
    Weight::new(value, unit).map_err(|error| format!("Not a valid weight: {error}."))
}

/// The inventory with `pairs` of `plate` added to what is there.
///
/// # Errors
/// The domain's reason ([`plate_problem`]) when the inventory would be invalid.
pub fn add_plate(
    inventory: &PlateInventory,
    plate: Weight,
    pairs: u32,
    unit: Unit,
) -> Result<PlateInventory, String> {
    let current = inventory.pairs_of(plate);
    let mut stock: Vec<PlateStock> = inventory
        .stock()
        .iter()
        .copied()
        .filter(|stock| stock.plate != plate)
        .collect();
    stock.push(PlateStock {
        plate,
        pairs: current.saturating_add(pairs),
    });
    PlateInventory::new(stock).map_err(|error| plate_problem(&error, unit))
}

/// The inventory with `pairs` pairs of `plate` (a size already in it).
///
/// # Errors
/// The domain's reason ([`plate_problem`]) when the inventory would be invalid.
pub fn set_pairs(
    inventory: &PlateInventory,
    plate: Weight,
    pairs: u32,
    unit: Unit,
) -> Result<PlateInventory, String> {
    let stock = inventory.stock().iter().map(|stock| PlateStock {
        plate: stock.plate,
        pairs: if stock.plate == plate {
            pairs
        } else {
            stock.pairs
        },
    });
    PlateInventory::new(stock).map_err(|error| plate_problem(&error, unit))
}

/// The inventory without `plate`, unless it is the last size (at least one must stay).
#[must_use]
pub fn remove_plate(inventory: &PlateInventory, plate: Weight) -> PlateInventory {
    if inventory.stock().len() <= 1 {
        return inventory.clone();
    }
    let stock = inventory
        .stock()
        .iter()
        .copied()
        .filter(|stock| stock.plate != plate);
    // Removing a size from a valid inventory keeps it valid.
    PlateInventory::new(stock).unwrap_or_else(|_| inventory.clone())
}

/// Why an inventory is refused, for the user, with weights in `unit`.
#[must_use]
pub fn plate_problem(error: &PlateInventoryError, unit: Unit) -> String {
    match error {
        PlateInventoryError::ZeroPlate => "A plate must weigh more than zero.".to_owned(),
        PlateInventoryError::DuplicatePlate(plate) => {
            format!("{} is already in the list.", weight_text(*plate, unit))
        }
        PlateInventoryError::OffGrid(plate) => format!(
            "{} is not a plate size: sizes go in steps of 0.025 kg or 0.125 lb.",
            weight_text(*plate, unit)
        ),
        PlateInventoryError::TooManyPairs { plate, max, .. } => format!(
            "At most {max} pairs of {} are allowed.",
            weight_text(*plate, unit)
        ),
        PlateInventoryError::TooManySizes { max } => {
            format!("At most {max} plate sizes are allowed.")
        }
    }
}

/// What the plan card says about the number of programs.
#[must_use]
pub fn programs_allowance(entitlements: &Entitlements) -> String {
    let limit = entitlements
        .limits
        .iter()
        .find(|limit| limit.quota == Quota::CustomPrograms)
        .map(|limit| limit.limit);
    match limit {
        Some(Limit::AtMost { max }) => {
            format!("Up to {max} active programs (archived ones don't count).")
        }
        Some(Limit::Unlimited) | None => "Unlimited programs.".to_owned(),
    }
}

/// What the cards change the settings through.
type Editor = UserSettings;

/// The Settings page.
#[component]
pub fn SettingsPage() -> Element {
    let user = use_user_settings();
    let editor = user;
    let session = use_session();

    let settings = user.get();
    rsx! {
        div { class: "io-page-header",
            h1 { class: "io-title", "Settings" }
        }
        match settings {
            Some(settings) => rsx! {
                UnitsCard { settings: settings.clone(), editor }
                BarCard { settings: settings.clone(), editor }
                PlatesCard { settings: settings.clone(), editor }
                TrainingMaxCard { unit: settings.unit }
                RestCard { settings, editor }
            },
            None if user.failed() => rsx! {
                Card { title: "Your settings",
                    p { class: "io-muted", "Your settings could not be loaded." }
                    Button {
                        variant: ButtonVariant::Secondary,
                        onclick: move |_| user.reload(),
                        "Try again"
                    }
                }
            },
            None if *session.read() == SessionStatus::Unverified => rsx! {
                Card { title: "Your settings",
                    p { class: "io-muted", "Your settings load as soon as the server can be reached." }
                }
            },
            None => rsx! { LoadingState { message: "Loading your settings…" } },
        }
        AppearanceCard {}
        Account {}
        PlanCard {}
        DataCard {}
    }
}

/// Two chips, On and Off.
#[component]
fn OnOff(#[props(into)] label: String, on: bool, on_change: EventHandler<bool>) -> Element {
    rsx! {
        div { class: "io-setting",
            span { class: "io-setting-name", "{label}" }
            div { class: "io-chips", role: "group", aria_label: "{label}",
                Chip { selected: on, onclick: move |_| on_change.call(true), "On" }
                Chip { selected: !on, onclick: move |_| on_change.call(false), "Off" }
            }
        }
    }
}

#[component]
fn UnitsCard(settings: Settings, editor: Editor) -> Element {
    let unit = settings.unit;
    // Set when the unit was just switched and the bar and plates are not the new unit's.
    let mut offer_standard = use_signal(|| false);
    let step = settings.weight_step(unit);
    rsx! {
        Card { title: "Units",
            div { class: "io-setting",
                span { class: "io-setting-name", "Weights in" }
                div { class: "io-chips", role: "group", aria_label: "Weight unit",
                    for choice in Unit::ALL {
                        Chip {
                            key: "{choice}",
                            selected: choice == unit,
                            onclick: move |_| {
                                editor.change(|settings| {
                                    if settings.unit != choice {
                                        offer_standard.set(!has_standard_equipment(&settings, choice));
                                    }
                                    with_unit(&settings, choice)
                                });
                            },
                            "{choice.symbol()}"
                        }
                    }
                }
            }
            if *offer_standard.read() && !has_standard_equipment(&settings, unit) {
                div { class: "io-waiting", role: "status",
                    p {
                        "Your bar and plates did not change ({weight_text(settings.bar_weight, unit)} bar). Use the standard {unit.symbol()} ones?"
                    }
                    Button {
                        variant: ButtonVariant::Primary,
                        onclick: move |_| {
                            offer_standard.set(false);
                            editor.change(|settings| with_standard_equipment(&settings));
                        },
                        "Use a {weight_text(standard_bar(unit), unit)} bar and {unit.symbol()} plates"
                    }
                    Button {
                        variant: ButtonVariant::Ghost,
                        onclick: move |_| offer_standard.set(false),
                        "Keep mine"
                    }
                }
            }
            div { class: "io-setting",
                span { class: "io-setting-name", "Weight step" }
                div { class: "io-chips", role: "group", aria_label: "Weight step",
                    for value in step_choices(unit).iter().copied() {
                        if let Ok(choice) = Weight::new(value, unit) {
                            Chip {
                                key: "{value}",
                                selected: choice == step,
                                onclick: move |_| {
                                    editor.change(|settings| settings.with_weight_step(unit, choice));
                                },
                                "{weight_text(choice, unit)}"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn BarCard(settings: Settings, editor: Editor) -> Element {
    let unit = settings.unit;
    let bar = settings.bar_weight;
    rsx! {
        Card { title: "Bar",
            WeightStepper {
                value: bar,
                step: bar_step(unit),
                min: bar_step(unit),
                max: max_bar(),
                on_change: move |weight| {
                    editor.change(|settings| Settings { bar_weight: weight, ..settings });
                },
            }
            div { class: "io-chips", role: "group", aria_label: "Common bars",
                for value in bar_choices(unit).iter().copied() {
                    if let Ok(choice) = Weight::new(value, unit) {
                        Chip {
                            key: "{value}",
                            selected: choice == bar,
                            onclick: move |_| {
                                editor.change(|settings| Settings { bar_weight: choice, ..settings });
                            },
                            "{weight_text(choice, unit)}"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn PlatesCard(settings: Settings, editor: Editor) -> Element {
    let unit = settings.unit;
    let mut input = use_signal(String::new);
    let mut problem = use_signal(|| None::<String>);

    let mut add = move || {
        let Some(current) = editor.peek() else {
            return;
        };
        let result = parse_weight(&input.peek(), unit)
            .and_then(|plate| add_plate(&current.plate_inventory, plate, 1, unit));
        match result {
            Ok(plate_inventory) => {
                problem.set(None);
                input.set(String::new());
                editor.change(|settings| Settings {
                    plate_inventory,
                    ..settings
                });
            }
            Err(message) => problem.set(Some(message)),
        }
    };
    let is_standard = settings.plate_inventory == PlateInventory::default_for(unit);

    rsx! {
        Card { title: "Plates",
            p { class: "io-muted", "The pairs you can load, for the plate calculator." }
            if settings.plate_inventory.is_empty() {
                p { class: "io-muted", "No plates yet: add at least one size." }
            }
            ul { class: "io-list",
                for stock in settings.plate_inventory.stock().iter().copied() {
                    PlateRow {
                        key: "{stock.plate.as_nanograms()}",
                        unit,
                        stock,
                        last: settings.plate_inventory.stock().len() <= 1,
                        editor,
                    }
                }
            }
            div { class: "io-field",
                label { r#for: "plate-size",
                    "Add a plate size "
                    span { class: "io-muted", "({unit.symbol()})" }
                }
                div { class: "io-inline",
                    input {
                        id: "plate-size",
                        class: "io-input",
                        r#type: "text",
                        inputmode: "decimal",
                        autocomplete: "off",
                        placeholder: if unit == Unit::Kg { "e.g. 0.5" } else { "e.g. 1.25" },
                        value: "{input}",
                        "aria-describedby": if problem.read().is_some() { "plate-problem" },
                        oninput: move |event| input.set(event.value()),
                        onkeydown: move |event: KeyboardEvent| {
                            if event.key() == Key::Enter {
                                add();
                            }
                        },
                    }
                    Button { variant: ButtonVariant::Secondary, onclick: move |_| add(), "Add" }
                }
                if let Some(message) = problem.read().clone() {
                    p { id: "plate-problem", class: "io-notice io-notice-error", role: "alert", "{message}" }
                }
            }
            if !is_standard {
                Button {
                    variant: ButtonVariant::Ghost,
                    onclick: move |_| {
                        editor.change(|settings| Settings {
                            plate_inventory: PlateInventory::default_for(unit),
                            ..settings
                        });
                    },
                    "Use the standard {unit.symbol()} plates"
                }
            }
        }
    }
}

#[component]
fn PlateRow(unit: Unit, stock: PlateStock, last: bool, editor: Editor) -> Element {
    let errors = use_errors();
    let plate = stock.plate;
    let name = weight_text(plate, unit);
    let pairs_label = if stock.pairs == 1 { "pair" } else { "pairs" };
    let set = move |pairs: u32| {
        let Some(current) = editor.peek() else {
            return;
        };
        match set_pairs(&current.plate_inventory, plate, pairs, unit) {
            Ok(plate_inventory) => editor.change(|settings| Settings {
                plate_inventory,
                ..settings
            }),
            Err(message) => {
                errors.show(BannerKind::Error, message);
            }
        }
    };
    rsx! {
        li { class: "io-row io-plate-row",
            div { class: "io-row-main",
                span { class: "io-row-title", "{name}" }
                span { class: "io-sr-only", "{stock.pairs} {pairs_label}" }
            }
            div { class: "io-mini-stepper", role: "group", aria_label: "Pairs of {name}",
                button {
                    r#type: "button",
                    class: "io-stepper-button io-stepper-minus",
                    aria_label: "One pair of {name} less",
                    disabled: stock.pairs <= 1,
                    onclick: move |_| set(stock.pairs.saturating_sub(1)),
                    "−"
                }
                output { aria_live: "polite", "{stock.pairs}" }
                button {
                    r#type: "button",
                    class: "io-stepper-button io-stepper-plus",
                    aria_label: "One pair of {name} more",
                    disabled: stock.pairs >= PlateInventory::MAX_PAIRS,
                    onclick: move |_| set(stock.pairs + 1),
                    "+"
                }
            }
            button {
                r#type: "button",
                class: "io-button io-button-danger",
                aria_label: "Remove {name} plates",
                disabled: last,
                title: if last { "Keep at least one plate size" },
                onclick: move |_| {
                    editor.change(|settings| Settings {
                        plate_inventory: remove_plate(&settings.plate_inventory, plate),
                        ..settings
                    });
                },
                "Remove"
            }
        }
    }
}

#[component]
fn RestCard(settings: Settings, editor: Editor) -> Element {
    let rest = settings.default_rest;
    rsx! {
        Card { title: "Rest timer",
            Stepper {
                label: "Default rest",
                value: i64::from(rest.get()),
                text: rest_text(rest),
                step: REST_STEP,
                min: MIN_REST,
                max: i64::from(MAX_DEFAULT_REST.get()),
                less_label: "{REST_STEP} seconds less",
                more_label: "{REST_STEP} seconds more",
                on_change: move |seconds: i64| {
                    let default_rest = Seconds::new(u32::try_from(seconds).unwrap_or(0));
                    editor.change(|settings| Settings { default_rest, ..settings });
                },
            }
            p { class: "io-muted io-hint", "Used when the program does not say how long to rest." }
            OnOff {
                label: "Sound",
                on: settings.sound_enabled,
                on_change: move |sound_enabled| editor.change(|settings| Settings { sound_enabled, ..settings }),
            }
            OnOff {
                label: "Vibration",
                on: settings.vibration_enabled,
                on_change: move |vibration_enabled| {
                    editor.change(|settings| Settings { vibration_enabled, ..settings });
                },
            }
        }
    }
}

/// One line of the training max card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainingMaxRow {
    pub exercise_id: ExerciseId,
    pub name: String,
    pub weight: Option<Weight>,
}

/// The training maxes to show: every exercise of the active program loaded as a percentage of a
/// training max (in program order, once each), then any other exercise the user has one for.
#[must_use]
pub fn training_max_rows(program: Option<&Program>, maxes: &[TrainingMax]) -> Vec<TrainingMaxRow> {
    let weight_of = |id: &ExerciseId| {
        maxes
            .iter()
            .find(|max| &max.exercise_id == id)
            .map(|max| max.weight)
    };
    let mut rows: Vec<TrainingMaxRow> = Vec::new();
    let exercises = program
        .into_iter()
        .flat_map(|program| program.days.iter())
        .flat_map(|day| day.exercises.iter())
        .filter(|exercise| exercise.load.is_some_and(Load::is_percent_of_training_max));
    for exercise in exercises {
        if rows.iter().all(|row| row.exercise_id != exercise.id) {
            rows.push(TrainingMaxRow {
                exercise_id: exercise.id.clone(),
                name: exercise.name.clone(),
                weight: weight_of(&exercise.id),
            });
        }
    }
    for max in maxes {
        if rows.iter().all(|row| row.exercise_id != max.exercise_id) {
            rows.push(TrainingMaxRow {
                exercise_id: max.exercise_id.clone(),
                name: max.exercise_id.to_string(),
                weight: Some(max.weight),
            });
        }
    }
    rows
}

/// What the training max card needs: the active program and the saved training maxes.
#[derive(Debug, Clone, PartialEq)]
struct TrainingMaxData {
    program: Option<Program>,
    maxes: Vec<TrainingMax>,
}

async fn load_training_maxes() -> Result<TrainingMaxData, ServerFnError> {
    let program = get_active_program().await?.map(|detail| detail.document);
    let maxes = training_maxes().await?;
    Ok(TrainingMaxData { program, maxes })
}

#[component]
fn TrainingMaxCard(unit: Unit) -> Element {
    let errors = use_errors();
    let mut data = use_resource(move || async move {
        let result = load_training_maxes().await;
        if let Err(error) = &result {
            errors.report(error);
        }
        result.ok()
    });
    let body = match &*data.read() {
        None => rsx! { p { class: "io-muted", role: "status", "Loading…" } },
        Some(None) => rsx! {
            p { class: "io-muted", "Your training maxes could not be loaded." }
            Button { variant: ButtonVariant::Secondary, onclick: move |_| data.restart(), "Try again" }
        },
        Some(Some(loaded)) => {
            let rows = training_max_rows(loaded.program.as_ref(), &loaded.maxes);
            if rows.is_empty() {
                rsx! {
                    p { class: "io-muted",
                        "None needed: your active program has no lifts loaded as a percentage of a training max."
                    }
                }
            } else {
                rsx! {
                    ul { class: "io-list",
                        for row in rows {
                            TrainingMaxLine {
                                key: "{row.exercise_id}",
                                row: row.clone(),
                                unit,
                                on_saved: move |()| data.restart(),
                            }
                        }
                    }
                }
            }
        }
    };
    rsx! {
        Card { title: "Training maxes",
            p { class: "io-muted io-hint",
                "The weights that percentage-based lifts are worked out from. Setting one restarts that lift's progression from it."
            }
            {body}
        }
    }
}

#[component]
fn TrainingMaxLine(row: TrainingMaxRow, unit: Unit, on_saved: EventHandler<()>) -> Element {
    let errors = use_errors();
    let mut input = use_signal(String::new);
    let mut problem = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let id = row.exercise_id.to_string();
    let field = format!("tm-{id}");
    let current = row
        .weight
        .map_or_else(|| "Not set".to_owned(), |weight| weight_text(weight, unit));

    let save = {
        let id = id.clone();
        move || {
            let weight = match parse_weight(&input.peek(), unit) {
                Ok(weight) => weight,
                Err(message) => {
                    problem.set(Some(message));
                    return;
                }
            };
            problem.set(None);
            busy.set(true);
            let id = id.clone();
            spawn(async move {
                match set_training_max(id, weight.as_kg()).await {
                    Ok(_) => {
                        input.set(String::new());
                        on_saved.call(());
                    }
                    Err(error) => errors.report(&error),
                }
                busy.set(false);
            });
        }
    };
    let remove = {
        let id = id.clone();
        move |_| {
            busy.set(true);
            let id = id.clone();
            spawn(async move {
                match delete_training_max(id).await {
                    Ok(()) => on_saved.call(()),
                    Err(error) => errors.report(&error),
                }
                busy.set(false);
            });
        }
    };
    let busy_now = *busy.read();
    rsx! {
        li { class: "io-row",
            div { class: "io-row-main",
                label { class: "io-row-title", r#for: "{field}", "{row.name}" }
                span { class: "io-muted io-row-meta", "{current}" }
            }
            div { class: "io-inline",
                input {
                    id: "{field}",
                    class: "io-input io-input-short",
                    r#type: "text",
                    inputmode: "decimal",
                    autocomplete: "off",
                    placeholder: "{unit.symbol()}",
                    value: "{input}",
                    disabled: busy_now,
                    oninput: move |event| input.set(event.value()),
                }
                Button {
                    variant: ButtonVariant::Secondary,
                    busy: busy_now,
                    onclick: {
                        let mut save = save.clone();
                        move |_| save()
                    },
                    "Set"
                }
                if row.weight.is_some() {
                    Button { variant: ButtonVariant::Ghost, disabled: busy_now, onclick: remove, "Clear" }
                }
            }
            if let Some(message) = problem.read().clone() {
                p { class: "io-notice io-notice-error", role: "alert", "{message}" }
            }
        }
    }
}

#[component]
fn PlanCard() -> Element {
    let errors = use_errors();
    let mut entitlements = use_resource(move || async move {
        let result = my_entitlements().await;
        if let Err(error) = &result {
            errors.report(error);
        }
        result.ok()
    });
    let body = match &*entitlements.read() {
        None => rsx! { p { class: "io-muted", role: "status", "Loading…" } },
        Some(None) => rsx! {
            p { class: "io-muted", "Your plan could not be loaded." }
            Button {
                variant: ButtonVariant::Secondary,
                onclick: move |_| entitlements.restart(),
                "Try again"
            }
        },
        Some(Some(entitlements)) => rsx! {
            div { class: "io-setting",
                span { class: "io-setting-name", "Current plan" }
                span { class: "io-plan", "{entitlements.plan.label()}" }
            }
            p { class: "io-muted io-hint", "{programs_allowance(entitlements)}" }
            p { class: "io-muted io-hint", "Changing plans is not available yet." }
        },
    };
    rsx! {
        section { id: "plan", class: "io-card", aria_labelledby: "plan-title",
            h2 { id: "plan-title", "Plan" }
            {body}
        }
    }
}

/// Exporting, importing and deleting the user's data.
// TODO(#22): replace this placeholder with export (download JSON), import and account deletion
// once the GDPR server functions are merged.
#[component]
fn DataCard() -> Element {
    rsx! {
        Card { title: "Your data",
            p { class: "io-muted",
                "Exporting your data, importing it and deleting your account are coming soon."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iron_oxide_domain::entitlements::Plan;

    fn kg(value: f64) -> Weight {
        Weight::from_kg(value).unwrap()
    }

    fn lb(value: f64) -> Weight {
        Weight::from_lb(value).unwrap()
    }

    #[test]
    fn rests_read_as_minutes_and_seconds() {
        assert_eq!(rest_text(Seconds::new(0)), "0:00");
        assert_eq!(rest_text(Seconds::new(45)), "0:45");
        assert_eq!(rest_text(Seconds::new(120)), "2:00");
        assert_eq!(rest_text(Seconds::new(195)), "3:15");
        assert_eq!(rest_text(MAX_DEFAULT_REST), "60:00");
    }

    #[test]
    fn the_rest_steps_reach_the_server_limit() {
        let max = i64::from(MAX_DEFAULT_REST.get());
        assert_eq!(max % REST_STEP, 0);
        assert_eq!(MIN_REST % REST_STEP, 0);
    }

    #[test]
    fn standard_bars_are_20_kg_and_45_lb() {
        assert_eq!(standard_bar(Unit::Kg), kg(20.0));
        assert_eq!(standard_bar(Unit::Lb), lb(45.0));
        assert_eq!(bar_step(Unit::Kg), kg(0.5));
        assert_eq!(bar_step(Unit::Lb), lb(1.0));
        assert!(max_bar() > lb(45.0));
    }

    #[test]
    fn switching_unit_keeps_the_weights_until_asked() {
        let settings = Settings::defaults();
        let in_lb = with_unit(&settings, Unit::Lb);
        assert_eq!(in_lb.unit, Unit::Lb);
        assert_eq!(in_lb.bar_weight, kg(20.0));
        assert_eq!(in_lb.plate_inventory, settings.plate_inventory);
        assert!(!has_standard_equipment(&in_lb, Unit::Lb));
        let standard = with_standard_equipment(&in_lb);
        assert_eq!(standard.bar_weight, lb(45.0));
        assert_eq!(
            standard.plate_inventory,
            PlateInventory::default_for(Unit::Lb)
        );
        assert!(has_standard_equipment(&standard, Unit::Lb));
        assert!(has_standard_equipment(&settings, Unit::Kg));
    }

    #[test]
    fn weights_are_read_in_the_users_unit() {
        assert_eq!(parse_weight("2.5", Unit::Kg), Ok(kg(2.5)));
        assert_eq!(parse_weight(" 1,25 ", Unit::Kg), Ok(kg(1.25)));
        assert_eq!(parse_weight("2.5", Unit::Lb), Ok(lb(2.5)));
        assert_eq!(
            parse_weight("", Unit::Kg),
            Err("Enter a weight.".to_owned())
        );
        assert!(parse_weight("abc", Unit::Kg).unwrap_err().contains("abc"));
        assert!(parse_weight("0", Unit::Kg).is_err());
        assert!(parse_weight("-5", Unit::Kg).is_err());
        assert!(parse_weight("inf", Unit::Kg).is_err());
        assert!(parse_weight("1e9", Unit::Kg).is_err());
    }

    #[test]
    fn adding_plates_validates_with_the_domain() {
        let inventory = PlateInventory::default_for(Unit::Kg);
        // A new size is inserted in order, heaviest first.
        let added = add_plate(&inventory, kg(0.5), 1, Unit::Kg).unwrap();
        assert_eq!(added.pairs_of(kg(0.5)), 1);
        assert_eq!(added.stock().last().unwrap().plate, kg(0.5));
        // An existing size gets one more pair.
        let more = add_plate(&inventory, kg(25.0), 1, Unit::Kg).unwrap();
        assert_eq!(more.pairs_of(kg(25.0)), 5);
        // Off-grid sizes are refused, in the user's unit.
        assert_eq!(
            add_plate(&inventory, kg(0.01), 1, Unit::Kg).unwrap_err(),
            "0.01 kg is not a plate size: sizes go in steps of 0.025 kg or 0.125 lb."
        );
    }

    #[test]
    fn pairs_change_and_plates_go() {
        let inventory = PlateInventory::default_for(Unit::Kg);
        let changed = set_pairs(&inventory, kg(20.0), 3, Unit::Kg).unwrap();
        assert_eq!(changed.pairs_of(kg(20.0)), 3);
        assert_eq!(changed.pairs_of(kg(25.0)), 4);
        assert_eq!(
            set_pairs(
                &inventory,
                kg(20.0),
                PlateInventory::MAX_PAIRS + 1,
                Unit::Lb
            )
            .unwrap_err(),
            "At most 50 pairs of 44.09 lb are allowed."
        );
        let removed = remove_plate(&inventory, kg(1.25));
        assert_eq!(removed.pairs_of(kg(1.25)), 0);
        assert_eq!(removed.stock().len(), inventory.stock().len() - 1);
        // The last size stays.
        let one = PlateInventory::new([PlateStock {
            plate: kg(20.0),
            pairs: 2,
        }])
        .unwrap();
        assert_eq!(remove_plate(&one, kg(20.0)), one);
    }

    #[test]
    fn the_plan_says_how_many_programs_it_keeps() {
        assert_eq!(
            programs_allowance(&Entitlements::of(Plan::Free)),
            "Up to 10 active programs (archived ones don't count)."
        );
        assert_eq!(
            programs_allowance(&Entitlements::of(Plan::Pro)),
            "Unlimited programs."
        );
    }

    #[test]
    fn training_maxes_follow_the_active_program_then_the_rest() {
        let program = Program::from_json(
            r#"{
              "schema_version": 1,
              "name": "531",
              "days": [
                { "id": "a", "name": "A", "exercises": [
                  { "id": "squat", "name": "Squat", "work": { "reps": { "sets": 3, "reps": 5 } },
                    "load": { "percent_of_training_max": 75 }, "rest": 180 },
                  { "id": "row", "name": "Row", "work": { "reps": { "sets": 3, "reps": 8 } },
                    "load": { "kg": 50 }, "rest": 90 }
                ] },
                { "id": "b", "name": "B", "exercises": [
                  { "id": "squat", "name": "Squat", "work": { "reps": { "sets": 1, "reps": 5 } },
                    "load": { "percent_of_training_max": 85 }, "rest": 180 },
                  { "id": "bench", "name": "Bench press", "work": { "reps": { "sets": 3, "reps": 5 } },
                    "load": { "percent_of_training_max": 75 }, "rest": 180 }
                ] }
              ],
              "rotation": ["a", "b"]
            }"#,
        )
        .unwrap();
        let max = |id: &str, weight: f64| TrainingMax {
            exercise_id: ExerciseId::new(id).unwrap(),
            weight: kg(weight),
            set_at: iron_oxide_domain::time::Timestamp::from_epoch_millis(0),
        };
        let maxes = [max("bench", 80.0), max("deadlift", 180.0)];
        let rows = training_max_rows(Some(&program), &maxes);
        let summary: Vec<(&str, &str, Option<Weight>)> = rows
            .iter()
            .map(|row| (row.exercise_id.as_str(), row.name.as_str(), row.weight))
            .collect();
        assert_eq!(
            summary,
            [
                ("squat", "Squat", None),
                ("bench", "Bench press", Some(kg(80.0))),
                ("deadlift", "deadlift", Some(kg(180.0))),
            ]
        );
        assert!(training_max_rows(None, &[]).is_empty());
    }
}
