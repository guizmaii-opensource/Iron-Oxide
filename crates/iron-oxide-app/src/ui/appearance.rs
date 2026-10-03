//! The theme choice (#121): System (the default, follows `prefers-color-scheme`), Light or Dark.
//!
//! The choice belongs to the device, not to the account: a phone and a laptop may want different
//! themes. It is kept in `localStorage` under one key, signed in or out; when storage is blocked
//! the app follows the system.
//!
//! An explicit choice is `data-theme` on the root element (the tokens of `assets/app.css`), and
//! the `theme-color` metas take its ground colour. [`ThemeHead`] applies the stored choice with an
//! inline script in `<head>`, before the first paint, so the page never flashes the other theme
//! (the sign-in screen included). The app shell sends no Content-Security-Policy today; the script
//! is fixed text, so a future policy can allow it by its hash.

use dioxus::prelude::*;

use super::components::Card;
use crate::pwa::{THEME_COLOR, THEME_COLOR_LIGHT};

/// The `localStorage` key of the choice, one per device.
pub const STORAGE_KEY: &str = "iron-oxide:theme";

/// The theme the user chose on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    /// Follows the system's light or dark setting.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    /// The stored value read back: anything but `light` or `dark` (nothing stored, storage
    /// blocked, an unknown value) is System.
    #[must_use]
    pub fn parse(stored: Option<&str>) -> Self {
        match stored.map(str::trim) {
            Some("light") => Self::Light,
            Some("dark") => Self::Dark,
            _ => Self::System,
        }
    }

    /// The value stored, and of `data-theme`; System stores nothing.
    #[must_use]
    pub const fn stored(self) -> Option<&'static str> {
        match self {
            Self::System => None,
            Self::Light => Some("light"),
            Self::Dark => Some("dark"),
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    /// The choice as a JavaScript expression for [`apply_script`].
    fn js_literal(self) -> &'static str {
        match self {
            Self::System => "null",
            Self::Light => "\"light\"",
            Self::Dark => "\"dark\"",
        }
    }
}

/// A script that applies `theme` (a JavaScript expression giving `"light"`, `"dark"` or anything
/// else for System): `data-theme` on the root element, and the `theme-color` metas' colour. Under
/// System each meta gets back its own scheme's colour (they carry a `media` query).
fn apply_script(theme: &str) -> String {
    format!(
        r#"(function (theme) {{
  var root = document.documentElement;
  var explicit = theme === "light" || theme === "dark";
  if (explicit) {{
    root.setAttribute("data-theme", theme);
  }} else {{
    root.removeAttribute("data-theme");
  }}
  var metas = document.querySelectorAll('meta[name="theme-color"]');
  for (var i = 0; i < metas.length; i++) {{
    var light = explicit
      ? theme === "light"
      : (metas[i].getAttribute("media") || "").indexOf("light") >= 0;
    metas[i].setAttribute("content", light ? "{THEME_COLOR_LIGHT}" : "{THEME_COLOR}");
  }}
}})({theme});"#
    )
}

/// The script run before the first paint: the stored choice, or System when there is none or
/// storage is blocked.
#[must_use]
pub fn pre_paint_script() -> String {
    apply_script(&format!(
        r#"(function () {{
  try {{
    return window.localStorage.getItem("{STORAGE_KEY}");
  }} catch (error) {{
    return null;
  }}
}})()"#
    ))
}

/// Applies the stored choice before the first paint. Goes in `<head>` after [`crate::pwa::PwaHead`]
/// (the `theme-color` metas must exist when it runs).
#[component]
pub fn ThemeHead() -> Element {
    let script = pre_paint_script();
    // Applied again once the client runs, in case hydration rewrote the metas.
    use_effect(|| apply(device::read()));
    rsx! {
        document::Script { "{script}" }
    }
}

/// Applies `choice` to the page now. The script is fixed text with one of three literals spliced
/// in, never user input.
fn apply(choice: ThemeChoice) {
    document::eval(&apply_script(choice.js_literal()));
}

/// The Appearance card of Settings: the theme, as a three-way segmented control.
#[component]
pub fn AppearanceCard() -> Element {
    // The server cannot know the device's choice: read once the client runs.
    let mut choice = use_signal(ThemeChoice::default);
    let mut not_kept = use_signal(|| false);
    use_effect(move || choice.set(device::read()));
    let current = *choice.read();

    rsx! {
        Card { title: "Appearance",
            div { class: "io-setting",
                span { id: "theme-label", class: "io-setting-name", "Theme" }
                span { class: "io-muted",
                    if current == ThemeChoice::System {
                        "Follows your device"
                    } else {
                        "On this device only"
                    }
                }
            }
            div {
                class: "io-segmented",
                role: "radiogroup",
                aria_labelledby: "theme-label",
                for option in ThemeChoice::ALL {
                    label { key: "{option.label()}", class: "io-segment",
                        input {
                            r#type: "radio",
                            name: "theme",
                            value: option.stored().unwrap_or("system"),
                            checked: option == current,
                            onchange: move |_| {
                                choice.set(option);
                                not_kept.set(!device::write(option));
                                apply(option);
                            },
                        }
                        span { "{option.label()}" }
                    }
                }
            }
            if *not_kept.read() {
                p { class: "io-muted io-hint", role: "status",
                    "This browser does not let the app remember it: the theme goes back to System on reload."
                }
            }
        }
    }
}

/// The choice in `localStorage`, best effort.
#[cfg(feature = "web")]
mod device {
    use super::{STORAGE_KEY, ThemeChoice};

    fn local_storage() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }

    pub fn read() -> ThemeChoice {
        let stored =
            local_storage().and_then(|storage| storage.get_item(STORAGE_KEY).ok().flatten());
        ThemeChoice::parse(stored.as_deref())
    }

    /// Whether the choice was stored.
    pub fn write(choice: ThemeChoice) -> bool {
        let Some(storage) = local_storage() else {
            return false;
        };
        match choice.stored() {
            Some(value) => storage.set_item(STORAGE_KEY, value).is_ok(),
            None => storage.remove_item(STORAGE_KEY).is_ok(),
        }
    }
}

/// No storage outside the browser.
#[cfg(not(feature = "web"))]
mod device {
    use super::ThemeChoice;

    /// Nothing stored: System.
    pub fn read() -> ThemeChoice {
        ThemeChoice::parse(None)
    }

    pub const fn write(_choice: ThemeChoice) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_values_are_read_back() {
        assert_eq!(ThemeChoice::parse(Some("light")), ThemeChoice::Light);
        assert_eq!(ThemeChoice::parse(Some("dark")), ThemeChoice::Dark);
        assert_eq!(ThemeChoice::parse(Some(" dark\n")), ThemeChoice::Dark);
        for choice in ThemeChoice::ALL {
            assert_eq!(ThemeChoice::parse(choice.stored()), choice);
        }
    }

    #[test]
    fn anything_else_is_system() {
        for stored in [
            None,
            Some(""),
            Some("system"),
            Some("Dark"),
            Some("auto"),
            Some("null"),
        ] {
            assert_eq!(
                ThemeChoice::parse(stored),
                ThemeChoice::System,
                "{stored:?}"
            );
        }
    }

    #[test]
    fn system_stores_nothing() {
        assert_eq!(ThemeChoice::System.stored(), None);
        assert_eq!(ThemeChoice::default(), ThemeChoice::System);
    }

    /// The script and the stylesheet agree on the values, and the script on the key and colours.
    #[test]
    fn the_pre_paint_script_uses_the_same_key_values_and_colours() {
        let script = pre_paint_script();
        assert!(script.contains(&format!("localStorage.getItem(\"{STORAGE_KEY}\")")));
        assert!(
            script.contains("catch (error)"),
            "blocked storage is System"
        );
        let css = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/app.css"),
        )
        .unwrap();
        for choice in [ThemeChoice::Light, ThemeChoice::Dark] {
            let value = choice.stored().unwrap();
            assert!(
                script.contains(&format!("theme === \"{value}\"")),
                "{value}"
            );
            assert!(
                css.contains(&format!("[data-theme=\"{value}\"] {{")),
                "{value}"
            );
        }
        assert!(script.contains(&format!("\"{THEME_COLOR_LIGHT}\"")));
        assert!(script.contains(&format!("\"{THEME_COLOR}\"")));
        assert!(
            !script.contains("{{") && !script.contains("}}"),
            "format escapes left over"
        );
    }

    #[test]
    fn the_choice_is_applied_as_a_literal() {
        assert!(apply_script(ThemeChoice::Light.js_literal()).ends_with("})(\"light\");"));
        assert!(apply_script(ThemeChoice::Dark.js_literal()).ends_with("})(\"dark\");"));
        assert!(apply_script(ThemeChoice::System.js_literal()).ends_with("})(null);"));
    }
}
