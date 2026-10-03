//! A segmented control: one choice out of a few, as equal segments 56 px high.

use dioxus::prelude::*;

/// One segment: the value it stands for and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub value: String,
    pub label: String,
}

impl Segment {
    #[must_use]
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

/// Native radio buttons in a radio group named `label` (arrow keys move the choice). `name`
/// groups the radios, so it must be unique on the page. A chip is a toggle button instead.
#[component]
pub fn Segmented(
    #[props(into)] name: String,
    #[props(into)] label: String,
    segments: Vec<Segment>,
    #[props(into)] selected: String,
    on_change: EventHandler<String>,
) -> Element {
    rsx! {
        div { class: "io-segmented", role: "radiogroup", aria_label: "{label}",
            for segment in segments {
                label { key: "{segment.value}", class: "io-segment",
                    input {
                        r#type: "radio",
                        name: "{name}",
                        value: "{segment.value}",
                        checked: segment.value == selected,
                        onchange: {
                            let value = segment.value.clone();
                            move |_| on_change.call(value.clone())
                        },
                    }
                    span { "{segment.label}" }
                }
            }
        }
    }
}
