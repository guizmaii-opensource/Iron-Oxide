//! The outbox in the app: the [`Outbox`] handle screens enqueue writes with, and the task that
//! delivers them (see the module docs of `crate::offline`).

use std::time::Duration;

use dioxus::prelude::*;

use super::platform::{self, BrowserEvent};
use super::queue::{Failure, OutboxStatus, Wake, Write, WriteKey};
use super::session::LocalSession;
use super::storage::QueueStore;
use crate::api::error::{ApiFailure, FailureKind};
use crate::api::sessions::{finish_session, save_set, start_session};
use crate::auth::api::me;
use crate::auth::types::{ACCOUNT_CHANGED_MESSAGE, EXPECTED_USER_HEADER, UserId};
use crate::rate_limit::{self, DEFAULT_RETRY_AFTER};

use super::backoff::Backoff;

/// Where the last signed-in user is remembered, so the outbox shows (and, once the server
/// confirms the account, sends) its writes even when the app starts offline.
const LAST_USER_KEY: &str = "iron-oxide:last-user";

/// How long to wait before checking again when another tab holds the drain lock.
const LOCKED_RECHECK: Duration = Duration::from_secs(3);

/// Shown when the browser's session belongs to another account than the queued writes (the
/// same text as the server's expected-user refusal).
pub const OTHER_ACCOUNT_MESSAGE: &str = ACCOUNT_CHANGED_MESSAGE;

/// The enqueue was refused: nobody is signed in on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotSignedIn;

/// What the drain task is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    /// Send what is ready (a write was enqueued, a timer fired).
    Drain,
    /// Skip the backoff, then send: online again, app start, sign-in, the user asked.
    Nudge,
    /// Another tab changed the queue: show it, and send what is ready.
    Refresh,
    /// A backoff timer fired; ignored unless it is the latest one.
    Timer(u64),
}

/// The app's outbox: queue a write, read what is unsaved. `Copy`; get it with [`use_outbox`].
#[derive(Clone, Copy, PartialEq)]
pub struct Outbox {
    status: Signal<OutboxStatus>,
    user: Signal<Option<UserId>>,
    store: CopyValue<Option<QueueStore>>,
    /// The latest backoff timer; older ones do nothing when they fire.
    timer: CopyValue<u64>,
    commands: Coroutine<Command>,
}

/// The outbox provided by [`use_outbox_provider`] higher in the tree.
#[must_use]
pub fn use_outbox() -> Outbox {
    use_context()
}

/// Creates the app's outbox and provides it to the components below. Call once, in the root.
pub fn use_outbox_provider() -> Outbox {
    let status = use_signal(OutboxStatus::default);
    let user = use_signal(|| None);
    let store = use_hook(|| CopyValue::new(None));
    let timer = use_hook(|| CopyValue::new(0));
    let mut handle: CopyValue<Option<Outbox>> = use_hook(|| CopyValue::new(None));
    let commands = use_coroutine(move |mut commands: UnboundedReceiver<Command>| async move {
        while let Ok(command) = commands.recv().await {
            let Some(outbox) = *handle.peek() else {
                continue;
            };
            match command {
                Command::Nudge => {
                    outbox.update(|queue| queue.nudge());
                }
                Command::Timer(id) if id != *outbox.timer.peek() => continue,
                Command::Drain | Command::Refresh | Command::Timer(_) => {}
            }
            // One command at a time: this task is the only sender in this tab.
            outbox.drain().await;
        }
    });
    let outbox = Outbox {
        status,
        user,
        store,
        timer,
        commands,
    };
    handle.set(Some(outbox));
    use_context_provider(|| outbox);

    // Client only, after the first render (hydration): the remembered user, the browser's
    // events, and a first attempt (app start).
    let mut listeners: CopyValue<Option<platform::Listeners>> = use_hook(|| CopyValue::new(None));
    use_effect(move || {
        if !cfg!(feature = "web") || listeners.peek().is_some() {
            return;
        }
        listeners.set(Some(platform::Listeners::install(
            move |event| match event {
                BrowserEvent::Online => commands.send(Command::Nudge),
                BrowserEvent::StorageChanged(key) => {
                    let ours = outbox
                        .store
                        .peek()
                        .as_ref()
                        .map(|store| store.key().to_owned());
                    if key.is_none() || key == ours {
                        commands.send(Command::Refresh);
                    }
                }
            },
        )));
        let remembered = platform::with_storage(|storage| {
            storage.get(LAST_USER_KEY).ok().flatten()?.parse().ok()
        });
        if let Some(user) = remembered {
            outbox.switch_user(Some(UserId::from_uuid(user)));
        }
    });
    outbox
}

impl Outbox {
    /// Queues `write` (persisted before this returns) and starts delivering it. Enqueueing the
    /// same write again does nothing. Returns its key, to follow it with [`Outbox::is_pending`].
    ///
    /// # Errors
    /// [`NotSignedIn`] when no user is signed in on this device.
    pub fn enqueue(&self, write: Write) -> Result<WriteKey, NotSignedIn> {
        if self.store.peek().is_none() {
            return Err(NotSignedIn);
        }
        let key = write.key();
        let now = platform::now();
        self.update(|queue| queue.enqueue(write, now));
        self.commands.send(Command::Drain);
        Ok(key)
    }

    /// What is unsaved (reactive).
    #[must_use]
    pub fn status(&self) -> OutboxStatus {
        self.status.read().clone()
    }

    /// How many writes are not delivered yet (reactive).
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.status.read().pending_count
    }

    /// Why writes are not delivered, when something went wrong (reactive).
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.status.read().last_error.clone()
    }

    /// Whether the write with this key is still queued (reactive).
    #[must_use]
    pub fn is_pending(&self, key: WriteKey) -> bool {
        if self.status.read().pending_count == 0 {
            return false;
        }
        let mut store = self.store;
        let mut store = store.write();
        store.as_mut().is_some_and(|store| {
            platform::with_storage(|storage| store.load(storage).contains(key))
        })
    }

    /// The queued writes, oldest first, each with the server's refusal when it was rejected
    /// (reactive). Reads the stored queue once, for screens that show several writes' state.
    #[must_use]
    pub fn queued(&self) -> Vec<(WriteKey, Option<String>)> {
        if self.status.read().pending_count == 0 {
            return Vec::new();
        }
        let mut store = self.store;
        let mut store = store.write();
        store.as_mut().map_or_else(Vec::new, |store| {
            platform::with_storage(|storage| {
                store
                    .load(storage)
                    .entries()
                    .map(|entry| (entry.write.key(), entry.failed.clone()))
                    .collect()
            })
        })
    }

    /// The user whose writes this outbox holds (reactive): `None` while signed out, and until
    /// the app has read the remembered user after its first render.
    #[must_use]
    pub fn user(&self) -> Option<UserId> {
        *self.user.read()
    }

    /// Tries again now, skipping the backoff (never a `429`'s delay).
    pub fn retry_now(&self) {
        self.commands.send(Command::Nudge);
    }

    /// Sends the writes the server rejected again (after the user fixed the cause).
    pub fn retry_failed(&self) {
        self.update(|queue| queue.retry_failed());
        self.commands.send(Command::Drain);
    }

    /// How many writes [`Outbox::discard_failed`] would give up: the refused ones and those
    /// that cannot succeed without them (a refused start takes its session's sets and finish).
    /// For the confirmation.
    #[must_use]
    pub fn discard_count(&self) -> usize {
        let mut store = self.store;
        let mut store = store.write();
        store.as_mut().map_or(0, |store| {
            platform::with_storage(|storage| store.load(storage).discard_plan().len())
        })
    }

    /// Gives up the writes the server refused and the writes that depend on them (the user
    /// chose to, in one action) and returns them.
    pub fn discard_failed(&self) -> Vec<Write> {
        let discarded = self
            .update(|queue| queue.discard_failed())
            .unwrap_or_default();
        self.commands.send(Command::Drain);
        discarded
    }

    /// The account panel reports who is signed in. Switching user shows (and sends) that user's
    /// writes; the previous user's stay on the device for their next sign-in.
    pub fn signed_in(&self, user: UserId) {
        platform::with_storage(|storage| {
            let _ = storage.set(LAST_USER_KEY, &user.as_uuid().to_string());
        });
        self.switch_user(Some(user));
    }

    /// The user signed out on purpose: forgets their session in progress on this device. Their
    /// undelivered writes are kept (per user) and sent at their next sign-in.
    pub fn signed_out(&self, user: UserId) {
        platform::with_storage(|storage| {
            let _ = LocalSession::clear(storage, user);
            let _ = storage.remove(LAST_USER_KEY);
        });
        self.switch_user(None);
    }

    fn switch_user(&self, user: Option<UserId>) {
        let mut this = *self;
        if *this.user.peek() == user && this.store.peek().is_some() == user.is_some() {
            this.commands.send(Command::Nudge);
            return;
        }
        this.user.set(user);
        this.store.set(user.map(QueueStore::new));
        *this.timer.write() += 1;
        this.refresh();
        if user.is_some() {
            this.commands.send(Command::Nudge);
        }
    }

    /// Applies `change` to the stored queue (re-read first) and publishes the new status.
    /// `None` when nobody is signed in.
    fn update<R>(&self, change: impl FnOnce(&mut super::queue::Queue) -> R) -> Option<R> {
        let mut store = self.store;
        let result = {
            let mut store = store.write();
            let store = store.as_mut()?;
            platform::with_storage(|storage| store.update(storage, change))
        };
        self.publish();
        Some(result)
    }

    /// Re-reads the stored queue and publishes its status.
    fn refresh(&self) {
        let mut store = self.store;
        if let Some(store) = store.write().as_mut() {
            platform::with_storage(|storage| {
                store.load(storage);
            });
        }
        self.publish();
    }

    fn publish(&self) {
        let status = self
            .store
            .peek()
            .as_ref()
            .map(QueueStore::status)
            .unwrap_or_default();
        let mut signal = self.status;
        if *signal.peek() != status {
            signal.set(status);
        }
    }

    fn wake(&self) -> Wake {
        let now = platform::now();
        self.store
            .peek()
            .as_ref()
            .map_or(Wake::Idle, |store| store.queue().wake(now))
    }

    /// Sends the ready writes one by one, in order, while this tab holds the drain lock.
    async fn drain(&self) {
        self.refresh();
        let Some(user) = *self.user.peek() else {
            return;
        };
        if self.wake() != Wake::Now {
            self.schedule();
            return;
        }
        let Some(_lock) = platform::try_lock(&format!("iron-oxide-outbox:{user}")).await else {
            // Another tab is sending; look again shortly in case it stops before our writes.
            self.schedule_in(LOCKED_RECHECK);
            return;
        };
        loop {
            if *self.user.peek() != Some(user) {
                return;
            }
            let now = platform::now();
            let Some(write) = self
                .update(|queue| queue.next_ready(now).cloned())
                .flatten()
            else {
                break;
            };
            // The server takes the user from the cookie, which another tab may have changed:
            // before every send, make sure it is still this queue's user. A mismatch stops the
            // drain; a failed check is a retry later, never a refusal of the write.
            let checked = me().await.map(|me| me.user_id);
            if let Err(failure) = account_check(user, &checked) {
                self.fail_head(failure);
                self.schedule();
                return;
            }
            if *self.user.peek() != Some(user) {
                return;
            }
            let result = send(user, &write).await;
            let now = platform::now();
            if *self.user.peek() != Some(user) {
                return;
            }
            match result {
                Ok(()) => {
                    self.update(|queue| queue.on_success(&write));
                }
                Err(error) => {
                    let failure = failure_of(&error);
                    let random = platform::random();
                    self.update(|queue| {
                        queue.on_failure(&write, failure, now, random, &Backoff::DEFAULT);
                    });
                }
            }
        }
        self.schedule();
    }

    /// Records `failure` against the head write (a failed account check).
    fn fail_head(&self, failure: Failure) {
        let now = platform::now();
        let random = platform::random();
        self.update(|queue| {
            if let Some(head) = queue.next_ready(now).cloned() {
                queue.on_failure(&head, failure, now, random, &Backoff::DEFAULT);
            }
        });
    }

    /// Arms the timer for the queue's next attempt, if it waits for one.
    fn schedule(&self) {
        match self.wake() {
            Wake::At(at) => self.schedule_in(platform::until(at, platform::now())),
            Wake::Now => self.schedule_in(Duration::ZERO),
            Wake::Idle => {}
        }
    }

    fn schedule_in(&self, delay: Duration) {
        let mut timer = self.timer;
        *timer.write() += 1;
        let id = *timer.peek();
        let commands = self.commands;
        spawn(async move {
            platform::sleep(delay).await;
            commands.send(Command::Timer(id));
        });
    }
}

/// Calls the server function behind `write`, with its exact arguments, asserting with
/// [`EXPECTED_USER_HEADER`] that it is for `user`: the server refuses it (`409`
/// [`ACCOUNT_CHANGED_MESSAGE`]) if the browser's session is another account's by then.
async fn send(user: UserId, write: &Write) -> Result<(), ServerFnError> {
    ExpectingUser::new(user, call(write.clone())).await
}

async fn call(write: Write) -> Result<(), ServerFnError> {
    match write {
        Write::StartSession {
            session_id,
            started_at,
            choice,
        } => start_session(session_id, started_at, choice)
            .await
            .map(drop),
        Write::SaveSet { session_id, set } => save_set(session_id, set).await,
        Write::FinishSession {
            session_id,
            outcome,
            finished_at,
        } => finish_session(session_id, outcome, finished_at)
            .await
            .map(drop),
    }
}

/// Runs a server-function call with [`EXPECTED_USER_HEADER`] among the client's request
/// headers, during its own polls only. The client reads those headers (Dioxus's
/// `get_request_headers`) when it builds the request, inside the first poll; the browser runs
/// one task at a time, so no other call ever sees the header.
struct ExpectingUser<F> {
    call: std::pin::Pin<Box<F>>,
    user: String,
}

impl<F> ExpectingUser<F> {
    fn new(user: UserId, call: F) -> Self {
        Self {
            call: Box::pin(call),
            user: user.as_uuid().to_string(),
        }
    }
}

impl<F: std::future::Future> std::future::Future for ExpectingUser<F> {
    type Output = F::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use dioxus::fullstack::{HeaderValue, get_request_headers, set_request_headers};
        let this = self.get_mut();
        let saved = get_request_headers();
        let mut headers = saved.clone();
        if let Ok(value) = HeaderValue::from_str(&this.user) {
            headers.insert(EXPECTED_USER_HEADER, value);
        }
        set_request_headers(headers);
        let polled = this.call.as_mut().poll(cx);
        set_request_headers(saved);
        polled
    }
}

/// What a failed call means for the queue (`docs/api.md`, "On the client"): network, `429`,
/// `502`-`504` retry (a `429` after its `Retry-After`, 30 s when it is missing); `401` waits for
/// sign-in; anything else, `500` included, is a rejection the user must see.
#[must_use]
pub fn failure_of(error: &ServerFnError) -> Failure {
    let ApiFailure { kind, message, .. } = ApiFailure::classify(error);
    if kind == FailureKind::Conflict && message == ACCOUNT_CHANGED_MESSAGE {
        // The server's expected-user check: another account is signed in. Not a refusal of
        // the write; it waits for its user to sign in again.
        return Failure::SignedOut { message };
    }
    if status_of(error) == Some(408) && !kind.is_retryable() {
        // A request timeout (a proxy, or the server's own slow-body limit): retry, whatever
        // `ApiFailure` calls it.
        return Failure::Retry {
            message: crate::api::error::TRANSIENT_MESSAGE.to_owned(),
            retry_after: None,
        };
    }
    match kind {
        FailureKind::Unauthorized => Failure::SignedOut { message },
        FailureKind::RateLimited => Failure::Retry {
            message,
            retry_after: Some(rate_limit::retry_after(error).unwrap_or(DEFAULT_RETRY_AFTER)),
        },
        kind if kind.is_retryable() => Failure::Retry {
            message,
            retry_after: None,
        },
        _ => Failure::Rejected { message },
    }
}

/// The HTTP status of a failed call, if it got one.
fn status_of(error: &ServerFnError) -> Option<u16> {
    match error {
        ServerFnError::ServerError { code, .. }
        | ServerFnError::Request(dioxus::fullstack::RequestError::Status(_, code)) => Some(*code),
        _ => None,
    }
}

/// Whether the browser's session (`me()`, done before every send) is still `user`'s. Another
/// account pauses the queue until `user` signs in again; a failed check is
/// [`account_check_failure`].
///
/// # Errors
/// The failure to record against the head write; the drain stops.
pub fn account_check(user: UserId, checked: &Result<UserId, ServerFnError>) -> Result<(), Failure> {
    match checked {
        Ok(signed_in) if *signed_in == user => Ok(()),
        Ok(_) => Err(Failure::SignedOut {
            message: OTHER_ACCOUNT_MESSAGE.to_owned(),
        }),
        Err(error) => Err(account_check_failure(error)),
    }
}

/// What a failed `me()` check before a send means: `401` waits for sign-in; anything else is a
/// retry later (`429` after its delay). The write was not sent, so it is never refused.
#[must_use]
pub fn account_check_failure(error: &ServerFnError) -> Failure {
    match failure_of(error) {
        Failure::Rejected { message } => Failure::Retry {
            message,
            retry_after: None,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus::fullstack::RequestError;

    fn server(code: u16, details: Option<serde_json::Value>) -> ServerFnError {
        ServerFnError::ServerError {
            message: format!("message {code}"),
            code,
            details,
        }
    }

    #[test]
    fn conflicts_and_invalid_writes_stop_the_queue() {
        for code in [400, 403, 404, 409, 413, 422, 500] {
            assert_eq!(
                failure_of(&server(code, None)),
                Failure::Rejected {
                    message: ApiFailure::classify(&server(code, None)).message
                },
                "{code}"
            );
        }
    }

    #[test]
    fn transient_and_network_failures_retry() {
        for error in [
            server(502, None),
            server(503, None),
            server(504, None),
            ServerFnError::Request(RequestError::Connect("refused".to_owned())),
            ServerFnError::Request(RequestError::Timeout("t".to_owned())),
        ] {
            assert!(
                matches!(
                    failure_of(&error),
                    Failure::Retry {
                        retry_after: None,
                        ..
                    }
                ),
                "{error:?}"
            );
        }
    }

    #[test]
    fn rate_limits_retry_after_their_delay() {
        let limited = server(429, Some(serde_json::json!({ "retry_after_secs": 12 })));
        assert_eq!(
            failure_of(&limited),
            Failure::Retry {
                message: "message 429".to_owned(),
                retry_after: Some(Duration::from_secs(12)),
            }
        );
        // Without a body (a bare status), the default delay.
        let bare = ServerFnError::Request(RequestError::Status("x".to_owned(), 429));
        assert!(matches!(
            failure_of(&bare),
            Failure::Retry { retry_after: Some(delay), .. } if delay == DEFAULT_RETRY_AFTER
        ));
    }

    #[test]
    fn signed_out_pauses() {
        assert_eq!(
            failure_of(&server(401, None)),
            Failure::SignedOut {
                message: "message 401".to_owned()
            }
        );
    }

    #[test]
    fn a_request_timeout_retries_in_either_shape() {
        let decoded = ServerFnError::ServerError {
            message: "HTTP 408: Request Timeout".to_owned(),
            code: 408,
            details: None,
        };
        let ours = server(408, None);
        let bare = ServerFnError::Request(RequestError::Status("x".to_owned(), 408));
        for error in [decoded, ours, bare] {
            assert!(
                matches!(
                    failure_of(&error),
                    Failure::Retry {
                        retry_after: None,
                        ..
                    }
                ),
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_failed_account_check_is_retried_never_refused() {
        for code in [404, 409, 422, 500, 503] {
            assert!(
                matches!(
                    account_check_failure(&server(code, None)),
                    Failure::Retry { .. }
                ),
                "{code}"
            );
        }
        assert!(matches!(
            account_check_failure(&server(401, None)),
            Failure::SignedOut { .. }
        ));
        assert!(matches!(
            account_check_failure(&server(429, Some(serde_json::json!({ "retry_after_secs": 9 })))),
            Failure::Retry { retry_after: Some(delay), .. } if delay == Duration::from_secs(9)
        ));
    }

    #[test]
    fn another_account_stops_the_drain_and_pauses_the_queue() {
        let a = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let b = UserId::from_uuid(uuid::Uuid::from_u128(2));
        assert_eq!(account_check(a, &Ok(a)), Ok(()));
        assert_eq!(
            account_check(a, &Ok(b)),
            Err(Failure::SignedOut {
                message: OTHER_ACCOUNT_MESSAGE.to_owned()
            })
        );
        assert!(matches!(
            account_check(a, &Err(server(500, None))),
            Err(Failure::Retry { .. })
        ));
    }

    #[test]
    fn the_servers_account_changed_refusal_pauses_instead_of_refusing() {
        let error = ServerFnError::ServerError {
            message: ACCOUNT_CHANGED_MESSAGE.to_owned(),
            code: 409,
            details: None,
        };
        assert_eq!(
            failure_of(&error),
            Failure::SignedOut {
                message: ACCOUNT_CHANGED_MESSAGE.to_owned()
            }
        );
        // Any other 409 is still a refusal.
        assert!(matches!(
            failure_of(&server(409, None)),
            Failure::Rejected { .. }
        ));
    }

    #[test]
    fn calls_carry_the_expected_user_only_while_they_are_polled() {
        use dioxus::fullstack::{HeaderMap, get_request_headers, set_request_headers};
        set_request_headers(HeaderMap::new());
        let user = UserId::from_uuid(uuid::Uuid::from_u128(7));
        let seen = ExpectingUser::new(user, async {
            get_request_headers()
                .get(EXPECTED_USER_HEADER)
                .and_then(|value| value.to_str().ok().map(str::to_owned))
        });
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut seen = std::pin::pin!(seen);
        assert_eq!(
            seen.as_mut().poll(&mut cx),
            std::task::Poll::Ready(Some(user.as_uuid().to_string()))
        );
        assert!(get_request_headers().get(EXPECTED_USER_HEADER).is_none());
    }
}
