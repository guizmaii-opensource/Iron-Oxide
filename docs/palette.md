# Colours, type and components

Iron Oxide uses the **Forge** direction (#26): dark-first, with a light theme that follows the
system (`prefers-color-scheme`). It is meant to be read on a phone, at arm's length, under gym
lighting: huge numerals, 72 px steppers, a 76 px Done button.

Everything lives in one stylesheet, `crates/iron-oxide-app/assets/app.css`, loaded by the root
component. Components use the `--io-*` tokens and never hard-code hex values. `data-theme="light"`
or `data-theme="dark"` on an element forces one theme for its subtree (the component gallery uses
it). The light tokens are written twice (in the media query and under `[data-theme="light"]`); a
unit test keeps the two blocks identical.

The dark ground `#121416` is also in `public/manifest.webmanifest` (`theme_color`,
`background_color`) and in `THEME_COLOR` in `src/pwa.rs`; the light ground `#f1ede6` in
`THEME_COLOR_LIGHT`. Tests check they match.

## Tokens

| Token | Dark | Light | Use |
|---|---|---|---|
| `--io-ground` | `#121416` | `#f1ede6` | Page background |
| `--io-surface` | `#1b1e22` | `#ffffff` | Cards |
| `--io-surface-edge` | `#1b1e22` | `#e2dcd2` | Card border (invisible in dark) |
| `--io-raised` | `#2a2f35` | `#ece7df` | Secondary buttons, the stepper's minus |
| `--io-chip` | `#24282d` | `#e6e1d8` | Chips |
| `--io-chip-ring` | `#e8703a` | `#17191c` | Selected chip ring (light only: in dark it is the fill) |
| `--io-track` | `#3a3f46` | `#d6d0c6` | Empty progress segments |
| `--io-track-soft` | `#2a2f35` | `#ddd7cd` | Empty progress bars |
| `--io-line` | `#2e3339` | `#d3ccc1` | Decorative borders |
| `--io-line-strong` | `#707780` | `#8c857b` | Input outlines (3:1) |
| `--io-text` | `#f2ece4` | `#17191c` | Text |
| `--io-muted` | `#a79f95` | `#5f5850` | Secondary text, labels |
| `--io-accent` | `#e8703a` | `#e8703a` | Accent **fills only** (stepper plus, progress, selected chip) |
| `--io-on-accent` | `#121416` | `#17191c` | Text on accent fills |
| `--io-accent-text` | `#f08a4b` | `#b04e1a` | Accent as text: labels, links, the active tab |
| `--io-action` / `--io-on-action` | `#e8703a` / `#121416` | `#17191c` / `#f1ede6` | The primary action (Done, primary buttons) |
| `--io-danger` | `#f06a5b` | `#b3261e` | Errors, destructive buttons |
| `--io-success` | `#5fbf7f` | `#256b40` | Success notes |
| `--io-warning` | `#e8b04a` | `#8a5a00` | Warnings |
| `--io-focus` | `#f08a4b` | `#b04e1a` | Focus ring |

The older names (`--io-bg`, `--io-surface-2`, `--io-border`, `--io-text-muted`, `--io-primary`,
`--io-on-primary`, `--io-primary-strong`) remain as aliases.

## Contrast

WCAG 2.x ratios. AA needs 4.5:1 for text, 3:1 for large text and UI component boundaries. The unit
tests in `src/ui/theme.rs` read the stylesheet and fail below these thresholds.

| Pair | Dark | Light |
|---|---|---|
| text on ground / surface | 15.73 / 14.25 | 15.09 / 17.61 |
| muted on ground / surface | 7.07 / 6.40 | 6.00 / 7.00 |
| accent-text on ground / surface | 7.42 / 6.72 | 4.56 / 5.32 |
| on-accent on accent | 5.99 | 5.71 |
| on-action on action | 5.99 | 15.09 |
| danger on ground / surface | 6.08 / 5.51 | 5.60 / 6.54 |
| line-strong on surface / ground | 3.70 | 3.65 / 3.13 |

Rules:

- Never use `--io-accent` as text: on the light ground it is 2.6:1. Use `--io-accent-text`.
- A selected chip is not told by its fill alone in the light theme: the accent against the light
  chip is 2.4:1. It also gets a 2 px inset ring (`--io-chip-ring`, `#17191c`), at least 3:1
  against the chip, the ground and the surface. In dark the fill is already over 3:1 and the
  ring token is the fill's colour, so no ring shows.
- The accent progress segments against the light track are below 3:1; progress is always also
  given as text ("SET 2 / 5") and through `aria-valuenow`.

## Type

Self-hosted from `public/fonts/` (Fontsource variable builds, Latin subset, SIL Open Font License
1.1, licence files next to them), preloaded and precached, so the app makes no third-party request
and works offline.

| Family | Use |
|---|---|
| Big Shoulders Display 700/900 | Numbers, titles (uppercase), big buttons |
| IBM Plex Sans 400/500/600 | Body |
| JetBrains Mono 500/700 | Labels: uppercase, letter-spacing 0.12em |

## Components

`src/ui/components/`: `Button` (primary, secondary, ghost, danger; 56 px, `xl` 76 px),
`IconButton` (44 px), `Stepper` and `WeightStepper` (72 px buttons), `Card`, `Chip`,
`ProgressSegments`, `BannerHost` (the error banner), `LoadingState` and `EmptyState`.
Debug builds show all of them, in both themes, at `/dev/components`.

Server errors go to the banner through `ui::errors::use_errors().report(&error)`, which uses
`ApiFailure::classify` (`docs/api.md`): a `401` shows the sign-in screen, a `429` says how long to
wait. Weights are shown with `ui::weight` (`weight_number`, `weight_text`) in the user's unit.

## Icon

The app icon is a front-on barbell grip plate: a rust-orange plate shading into oxide red, three grip slots, and an iron hub on an iron-grey background. The SVG sources live in `crates/iron-oxide-app/icons/`. `icons/render.sh` regenerates the PNGs and `favicon.ico` in `public/` (it needs `rsvg-convert` and ImageMagick). The maskable variant keeps the plate inside the central 80% safe zone, and the apple-touch icon is opaque because iOS applies its own mask.
