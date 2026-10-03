//! The "Forge" theme (#26): the stylesheet and the self-hosted fonts.
//!
//! The tokens (dark and light) live in `assets/app.css`. The fonts are static files in
//! `public/fonts/`, so their URLs never change: `@font-face` points at them, the app preloads them
//! and the service worker precaches them (tests in `crate::pwa` check both).

use dioxus::prelude::*;

/// The only stylesheet of the app.
pub const APP_CSS: Asset = asset!("/assets/app.css");

/// The fonts, preloaded by the app root: Big Shoulders Display (numbers, titles), IBM Plex Sans
/// (body) and JetBrains Mono (labels). Variable fonts, Latin subset, SIL OFL 1.1.
pub const FONT_URLS: [&str; 3] = [
    "/fonts/big-shoulders-display-latin-wght.woff2",
    "/fonts/ibm-plex-sans-latin-wght.woff2",
    "/fonts/jetbrains-mono-latin-wght.woff2",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn css() -> String {
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/app.css"))
            .unwrap()
    }

    /// The `--io-name: #rrggbb;` declarations of a block.
    fn colours(block: &str) -> BTreeMap<String, String> {
        block
            .lines()
            .filter_map(|line| {
                let (name, value) = line.trim().strip_prefix("--io-")?.split_once(':')?;
                let value = value.split(';').next()?.trim();
                value
                    .starts_with('#')
                    .then(|| (name.to_owned(), value.to_ascii_lowercase()))
            })
            .collect()
    }

    /// The text of the block that starts at `selector`, up to its closing brace.
    fn block<'a>(css: &'a str, selector: &str) -> &'a str {
        let start = css
            .find(selector)
            .unwrap_or_else(|| panic!("no {selector}"));
        let end = start + css[start..].find('}').unwrap();
        &css[start..end]
    }

    fn dark() -> BTreeMap<String, String> {
        colours(block(&css(), ":root,\n[data-theme=\"dark\"] {"))
    }

    fn light() -> BTreeMap<String, String> {
        colours(block(&css(), "[data-theme=\"light\"] {"))
    }

    /// The parts between the `light:begin` and `light:end` markers.
    fn light_blocks(css: &str) -> Vec<String> {
        css.split("/* light:begin */")
            .skip(1)
            .map(|part| part.split("/* light:end */").next().unwrap().to_owned())
            .collect()
    }

    /// Every rule is closed before the next one starts: a rule cut in two (as a bad merge once
    /// did) makes the browser drop every rule after it, silently.
    #[test]
    fn every_rule_is_closed_before_the_next_one() {
        let css = css();
        // Comments out, keeping the line count.
        let mut text = String::new();
        let mut rest = css.as_str();
        while let Some(start) = rest.find("/*") {
            text.push_str(&rest[..start]);
            let end = rest[start..]
                .find("*/")
                .map_or(rest.len(), |end| start + end + 2);
            text.extend(rest[start..end].chars().filter(|&c| c == '\n'));
            rest = &rest[end..];
        }
        text.push_str(rest);

        let mut open: Vec<(String, usize)> = Vec::new();
        let mut selector = String::new();
        for (line_index, line) in text.lines().enumerate() {
            for c in line.chars() {
                match c {
                    '{' => {
                        let name = selector.trim().to_owned();
                        if let Some((parent, at)) = open.last() {
                            assert!(
                                parent.starts_with('@'),
                                "`{name}` (line {}) opens inside `{parent}` (line {at}), \
                                 which is not closed",
                                line_index + 1
                            );
                        }
                        open.push((name, line_index + 1));
                        selector.clear();
                    }
                    '}' => {
                        assert!(
                            open.pop().is_some(),
                            "stray `}}` at line {}",
                            line_index + 1
                        );
                        selector.clear();
                    }
                    ';' => selector.clear(),
                    _ => selector.push(c),
                }
            }
            selector.push(' ');
        }
        assert!(open.is_empty(), "unclosed: {open:?}");
    }

    #[test]
    fn the_two_light_blocks_are_identical() {
        let css = css();
        let blocks = light_blocks(&css);
        assert_eq!(blocks.len(), 2);
        let declarations = |block: &str| -> Vec<String> {
            block
                .lines()
                .map(str::trim)
                .filter(|line| line.starts_with("--io-") || line.starts_with("color-scheme"))
                .map(str::to_owned)
                .collect()
        };
        assert!(blocks[0].contains("@media (prefers-color-scheme: light)"));
        assert_eq!(declarations(&blocks[0]), declarations(&blocks[1]));
        assert!(declarations(&blocks[0]).len() > 10);
    }

    #[test]
    fn both_themes_define_the_same_tokens() {
        let (dark, light) = (dark(), light());
        assert_eq!(
            dark.keys().collect::<Vec<_>>(),
            light.keys().collect::<Vec<_>>()
        );
        // The values chosen by the maintainer (#26).
        assert_eq!(dark["ground"], "#121416");
        assert_eq!(dark["surface"], "#1b1e22");
        assert_eq!(dark["text"], "#f2ece4");
        assert_eq!(dark["muted"], "#a79f95");
        assert_eq!(dark["accent"], "#e8703a");
        assert_eq!(dark["accent-text"], "#f08a4b");
        assert_eq!(light["ground"], "#f1ede6");
        assert_eq!(light["surface"], "#ffffff");
        assert_eq!(light["text"], "#17191c");
        assert_eq!(light["muted"], "#5f5850");
        assert_eq!(light["accent"], "#e8703a");
        assert_eq!(light["accent-text"], "#b04e1a");
    }

    /// WCAG 2.x relative luminance of `#rrggbb`.
    fn luminance(hex: &str) -> f64 {
        let channel = |index: usize| {
            let value =
                f64::from(u8::from_str_radix(&hex[1 + 2 * index..3 + 2 * index], 16).unwrap())
                    / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(0) + 0.7152 * channel(1) + 0.0722 * channel(2)
    }

    /// WCAG 2.x contrast ratio.
    fn contrast(a: &str, b: &str) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn contrast_ratio_matches_known_values() {
        assert!((contrast("#000000", "#ffffff") - 21.0).abs() < 1e-9);
        assert!((contrast("#777777", "#777777") - 1.0).abs() < 1e-9);
    }

    /// AA for normal text: 4.5:1. Every text colour on every background it is used on, in both
    /// themes. The ratios are listed in docs/palette.md.
    #[test]
    fn text_colours_pass_wcag_aa_in_both_themes() {
        let pairs = [
            ("text", "ground"),
            ("text", "surface"),
            ("text", "raised"),
            ("text", "chip"),
            ("muted", "ground"),
            ("muted", "surface"),
            ("muted", "raised"),
            ("accent-text", "ground"),
            ("accent-text", "surface"),
            ("on-accent", "accent"),
            ("on-action", "action"),
            ("danger", "ground"),
            ("danger", "surface"),
            ("success", "surface"),
            ("warning", "surface"),
        ];
        for (name, theme) in [("dark", dark()), ("light", light())] {
            for (foreground, background) in pairs {
                let ratio = contrast(&theme[foreground], &theme[background]);
                assert!(
                    ratio >= 4.5,
                    "{name}: {foreground} on {background} is {ratio:.2}:1"
                );
            }
        }
    }

    /// AA for UI components: 3:1 for input outlines and the focus ring.
    #[test]
    fn control_outlines_pass_wcag_aa_in_both_themes() {
        for (name, theme) in [("dark", dark()), ("light", light())] {
            for (foreground, background) in [
                ("line-strong", "ground"),
                ("line-strong", "surface"),
                ("focus", "ground"),
                ("focus", "surface"),
            ] {
                let ratio = contrast(&theme[foreground], &theme[background]);
                assert!(
                    ratio >= 3.0,
                    "{name}: {foreground} on {background} is {ratio:.2}:1"
                );
            }
        }
    }

    /// A selected chip differs from an unselected one by at least 3:1 (WCAG 1.4.11). In the light
    /// theme the fill alone is about 2.4:1 (#104), so its inset ring is the cue; in dark the fill
    /// reaches 3:1 and the ring is the fill's colour (no visible ring).
    #[test]
    fn a_selected_chip_has_a_3_to_1_cue_in_both_themes() {
        let css = css();
        let selected = block(&css, ".io-chip[aria-pressed=\"true\"] {");
        assert!(
            selected.contains("box-shadow: inset 0 0 0 2px var(--io-chip-ring);"),
            "{selected}"
        );
        let neighbours = ["chip", "ground", "surface"];
        let light = light();
        assert!(
            contrast(&light["accent"], &light["chip"]) < 3.0,
            "why the ring exists"
        );
        for neighbour in neighbours {
            let ratio = contrast(&light["chip-ring"], &light[neighbour]);
            assert!(
                ratio >= 3.0,
                "light: the ring next to {neighbour} is {ratio:.2}:1"
            );
        }
        let dark = dark();
        assert_eq!(dark["chip-ring"], dark["accent"], "no visible ring in dark");
        for neighbour in neighbours {
            let ratio = contrast(&dark["accent"], &dark[neighbour]);
            assert!(
                ratio >= 3.0,
                "dark: the fill next to {neighbour} is {ratio:.2}:1"
            );
        }
    }

    /// Tap targets are 56 px; 44 px is only for the header's icon buttons (and the header rows:
    /// the top bar and the workout's header, and the unsaved indicator's buttons in the top bar).
    #[test]
    fn only_header_icon_buttons_use_the_small_tap_size() {
        let css = css();
        let mut selectors = Vec::new();
        for (index, _) in css.match_indices("var(--io-tap-small)") {
            let block_start = css[..index].rfind('{').unwrap();
            let rule_start = css[..block_start].rfind('}').map_or(0, |end| end + 1);
            selectors.push(
                css[rule_start..block_start]
                    .trim()
                    .rsplit("*/")
                    .next()
                    .unwrap()
                    .trim(),
            );
        }
        selectors.sort_unstable();
        selectors.dedup();
        assert_eq!(
            selectors,
            [
                ".io-icon-button",
                ".io-session-header",
                ".io-topbar",
                ".io-unsaved__button"
            ]
        );
        assert!(block(&css, "button.io-chip {").contains("min-height: var(--io-tap);"));
        assert!(block(&css, ".io-banner .io-icon-button {").contains("height: var(--io-tap);"));
    }

    #[test]
    fn every_font_is_declared_in_the_stylesheet() {
        let css = css();
        for url in FONT_URLS {
            assert!(css.contains(&format!("url(\"{url}\")")), "{url}");
        }
        assert_eq!(css.matches("@font-face").count(), FONT_URLS.len());
        // Nothing is loaded from another origin.
        assert!(!css.contains("http://") && !css.contains("https://"));
    }

    #[test]
    fn every_font_has_its_licence() {
        let fonts = Path::new(env!("CARGO_MANIFEST_DIR")).join("public/fonts");
        for url in FONT_URLS {
            let name = url
                .strip_prefix("/fonts/")
                .and_then(|name| name.strip_suffix("-latin-wght.woff2"))
                .unwrap();
            let licence = std::fs::read_to_string(fonts.join(format!("{name}-OFL.txt"))).unwrap();
            assert!(licence.contains("SIL Open Font License"), "{name}");
        }
    }
}
