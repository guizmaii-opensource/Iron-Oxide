//! The "unsaved" indicator (#30): a small pill in the top bar's status slot (`#io-status`), shown
//! only while the outbox holds writes the server has not confirmed, or something went wrong.
//! Hidden otherwise. Styled by `assets/app.css` (`.io-unsaved`), on the theme's tokens.

use dioxus::prelude::*;

use crate::auth::browser;
use crate::offline::{OutboxStatus, use_outbox};

/// Asked before giving up the refused writes and those that depend on them (a refused start
/// takes its session's sets and finish): they are lost for good, so it says how many.
fn discard_confirm(count: usize) -> String {
    let changes = if count == 1 { "change" } else { "changes" };
    format!("Discard {count} unsaved {changes}? They will not be saved.")
}

/// What the pill says.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Label {
    /// The rejected writes need the user (`role="alert"`) rather than patience.
    failed: bool,
    title: String,
    detail: Option<String>,
}

/// `None`: nothing to show.
fn label(status: &OutboxStatus) -> Option<Label> {
    if status.is_clean() {
        return None;
    }
    let count = status.pending_count;
    let changes = if count == 1 { "change" } else { "changes" };
    Some(if status.failed_count > 0 {
        Label {
            failed: true,
            title: format!("{count} {changes} not saved"),
            detail: status.last_error.clone(),
        }
    } else if count == 0 {
        Label {
            failed: false,
            title: "Not saved".to_owned(),
            detail: status.last_error.clone(),
        }
    } else {
        Label {
            failed: false,
            title: format!("{count} unsaved {changes}"),
            detail: status.last_error.clone(),
        }
    })
}

/// The indicator, rendered by the shell's top bar. Needs [`crate::offline::use_outbox_provider`]
/// above it.
#[component]
pub fn Unsaved() -> Element {
    let outbox = use_outbox();
    let Some(label) = label(&outbox.status()) else {
        return rsx! {};
    };
    let role = if label.failed { "alert" } else { "status" };
    rsx! {
        div {
            class: if label.failed { "io-unsaved io-unsaved--failed" } else { "io-unsaved" },
            role,
            "aria-live": if label.failed { "assertive" } else { "polite" },
            span { class: "io-unsaved__dot", "aria-hidden": "true" }
            span { class: "io-unsaved__text",
                span { class: "io-unsaved__title", "{label.title}" }
                if let Some(detail) = label.detail {
                    span { class: "io-unsaved__detail", "{detail}" }
                }
            }
            button {
                class: "io-unsaved__button",
                r#type: "button",
                onclick: move |_| {
                    if label.failed {
                        outbox.retry_failed();
                    } else {
                        outbox.retry_now();
                    }
                },
                "Retry"
            }
            if label.failed {
                button {
                    class: "io-unsaved__button",
                    r#type: "button",
                    onclick: move |_| {
                        let count = outbox.discard_count();
                        if count > 0 && browser::confirm(&discard_confirm(count)) {
                            outbox.discard_failed();
                        }
                    },
                    "Discard"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(pending_count: usize, failed_count: usize, error: Option<&str>) -> OutboxStatus {
        OutboxStatus {
            pending_count,
            failed_count,
            last_error: error.map(str::to_owned),
        }
    }

    #[test]
    fn hidden_when_everything_is_saved() {
        assert_eq!(label(&status(0, 0, None)), None);
    }

    #[test]
    fn counts_pending_writes() {
        let one = label(&status(1, 0, None)).unwrap();
        assert_eq!(one.title, "1 unsaved change");
        assert!(!one.failed);
        let offline = label(&status(3, 0, Some("Cannot reach the server."))).unwrap();
        assert_eq!(offline.title, "3 unsaved changes");
        assert_eq!(offline.detail.as_deref(), Some("Cannot reach the server."));
    }

    #[test]
    fn rejected_writes_are_an_alert_with_the_server_message() {
        let failed = label(&status(2, 1, Some("This session has already ended."))).unwrap();
        assert!(failed.failed);
        assert_eq!(failed.title, "2 changes not saved");
        assert_eq!(
            failed.detail.as_deref(),
            Some("This session has already ended.")
        );
    }

    #[test]
    fn the_discard_confirmation_says_how_many_changes_go() {
        assert_eq!(
            discard_confirm(1),
            "Discard 1 unsaved change? They will not be saved."
        );
        assert_eq!(
            discard_confirm(4),
            "Discard 4 unsaved changes? They will not be saved."
        );
    }
}
