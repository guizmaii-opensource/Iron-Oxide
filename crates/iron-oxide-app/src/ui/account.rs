//! The account panel: sign up / sign in when signed out; passkeys, Google and sign-out when
//! signed in (#5).
//!
//! The signed-in user is loaded with `me()` on the client only; server-side rendering shows the
//! neutral "Loading" state so hydration matches. Only one ceremony runs at a time: the server
//! keeps a single ceremony per session, so a second one would replace the first.

use std::rc::Rc;

use dioxus::prelude::*;

use super::shell::{SessionStatus, set_session, use_session};
use crate::api::error::ApiFailure;
use crate::auth::api::{
    google_begin, google_unlink, is_unauthorized, me, passkey_add_begin, passkey_add_finish,
    passkey_remove, passkey_sign_in_begin, passkey_sign_in_finish, passkey_sign_up_begin,
    passkey_sign_up_finish, rename_account, sign_out, sign_out_everywhere,
};
use crate::auth::browser::{
    self, BrowserError, GoogleCallbackListener, GoogleNavigation, GooglePopup,
};
use crate::auth::types::{
    GoogleCallbackMessage, GoogleIntent, MAX_NAME_CHARS, Me, PasskeyId, PasskeyInfo, normalize_name,
};
use crate::offline::use_outbox;

/// How often the app re-checks `me()` while waiting for Google. The popup's `done` message is the
/// fast path; this catches a popup that finished in the app's cookie jar without the message
/// getting through (timers pause in the background, so it also fires soon after returning).
const GOOGLE_POLL_MS: i32 = 2_000;

/// What the panel shows.
#[derive(Debug, Clone, PartialEq)]
enum AccountState {
    /// Waiting for `me()` (and the server-rendered state).
    Loading,
    /// `me()` failed for another reason than being signed out.
    LoadFailed(String),
    SignedOut,
    SignedIn(Me),
}

/// A message under the buttons: an error (`role="alert"`) or a mild note (`role="status"`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Notice {
    Error(String),
    Info(String),
}

/// The ceremony or call in flight; every sign-in button is disabled meanwhile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    SignUp,
    SignIn,
    Google,
    AddPasskey,
    RemovePasskey(PasskeyId),
    UnlinkGoogle,
    SignOut,
    Rename,
    SignOutEverywhere,
}

/// Why a sign-in step failed.
#[derive(Debug)]
enum Failure {
    Server(ServerFnError),
    Browser(BrowserError),
}

/// Whether the server says we are not signed in. The 401 can arrive as a decoded server error or
/// as a bare HTTP status, depending on where the request was rejected.
fn is_signed_out_error(error: &ServerFnError) -> bool {
    is_unauthorized(error)
}

/// The text shown for a failed server call: the server's own message for 4xx and 503 answers, a
/// generic one otherwise (see `ApiFailure::classify`).
fn server_message(error: &ServerFnError) -> String {
    ApiFailure::classify(error).message
}

/// The notice for a failure. A cancelled passkey prompt is a gentle note, not an error.
fn failure_notice(failure: &Failure) -> Notice {
    match failure {
        Failure::Browser(error) if error.is_cancelled() => Notice::Info(error.to_string()),
        Failure::Browser(error) => Notice::Error(error.to_string()),
        Failure::Server(error) => Notice::Error(server_message(error)),
    }
}

/// The name to save, trimmed, or why it cannot be saved (the server checks the same).
fn new_name(raw: &str) -> Result<String, String> {
    match normalize_name(raw) {
        Ok(Some(name)) => Ok(name),
        Ok(None) => Err("Your name must not be blank.".to_owned()),
        Err(problem) => Err(format!("Your name {problem}.")),
    }
}

/// Where the panel goes after a call that returns the signed-in user.
#[derive(Debug, Clone, PartialEq)]
struct Outcome {
    /// The new state, or `None` to keep the current one.
    state: Option<AccountState>,
    notice: Option<Notice>,
}

/// Decides the outcome of a call returning `Me`. A 401 while signed in means the session ended.
fn outcome(result: Result<Me, Failure>, signed_in: bool, success: Option<&str>) -> Outcome {
    match result {
        Ok(me) => Outcome {
            state: Some(AccountState::SignedIn(me)),
            notice: success.map(|text| Notice::Info(text.to_owned())),
        },
        Err(Failure::Server(error)) if signed_in && is_signed_out_error(&error) => Outcome {
            state: Some(AccountState::SignedOut),
            notice: Some(Notice::Info(
                "Your session has ended. Please sign in again.".to_owned(),
            )),
        },
        Err(failure) => Outcome {
            state: None,
            notice: Some(failure_notice(&failure)),
        },
    }
}

/// The result of loading `me()`.
fn loaded(result: Result<Me, ServerFnError>) -> AccountState {
    match result {
        Ok(me) => AccountState::SignedIn(me),
        Err(error) if is_signed_out_error(&error) => AccountState::SignedOut,
        Err(error) => AccountState::LoadFailed(server_message(&error)),
    }
}

/// The date part of an RFC 3339 timestamp (`2026-09-28T…` → `2026-09-28`).
fn short_date(timestamp: &str) -> &str {
    match timestamp.get(..10) {
        Some(date)
            if date.bytes().enumerate().all(|(i, b)| {
                if i == 4 || i == 7 {
                    b == b'-'
                } else {
                    b.is_ascii_digit()
                }
            }) =>
        {
            date
        }
        _ => timestamp,
    }
}

/// The panel's shared state. `Copy`, so event handlers and tasks can each take one.
#[derive(Clone, Copy, PartialEq)]
struct Auth {
    state: Signal<AccountState>,
    busy: Signal<Option<Busy>>,
    notice: Signal<Option<Notice>>,
    /// The Google flow this window is waiting for, if any. Taken by the first callback message,
    /// so the copy arriving on the other path is ignored.
    google: Signal<Option<GoogleIntent>>,
    popup: Signal<Option<GooglePopup>>,
}

impl Auth {
    fn signed_in(&self) -> bool {
        matches!(*self.state.peek(), AccountState::SignedIn(_))
    }

    /// Marks `busy` as started, unless something else is running.
    fn start(mut self, busy: Busy) -> bool {
        if self.busy.peek().is_some() {
            return false;
        }
        self.busy.set(Some(busy));
        self.notice.set(None);
        true
    }

    fn apply(mut self, outcome: Outcome) {
        if let Some(state) = outcome.state {
            self.state.set(state);
        }
        self.notice.set(outcome.notice);
        self.busy.set(None);
    }

    fn finish(self, result: Result<Me, Failure>, success: Option<&str>) {
        self.apply(outcome(result, self.signed_in(), success));
    }

    fn fail(self, notice: Notice) {
        self.apply(Outcome {
            state: None,
            notice: Some(notice),
        });
    }

    /// Stops waiting for Google and closes the popup.
    fn end_google(mut self) {
        self.google.set(None);
        if let Some(popup) = self.popup.take() {
            popup.close();
        }
    }

    async fn load(mut self) {
        self.state.set(AccountState::Loading);
        self.state.set(loaded(me().await));
    }
}

/// The account panel.
#[component]
pub fn Account() -> Element {
    let auth = Auth {
        state: use_signal(|| AccountState::Loading),
        busy: use_signal(|| None),
        notice: use_signal(|| None),
        google: use_signal(|| None),
        popup: use_signal(|| None),
    };

    // The Google callback messages, handled one at a time inside the Dioxus runtime (the
    // browser listeners run outside it and only forward).
    let messages = use_coroutine(
        move |mut events: UnboundedReceiver<GoogleEvent>| async move {
            while let Ok(event) = events.recv().await {
                handle_google_event(auth, event).await;
            }
        },
    );
    use_hook(move || {
        let sender = messages.tx();
        Rc::new(GoogleCallbackListener::install(move |message| {
            // Only fails once the panel is gone.
            let _ = sender.unbounded_send(GoogleEvent::Message(message));
        }))
    });
    // While waiting for Google, check with the server regularly (see `GOOGLE_POLL_MS`). Client
    // only; the check goes through the same queue as the callback messages.
    use_hook(move || {
        if cfg!(feature = "web") {
            let sender = messages.tx();
            spawn(async move {
                loop {
                    browser::sleep(GOOGLE_POLL_MS).await;
                    if auth.google.peek().is_some()
                        && sender.unbounded_send(GoogleEvent::Check).is_err()
                    {
                        break;
                    }
                }
            });
        }
    });

    // Client only: on the server the panel stays "Loading" so hydration matches.
    use_effect(move || {
        if cfg!(feature = "web") {
            spawn(auth.load());
        }
    });

    // The shell shows the app or the sign-in screen from the session status: keep it in step.
    let session = use_session();
    use_effect(move || match *auth.state.read() {
        AccountState::SignedIn(_) => set_session(session, SessionStatus::SignedIn),
        AccountState::SignedOut => set_session(session, SessionStatus::SignedOut),
        AccountState::Loading | AccountState::LoadFailed(_) => {}
    });

    // The outbox (#30) sends the signed-in user's writes.
    let outbox = use_outbox();
    use_effect(move || {
        if let AccountState::SignedIn(me) = &*auth.state.read() {
            outbox.signed_in(me.user_id);
        }
    });

    let state = auth.state.read().clone();
    rsx! {
        section { class: "io-card io-account", "aria-labelledby": "account-title",
            match state {
                AccountState::Loading => rsx! {
                    h2 { id: "account-title", "Your account" }
                    p { class: "io-muted", role: "status", "Loading…" }
                },
                AccountState::LoadFailed(message) => rsx! {
                    h2 { id: "account-title", "Your account" }
                    p { class: "io-notice io-notice-error", role: "alert", "Could not load your account: {message}" }
                    button {
                        class: "io-button io-button-primary",
                        onclick: move |_| {
                            spawn(auth.load());
                        },
                        "Try again"
                    }
                },
                AccountState::SignedOut => rsx! { SignedOut { auth } },
                AccountState::SignedIn(me) => rsx! { SignedIn { auth, me } },
            }
            NoticeView { auth }
        }
    }
}

/// Something for the Google flow to react to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GoogleEvent {
    /// The callback page's message.
    Message(GoogleCallbackMessage),
    /// Time to re-check `me()` (see `GOOGLE_POLL_MS`).
    Check,
}

/// Whether the account now shows the Google flow as complete.
fn google_complete(intent: GoogleIntent, me: &Me) -> bool {
    match intent {
        // Waiting started signed out: any account now means the sign-in went through.
        GoogleIntent::SignIn => true,
        GoogleIntent::Link => me.google_linked,
    }
}

/// Handles one Google event, if this window is waiting for Google.
async fn handle_google_event(auth: Auth, event: GoogleEvent) {
    let Some(intent) = auth.google.peek().as_ref().copied() else {
        // Not waiting (already handled, or the user pressed Cancel). A late `done` still means
        // the session changed server-side: reload so the panel matches it.
        if event == GoogleEvent::Message(GoogleCallbackMessage::Done) && auth.busy.peek().is_none()
        {
            auth.load().await;
        }
        return;
    };
    let announced_done = match event {
        GoogleEvent::Message(GoogleCallbackMessage::Error { message }) => {
            auth.end_google();
            auth.fail(Notice::Error(message));
            return;
        }
        GoogleEvent::Message(GoogleCallbackMessage::Done) => true,
        GoogleEvent::Check => false,
    };
    // The callback completes the flow in the browser context that holds our session cookie;
    // all the app has to do is look at the account again.
    let result = me().await;
    if auth.google.peek().is_none() {
        // Cancelled meanwhile.
        return;
    }
    let success = match intent {
        GoogleIntent::SignIn => None,
        GoogleIntent::Link => Some("Google account linked."),
    };
    match result {
        Ok(me) if google_complete(intent, &me) => {
            auth.end_google();
            auth.finish(Ok(me), success);
        }
        // Not there yet: keep waiting, unless the popup said it was done.
        _ if !announced_done => {}
        Ok(me) => {
            auth.end_google();
            auth.finish(Ok(me), None);
            let mut notice = auth.notice;
            notice.set(Some(Notice::Error(
                "Google was not linked. Please try again.".to_owned(),
            )));
        }
        Err(error) if is_signed_out_error(&error) => {
            auth.end_google();
            auth.fail(Notice::Error(
                "Google sign-in did not complete. Please try again.".to_owned(),
            ));
        }
        Err(error) => {
            auth.end_google();
            auth.finish(Err(Failure::Server(error)), None);
        }
    }
}

/// Starts the Google flow. Must run synchronously in the tap's handler: the popup is opened
/// before anything is awaited (iOS Safari blocks popups opened later).
fn start_google(auth: Auth, intent: GoogleIntent) {
    start_google_in(auth, intent, GooglePopup::open());
}

/// Restarts the Google flow as a full-page redirect in this window: for when the popup cannot
/// finish it (in an installed iOS web app the popup may not share the app's cookies).
fn restart_google_here(mut auth: Auth) {
    let Some(intent) = auth.google.peek().as_ref().copied() else {
        return;
    };
    auth.end_google();
    auth.busy.set(None);
    start_google_in(auth, intent, GooglePopup::this_window());
}

fn start_google_in(mut auth: Auth, intent: GoogleIntent, popup: GooglePopup) {
    if !auth.start(Busy::Google) {
        return;
    }
    auth.google.set(Some(intent));
    spawn(async move {
        let url = match google_begin(intent, popup.is_open()).await {
            Ok(url) => url,
            Err(error) => {
                popup.close();
                auth.end_google();
                auth.finish(Err(Failure::Server(error)), None);
                return;
            }
        };
        match popup.navigate(&url) {
            Ok(GoogleNavigation::Popup) => auth.popup.set(Some(popup)),
            // This page is navigating to Google; the callback brings the user back.
            Ok(GoogleNavigation::Redirect) => {}
            Err(_) => {
                auth.end_google();
                auth.fail(Notice::Error(
                    "Could not open Google sign-in. Please try again.".to_owned(),
                ));
            }
        }
    });
}

/// Cancels waiting for Google (the user closed the popup, or changed their mind).
fn cancel_google(mut auth: Auth) {
    auth.end_google();
    auth.busy.set(None);
    auth.notice
        .set(Some(Notice::Info("Google sign-in cancelled.".to_owned())));
}

#[component]
fn NoticeView(auth: Auth) -> Element {
    match auth.notice.read().clone() {
        Some(Notice::Error(text)) => rsx! {
            p { class: "io-notice io-notice-error", role: "alert", "{text}" }
        },
        Some(Notice::Info(text)) => rsx! {
            p { class: "io-notice io-notice-info", role: "status", "{text}" }
        },
        None => rsx! {},
    }
}

#[component]
fn GoogleWaiting(auth: Auth) -> Element {
    if *auth.busy.read() != Some(Busy::Google) {
        return rsx! {};
    }
    rsx! {
        div { class: "io-waiting", role: "status",
            p { "Continue in the Google window. If it does not bring you back signed in, continue in this window instead." }
            button {
                class: "io-button io-button-secondary",
                onclick: move |_| restart_google_here(auth),
                "Continue in this window"
            }
            button {
                class: "io-button io-button-secondary",
                onclick: move |_| cancel_google(auth),
                "Cancel"
            }
        }
    }
}

#[component]
fn SignedOut(auth: Auth) -> Element {
    let mut name = use_signal(String::new);
    let busy = *auth.busy.read();
    let disabled = busy.is_some();

    let sign_up = move |_| {
        let display_name = match normalize_name(&name.peek()) {
            Ok(display_name) => display_name.unwrap_or_default(),
            Err(problem) => {
                auth.fail(Notice::Error(format!("Your name {problem}.")));
                return;
            }
        };
        if !auth.start(Busy::SignUp) {
            return;
        }
        spawn(async move {
            let result = async {
                let options = passkey_sign_up_begin(display_name)
                    .await
                    .map_err(Failure::Server)?;
                let credential = browser::create_passkey(options)
                    .await
                    .map_err(Failure::Browser)?;
                passkey_sign_up_finish(credential)
                    .await
                    .map_err(Failure::Server)
            }
            .await;
            auth.finish(result, None);
        });
    };

    let sign_in = move |_| {
        if !auth.start(Busy::SignIn) {
            return;
        }
        spawn(async move {
            let result = async {
                let options = passkey_sign_in_begin().await.map_err(Failure::Server)?;
                let credential = browser::get_passkey(options)
                    .await
                    .map_err(Failure::Browser)?;
                passkey_sign_in_finish(credential)
                    .await
                    .map_err(Failure::Server)
            }
            .await;
            auth.finish(result, None);
        });
    };

    rsx! {
        h2 { id: "account-title", "Sign in" }
        p { class: "io-muted", "Use a passkey saved on your phone or computer, or your Google account." }
        div { class: "io-field",
            label { r#for: "display-name", "Your name " span { class: "io-muted", "(optional, for a new account)" } }
            input {
                id: "display-name",
                class: "io-input",
                r#type: "text",
                autocomplete: "name",
                maxlength: MAX_NAME_CHARS,
                value: "{name}",
                disabled,
                oninput: move |event| name.set(event.value()),
            }
        }
        div { class: "io-actions",
            button {
                id: "passkey-sign-in",
                class: "io-button io-button-primary",
                disabled,
                "aria-busy": busy == Some(Busy::SignIn),
                onclick: sign_in,
                if busy == Some(Busy::SignIn) { "Signing in…" } else { "Sign in with a passkey" }
            }
            button {
                id: "passkey-sign-up",
                class: "io-button io-button-secondary",
                disabled,
                "aria-busy": busy == Some(Busy::SignUp),
                onclick: sign_up,
                if busy == Some(Busy::SignUp) { "Creating account…" } else { "Create account with a passkey" }
            }
            button {
                id: "google-sign-in",
                class: "io-button io-button-secondary",
                disabled,
                "aria-busy": busy == Some(Busy::Google),
                onclick: move |_| start_google(auth, GoogleIntent::SignIn),
                "Continue with Google"
            }
        }
        GoogleWaiting { auth }
    }
}

#[component]
fn SignedIn(auth: Auth, me: Me) -> Element {
    let mut nickname = use_signal(String::new);
    let mut name = use_signal(|| me.display_name.clone().unwrap_or_default());
    let busy = *auth.busy.read();
    let disabled = busy.is_some();
    let last_method = me.sign_in_methods() <= 1;

    let add_passkey = move |_| {
        let label = match normalize_name(&nickname.peek()) {
            Ok(label) => label.unwrap_or_default(),
            Err(problem) => {
                auth.fail(Notice::Error(format!("The passkey name {problem}.")));
                return;
            }
        };
        if !auth.start(Busy::AddPasskey) {
            return;
        }
        spawn(async move {
            let result = async {
                let options = passkey_add_begin().await.map_err(Failure::Server)?;
                let credential = browser::create_passkey(options)
                    .await
                    .map_err(Failure::Browser)?;
                passkey_add_finish(credential, label)
                    .await
                    .map_err(Failure::Server)
            }
            .await;
            if result.is_ok() {
                nickname.set(String::new());
            }
            auth.finish(result, Some("Passkey added."));
        });
    };

    let unlink_google = move |_| {
        if auth.busy.peek().is_some()
            || !browser::confirm(
                "Unlink your Google account? You will no longer be able to sign in with it.",
            )
        {
            return;
        }
        if !auth.start(Busy::UnlinkGoogle) {
            return;
        }
        spawn(async move {
            let result = google_unlink().await.map_err(Failure::Server);
            auth.finish(result, Some("Google account unlinked."));
        });
    };

    let outbox = use_outbox();
    let user_id = me.user_id;
    let do_sign_out = move |_| {
        if !auth.start(Busy::SignOut) {
            return;
        }
        spawn(async move {
            let mut auth = auth;
            match sign_out().await {
                Ok(()) => {
                    outbox.signed_out(user_id);
                    auth.state.set(AccountState::SignedOut);
                    auth.notice
                        .set(Some(Notice::Info("You are signed out.".to_owned())));
                    auth.busy.set(None);
                }
                Err(error) if is_signed_out_error(&error) => {
                    outbox.signed_out(user_id);
                    auth.state.set(AccountState::SignedOut);
                    auth.busy.set(None);
                }
                Err(error) => auth.fail(Notice::Error(server_message(&error))),
            }
        });
    };

    let rename = move |_| {
        let display_name = match new_name(&name.peek()) {
            Ok(display_name) => display_name,
            Err(problem) => {
                auth.fail(Notice::Error(problem));
                return;
            }
        };
        if !auth.start(Busy::Rename) {
            return;
        }
        spawn(async move {
            let result = rename_account(display_name).await.map_err(Failure::Server);
            if let Ok(me) = &result {
                name.set(me.display_name.clone().unwrap_or_default());
            }
            auth.finish(result, Some("Name saved."));
        });
    };

    let do_sign_out_everywhere = move |_| {
        if auth.busy.peek().is_some()
            || !browser::confirm(
                "Sign out on every device, this one included? You will need to sign in again \
                 everywhere.",
            )
        {
            return;
        }
        if !auth.start(Busy::SignOutEverywhere) {
            return;
        }
        spawn(async move {
            let mut auth = auth;
            match sign_out_everywhere().await {
                Ok(()) => {
                    outbox.signed_out(user_id);
                    auth.state.set(AccountState::SignedOut);
                    auth.notice.set(Some(Notice::Info(
                        "You are signed out on every device.".to_owned(),
                    )));
                    auth.busy.set(None);
                }
                Err(error) if is_signed_out_error(&error) => {
                    outbox.signed_out(user_id);
                    auth.state.set(AccountState::SignedOut);
                    auth.busy.set(None);
                }
                Err(error) => auth.fail(Notice::Error(server_message(&error))),
            }
        });
    };

    let greeting = match &me.display_name {
        Some(name) => format!("Hi, {name}"),
        None => "Signed in".to_owned(),
    };
    let last_method_hint = "This is your only way to sign in. Add another passkey or link Google \
                            before removing it.";

    rsx! {
        div { class: "io-account-header",
            h2 { id: "account-title", "{greeting}" }
            button {
                id: "sign-out",
                class: "io-button io-button-secondary",
                disabled,
                "aria-busy": busy == Some(Busy::SignOut),
                onclick: do_sign_out,
                if busy == Some(Busy::SignOut) { "Signing out…" } else { "Sign out" }
            }
        }

        h3 { "Your name" }
        div { class: "io-field",
            label { r#for: "account-name", class: "io-sr-only", "Your name" }
            div { class: "io-inline",
                input {
                    id: "account-name",
                    class: "io-input",
                    r#type: "text",
                    autocomplete: "name",
                    maxlength: MAX_NAME_CHARS,
                    value: "{name}",
                    disabled,
                    oninput: move |event| name.set(event.value()),
                }
                button {
                    id: "account-rename",
                    class: "io-button io-button-secondary",
                    disabled,
                    "aria-busy": busy == Some(Busy::Rename),
                    onclick: rename,
                    if busy == Some(Busy::Rename) { "Saving…" } else { "Save" }
                }
            }
        }

        h3 { "Passkeys" }
        if me.passkeys.is_empty() {
            p { class: "io-muted", "No passkeys yet." }
        }
        ul { class: "io-list",
            for passkey in me.passkeys.iter().cloned() {
                PasskeyRow { key: "{passkey.id}", auth, passkey, last_method }
            }
        }
        if last_method {
            p { id: "last-method-hint", class: "io-muted io-hint", "{last_method_hint}" }
        }
        div { class: "io-field",
            label { r#for: "passkey-nickname", "Name for the new passkey " span { class: "io-muted", "(optional)" } }
            input {
                id: "passkey-nickname",
                class: "io-input",
                r#type: "text",
                maxlength: MAX_NAME_CHARS,
                placeholder: "e.g. Work laptop",
                value: "{nickname}",
                disabled,
                oninput: move |event| nickname.set(event.value()),
            }
        }
        button {
            id: "passkey-add",
            class: "io-button io-button-primary",
            disabled,
            "aria-busy": busy == Some(Busy::AddPasskey),
            onclick: add_passkey,
            if busy == Some(Busy::AddPasskey) { "Adding passkey…" } else { "Add a passkey" }
        }

        h3 { "Google" }
        if me.google_linked {
            div { class: "io-row",
                div { class: "io-row-main",
                    span { class: "io-row-title", "Google linked" }
                }
                button {
                    id: "google-unlink",
                    class: "io-button io-button-danger",
                    disabled: disabled || last_method,
                    "aria-describedby": if last_method { "last-method-hint" },
                    "aria-busy": busy == Some(Busy::UnlinkGoogle),
                    onclick: unlink_google,
                    "Unlink"
                }
            }
        } else {
            button {
                id: "google-link",
                class: "io-button io-button-secondary",
                disabled,
                "aria-busy": busy == Some(Busy::Google),
                onclick: move |_| start_google(auth, GoogleIntent::Link),
                "Link Google"
            }
            GoogleWaiting { auth }
        }

        h3 { "Devices" }
        p { class: "io-muted io-hint",
            "Lost a phone, or signed in on a shared computer? Sign out everywhere at once."
        }
        button {
            id: "sign-out-everywhere",
            class: "io-button io-button-danger",
            disabled,
            "aria-busy": busy == Some(Busy::SignOutEverywhere),
            onclick: do_sign_out_everywhere,
            if busy == Some(Busy::SignOutEverywhere) { "Signing out…" } else { "Sign out on every device" }
        }
    }
}

#[component]
fn PasskeyRow(auth: Auth, passkey: PasskeyInfo, last_method: bool) -> Element {
    let busy = *auth.busy.read();
    let id = passkey.id;
    let nickname = passkey.nickname.clone();
    let remove = move |_| {
        if auth.busy.peek().is_some()
            || !browser::confirm(&format!(
                "Remove the passkey \u{201c}{nickname}\u{201d}? You will no longer be able to \
                 sign in with it."
            ))
        {
            return;
        }
        if !auth.start(Busy::RemovePasskey(id)) {
            return;
        }
        spawn(async move {
            let result = passkey_remove(id).await.map_err(Failure::Server);
            auth.finish(result, Some("Passkey removed."));
        });
    };
    let created = short_date(&passkey.created_at).to_owned();
    let last_used = passkey
        .last_used_at
        .as_deref()
        .map_or_else(|| "never".to_owned(), |at| short_date(at).to_owned());

    rsx! {
        li { class: "io-row",
            div { class: "io-row-main",
                span { class: "io-row-title",
                    "{passkey.nickname}"
                    if passkey.backed_up {
                        span { class: "io-badge", title: "Synced by your passkey provider", "synced" }
                    }
                }
                span { class: "io-muted io-row-meta", "Added {created} · Last used {last_used}" }
            }
            button {
                class: "io-button io-button-danger",
                disabled: busy.is_some() || last_method,
                "aria-describedby": if last_method { "last-method-hint" },
                "aria-busy": busy == Some(Busy::RemovePasskey(id)),
                "aria-label": "Remove passkey {passkey.nickname}",
                onclick: remove,
                "Remove"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::types::UserId;
    use dioxus::fullstack::RequestError;

    fn me() -> Me {
        Me {
            user_id: UserId::from_uuid(uuid::Uuid::nil()),
            display_name: Some("Jules".to_owned()),
            passkeys: vec![],
            google_linked: true,
        }
    }

    #[test]
    fn google_is_complete_once_signed_in_or_linked() {
        let mut account = me();
        assert!(google_complete(GoogleIntent::SignIn, &account));
        assert!(google_complete(GoogleIntent::Link, &account));
        account.google_linked = false;
        assert!(google_complete(GoogleIntent::SignIn, &account));
        assert!(!google_complete(GoogleIntent::Link, &account));
    }

    fn server_error(code: u16, message: &str) -> ServerFnError {
        ServerFnError::ServerError {
            message: message.to_owned(),
            code,
            details: None,
        }
    }

    #[test]
    fn signed_out_errors_are_401_in_either_shape() {
        assert!(is_signed_out_error(&server_error(401, "Please sign in.")));
        assert!(is_signed_out_error(&ServerFnError::Request(
            RequestError::Status("Unauthorized".to_owned(), 401)
        )));
        assert!(!is_signed_out_error(&server_error(403, "x")));
        assert!(!is_signed_out_error(&ServerFnError::Request(
            RequestError::Status("x".to_owned(), 500)
        )));
    }

    #[test]
    fn server_error_messages_are_shown_as_they_are() {
        assert_eq!(
            server_message(&server_error(409, "This passkey is already registered.")),
            "This passkey is already registered."
        );
        assert_eq!(
            server_message(&ServerFnError::Request(RequestError::Connect("x".into()))),
            crate::api::error::NETWORK_MESSAGE
        );
        // A 500's text is never shown.
        assert_eq!(
            server_message(&server_error(500, "db.rs:12 panicked")),
            crate::api::error::GENERIC_MESSAGE
        );
    }

    #[test]
    fn cancelled_ceremonies_are_gentle_notes() {
        assert!(matches!(
            failure_notice(&Failure::Browser(BrowserError::Cancelled)),
            Notice::Info(_)
        ));
        assert_eq!(
            failure_notice(&Failure::Browser(BrowserError::AlreadyRegistered)),
            Notice::Error("This passkey is already registered.".to_owned())
        );
        assert_eq!(
            failure_notice(&Failure::Server(server_error(400, "Sign-in failed."))),
            Notice::Error("Sign-in failed.".to_owned())
        );
    }

    #[test]
    fn success_signs_in_with_the_optional_note() {
        assert_eq!(
            outcome(Ok(me()), false, None),
            Outcome {
                state: Some(AccountState::SignedIn(me())),
                notice: None
            }
        );
        assert_eq!(
            outcome(Ok(me()), true, Some("Passkey added.")).notice,
            Some(Notice::Info("Passkey added.".to_owned()))
        );
    }

    #[test]
    fn a_401_while_signed_in_signs_out() {
        let result = outcome(
            Err(Failure::Server(server_error(401, "Please sign in."))),
            true,
            None,
        );
        assert_eq!(result.state, Some(AccountState::SignedOut));
        assert!(matches!(result.notice, Some(Notice::Info(_))));
    }

    #[test]
    fn a_401_while_signed_out_is_just_an_error() {
        let result = outcome(
            Err(Failure::Server(server_error(401, "Please sign in."))),
            false,
            None,
        );
        assert_eq!(result.state, None);
        assert_eq!(
            result.notice,
            Some(Notice::Error("Please sign in.".to_owned()))
        );
    }

    #[test]
    fn other_failures_keep_the_state() {
        let result = outcome(
            Err(Failure::Server(server_error(
                409,
                "This is your last way to sign in.",
            ))),
            true,
            Some("Passkey removed."),
        );
        assert_eq!(result.state, None);
        assert_eq!(
            result.notice,
            Some(Notice::Error(
                "This is your last way to sign in.".to_owned()
            ))
        );
    }

    #[test]
    fn loading_maps_401_to_signed_out_and_other_errors_to_a_retry() {
        assert_eq!(loaded(Ok(me())), AccountState::SignedIn(me()));
        assert_eq!(
            loaded(Err(server_error(401, "Please sign in."))),
            AccountState::SignedOut
        );
        assert_eq!(
            loaded(Err(ServerFnError::Request(RequestError::Status(
                "Unauthorized".to_owned(),
                401
            )))),
            AccountState::SignedOut
        );
        assert_eq!(
            loaded(Err(server_error(
                503,
                "The server is busy. Please try again."
            ))),
            AccountState::LoadFailed("The server is busy. Please try again.".to_owned())
        );
    }

    #[test]
    fn short_date_keeps_the_date_of_rfc3339_timestamps() {
        assert_eq!(short_date("2026-09-28T12:34:56.789Z"), "2026-09-28");
        assert_eq!(short_date("2026-09-28"), "2026-09-28");
        assert_eq!(short_date("yesterday"), "yesterday");
        assert_eq!(short_date("2026/09/28T00:00:00Z"), "2026/09/28T00:00:00Z");
        assert_eq!(short_date("é2026-09-28"), "é2026-09-28");
    }

    #[test]
    fn a_new_name_is_trimmed_and_must_not_be_blank() {
        assert_eq!(new_name("  Jules "), Ok("Jules".to_owned()));
        assert_eq!(
            new_name("   "),
            Err("Your name must not be blank.".to_owned())
        );
        assert!(new_name("a\u{7}b").unwrap_err().contains("control"));
        assert!(new_name(&"x".repeat(MAX_NAME_CHARS + 1)).is_err());
    }
}
