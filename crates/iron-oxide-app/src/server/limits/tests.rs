//! The body cap and the timeouts, through the full router (#74).

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use dioxus::prelude::*;
use dioxus::server::axum::{
    body::to_bytes,
    http::{
        Request as HttpRequest, StatusCode,
        header::{CONTENT_LENGTH, CONTENT_TYPE},
    },
};
use http_body::Frame;
use iron_oxide_domain::{
    ExerciseId, LoggedSet, PlateInventory, Reps, SLUG_MAX_LEN, SessionId, SetId, Weight,
    time::Timestamp,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use webauthn_rs_proto::ResidentKeyRequirement;

use super::*;
use crate::api::error::{ApiFailure, FailureKind};
use crate::server::api::error::{INTERNAL, UNAUTHORIZED};
use crate::server::api::errors_layer::tests::client_error;
use crate::server::api::testing::TestApi;
use crate::server::auth::test_support::{Browser, Passkey, TestApp};
use crate::server::db;

/// A secret only the panicking function knows: it must never reach the client.
const PANIC_TEXT: &str = "panic text that must stay on the server";

/// Panics: Dioxus answers a plain-text 500, with the panic's text in debug builds.
#[post("/api/test/limits/panic")]
async fn test_panics() -> Result<(), ServerFnError> {
    panic!("{PANIC_TEXT}")
}

/// Takes longer than the test's body-read timeout, after its body has arrived.
#[post("/api/test/limits/slow")]
async fn test_slow() -> Result<(), ServerFnError> {
    tokio::time::sleep(SLOW).await;
    Ok(())
}

const SLOW: Duration = Duration::from_millis(600);

/// A server function that needs a signed-in user, with one argument.
const SAVE_SET: &str = "/api/sessions/save-set";

const SHORT: RequestLimits = RequestLimits {
    body: DEFAULT_BODY_LIMIT,
    body_read_timeout: Duration::from_millis(200),
    import_body_read_timeout: Duration::from_millis(200),
};

async fn signed_out(limits: RequestLimits) -> Browser {
    TestApp::with_config(db::tests::unreachable_pool(), |config| {
        config.request_limits = limits;
    })
    .await
    .browser()
}

/// A body sent in `chunks`, with no length known in advance (as `Transfer-Encoding: chunked`),
/// then, if `stall`, never finished.
struct Chunked {
    chunks: VecDeque<Bytes>,
    stall: bool,
}

impl HttpBody for Chunked {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match self.chunks.pop_front() {
            Some(chunk) => Poll::Ready(Some(Ok(Frame::data(chunk)))),
            // Never woken: the read timeout ends the wait.
            None if self.stall => Poll::Pending,
            None => Poll::Ready(None),
        }
    }
}

/// A body of `total` spaces, sent in 8 KiB chunks with no length known in advance; if `stall`,
/// never finished.
pub(crate) fn chunked(total: usize, stall: bool) -> Body {
    let chunks = (0..total)
        .step_by(8 * 1024)
        .map(|start| Bytes::from(vec![b' '; (total - start).min(8 * 1024)]))
        .collect();
    let body = Body::new(Chunked { chunks, stall });
    assert_eq!(body.size_hint().exact(), None);
    body
}

/// A JSON body of exactly `size` bytes: `{"x": "…"}`, padded with spaces.
fn padded(size: usize) -> String {
    let mut body = String::from("{}");
    body.insert_str(1, &" ".repeat(size - 2));
    body
}

/// How a request is sent: its `Content-Length` (true or not) and how the body arrives.
#[derive(Debug, Clone, Copy)]
enum Sending {
    /// `Content-Length` announces the real size.
    Announced,
    /// `Content-Length` says 10 bytes, the body is larger.
    Lying,
    /// No length, in 8 KiB chunks.
    Chunked,
}

fn post(browser: &Browser, path: &str, size: usize, sending: Sending) -> HttpRequest<Body> {
    let request = browser
        .request("POST", path)
        .header(CONTENT_TYPE, "application/json");
    match sending {
        Sending::Announced => request
            .header(CONTENT_LENGTH, size)
            .body(Body::from(padded(size))),
        Sending::Lying => request
            .header(CONTENT_LENGTH, 10)
            .body(Body::from(padded(size))),
        Sending::Chunked => request.body(chunked(size, false)),
    }
    .unwrap()
}

async fn answer(browser: &mut Browser, request: HttpRequest<Body>) -> (StatusCode, Value) {
    let response = browser.send(request).await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&bytes)));
    (status, body)
}

/// Asserts `body` is the `/api/` error shape with `status` and `message`, and that the client
/// classifies it as `kind`.
async fn assert_error(status: StatusCode, body: &Value, message: &str, kind: FailureKind) {
    assert_eq!(body["code"], status.as_u16(), "{body}");
    assert_eq!(body["data"]["ServerError"]["message"], message, "{body}");
    let bytes = serde_json::to_vec(body).unwrap();
    let failure = ApiFailure::classify(&client_error(status, &bytes).await);
    assert_eq!(failure.kind, kind, "{body}");
}

#[tokio::test]
async fn an_oversize_body_is_a_413_for_a_signed_out_client() {
    let mut browser = signed_out(RequestLimits::default()).await;
    for sending in [Sending::Announced, Sending::Lying, Sending::Chunked] {
        let request = post(&browser, SAVE_SET, DEFAULT_BODY_LIMIT + 1, sending);
        let (status, body) = answer(&mut browser, request).await;
        // The cap runs before the function's extractors: 413, not 401.
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{sending:?}");
        assert_error(status, &body, TOO_LARGE, FailureKind::Invalid).await;
    }
    // Far past axum's 2 MiB default, where Dioxus alone panics.
    let request = post(&browser, SAVE_SET, 3 << 20, Sending::Chunked);
    assert_eq!(
        answer(&mut browser, request).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn a_body_at_the_limit_reaches_the_function() {
    let mut browser = signed_out(RequestLimits::default()).await;
    for sending in [Sending::Announced, Sending::Chunked] {
        let request = post(&browser, SAVE_SET, DEFAULT_BODY_LIMIT, sending);
        let (status, body) = answer(&mut browser, request).await;
        // Read whole, then refused by `AuthUser`.
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{sending:?}: {body}");
        assert_error(status, &body, UNAUTHORIZED, FailureKind::Unauthorized).await;
    }
}

#[tokio::test]
async fn a_page_with_an_oversize_body_is_a_plain_413() {
    let mut browser = signed_out(RequestLimits::default()).await;
    let request = post(
        &browser,
        "/some/page",
        DEFAULT_BODY_LIMIT + 1,
        Sending::Chunked,
    );
    let response = browser.send(request).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_panic_is_a_generic_500_without_its_text() {
    let mut browser = signed_out(RequestLimits::default()).await;
    let request = post(&browser, "/api/test/limits/panic", 2, Sending::Announced);
    let response = browser.send(request).await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{text}");
    assert!(!text.contains(PANIC_TEXT), "{text}");
    assert!(!text.contains("panicked"), "{text}");
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_error(status, &body, INTERNAL, FailureKind::Other).await;
}

/// No HTTP timeout on `/api/` calls: a slow function is answered once it has finished, never with
/// a `503` while it may still write (`server::limits`).
#[tokio::test]
async fn a_slow_call_is_answered_when_it_finishes() {
    let mut browser = signed_out(SHORT).await;
    let started = Instant::now();
    let request = post(&browser, "/api/test/limits/slow", 2, Sending::Announced);
    let response = browser.send(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(started.elapsed() >= SLOW);
}

#[tokio::test]
async fn a_slow_body_is_a_retryable_408() {
    let mut browser = signed_out(SHORT).await;
    let started = Instant::now();
    let request = browser
        .request("POST", SAVE_SET)
        .header(CONTENT_TYPE, "application/json")
        .body(chunked(1024, true))
        .unwrap();
    let (status, body) = answer(&mut browser, request).await;
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT, "{body}");
    assert_error(status, &body, BODY_TIMEOUT, FailureKind::Network).await;
    assert!(
        ApiFailure::classify(&client_error(status, &serde_json::to_vec(&body).unwrap()).await)
            .is_retryable()
    );
}

#[tokio::test]
async fn the_health_checks_are_unaffected() {
    let mut browser = signed_out(SHORT).await;
    let (status, _, body) = browser.get("/healthz").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "ok"));
}

/// The largest bodies the client legitimately sends, measured: each must stay far below the
/// default limit. The numbers are recorded in `docs/api.md`.
#[test]
fn the_default_body_limit_covers_every_server_function() {
    // A passkey registration from a software authenticator, which sends a `packed` attestation
    // with its certificate (browsers send `none`, as the server asks: smaller).
    let mut ccr = crate::server::auth::tests::webauthn()
        .start_passkey_registration(uuid::Uuid::now_v7(), &"n".repeat(64), &"n".repeat(64), None)
        .unwrap()
        .0;
    if let Some(selection) = ccr.public_key.authenticator_selection.as_mut() {
        selection.resident_key = Some(ResidentKeyRequirement::Required);
        selection.require_resident_key = true;
    }
    let credential = Passkey::new().register(ccr);
    let passkey = json!({ "credential": credential, "nickname": "n".repeat(64) });

    // Settings with the largest plate inventory: 16 sizes, the longest decimals.
    let plates: Vec<Value> = (0..PlateInventory::MAX_SIZES)
        .map(|i| json!({ "plate": 1.234_567_890_123_456_7 + i as f64, "pairs": 50 }))
        .collect();
    let settings = json!({ "settings": {
        "unit": "kg", "bar_weight": 20.123_456_789_012_345, "plate_inventory": plates,
        "default_rest": 3600, "sound_enabled": true,
    }});

    let set = LoggedSet {
        id: SetId::new_v7(),
        exercise: ExerciseId::new("e".repeat(SLUG_MAX_LEN)).unwrap(),
        set_index: u16::MAX,
        reps: Reps::MAX,
        weight: Some(Weight::MAX),
        duration: None,
        warm_up: false,
        completed_at: Timestamp::from_epoch_millis(i64::MAX),
        target: None,
    };
    let save_set = json!({ "session_id": SessionId::new_v7(), "set": set });

    let mut largest = 0;
    for (name, body) in [
        ("passkey registration", passkey),
        ("settings update", settings),
        ("save set", save_set),
    ] {
        let size = serde_json::to_vec(&body).unwrap().len();
        println!("{name}: {size} bytes");
        largest = largest.max(size);
    }
    assert!(largest * 8 < DEFAULT_BODY_LIMIT, "{largest} bytes");
}

/// The cap with a signed-in client: the same 413, and the session still works after it.
#[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
#[ignore = "needs Postgres"]
async fn an_oversize_body_is_a_413_for_a_signed_in_client(db: PgPool) {
    let api = TestApi::new(db).await;
    let mut user = api.user("A").await;
    for sending in [Sending::Announced, Sending::Lying, Sending::Chunked] {
        let request = user.post(SAVE_SET).header(
            CONTENT_LENGTH,
            match sending {
                Sending::Announced => DEFAULT_BODY_LIMIT + 1,
                _ => 10,
            },
        );
        let request = match sending {
            Sending::Chunked => user
                .post(SAVE_SET)
                .body(chunked(DEFAULT_BODY_LIMIT + 1, false)),
            _ => request.body(Body::from(padded(DEFAULT_BODY_LIMIT + 1))),
        }
        .unwrap();
        let (status, body) = user.send(request).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{sending:?}: {body}");
        assert_error(status, &body, TOO_LARGE, FailureKind::Invalid).await;
    }
    // At the limit, the function runs (and refuses the padding as arguments that do not decode).
    let request = user
        .post(SAVE_SET)
        .body(chunked(DEFAULT_BODY_LIMIT, false))
        .unwrap();
    assert_eq!(user.send(request).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let settings: Value = user.call("/api/settings/get", json!({})).await.unwrap();
    assert!(settings.is_object());
}
