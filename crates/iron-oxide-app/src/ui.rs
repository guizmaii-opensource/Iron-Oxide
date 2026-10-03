//! User interface: the app root, the shell and its pages, and the shared components.
//!
//! - `theme`: the stylesheet and the fonts.
//! - `shell`: the routes, the layout with the bottom navigation, and the sign-in gate.
//! - `components`: the reusable components.
//! - `errors`: the banner every server error is reported to.
//! - `weight`: weights in the user's unit.
//! - `plates`: the plate calculator (inline, as a sheet, and the `/tools/plates` page).
//! - `user_settings`: the user's settings, shared by every screen, loaded and saved there.
//! - `settings`: the Settings page.
//! - `prefs`: the weight steps offered; moves the old device-only preferences to the server.
//! - `programs`: the Programs page.
//! - `history`: the history screens (#33).
//! - `home`: the home screen (#27).
//! - `session`: the workout session screens (#28).

mod account;
#[cfg_attr(
    not(debug_assertions),
    allow(
        dead_code,
        unused_imports,
        reason = "used by the screens of #27-#34; until then only by the debug gallery"
    )
)]
mod components;
#[cfg_attr(
    not(debug_assertions),
    allow(
        dead_code,
        unused_imports,
        reason = "used by the screens of #27-#34; until then only by the debug gallery"
    )
)]
mod errors;
#[cfg(debug_assertions)]
mod gallery;
mod history;
mod home;
mod plates;
mod prefs;
mod programs;
mod session;
mod settings;
mod shell;
pub(crate) mod theme;
pub mod unsaved;
mod user_settings;
#[cfg_attr(
    not(debug_assertions),
    allow(
        dead_code,
        unused_imports,
        reason = "used by the screens of #27-#34; until then only by the debug gallery"
    )
)]
mod weight;

use dioxus::prelude::*;

use crate::pwa::PwaHead;
use components::BannerHost;
use shell::Route;

/// Root component: the head, the shared state, the routes and the banner.
#[component]
pub fn App() -> Element {
    shell::use_session_provider();
    errors::use_errors_provider();
    let unit = weight::use_unit_provider();
    user_settings::use_settings_provider(unit);
    programs::use_program_intents_provider();
    crate::offline::use_outbox_provider();

    rsx! {
        document::Title { "Iron Oxide" }
        PwaHead {}
        document::Meta {
            name: "viewport",
            content: "width=device-width, initial-scale=1, viewport-fit=cover",
        }
        for url in theme::FONT_URLS {
            document::Link {
                rel: "preload",
                href: url,
                r#as: "font",
                r#type: "font/woff2",
                crossorigin: "anonymous",
            }
        }
        document::Stylesheet { href: theme::APP_CSS }
        Router::<Route> {}
        BannerHost {}
        unsaved::Unsaved {}
    }
}
