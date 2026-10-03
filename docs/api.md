# Server functions (API conventions)

Every server function follows the same rules (#68), so the client, the retry queue (#30) and the
tests can treat them all alike.

## Layout

| Where | What |
|---|---|
| `crates/iron-oxide-app/src/api.rs` | One `pub mod <area>;` line per area |
| `crates/iron-oxide-app/src/api/<area>.rs` | The area's `#[post]` server functions and the types they exchange (DTOs) |
| `crates/iron-oxide-app/src/api/error.rs` | Client side: `ApiFailure::classify` for the UI and the retry queue |
| `crates/iron-oxide-app/src/server/api/<area>.rs` | Server-only logic behind the area's functions, and its tests |
| `crates/iron-oxide-app/src/server/api/error.rs` | `ApiError` and its conversions |
| `crates/iron-oxide-app/src/server/api/errors_layer.rs` | The layer that gives every `/api/` error the same body |
| `crates/iron-oxide-app/src/server/api/testing.rs` | The endpoint test harness |

Sign-in (`src/auth/api.rs`, #5) predates this layout and keeps its own `AuthError`, with the same
mapping style.

A server function only extracts what it needs and calls the area's logic:

```rust
// src/api/sessions.rs
#[post("/api/sessions/get", state: Extension<AppState>, user: AuthUser)]
pub async fn get_session(session_id: SessionId) -> Result<SessionView, ServerFnError> {
    Ok(sessions::get(&state.db, user.owner(), session_id).await?)
}
```

- Every function is a `POST` under `/api/<area>/<name>`. The CSRF layer checks every `POST`,
  and nothing is cached. The only exception is `server_time` (`GET /api/server-time`), which
  predates the conventions and reads nothing of the user's.
- The server-only arguments go after the route: `state: Extension<AppState>` for the pool and
  `user: AuthUser` for the signed-in user. Without a valid session, `AuthUser` rejects the call with
  `401` before the body runs.
- **The user always comes from the session.** `user.owner()` is the repository's owner key. Never
  accept a user id from the client. Every repository call takes it and scopes every query by it
  (see `docs/database.md`).
- **Expected user (#30).** A request may carry `X-Io-Expected-User: <user id>`. The retry queue
  sends it with every write. `AuthUser` then refuses the request with `409` "Signed in with
  another account. …" (`auth::types::ACCOUNT_CHANGED_MESSAGE`) unless it names the session's
  user, before the body runs. Without the header nothing changes. The header never selects a
  user; it can only refuse.
- Arguments and results use the domain types: the typed ids (`SessionId`, …), `DayId`, `Weight`,
  and so on. Times are `iron_oxide_domain::time::Timestamp`, milliseconds since the Unix epoch in
  UTC, serialized as a JSON integer. `server::api::timestamp` and `server::api::offset_date_time`
  convert to and from the database's `timestamptz`.
- The logic returns `Result<_, ApiError>`, and `?` turns it into a `ServerFnError`.

## Errors

`ApiError` (server-only) becomes a `ServerFnError::ServerError { code, message }`. The message is
short, generic and safe to show. Details such as database errors, constraint names and ids are
logged, never returned.

| Variant | Status | Message | When |
|---|---|---|---|
| `NotFound` | 404 | `Not found.` | No such row **among the caller's own**. Another user's id gives exactly the same answer as an id that does not exist. |
| `Conflict(msg)` | 409 | `msg` | An id reused with different content, a session that has already ended |
| `Invalid(msg)` | 422 | `msg` | Invalid input: a domain value, a program document, a database `CHECK` (`Invalid value.`) |
| `InvalidProgramIn(message, problems)` | 422 | `message` | A program document inside a larger upload (an account import): the message names it and its first problem, and the `details` carry all of them as `ProgramProblems` |
| `InvalidField { field, message }` | 422 | `message` | Invalid input in one named field; `{"field": path}` (e.g. `bar_weight`) is sent as the error details, so the UI can point at it |
| `InvalidProgram(problems)` | 422 | `This program is not valid.` | An uploaded program document that does not parse or breaks a rule. `problems` (`ProgramProblems`) is sent as the error details, see [Programs](#programs-srcapiprogramsrs-19). |
| `TooLarge(msg)` | 413 | `msg` | A request body or document past its size limit |
| `Transient(detail)` | 503 | `The server is busy. Please try again.` | The same request can simply be retried: it may have been saved before a dropped connection, but every write is idempotent. Covers a concurrent write, a pool timeout, a dropped connection, a serialization failure or a deadlock. |
| `Unauthorized` | 401 | `Please sign in.` | Not signed in (normally rejected earlier by `AuthUser`) |
| `Forbidden(msg)` | 403 | `msg` | Plan gating (#21) |
| `Busy(secs)` | 503 | `The server is busy. Please try again.` | Every slot for this work is taken (account imports and deletions). `details` carry `retry_after_secs`, and the error layer sends it as `Retry-After` too. |
| `Internal(detail)` | 500 | `Something went wrong. Please try again.` | Bugs, corrupt stored data, any other database failure |

The conversions:

- `From<RepoError>`:
  - `NotFound` → 404
  - `Conflict` and `SessionEnded` → 409
  - `Transient` → 503
  - `Invalid` → 422
  - `Corrupt` → 500
  - `Database` → 503 if transient, 500 otherwise
- `From<AuthError>` keeps sign-in's status. Session-store errors (#74): a `Backend` error the store
  marked retryable (the same `sqlx` errors as `Database`'s transient ones: pool timeout or closed,
  I/O, serialization failure, deadlock, admin shutdown) → 503; any other `Backend` error (a
  permanent database error, no free session id), `Encode` and `Decode` → 500. The store
  (`server::auth::session`) marks them, as it is the last place that sees the typed `sqlx::Error`.
- `From<ValueError>` and `From<ProgramError>` → 422 with the domain's message, which only repeats what
  the user sent.
- `From<SessionError>`: 409 or 422 with a fixed message. The domain's own text names ids, so it is
  never used.

Messages written for users go in the variant. Anything else goes in the log.

`ApiError` is meant to grow: an area that needs another status adds a variant with its `public()`
status and message (and, for structured data such as a list of problems, `details` on the
`ServerFnError`). The client maps statuses, not variants.

### The error body

Every failed `/api/` call that answers JSON has the same body, whatever produced it:

```json
{ "message": "Not found.", "code": 404, "data": { "ServerError": { "message": "Not found.", "code": 404 } } }
```

`data.ServerError` may also have `details`, structured data for the UI (a list of problems, a
429's `retry_after_secs`). The Dioxus client decodes `data` into `ServerFnError::ServerError {
message, code, details }`: **our message and details are that variant's own `message` and
`details`**.

Dioxus produces other shapes on its own. An error returned by a server function has Dioxus's
`Display` text (`error running server function: …`) as the top `message`. An extractor rejection
(`AuthUser`'s 401, the CSRF 403) or arguments that do not decode give `{"error": text}`. A body
with a `data` that is not a `ServerError` (see the 429 below) would not decode on the client.
`server::api::errors_layer` rewrites all of them into the shape above. A **5xx** body that is none
of these shapes (not JSON: Dioxus's plain-text panic answer, which quotes the panic in debug
builds; or JSON of another shape) gets the generic message and no details too, keeping its status
(`503` gets `The server is busy. Please try again.`); its text is logged, never sent. Other 4xx
bodies that are not JSON (axum's own 405 or 415) are left alone; the client classifies them by
status.

**Arguments that do not decode** are `422 Invalid request.` They include a malformed id, a wrong
type or a missing field. Dioxus answers them with a `500` whose text is a serde error.

**Every 5xx** gets a fixed message and no details, whatever the function put in it
(`ServerFnError::new(detail)`, an `anyhow` error, a `{"message", "data"}` body from another
layer): `503` gets `The server is busy. Please try again.`, every other 5xx the generic message.
The only detail kept is a 503's `retry_after_secs` (a whole number from 1 to 3600, as the busy
account import and deletion send), which also becomes the `Retry-After` header. The original text
is logged.

**429 (rate limiting, #72).** The body is

```json
{ "message": "Too many requests. Please wait a moment.", "code": 429,
  "data": { "ServerError": { "message": "Too many requests. Please wait a moment.", "code": 429,
                             "details": { "retry_after_secs": 30 } } } }
```

plus the `Retry-After: 30` header. A limiter may also send `{"message", "code": 429, "data":
{"retry_after_secs": 30}}`: the layer moves that `data` into `details`. On the client,
`ApiFailure::retry_after_secs()` reads it.

### On the client

`crate::api::error::ApiFailure::classify(&ServerFnError)` gives a `FailureKind`, the message to
show and any structured `details`:

- It handles a decoded `ServerError { message, code, details }` and a bare
  `RequestError::Status(_, code)`.
- It shows **our** message, the `ServerError`'s own `message`, for every 4xx and for 503. For 500s
  and unknown statuses, and for answers that are not ours (`message` = `HTTP {code}: {text}`, the
  client's fallback for a body that is not our JSON), it shows a generic message for the kind and
  drops the details.
- 413 counts as `Invalid`; 408 (the body did not arrive in time) counts as `Network`, retryable.
- **Retryable:** `Transient` (503, 502, 504), `RateLimited` (429, honouring `Retry-After`), and
  `Network` (timeouts, connection failures, the request never answered, 408).
- **Not retryable:** 400, 401, 403, 404, 409, 413, 422 and 500. Retrying the same request cannot fix
  them. A 401 means going back to sign-in.
- **The retry queue (#30)** follows this classification (408 included, as `Network`), with one
  exception: the `409` "Signed in with another account. …" answered to a write whose
  `X-Io-Expected-User` header is not the session's user (see [Layout](#layout)) pauses the queue
  until that user signs in again, instead of refusing the write.

### Request limits (#74)

`server::limits` caps every request body; everything else that can block a call is bounded where it
happens:

| Limit | Value | Answer |
|---|---|---|
| Request body, default (`DEFAULT_BODY_LIMIT`) | 64 KiB | `413 This request is too large.` |
| Request body, `/api/programs/upload` (`UPLOAD_BODY_LIMIT`) | 528 KiB (2 × 256 KiB + 16 KiB), see [Programs](#programs-srcapiprogramsrs-19) | `413 The program file is too large (the limit is 256 KiB).` |
| Request body, `/api/account/import` (`IMPORT_BODY_LIMIT`), see [`docs/export-format.md`](export-format.md) | 16 MiB + 64 KiB (document 8 MiB) | `413 The export file is too large (the limit is 8 MiB).` |
| Request body, `POST /webhooks/stripe` (outside Dioxus, `DefaultBodyLimit`) | 256 KiB | axum's `413` |
| Body read (`BODY_READ_TIMEOUT`), from the end of the headers | 10 s; 60 s for `/api/account/import` | `408 The request took too long to arrive. Please try again.` |
| Waiting for a pooled connection (`db::ACQUIRE_TIMEOUT`) | 10 s | `503 The server is busy. Please try again.` |
| One statement, an idle transaction, a whole transaction (`db::STATEMENT_DEADLINE`, set by Postgres on every pooled connection) | 5 s each; 60 s for an account import or deletion (`db::account::long_connection`) | the statement or session is aborted and the transaction rolled back: `503` (`57014`, `25P03`, `25P04` are transient) |
| Each call to Google (OIDC discovery, token exchange; `google::HTTP_TIMEOUT`, `CONNECT_TIMEOUT`) | 10 s in total, 5 s to connect | `503` (`AuthError::GoogleUnavailable`); WebAuthn makes no outbound calls |

- **Why a cap of our own.** Dioxus 0.7.10 reads a server function's body with `unwrap`, so a body
  it cannot read (past axum's 2 MiB default, a dropped connection) panics into a `500`. The cap
  layer reads the whole body first, at most the limit, and hands Dioxus a complete one. It refuses
  a body at once when `Content-Length` announces more, and otherwise as soon as more has arrived:
  a chunked body or a lying `Content-Length` does not get past it.
- **Where.** The cap wraps everything Dioxus serves (server functions and pages; outside `/api/`
  the `413` is plain text), inside the CSRF check, the per-IP rate limit, the session and the
  per-user rate limit, so refused requests never get their body read. It runs before the
  function's extractors: **an oversize body gets `413` whether the client is signed in or not.**
  The upload is the exception: it checks the session first (`401` signed out) and reads its own
  body with the same helper and timeout (`limits::OWN_BODY_LIMIT`).
- **The 64 KiB default.** The largest legitimate bodies, measured by
  `the_default_body_limit_covers_every_server_function`: a passkey registration 863 bytes (from a
  software authenticator sending a `packed` attestation with its certificate; browsers send
  `none`, as the server asks), a settings update with a full plate inventory 751 bytes, a logged
  set 297 bytes. The test fails if one grows past an eighth of the cap. A new function whose
  arguments can be larger gets its own limit like the upload.
- **No timeout on the call itself.** The server answers once the server function has finished
  (or the database aborted its work). An HTTP timeout around the call could not stop it: Dioxus
  0.7.10 runs each server function in a detached task (`spawn_pinned`), so a `503` sent early
  would leave the function running, and an older write (settings, a training max, the active
  program) could commit after the client had moved on and overwrite a newer value. So each wait
  is bounded at its source instead (the table above). Every pooled connection sets
  `statement_timeout`, `idle_in_transaction_session_timeout` and `transaction_timeout`
  (Postgres 17+) to 5 s: Postgres aborts a slow or stuck transaction and rolls it back, and the
  function fails with a `503`. The migrations run on a connection of their own without these
  deadlines (they may wait for another instance's lock).
- **408 is different.** The body is read before Dioxus starts the function, so a `408` means the
  function never ran.
- **Not covered yet.** Nothing bounds how long a client takes to send its *headers*: hyper's
  header-read timeout (30 s by default) needs a timer, which `axum::serve` (0.8) does not set and
  does not let us set. It would take our own hyper-util accept loop.

## Idempotency

Anything the client creates gets its id on the client, a UUIDv7 from the domain's `new_v7()`, so a
retried request is recognised:

- same id, same content: success, and nothing changes (the repository reports `Change::Unchanged`);
- same id, different content: `409`;
- concurrent duplicates: one row, the others get the same success, or a `503` that a retry turns
  into it.

**Every write endpoint, updates included, must succeed unchanged when replayed.** A `503` can
follow a write that was committed (the connection dropped during `COMMIT`), and the retry queue
then sends it again.

Timestamps that are part of what is saved (`started_at`, `completed_at`, `finished_at`) come from
the client, in the request. If the server stamped `now()`, a retry would carry a different time
and look like a conflict. The retry queue (#30) re-sends exactly the same body.

## Endpoints

### Sessions and sets (#18, `src/api/sessions.rs`)

| Function | Route | What it does |
|---|---|---|
| `start_session(session_id, started_at)` | `/api/sessions/start` | Starts a session of the active program's latest version, on the next day of its rotation. Returns a `SessionView`. |
| `get_session(session_id)` | `/api/sessions/get` | One session (`SessionView`) |
| `get_in_progress_session()` | `/api/sessions/in-progress` | The most recently started session in progress, with its sets in the order they were completed, or `null` |
| `get_next_session_plan()` | `/api/sessions/next-plan` | Today's plan before starting: the next day of the active program (latest version) and its targets from every completed session. `409` with no active program. |
| `get_session_plan(session_id)` | `/api/sessions/plan` | The session's day (name, exercises in program order). For each exercise: its definition in the session's version, and the progression engine's `NextTargets`, computed from the history before the session. |
| `save_set(session_id, set)` | `/api/sessions/save-set` | Logs a `LoggedSet<Timestamp>` |
| `finish_session(session_id, outcome, finished_at)` | `/api/sessions/finish` | Ends the session (`completed`, `skipped` or `abandoned`) and returns a `SessionSummary` |

Rules:

- **Starting.** The day is `next_day(rotation, history)`, where the history is every session of
  the active program, across all its versions.
  - A retry (same id, same `started_at`) returns the session it created, even after it ended. The
    day is never recomputed.
  - The same id with another `started_at` is `409`.
  - `409` when another session is in progress: the user must finish or abandon it first. The
    database enforces it too, with a partial unique index (`workout_sessions_one_in_progress_idx`):
    of two devices starting different sessions at the same instant, exactly one succeeds and the
    other gets the same `409`.
  - `409` with no active program.
  - `409` for a rotation that repeats a day. Programs allow it, but `next_day` does not support it
    yet.
- **Sets.** The domain's `SessionLog::add_set` decides first; then the repository's upsert settles
  races.
  - The same set again is `200`, even after the session ended.
  - The same id with other values is `409`, also when that id was logged in another session.
  - A new set in an ended session is `409`.
  - A set completed before the session started is `422`.
  - Values the database refuses (a weight above the limit) are `422`.
  - `set_index` numbers the sets of one exercise and kind (warm-up or working) from 0. The
    prescribed working sets are `0..n`. Extras (a top single, back-off sets) are `n` and up. A
    skipped working set leaves a gap.
  - The exercise does not have to be on the session's day, so an added exercise is fine. Only the
    day's exercises get targets and progression.
  - **`target` (#60, optional).** The `SetTarget` the session screen showed for the set (its
    prefill: weight and reps, hold or intervals), saved with it and returned by every read of the
    set. It is part of the set's content, so the same id with another target is `409`. The
    progression judges a training max session's set against its target exactly (lifted at least
    the target's weight); a set without one (logged before #60, an extra, an added exercise) is
    judged with the legacy 1.25 kg tolerance, and so is a set whose target has no weight (never a
    training max prescription). The session screen sends a target only when it is a real
    prescription: none for the empty bar it offers when the plan needs a training max the lifter
    has not entered, or a training max entered while the workout is open would judge those sets
    against the bar. The server checks the target's types, not its values (a target lighter than
    the bar is fine: dumbbells, kettlebells): it is what the client says it showed, and it only
    ever moves the sender's own progression.
- **Finishing.** The domain's `SessionLog::end` checks the time: not before the start or before a
  logged set (`422`). A retry with the same outcome and time returns the same summary. Another
  outcome or time is `409`. The summary is computed from stored data up to and including the
  session, so a retry made later gets the same one, unless the training max or the unit changed in
  between: `changes` and `needs_training_max` depend on them. It contains:
  - `volume`: the working sets, weighted and not timed, through `From<&LoggedSet> for
    Option<PerformedSet>`.
  - `prs`: completed sessions only. They are compared with every earlier completed session of
    **any** program.
  - `changes`: completed sessions only. The `ProgressionChange` for each exercise of the day that
    the session has working sets of.
  - `needs_training_max`: the day's exercises loaded as a percentage of a training max the user
    has not entered.
- **History given to the progression engine**, as agreed on #12 and #18:
  - completed sessions of the session's program, all versions, that started before the planned
    session (by start, then id);
  - each session judged against its own prescription, meaning the exercise on its day in the
    version it was run from;
  - for a training-max exercise, only the sets completed after the training max's `set_at`;
  - the training max the engine returns is only displayed. It is never stored.

  A stored version that no longer parses leaves its sessions unjudged (`Prescription` `None`).
  Stored sets that fall outside their session's time span, because of client clocks or a set saved
  while its session was finishing, have their time clamped into the span for the domain. The time
  plays no part in the rules applied to stored data, and refusing them would block the session for
  good.

## Tests

Server functions are tested through the real router, as signed-in users, with
`server::api::testing`:

- `TestApi::new(db)` builds the full app over the fresh database that `#[sqlx::test]` provides.
- `api.user(name)` and `api.users_a_and_b()` sign users up with a software passkey (#5's test
  support). Each user has their own browser and cookie.
- `user.id` is the repository owner key, used to seed data with `server::db::testing`.
- `user.call::<T>(path, json!({ ... }))` sends a `POST` with the arguments as JSON and decodes the
  result, or returns a `CallError { status, message }`. `user.call_err(...)` expects a failure.
- `assert_not_found_for_other_user(&mut b, path, a_id, |id| json!({ ... }))` checks that B gets
  exactly the `404 Not found.` of an id that exists for nobody, both for A's id and for a random
  one.
- `assert_unauthorized_when_signed_out(&api, path, body)` checks for a `401`.

Tests that need Postgres go under `server::api::<area>::tests`, with
`#[sqlx::test(migrator = "crate::server::db::MIGRATOR")]` and `#[ignore = "needs Postgres"]`. CI
checks that every one of them passed.

**Isolation tests are required for every endpoint.** Name them `another_users_*`. CI counts them and
enforces a floor, so raise the floor in `scripts/check-postgres-tests.sh` when you add some. For each of A's ids that an
endpoint accepts, B gets 404 from reads, updates and deletes, and nothing of A's appears in B's
lists. After a refused write, A's data is unchanged.

```rust
#[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
#[ignore = "needs Postgres"]
async fn another_users_session_is_not_found(db: PgPool) {
    let api = TestApi::new(db).await;
    let (mut a, mut b) = api.users_a_and_b().await;
    let session = db_testing::session(&api.db, a.id).await;
    let body = |id: Uuid| json!({ "session_id": id });

    testing::assert_not_found_for_other_user(&mut b, GET, session.as_uuid(), body).await;
    // A still sees it: the 404 was about B, not about the id.
    let view: Result<SessionView, _> = a.call(GET, body(session.as_uuid())).await;
    assert!(view.is_ok(), "{view:?}");
}
```

Unit tests without a database stay next to the code as usual.

## Endpoints

### History (`src/api/history.rs`, #20)

The history is the user's **ended** sessions (completed, skipped or abandoned). Weights are the
domain `Weight` (kg numbers on the wire); the UI converts them to the user's unit. e1RM uses the
Epley formula.

| Path | Arguments | Result |
|---|---|---|
| `/api/history/page` | `cursor: Option<HistoryCursor>`, `limit: Option<u32>` (default 20, 1 to 100) | `HistoryPage { sessions, next }`: ended sessions, most recently finished first (ties by id, descending). `next` is `None` on the last page. Each `SessionSummary` carries `day_name` (from the session's own program version), `volume` (working sets, as the end-of-session summary counts it) and `set_pr` (see below). |
| `/api/history/session` | `session_id` | `SessionDetails`: the session (ended or still in progress) and its sets grouped by exercise (in the order each was first logged), with each exercise's top set, best e1RM and volume. The session's `day_name`, `volume` and `set_pr` are the list's. |
| `/api/history/exercise-series` | `exercise_id` (a slug) | `ExerciseSeries`: one `ExercisePoint` per ended session with a weighted working set, oldest first: the top set, the best e1RM and the exercise's volume in that session. Timed sets (holds) are left out. |
| `/api/history/exercises` | none | The exercises logged in ended sessions, most recently trained first, with the number of sessions. |

**PR flags.** `set_pr` is true exactly when the session's end-of-session summary reports a personal
record: only completed sessions, against the sets of the user's completed sessions started before it
(`ExerciseRecords`, Epley). A page costs three queries whatever its size: the page, its sets
(`sets::list_for_sessions`), and the record history of the exercises it logged up to its latest
completed session (`sets::completed_for_exercises_before`), replayed in order.

- **Cursor.** `HistoryCursor` is opaque to the client: pass back the `next` of the previous page.
  It holds the last session's `finished_at` in **microseconds** (the database's precision) and its
  id. A millisecond cursor would skip sessions finished within the same millisecond. A cursor
  whose time is outside what can be stored (before 4714 BC, Postgres' earliest `timestamptz`, or
  after the year 9999) is `422`; another user's cursor just gives an empty page. At every page
  size, including the largest, `next` is set whenever another session follows.
- **Charts.** A point's key is the session's start time and id (`SeriesKey`), so two sessions
  started in the same millisecond stay apart. Sets without a weight (body-weight work) have no
  point; sets of abandoned sessions count (they were lifted). A session still in progress is left
  out until it ends.
- Errors: `422` for a page size out of range, a bad cursor or an exercise id that is not a slug;
  `404` for a session that is not the user's.

### Account (`src/auth/api.rs`, #103)

Sign-in keeps its own module and `AuthError` (see [Layout](#layout)); these two complete it.

| Path | Arguments | Result |
|---|---|---|
| `/api/auth/rename` | `display_name` | The renamed `Me`. Trimmed; blank, longer than 64 characters or with a control character is `400` with the reason. |
| `/api/auth/sign-out-everywhere` | none | Nothing. Deletes every session of the user, this one included, and clears this device's cookie: every device is signed out. |

### Settings (`src/api/settings.rs`, #20)

| Path | Arguments | Result |
|---|---|---|
| `/api/settings/get` | none | `Settings`. A user who never saved any gets `Settings::defaults()`: kg, a 20 kg bar, the domain's default kg plate inventory (`PlateInventory::default_for(Kg)`), 120 s of rest, sound and vibration on, weight steps of 2.5 kg and 5 lb (`kg_weight_step`, `lb_weight_step`, one per unit; `Settings::weight_step(unit)` picks the one in use). |
| `/api/settings/update` | `settings: SettingsUpdate` | The saved `Settings` (plates sorted heaviest first). A full replace, so a retry is harmless. Concurrent updates (two devices) are last-writer-wins: the row always holds one whole update, never fields mixed from two. |
| `/api/settings/training-maxes` | none | The user's `TrainingMax`es, by exercise id. |
| `/api/settings/training-max/set` | `exercise_id`, `weight` (kg) | The saved `TrainingMax`. |
| `/api/settings/training-max/delete` | `exercise_id` | Nothing; `404` if the user has no training max for it. |

- **Validation (`422`, with the reason).** `SettingsUpdate` carries the values the user types
  unchecked: `bar_weight` and each plate as kg numbers (the same JSON as a `Weight`), the plate
  inventory as a plain list. The server validates them with the domain (`Weight::from_kg`,
  `PlateInventory::new`: no zero, duplicate or off-grid plate, at most 50 pairs and 16 sizes), and
  the default rest must be at most one hour. The bar must weigh more than zero and the inventory
  must keep at least one plate size. Each weight step must be more than 0 and at most 25 kg
  (`MAX_WEIGHT_STEP_KG`). Each refusal is an `InvalidField` naming `bar_weight`,
  `plate_inventory`, `default_rest`, `kg_weight_step` or `lb_weight_step`.
- **Weight step and vibration (#103)** used to be kept on the device. On the first load after
  #103, the app carries a value still on the device over into a setting the server has at its
  default, saves it, and removes the device's copy.
  In `Settings` the three fields default when missing, so an export made before #103 still
  imports (decision log #41). In `SettingsUpdate` they are optional: a client built before #103
  leaves them out, and they keep their saved values. Weights out of range get a fixed message ("… must be between 0 and 2000 kg."), never the number echoed back. A typed `Weight` or `PlateInventory` argument would
  fail while the body is decoded, before the function runs, and only give the generic
  `422 Invalid request.` without saying which value is wrong.
- **Defaults only when nothing was saved.** The defaults apply only while there is no
  `user_settings` row (`settings::find` returns `None`).
- **Training maxes and the progression anchor.** Setting a training max always moves its `set_at`
  to now, on the server's clock, even when the weight is unchanged: the progression (#57) restarts
  from this value and replays only the sets logged after it. This is the one write whose time
  comes from the server, since the point is "from now on". A retried request moves the anchor by a
  few seconds, which only matters if a set was logged in between. The weight must be more than
  zero.

## Account data (`src/api/account.rs`, #22)

The user's GDPR rights: export everything they own, import such an export, delete the account. The
format, the import rules and the deletion are in [`docs/export-format.md`](export-format.md).

| Function | Route | Arguments | Returns | Errors |
|---|---|---|---|---|
| `export_account_data` | `/api/account/export` | | `ExportDocument` | 413 past `MAX_EXPORT_BYTES` (8 MiB) |
| `import_account_data` | `/api/account/import` | `document` (the export's JSON text) | `ImportSummary` (what was added) | 413 body or document too large; 422 not an export, unsupported `format_version`, invalid content (`InvalidProgramIn` for a program version); 403 over the plan's program limit; 503 with `Retry-After` when 2 account operations already run |
| `delete_account` | `/api/account/delete` | | `()`, and the cookie is cleared | 403 without a sign-in in the last 10 minutes; 503 with `Retry-After` when 2 account operations already run |

- **Import is idempotent.** Rows are matched by their keys and what the account already has wins:
  the same export imported twice adds nothing the second time.
- **Import body.** A middleware checks the session, takes one of the 2 import slots, then reads
  the body with `limits::read_body` (`IMPORT_BODY_LIMIT`, 60 s read timeout) before anything else
  does. The route is in `limits::OWN_BODY_LIMIT` and also raises axum's 2 MiB `DefaultBodyLimit`,
  so a large export imports instead of panicking in Dioxus' extractor.
- **Database deadlines.** An import and an account deletion run on a connection of their own with
  60 s deadlines instead of the pool's 5 s (`docs/export-format.md`).
- **Isolation.** Every read and write is scoped to the session's user. An export never contains
  another user's rows. Importing someone else's export gives the importer their own copies (new
  program ids) and never touches the owner's rows. The tests are the `another_users_*` in
  `server::api::account::tests`.
- **Rate limits.** The three routes share the `account_data` group (`docs/rate-limiting.md`).

## Programs (`src/api/programs.rs`, #19)

Every function is a `POST` that needs a signed-in user and only reads or changes that user's
programs. Built-in programs are read-only: a user trains with a copy, which is their own program.
An id of another user's program, or of a built-in's own row, gets the same `404` as an id that does
not exist.

| Function | Route | Arguments | Returns | Errors |
|---|---|---|---|---|
| `list_builtin_programs` | `/api/programs/builtins` | | `Vec<BuiltinProgramView>` (`builtin_id`, name, version, parsed document) | |
| `copy_builtin_program` | `/api/programs/copy-builtin` | `builtin_id`, `creation_id` | `ProgramDetail` of the copy | 404 unknown (or malformed) built-in id; 409 `creation_id` already used for another request |
| `list_programs` | `/api/programs/list` | `include_archived` | `Vec<ProgramView>`, oldest first | |
| `get_program` | `/api/programs/get` | `program_id` | `ProgramDetail` (latest version) | 404 |
| `get_active_program` | `/api/programs/active` | | `Option<ProgramDetail>` (latest version) | |
| `set_active_program` | `/api/programs/active/set` | `program_id` | `ProgramDetail` | 404; 409 archived |
| `upload_program` | `/api/programs/upload` | `target`, `document` | `UploadOutcome` (`program`, `version`, `saved`) | 404; 409 `creation_id` reused; 413; 422 `InvalidProgram` with `ProgramProblems` |
| `list_program_versions` | `/api/programs/versions` | `program_id` | `Vec<VersionView>`, oldest first, without documents | 404 |
| `set_program_archived` | `/api/programs/archive` | `program_id`, `archived` | `()` | 404; 409 archiving the active program |

- **Idempotency.** A copy and a new-program upload take a client `creation_id` (UUIDv7): a retry
  returns the program already created (`saved: false` for an upload); the same `creation_id` with a
  different built-in or document is `409`. A new version whose document equals the program's latest
  version adds nothing and returns that version with `saved: false`; documents are compared as
  jsonb, so formatting and key order do not matter.
- **Uploads are untrusted.** `target` is `{"kind": "new_program", "creation_id": …}` (named after
  the document's `name`) or `{"kind": "new_version", "program_id": …}` (the program keeps its own
  name). In order:
  1. A middleware on the route first checks the session (a signed-out client gets its `401`
     without the body being read), then reads the whole body before Dioxus does and refuses it
     with `413` past `UPLOAD_BODY_LIMIT` (2 × `MAX_DOCUMENT_BYTES` + 16 KiB: the document travels as a JSON
     string, where `"`, `\` and line breaks take two bytes). It checks `Content-Length` first and
     then counts the bytes actually read, so a missing or lying header does not get past it. A
     body that takes longer than 10 s to arrive is `408` (see [Request limits](#request-limits-74)).
     This route is exempt from the 64 KiB default cap.
  2. A document over `MAX_DOCUMENT_BYTES` (256 KiB) is refused with `413`, before parsing.
  3. `Program::from_json` parses and validates it. A failure is `422` with `ProgramProblems`
     (`{errors: [{path, message, line?, column?}], omitted}`) as the error details: a parse error
     is one entry with its line and column; broken rules are at most `MAX_REPORTED_ERRORS` entries,
     the rest counted in `omitted`. Read them on the client with `ProgramProblems::from_error`.
     The rules include texts: names must not contain any C0 control character (U+0000 to
     U+001F), descriptions and notes none but tab, line feed and carriage return. Postgres cannot
     store U+0000, so this also keeps such a document from reaching the database.
  4. The document is stored as uploaded (like the built-ins), not re-serialized.
- **Archive, never delete.** Archiving hides a program from `list_programs` (unless
  `include_archived`) and keeps its versions and the sessions run from them; it can still be read
  and get new versions, and `archived: false` restores it. The active program cannot be archived and
  an archived program cannot be made active (`409`), so the active program is never hidden. Both
  calls check and write in one transaction that first locks the program's row, so concurrent calls
  on the same program run one after the other (one wins, the other gets `409`). Triggers enforce
  the rule in the database too (migration `20260928220000_active_program_never_archived`).
- **Timestamps.** `created_at` in `ProgramView` and `VersionView` is a `Timestamp` (milliseconds,
  see [Layout](#layout)).
