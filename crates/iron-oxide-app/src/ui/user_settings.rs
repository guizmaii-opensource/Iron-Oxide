//! The user's settings, shared by every screen (#34), and the one place that saves them.
//!
//! [`use_user_settings`] gives the signed-in user's settings (bar, plates, rest, sound). They are
//! loaded as soon as the session is signed in, not when the Settings page opens, and they drive the
//! display unit of [`crate::ui::weight`], so every screen shows weights in the user's unit.
//!
//! Saving lives here, at the app root, not in the Settings page, so leaving the page never drops a
//! change. Two values are kept: `confirmed`, what the server last acknowledged, and `desired`, what
//! the screens show.
//! - [`UserSettings::change`] changes `desired` and wakes the saver. A change back to the
//!   confirmed value sends nothing.
//! - One save is in flight at a time. When it ends, if `desired != confirmed`, `desired` is sent
//!   (the latest wins; changes made meanwhile are coalesced).
//! - On success, `confirmed` becomes what was saved. On a failure, each field the failed request
//!   changed goes back to its confirmed value in `desired`, unless the user has changed that field
//!   again since (the newer edit wins); the error is shown and saving goes on ([`roll_back`]).
//! - When the page is hidden or unloaded with a change not yet confirmed, `desired` is sent with a
//!   `keepalive` request, which the browser completes after the page is gone. At most once per
//!   change, and not when the request in flight already carries the same settings.
//!
//! Everything is tied to the user who loaded it. Signing out (or the session ending) clears the
//! settings and bumps a generation: an answer that arrives afterwards, for a load or a save, is
//! dropped, so one user's settings can never be shown to, or saved for, the next.

use dioxus::prelude::*;
use iron_oxide_domain::Unit;

use super::errors::{Errors, use_errors};
use super::shell::{SessionStatus, use_session};
use super::weight::UnitSetting;
use crate::api::settings::{Settings, SettingsUpdate, get_settings, update_settings};
use crate::auth::api::me;
use crate::auth::types::UserId;

/// The signals behind [`UserSettings`].
#[derive(Clone, Copy, PartialEq)]
struct State {
    /// `desired`: what the screens show, the confirmed settings with the user's changes not yet
    /// saved. `None` until loaded, and while signed out.
    current: Signal<Option<Settings>>,
    /// The settings as the server last confirmed them, to roll back to.
    confirmed: Signal<Option<Settings>>,
    /// The user the settings belong to.
    user: Signal<Option<UserId>>,
    /// Bumped when the user signs out: work started before is dropped when it finishes.
    generation: Signal<u64>,
    /// The generation being loaded, if a load is in flight.
    loading: Signal<Option<u64>>,
    /// Whether the last load failed (the banner says why).
    failed: Signal<bool>,
    session: Signal<SessionStatus>,
    unit: UnitSetting,
    errors: Errors,
}

/// The settings shared by every screen, provided by the app root. `Copy`.
#[derive(Clone, Copy, PartialEq)]
pub struct UserSettings {
    state: State,
    saver: Coroutine<()>,
}

impl State {
    /// Shows `settings` (and their unit everywhere).
    fn show(self, settings: Option<Settings>) {
        let mut unit = self.unit.0;
        let wanted = settings.as_ref().map_or(Unit::Kg, |settings| settings.unit);
        if *unit.peek() != wanted {
            unit.set(wanted);
        }
        let mut current = self.current;
        current.set(settings);
    }

    /// Whether work started in `generation` still applies.
    fn is_current(self, generation: u64) -> bool {
        *self.generation.peek() == generation && *self.session.peek() != SessionStatus::SignedOut
    }

    async fn load(mut self) {
        let generation = *self.generation.peek();
        if *self.loading.peek() == Some(generation) {
            return;
        }
        self.loading.set(Some(generation));
        self.failed.set(false);
        let result = async {
            let user = me().await?.user_id;
            let settings = get_settings().await?;
            Ok::<_, ServerFnError>((user, settings))
        }
        .await;
        if !self.is_current(generation) {
            // Signed out meanwhile: these settings are not for whoever is here now.
            return;
        }
        self.loading.set(None);
        match result {
            Ok((user, settings)) => {
                self.user.set(Some(user));
                self.confirmed.set(Some(settings.clone()));
                self.show(Some(settings));
            }
            Err(error) => {
                self.errors.report(&error);
                self.failed.set(true);
            }
        }
    }

    /// Forgets everything (signed out).
    fn clear(mut self) {
        let next = *self.generation.peek() + 1;
        self.generation.set(next);
        self.loading.set(None);
        self.failed.set(false);
        self.user.set(None);
        self.confirmed.set(None);
        self.show(None);
        unsaved::desired(None);
    }

    /// Saves `desired` whenever it differs from `confirmed`, one request at a time.
    async fn save_loop(self, mut wakes: UnboundedReceiver<()>) {
        while wakes.recv().await.is_ok() {
            loop {
                while wakes.try_recv().is_ok() {}
                let generation = *self.generation.peek();
                let (Some(desired), Some(confirmed)) =
                    (self.current.peek().clone(), self.confirmed.peek().clone())
                else {
                    break;
                };
                if desired == confirmed {
                    break;
                }
                let sent = desired;
                unsaved::in_flight(Some(&sent));
                let result = update_settings(SettingsUpdate::from(sent.clone())).await;
                unsaved::in_flight(None);
                if !self.is_current(generation) {
                    break;
                }
                let desired = self.current.peek().clone();
                match result {
                    Ok(saved) => {
                        let mut confirmed = self.confirmed;
                        confirmed.set(Some(saved.clone()));
                        // The server's form of what was sent (plates sorted), if nothing changed.
                        if desired.as_ref() == Some(&sent) {
                            self.show(Some(saved));
                        }
                    }
                    Err(error) => {
                        self.errors.report(&error);
                        if let Some(desired) = desired {
                            self.show(Some(roll_back(desired, &sent, &confirmed)));
                        }
                    }
                }
                self.note_unsaved();
            }
        }
    }

    /// Records what is not saved yet, for the last-chance save.
    fn note_unsaved(self) {
        let desired = self.current.peek().clone();
        let unsaved = desired.filter(|desired| self.confirmed.peek().as_ref() != Some(desired));
        unsaved::desired(unsaved.as_ref());
    }
}

/// `desired` after the save of `sent` failed: every field `sent` changed from `confirmed` goes
/// back to its confirmed value, unless `desired` no longer has the sent value (the user changed
/// that field again since: the newer edit wins and will be sent next).
#[must_use]
pub fn roll_back(desired: Settings, sent: &Settings, confirmed: &Settings) -> Settings {
    fn field<T: PartialEq + Clone>(desired: T, sent: &T, confirmed: &T) -> T {
        if sent != confirmed && desired == *sent {
            confirmed.clone()
        } else {
            desired
        }
    }
    Settings {
        unit: field(desired.unit, &sent.unit, &confirmed.unit),
        bar_weight: field(desired.bar_weight, &sent.bar_weight, &confirmed.bar_weight),
        plate_inventory: field(
            desired.plate_inventory,
            &sent.plate_inventory,
            &confirmed.plate_inventory,
        ),
        default_rest: field(
            desired.default_rest,
            &sent.default_rest,
            &confirmed.default_rest,
        ),
        sound_enabled: field(
            desired.sound_enabled,
            &sent.sound_enabled,
            &confirmed.sound_enabled,
        ),
        kg_weight_step: field(
            desired.kg_weight_step,
            &sent.kg_weight_step,
            &confirmed.kg_weight_step,
        ),
        lb_weight_step: field(
            desired.lb_weight_step,
            &sent.lb_weight_step,
            &confirmed.lb_weight_step,
        ),
        vibration_enabled: field(
            desired.vibration_enabled,
            &sent.vibration_enabled,
            &confirmed.vibration_enabled,
        ),
    }
}

impl UserSettings {
    /// The settings, if loaded.
    #[must_use]
    pub fn get(&self) -> Option<Settings> {
        self.state.current.read().clone()
    }

    /// The settings without subscribing to them (for event handlers).
    #[must_use]
    pub fn peek(&self) -> Option<Settings> {
        self.state.current.peek().clone()
    }

    /// Whether loading failed; [`UserSettings::reload`] tries again.
    #[must_use]
    pub fn failed(&self) -> bool {
        *self.state.failed.read()
    }

    /// Loads the settings again.
    pub fn reload(self) {
        spawn(self.load());
    }

    /// Loads the settings, then carries over what this device still kept before #103.
    async fn load(self) {
        self.state.load().await;
        let (Some(user), Some(settings)) = (*self.state.user.peek(), self.peek()) else {
            return;
        };
        if let Some(device) = super::prefs::take_device_prefs(user) {
            let carried = super::prefs::carry_over(&settings, device);
            if carried != settings {
                self.change(|_| carried);
            }
        }
    }

    /// Changes the settings with `edit`: on screen at once, then saved.
    pub fn change(self, edit: impl FnOnce(Settings) -> Settings) {
        let Some(settings) = self.peek() else {
            return;
        };
        self.state.show(Some(edit(settings)));
        self.state.note_unsaved();
        self.saver.send(());
    }
}

/// Provides the shared settings, loads them whenever the session becomes signed in, and saves
/// changes. Called once, by the app root, after the session, the banner and the unit.
pub fn use_settings_provider(unit: UnitSetting) -> UserSettings {
    let state = State {
        current: use_signal(|| None),
        confirmed: use_signal(|| None),
        user: use_signal(|| None),
        generation: use_signal(|| 0),
        loading: use_signal(|| None),
        failed: use_signal(|| false),
        session: use_session(),
        unit,
        errors: use_errors(),
    };
    let saver = use_coroutine(move |changes| state.save_loop(changes));
    let settings = use_context_provider(|| UserSettings { state, saver });
    use_hook(unsaved::flush_when_hidden);
    // Client only: the server renders the signed-out shell, so hydration matches.
    use_effect(move || {
        if !cfg!(feature = "web") {
            return;
        }
        match *state.session.read() {
            SessionStatus::SignedIn => {
                spawn(settings.load());
            }
            SessionStatus::SignedOut => state.clear(),
            SessionStatus::Checking | SessionStatus::Unverified => {}
        }
    });
    settings
}

/// The shared settings.
#[must_use]
pub fn use_user_settings() -> UserSettings {
    use_context::<UserSettings>()
}

/// The latest settings not yet confirmed by the server, as the request body that saves them, for
/// the last-chance save when the page is hidden or closed. Kept outside the Dioxus runtime, since
/// the browser calls the listeners outside it.
mod unsaved {
    use std::cell::RefCell;

    use crate::api::settings::{Settings, SettingsUpdate};

    /// The server function's route (see `crate::api::settings::update_settings`).
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub const UPDATE_PATH: &str = "/api/settings/update";

    /// What the last-chance save needs to know.
    #[derive(Debug, Default)]
    pub struct Flush {
        /// The body that saves the unsaved settings, if any.
        desired: Option<String>,
        /// Bumped each time `desired` changes.
        revision: u64,
        /// The body of the request in flight, if any.
        in_flight: Option<String>,
        /// The revision last sent by a flush.
        flushed: Option<u64>,
    }

    impl Flush {
        pub fn set_desired(&mut self, body: Option<String>) {
            if body != self.desired {
                self.desired = body;
                self.revision += 1;
            }
        }

        pub fn set_in_flight(&mut self, body: Option<String>) {
            self.in_flight = body;
        }

        /// The body to send now, if any: each revision at most once, and not when the request in
        /// flight carries the same body.
        pub fn take(&mut self) -> Option<String> {
            let body = self.desired.clone()?;
            if self.flushed == Some(self.revision) || self.in_flight.as_ref() == Some(&body) {
                return None;
            }
            self.flushed = Some(self.revision);
            Some(body)
        }
    }

    thread_local! {
        static FLUSH: RefCell<Flush> = RefCell::new(Flush::default());
    }

    /// The JSON body of `update_settings(settings)`.
    #[must_use]
    pub fn body(settings: &Settings) -> String {
        serde_json::json!({ "settings": SettingsUpdate::from(settings.clone()) }).to_string()
    }

    /// The settings not saved yet (`None`: everything is saved).
    pub fn desired(settings: Option<&Settings>) {
        FLUSH.with(|flush| flush.borrow_mut().set_desired(settings.map(body)));
    }

    /// The settings the request in flight saves.
    pub fn in_flight(settings: Option<&Settings>) {
        FLUSH.with(|flush| flush.borrow_mut().set_in_flight(settings.map(body)));
    }

    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    fn take() -> Option<String> {
        FLUSH.with(|flush| flush.borrow_mut().take())
    }

    /// Installs the listeners that send the unsaved settings when the page is hidden (the user
    /// switches app, locks the phone) or unloaded. A close fires both events: [`Flush::take`] sends
    /// once.
    #[cfg(feature = "web")]
    pub fn flush_when_hidden() {
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;

        let Some(window) = web_sys::window() else {
            return;
        };
        let flush = Closure::<dyn Fn()>::new(|| {
            let hidden = web_sys::window()
                .and_then(|window| window.document())
                .is_none_or(|document| document.hidden());
            if hidden && let Some(body) = take() {
                send_keepalive(&body);
            }
        });
        let callback = flush.as_ref().unchecked_ref();
        let _ = window.add_event_listener_with_callback("pagehide", callback);
        if let Some(document) = window.document() {
            let _ = document.add_event_listener_with_callback("visibilitychange", callback);
        }
        // Lives as long as the page.
        flush.forget();
    }

    #[cfg(not(feature = "web"))]
    pub const fn flush_when_hidden() {}

    /// A `POST` the browser completes even if the page goes away (`keepalive`).
    #[cfg(feature = "web")]
    fn send_keepalive(body: &str) {
        use wasm_bindgen::JsValue;

        let Some(window) = web_sys::window() else {
            return;
        };
        let init = web_sys::RequestInit::new();
        init.set_method("POST");
        init.set_body(&JsValue::from_str(body));
        let headers = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &headers,
            &JsValue::from_str("content-type"),
            &JsValue::from_str("application/json"),
        );
        init.set_headers(&headers);
        let _ = js_sys::Reflect::set(&init, &JsValue::from_str("keepalive"), &JsValue::TRUE);
        // Best effort: the page is going away, nobody can show an error.
        let _ = window.fetch_with_str_and_init(UPDATE_PATH, &init);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_body_is_the_server_functions_arguments() {
            let settings = Settings::defaults();
            let body: serde_json::Value = serde_json::from_str(&body(&settings)).unwrap();
            let update: SettingsUpdate = serde_json::from_value(body["settings"].clone()).unwrap();
            assert_eq!(update, SettingsUpdate::from(settings));
        }

        #[test]
        fn a_flush_sends_each_change_once() {
            let mut flush = Flush::default();
            assert_eq!(flush.take(), None);
            flush.set_desired(Some("a".to_owned()));
            // visibilitychange, then pagehide: one request.
            assert_eq!(flush.take(), Some("a".to_owned()));
            assert_eq!(flush.take(), None);
            // The same body set again is not a new change.
            flush.set_desired(Some("a".to_owned()));
            assert_eq!(flush.take(), None);
            // A newer change is sent.
            flush.set_desired(Some("b".to_owned()));
            assert_eq!(flush.take(), Some("b".to_owned()));
            // Saved: nothing to send.
            flush.set_desired(None);
            assert_eq!(flush.take(), None);
        }

        #[test]
        fn a_flush_skips_what_the_request_in_flight_saves() {
            let mut flush = Flush::default();
            flush.set_desired(Some("a".to_owned()));
            flush.set_in_flight(Some("a".to_owned()));
            assert_eq!(flush.take(), None);
            // Something newer than the request in flight is sent.
            flush.set_desired(Some("b".to_owned()));
            assert_eq!(flush.take(), Some("b".to_owned()));
            flush.set_in_flight(None);
            assert_eq!(flush.take(), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use iron_oxide_domain::{Seconds, Weight};

    use super::*;

    fn kg(value: f64) -> Weight {
        Weight::from_kg(value).unwrap()
    }

    fn with_bar(settings: &Settings, bar: f64) -> Settings {
        Settings {
            bar_weight: kg(bar),
            ..settings.clone()
        }
    }

    #[test]
    fn a_refused_field_goes_back_and_a_later_edit_of_another_field_stays() {
        let confirmed = Settings::defaults();
        // Bar 10 sent and refused; rest changed meanwhile.
        let sent = with_bar(&confirmed, 10.0);
        let desired = Settings {
            default_rest: Seconds::new(135),
            ..sent.clone()
        };
        let after = roll_back(desired, &sent, &confirmed);
        assert_eq!(after.bar_weight, confirmed.bar_weight);
        assert_eq!(after.default_rest, Seconds::new(135));
        // What goes next carries only the rest change.
        assert_eq!(
            after,
            Settings {
                default_rest: Seconds::new(135),
                ..confirmed
            }
        );
    }

    #[test]
    fn a_newer_edit_of_the_refused_field_wins() {
        let confirmed = Settings::defaults();
        let sent = with_bar(&confirmed, 10.0);
        let desired = with_bar(&confirmed, 15.0);
        assert_eq!(roll_back(desired.clone(), &sent, &confirmed), desired);
    }

    #[test]
    fn the_same_value_tapped_twice_is_rolled_back_with_the_failure() {
        // 15 kg tapped twice while the first save fails: the screen goes back to the confirmed
        // bar, and there is nothing left to send (desired == confirmed).
        let confirmed = Settings::defaults();
        let sent = with_bar(&confirmed, 15.0);
        let after = roll_back(sent.clone(), &sent, &confirmed);
        assert_eq!(after, confirmed);
    }

    #[test]
    fn fields_the_request_did_not_change_are_left_alone() {
        let confirmed = Settings::defaults();
        let sent = with_bar(&confirmed, 10.0);
        let desired = Settings {
            sound_enabled: !confirmed.sound_enabled,
            ..sent.clone()
        };
        let after = roll_back(desired, &sent, &confirmed);
        assert_eq!(after.sound_enabled, !confirmed.sound_enabled);
        assert_eq!(after.bar_weight, confirmed.bar_weight);
    }
}
