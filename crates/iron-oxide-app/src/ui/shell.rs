//! The app shell (#26): the routes, the mobile layout with its bottom navigation, and the sign-in
//! gate.
//!
//! Every page except the gallery needs an account, so the shell checks the session (`me()`,
//! client only) and shows the sign-in screen while signed out. A `401` from any server call
//! (see [`crate::ui::errors`]) or signing out brings it back.
//!
//! Offline first, deliberately: when `me()` cannot answer (offline, `429`, `503`), the app still
//! opens, so it works at the gym without signal. A banner says so, and `me()` is retried with a
//! backoff until the server answers; a `401` then shows the sign-in screen.

use dioxus::prelude::*;
use iron_oxide_domain::{ExerciseId, SessionId};

use super::account::Account;
use super::components::icons::{HistoryIcon, HomeIcon, ProgramsIcon, SettingsIcon};
use super::components::{EmptyState, LoadingState};
use super::errors::{BannerKind, use_errors};
use super::history::{ExerciseProgress, History, HistoryLayout, HistorySession};
use super::home::Home;
use super::plates::PlateTool;
use crate::api::error::{ApiFailure, FailureKind};
use crate::auth::api::{is_unauthorized, me};
use crate::auth::browser;

/// The app's pages. Home, History, Programs and Settings are filled by their own tickets; Workout
/// is the session in progress (#28), full screen: no bottom navigation.
#[derive(Routable, Clone, PartialEq, Debug)]
#[rustfmt::skip]
pub enum Route {
    #[layout(Shell)]
        #[route("/")]
        Home {},
        #[layout(HistoryLayout)]
            #[route("/history")]
            History {},
            #[route("/history/session/:id")]
            HistorySession { id: SessionId },
            #[route("/history/exercise/:exercise")]
            ExerciseProgress { exercise: ExerciseId },
        #[end_layout]
        #[route("/programs")]
        Programs {},
        #[route("/settings")]
        Settings {},
        #[route("/tools/plates")]
        PlateTool {},
        #[route("/session")]
        Workout {},
        #[route("/:..segments")]
        NotFound { segments: Vec<String> },
    #[end_layout]
    #[route("/dev/components")]
    Gallery {},
}

/// Whether the user is signed in, as far as the app knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// Not known yet (server rendering, and until `me()` answers).
    Checking,
    SignedIn,
    SignedOut,
    /// `me()` could not answer (offline, `429`, `503`): the app is shown with what is on this
    /// device while the check is retried.
    Unverified,
}

/// The longest wait between two session checks.
const MAX_RETRY_SECS: u64 = 60;

/// How long to wait before session check number `attempt + 1`: 2, 4, 8… seconds up to a minute,
/// and never less than a `429`'s `Retry-After`.
#[must_use]
pub fn retry_delay_secs(attempt: u32, retry_after: Option<u64>) -> u64 {
    let backoff = 2_u64
        .saturating_pow(attempt.saturating_add(1))
        .min(MAX_RETRY_SECS);
    backoff.max(retry_after.unwrap_or(0))
}

/// The banner while the session cannot be checked.
#[must_use]
pub const fn unverified_message(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Network => "Offline \u{2014} showing what's on this device.",
        _ => "Can't reach the server, retrying. Showing what's on this device.",
    }
}

/// Provides the session status. Called once, by the app root.
pub fn use_session_provider() -> Signal<SessionStatus> {
    let status = use_signal(|| SessionStatus::Checking);
    use_context_provider(|| status)
}

/// The session status, to read or to update.
#[must_use]
pub fn use_session() -> Signal<SessionStatus> {
    use_context::<Signal<SessionStatus>>()
}

/// Sets the session status, unless it already is `status` (so that readers do not re-render).
pub fn set_session(mut session: Signal<SessionStatus>, status: SessionStatus) {
    if *session.peek() != status {
        session.set(status);
    }
}

/// The layout of every page: top bar, page, bottom navigation; or the sign-in screen.
#[component]
fn Shell() -> Element {
    let session = use_session();
    let errors = use_errors();
    let route = use_route::<Route>();
    // The workout keeps the whole screen for the set: no bottom navigation.
    let focused = matches!(route, Route::Workout {});

    // Client only: on the server the shell stays "Checking", so hydration matches.
    use_effect(move || {
        if cfg!(feature = "web") {
            spawn(check_session(session, errors));
        }
    });

    let status = *session.read();
    match status {
        SessionStatus::Checking => rsx! {
            div { class: "io-shell io-shell-bare",
                TopBar {}
                LoadingState {}
            }
        },
        SessionStatus::SignedOut => rsx! {
            main { class: "io-shell io-shell-bare",
                div { class: "io-page-header",
                    h1 { class: "io-title", "Iron Oxide" }
                    p { class: "io-muted", "Zero-cost gains. The only overhead is the barbell." }
                }
                Account {}
            }
        },
        SessionStatus::SignedIn | SessionStatus::Unverified => rsx! {
            div { class: if focused { "io-shell io-shell-bare" } else { "io-shell" },
                TopBar {}
                main { class: "io-page", Outlet::<Route> {} }
            }
            if !focused {
                BottomNav {}
            }
        },
    }
}

/// Checks the session with `me()`, retrying with a backoff while the server cannot answer (see
/// the module docs). Stops as soon as the status is known, here or elsewhere (a sign-in, a `401`).
async fn check_session(session: Signal<SessionStatus>, errors: super::errors::Errors) {
    let mut attempt = 0_u32;
    let mut banner = None;
    loop {
        let status = match me().await {
            Ok(_) => SessionStatus::SignedIn,
            Err(error) if is_unauthorized(&error) => SessionStatus::SignedOut,
            Err(error) => {
                let failure = ApiFailure::classify(&error);
                if matches!(
                    *session.peek(),
                    SessionStatus::Checking | SessionStatus::Unverified
                ) {
                    set_session(session, SessionStatus::Unverified);
                    banner =
                        Some(errors.show(BannerKind::Warning, unverified_message(failure.kind)));
                } else {
                    // Known meanwhile (signed in or out elsewhere).
                    return;
                }
                let secs = retry_delay_secs(attempt, failure.retry_after_secs());
                browser::sleep(i32::try_from(secs * 1_000).unwrap_or(i32::MAX)).await;
                attempt = attempt.saturating_add(1);
                continue;
            }
        };
        if matches!(
            *session.peek(),
            SessionStatus::Checking | SessionStatus::Unverified
        ) {
            set_session(session, status);
        }
        if let Some(id) = banner {
            errors.dismiss_if(id);
        }
        return;
    }
}

/// The top bar: the brand, and room for status indicators (offline, unsaved changes).
#[component]
fn TopBar() -> Element {
    rsx! {
        header { class: "io-topbar",
            span { class: "io-label", "Iron Oxide" }
            div { id: "io-status", class: "io-topbar-status",
                super::unsaved::Unsaved {}
            }
        }
    }
}

/// One entry of the bottom navigation.
struct NavItem {
    route: Route,
    label: &'static str,
    icon: fn() -> Element,
}

fn nav_items() -> [NavItem; 4] {
    [
        NavItem {
            route: Route::Home {},
            label: "Home",
            icon: || rsx! { HomeIcon {} },
        },
        NavItem {
            route: Route::History {},
            label: "History",
            icon: || rsx! { HistoryIcon {} },
        },
        NavItem {
            route: Route::Programs {},
            label: "Programs",
            icon: || rsx! { ProgramsIcon {} },
        },
        NavItem {
            route: Route::Settings {},
            label: "Settings",
            icon: || rsx! { SettingsIcon {} },
        },
    ]
}

/// Whether `current` is in the section of the tab `tab` (the history tab covers a session's
/// details and an exercise's charts).
fn in_section(tab: &Route, current: &Route) -> bool {
    match (tab, current) {
        (Route::History {}, Route::HistorySession { .. } | Route::ExerciseProgress { .. }) => true,
        _ => tab == current,
    }
}

/// The bottom navigation: four 64 px tabs, above the home indicator.
#[component]
fn BottomNav() -> Element {
    let current = use_route::<Route>();
    rsx! {
        nav { class: "io-nav", aria_label: "Main",
            ul {
                for item in nav_items() {
                    li { key: "{item.label}",
                        Link {
                            to: item.route.clone(),
                            // `Link` sets `aria-current="page"` itself on an exact match (and drops
                            // any other value on navigation); the tab's highlight follows the
                            // section, so a session's details still light History.
                            "data-section": in_section(&item.route, &current),
                            {(item.icon)()}
                            span { "{item.label}" }
                        }
                    }
                }
            }
        }
    }
}

/// A page's title.
#[component]
fn PageHeader(#[props(into)] title: String, #[props(into)] subtitle: Option<String>) -> Element {
    rsx! {
        div { class: "io-page-header",
            h1 { class: "io-title", "{title}" }
            if let Some(subtitle) = subtitle {
                p { class: "io-muted", "{subtitle}" }
            }
        }
    }
}

#[component]
fn Programs() -> Element {
    rsx! { super::programs::ProgramsPage {} }
}

#[component]
fn Settings() -> Element {
    rsx! { super::settings::SettingsPage {} }
}

#[component]
fn Workout() -> Element {
    rsx! { super::session::SessionPage {} }
}

#[component]
fn NotFound(segments: Vec<String>) -> Element {
    let path = format!("/{}", segments.join("/"));
    rsx! {
        EmptyState {
            title: "Not found",
            message: "There is nothing at {path}.",
            Link { class: "io-button io-button-secondary", to: Route::Home {}, "Go home" }
        }
    }
}

/// The component gallery: debug builds only. Release builds answer with the not-found page.
#[component]
fn Gallery() -> Element {
    #[cfg(debug_assertions)]
    {
        rsx! { super::gallery::Gallery {} }
    }
    #[cfg(not(debug_assertions))]
    {
        rsx! {
            main { class: "io-shell io-shell-bare",
                NotFound { segments: vec!["dev".to_owned(), "components".to_owned()] }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_have_their_paths() {
        assert_eq!(Route::Home {}.to_string(), "/");
        assert_eq!(Route::History {}.to_string(), "/history");
        assert_eq!(Route::Programs {}.to_string(), "/programs");
        assert_eq!(Route::Settings {}.to_string(), "/settings");
        assert_eq!(Route::PlateTool {}.to_string(), "/tools/plates");
        assert_eq!(Route::Workout {}.to_string(), "/session");
        assert_eq!(Route::Gallery {}.to_string(), "/dev/components");
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(7));
        let path = format!("/history/session/{session}");
        assert_eq!(Route::HistorySession { id: session }.to_string(), path);
        assert_eq!(
            path.parse::<Route>().unwrap(),
            Route::HistorySession { id: session }
        );
        let exercise = ExerciseId::new("back-squat").unwrap();
        assert_eq!(
            Route::ExerciseProgress {
                exercise: exercise.clone()
            }
            .to_string(),
            "/history/exercise/back-squat"
        );
        assert_eq!(
            "/history/exercise/back-squat".parse::<Route>().unwrap(),
            Route::ExerciseProgress { exercise }
        );
        assert_eq!("/history".parse::<Route>().unwrap(), Route::History {});
        assert_eq!(
            "/nope/x".parse::<Route>().unwrap(),
            Route::NotFound {
                segments: vec!["nope".to_owned(), "x".to_owned()]
            }
        );
    }

    #[test]
    fn the_history_tab_covers_its_screens() {
        let history = Route::History {};
        let session = Route::HistorySession {
            id: SessionId::from_uuid(uuid::Uuid::from_u128(7)),
        };
        assert!(in_section(&history, &history));
        assert!(in_section(&history, &session));
        assert!(!in_section(&Route::Home {}, &session));
        assert!(!in_section(&Route::Home {}, &history));
    }

    #[test]
    fn session_checks_back_off_up_to_a_minute() {
        let delays: Vec<_> = (0..7)
            .map(|attempt| retry_delay_secs(attempt, None))
            .collect();
        assert_eq!(delays, [2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(retry_delay_secs(u32::MAX, None), 60);
        // A 429's Retry-After is honoured.
        assert_eq!(retry_delay_secs(0, Some(42)), 42);
        assert_eq!(retry_delay_secs(5, Some(10)), 60);
    }

    #[test]
    fn the_unverified_banner_says_why() {
        assert!(unverified_message(FailureKind::Network).starts_with("Offline"));
        for kind in [
            FailureKind::RateLimited,
            FailureKind::Transient,
            FailureKind::Other,
        ] {
            assert!(unverified_message(kind).starts_with("Can't reach the server, retrying"));
        }
    }

    #[test]
    fn the_bottom_navigation_has_the_four_sections() {
        let labels: Vec<_> = nav_items().iter().map(|item| item.label).collect();
        assert_eq!(labels, ["Home", "History", "Programs", "Settings"]);
    }
}
