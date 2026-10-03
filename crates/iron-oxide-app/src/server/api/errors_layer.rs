//! One error body for every server function (#68).
//!
//! Dioxus 0.7 answers a failed server function in several shapes:
//!
//! - an error returned by the function's body: `{"message", "code", "data": {"ServerError":
//!   {"message", "code", "details"?}}}`, where the top `message` is Dioxus's `Display` text
//!   (`error running server function: Not found. (details: None)`);
//! - another body shape with `data` that is not a `ServerError` (a rate-limit body, another
//!   `ServerFnError` variant);
//! - an extractor rejection (`AuthUser`'s 401) or a request Dioxus could not decode (malformed or
//!   missing arguments): `{"error": text, "details"?}`. Bad arguments come out as a `500` whose
//!   text is a serde error.
//!
//! This layer rewrites them, for `/api/` routes only, into
//! `{"message": m, "code": c, "data": {"ServerError": {"message": m, "code": c, "details"?: d}}}`.
//! The Dioxus client decodes `data` into `ServerFnError::ServerError { message: m, code: c,
//! details: d }`: our message and details are the `ServerError`'s own fields.
//!
//! - Arguments that do not decode become `422 Invalid request.`
//! - Every 5xx gets a fixed message and no details: nothing a function puts in a 500
//!   (`ServerFnError::new(detail)`, an `anyhow` error) or a 503 reaches the client. A 503 gets the
//!   retry message ([`TRANSIENT`]), every other 5xx the generic one ([`INTERNAL`]). The one detail
//!   a 503 keeps is a bounded `retry_after_secs` (1 to 3600), so its `Retry-After` survives. The
//!   original is logged.
//! - A 5xx whose body is not one of these shapes (not JSON: a panic's text, which Dioxus includes
//!   in debug builds; or JSON of another shape) gets the generic message too, keeping its status
//!   (a 503 gets the retry message). Other non-JSON 4xx bodies (axum's own 405 or 415) are kept.

use dioxus::logger::tracing;
use dioxus::server::axum::{
    body::{Body, to_bytes},
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::Response,
};
use serde_json::{Value, json};

use super::error::{INTERNAL, TRANSIENT};

/// The message of a request whose arguments do not decode.
pub const INVALID_REQUEST: &str = "Invalid request.";

/// Error bodies are small; a bigger or unreadable one is replaced by the generic message.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// The prefixes of Dioxus's own texts for arguments it could not decode.
const BAD_ARGUMENTS: [&str; 3] = [
    "error deserializing server function",
    "missing argument",
    "error deserializing request",
];

/// The axum middleware: see the module documentation.
pub async fn normalize(request: Request, next: Next) -> Response {
    let is_api = request.uri().path().starts_with("/api/");
    let response = next.run(request).await;
    let status = response.status();
    if !is_api || !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let (parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_ERROR_BODY).await else {
        tracing::error!(%status, "server function error body too large or unreadable");
        return rebuild(parts, status, generic(status), None);
    };
    let rewritten = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| rewrite(status, &value));
    match rewritten {
        Some((status, message, details)) => rebuild(parts, status, &message, details),
        // A 5xx we cannot read (a panic's text, a layer's plain-text error): its text may say
        // anything, so it never reaches the client.
        None if status.is_server_error() => {
            tracing::error!(
                %status,
                body = %String::from_utf8_lossy(&bytes),
                "server function failed with an unknown error body"
            );
            rebuild(parts, status, generic(status), None)
        }
        None => Response::from_parts(parts, Body::from(bytes)),
    }
}

/// An `/api/` error response in the shape every `/api/` error has, for layers that refuse a
/// request before any server function runs.
pub fn error_response(status: StatusCode, message: &str, details: Option<Value>) -> Response {
    let parts = Response::new(()).into_parts().0;
    rebuild(parts, status, message, details)
}

/// The generic message of a 5xx: retry for a 503, "something went wrong" otherwise.
fn generic(status: StatusCode) -> &'static str {
    if status == StatusCode::SERVICE_UNAVAILABLE {
        TRANSIENT
    } else {
        INTERNAL
    }
}

/// The status, message and details of the rewritten body, or `None` to keep it.
fn rewrite(status: StatusCode, body: &Value) -> Option<(StatusCode, String, Option<Value>)> {
    let (status, message, details) =
        if let Some(inner) = body.get("data").and_then(|data| data.get("ServerError")) {
            // Returned by the function's body.
            let message = inner.get("message")?.as_str()?.to_owned();
            (status, message, inner.get("details").cloned())
        } else if let (Some(message), Some(data)) = (
            body.get("message").and_then(Value::as_str),
            body.get("data"),
        ) {
            // Another `{"message", "code", "data"}` body: `data` becomes the details.
            (status, message.to_owned(), Some(data.clone()))
        } else {
            let text = body.get("error")?.as_str()?;
            if status == StatusCode::INTERNAL_SERVER_ERROR
                && BAD_ARGUMENTS.iter().any(|prefix| text.starts_with(prefix))
            {
                tracing::info!(error = text, "server function arguments do not decode");
                return Some((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    INVALID_REQUEST.to_owned(),
                    None,
                ));
            }
            (status, text.to_owned(), body.get("details").cloned())
        };
    if status.is_server_error() {
        let public = generic(status);
        let kept = (status == StatusCode::SERVICE_UNAVAILABLE)
            .then(|| retry_after_only(details.as_ref()))
            .flatten();
        if message != public || details != kept {
            tracing::error!(%status, error = message, ?details, "server function failed");
        }
        return Some((status, public.to_owned(), kept));
    }
    Some((status, message, details))
}

/// The longest `retry_after_secs` a `503` passes on: longer is not a wait anyone retries after.
const MAX_RETRY_AFTER_SECS: u64 = 3_600;

/// The only detail a `503` keeps: `{"retry_after_secs": n}`, a whole number of seconds from 1 to
/// [`MAX_RETRY_AFTER_SECS`] (a busy account import or deletion, #22). Anything else is dropped.
fn retry_after_only(details: Option<&Value>) -> Option<Value> {
    let secs = details?
        .get(crate::rate_limit::RETRY_AFTER_SECS)?
        .as_u64()
        .filter(|secs| (1..=MAX_RETRY_AFTER_SECS).contains(secs))?;
    Some(json!({ crate::rate_limit::RETRY_AFTER_SECS: secs }))
}

fn rebuild(
    mut parts: dioxus::server::axum::http::response::Parts,
    status: StatusCode,
    message: &str,
    details: Option<Value>,
) -> Response {
    let code = status.as_u16();
    let mut inner = json!({ "message": message, "code": code });
    if let (Some(details), Some(object)) = (details, inner.as_object_mut()) {
        object.insert("details".to_owned(), details);
    }
    let body = json!({ "message": message, "code": code, "data": { "ServerError": inner } });
    // A retryable answer that says when to retry (`retry_after_secs` in its details) also says it
    // in the standard header, unless its layer already set it (the 429s).
    let retry_after = body["data"]["ServerError"]["details"]["retry_after_secs"].as_u64();
    if let Some(secs) = retry_after.filter(|_| {
        matches!(
            status,
            StatusCode::SERVICE_UNAVAILABLE | StatusCode::TOO_MANY_REQUESTS
        )
    }) {
        parts
            .headers
            .entry(header::RETRY_AFTER)
            .or_insert(HeaderValue::from(secs));
    }
    parts.status = status;
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Response::from_parts(parts, Body::from(body.to_string()))
}

#[cfg(test)]
pub(crate) mod tests {
    use dioxus::fullstack::{RequestDecodeErr, ServerFnDecoder};
    use dioxus::prelude::ServerFnError;

    use super::*;
    use crate::api::error::{ApiFailure, FailureKind};

    /// What the Dioxus client makes of an error response: `decode_client_response` (the body's
    /// `message`, `code` and `data`, or `HTTP {code}: {text}` when it is not that JSON), then the
    /// real `decode_client_err` of a `Result<_, ServerFnError>` server function.
    pub(crate) async fn client_error(status: StatusCode, body: &[u8]) -> ServerFnError {
        let decoded = match serde_json::from_slice::<Value>(body) {
            Ok(value) if value.get("message").is_some() && value.get("code").is_some() => {
                ServerFnError::ServerError {
                    message: value["message"].as_str().unwrap_or_default().to_owned(),
                    code: u16::try_from(value["code"].as_u64().unwrap_or(0)).unwrap(),
                    details: value.get("data").cloned(),
                }
            }
            _ => ServerFnError::ServerError {
                message: format!(
                    "HTTP {}: {}",
                    status.as_u16(),
                    String::from_utf8_lossy(body)
                ),
                code: status.as_u16(),
                details: None,
            },
        };
        let decoder = ServerFnDecoder::<Result<(), ServerFnError>>::new();
        (&&&decoder)
            .decode_client_err(Ok(Err(decoded)))
            .await
            .unwrap_err()
    }

    /// The layer's output for `body`, as the client classifies it.
    async fn through_the_client(status: StatusCode, body: Value) -> (StatusCode, ApiFailure) {
        let (status, message, details) = rewrite(status, &body).unwrap();
        let parts = Response::new(()).into_parts().0;
        let response = rebuild(parts, status, &message, details);
        let status = response.status();
        let bytes = to_bytes(response.into_body(), MAX_ERROR_BODY)
            .await
            .unwrap();
        (
            status,
            ApiFailure::classify(&client_error(status, &bytes).await),
        )
    }

    /// Dioxus's body for an error returned by a function.
    fn returned(code: u16, message: &str, details: Option<Value>) -> Value {
        let mut inner = json!({ "message": message, "code": code });
        if let Some(details) = details {
            inner["details"] = details;
        }
        json!({
            "message": format!("error running server function: {message} (details: None)"),
            "code": code,
            "data": { "ServerError": inner }
        })
    }

    #[tokio::test]
    async fn our_message_and_details_reach_the_client() {
        let (status, failure) =
            through_the_client(StatusCode::CONFLICT, returned(409, "Taken.", None)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            (failure.kind, failure.message.as_str()),
            (FailureKind::Conflict, "Taken.")
        );

        let problems = json!(["days[0].name is empty"]);
        let body = returned(422, "Invalid program.", Some(problems.clone()));
        let (_, failure) = through_the_client(StatusCode::UNPROCESSABLE_ENTITY, body).await;
        assert_eq!(failure.message, "Invalid program.");
        assert_eq!(failure.details, Some(problems));
    }

    #[tokio::test]
    async fn a_rate_limit_body_keeps_its_retry_after() {
        // The shape #72 sends; the layer turns its `data` into the details.
        let body = json!({
            "message": "Too many requests. Please wait a moment.",
            "code": 429,
            "data": { "retry_after_secs": 30 }
        });
        let (status, failure) = through_the_client(StatusCode::TOO_MANY_REQUESTS, body).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(failure.kind, FailureKind::RateLimited);
        assert!(failure.is_retryable());
        assert_eq!(failure.retry_after_secs(), Some(30));
    }

    #[tokio::test]
    async fn an_extractor_rejection_keeps_its_status_and_message() {
        let body = json!({ "error": "Please sign in." });
        let (status, failure) = through_the_client(StatusCode::UNAUTHORIZED, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(failure.message, "Please sign in.");
    }

    #[tokio::test]
    async fn arguments_that_do_not_decode_are_422_without_the_serde_text() {
        for text in [
            "error deserializing server function results: UUID parsing failed: invalid character",
            "error deserializing server function arguments: missing field `session_id`",
            "missing argument session_id",
        ] {
            let body = json!({ "error": text });
            let (status, failure) =
                through_the_client(StatusCode::INTERNAL_SERVER_ERROR, body).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(failure.message, INVALID_REQUEST);
        }
    }

    #[tokio::test]
    async fn no_5xx_text_or_details_reach_the_wire() {
        let secret = "relation workout_sets at 10.0.0.3";
        for (status, body) in [
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                returned(500, secret, Some(json!(secret))),
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": secret }),
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "message": secret, "code": 500, "data": { "Deserialization": secret } }),
            ),
            (StatusCode::BAD_GATEWAY, returned(502, secret, None)),
        ] {
            let (message, _, details) = {
                let (s, m, d) = rewrite(status, &body).unwrap();
                (m, s, d)
            };
            assert_eq!(message, INTERNAL);
            assert_eq!(details, None);
        }
        // A 503 always gets the fixed retry message, whatever its body says (#104).
        for body in [
            returned(503, "Please try again.", None),
            returned(503, secret, Some(json!(secret))),
            json!({ "message": secret, "code": 503, "data": { "where": secret } }),
            json!({ "error": secret, "details": secret }),
        ] {
            let (status, message, details) =
                rewrite(StatusCode::SERVICE_UNAVAILABLE, &body).unwrap();
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(message, TRANSIENT, "{body}");
            assert_eq!(details, None, "{body}");
        }
        let (status, failure) = through_the_client(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "message": secret, "code": 503, "data": { "where": secret } }),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(failure.message, TRANSIENT);
        assert!(failure.is_retryable());
    }

    #[tokio::test]
    async fn a_503_keeps_only_a_bounded_retry_after() {
        let secret = "relation workout_sets at 10.0.0.3";
        let busy = |details: Value| returned(503, secret, Some(details));
        for (details, kept) in [
            (
                json!({ "retry_after_secs": 5 }),
                Some(json!({ "retry_after_secs": 5 })),
            ),
            (
                json!({ "retry_after_secs": 5, "where": secret }),
                Some(json!({ "retry_after_secs": 5 })),
            ),
            (json!({ "retry_after_secs": 0 }), None),
            (json!({ "retry_after_secs": 3_601 }), None),
            (json!({ "retry_after_secs": -1 }), None),
            (json!({ "retry_after_secs": 1.5 }), None),
            (json!({ "retry_after_secs": "5" }), None),
            (json!(secret), None),
        ] {
            let (_, message, got) =
                rewrite(StatusCode::SERVICE_UNAVAILABLE, &busy(details.clone())).unwrap();
            assert_eq!(message, TRANSIENT, "{details}");
            assert_eq!(got, kept, "{details}");
        }
        // Other 5xx keep nothing, not even a wait.
        let (_, _, got) = rewrite(
            StatusCode::INTERNAL_SERVER_ERROR,
            &returned(500, secret, Some(json!({ "retry_after_secs": 5 }))),
        )
        .unwrap();
        assert_eq!(got, None);
        let (status, failure) = through_the_client(
            StatusCode::SERVICE_UNAVAILABLE,
            busy(json!({ "retry_after_secs": 5 })),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(failure.message, TRANSIENT);
        assert_eq!(failure.details, Some(json!({ "retry_after_secs": 5 })));
        // And the `Retry-After` header.
        let (status, message, details) = rewrite(
            StatusCode::SERVICE_UNAVAILABLE,
            &busy(json!({ "retry_after_secs": 5, "where": secret })),
        )
        .unwrap();
        let response = rebuild(Response::new(()).into_parts().0, status, &message, details);
        assert_eq!(response.headers()[header::RETRY_AFTER], "5");
    }

    /// `normalize` around a route answering `status` with `body`.
    async fn normalized(
        path: &str,
        status: StatusCode,
        body: &'static str,
    ) -> (StatusCode, String) {
        use dioxus::server::axum::{Router, middleware::from_fn, routing::get};
        use tower::ServiceExt;
        let router = Router::new()
            .route(path, get(move || async move { (status, body) }))
            .layer(from_fn(normalize));
        let response = router
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), MAX_ERROR_BODY)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_5xx_body_of_another_shape_never_reaches_the_client() {
        let secret = "Server function panicked: task 7 panicked with message \"at 10.0.0.3\"";
        for (status, body, message) in [
            (StatusCode::INTERNAL_SERVER_ERROR, secret, INTERNAL),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"other": "at 10.0.0.3"}"#,
                INTERNAL,
            ),
            (StatusCode::BAD_GATEWAY, secret, INTERNAL),
            (StatusCode::SERVICE_UNAVAILABLE, secret, TRANSIENT),
        ] {
            let (got, text) = normalized("/api/x", status, body).await;
            assert_eq!(got, status);
            assert!(!text.contains("10.0.0.3"), "{text}");
            let value: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["data"]["ServerError"]["message"], message);
            assert_eq!(value["code"], status.as_u16());
        }
        // Outside `/api/`, and for a 4xx of another shape, the body is kept.
        assert_eq!(
            normalized("/page", StatusCode::INTERNAL_SERVER_ERROR, secret)
                .await
                .1,
            secret
        );
        assert_eq!(
            normalized("/api/x", StatusCode::METHOD_NOT_ALLOWED, "nope")
                .await
                .1,
            "nope"
        );
    }

    #[test]
    fn unknown_shapes_are_kept() {
        assert_eq!(
            rewrite(StatusCode::BAD_REQUEST, &json!({ "other": 1 })),
            None
        );
        assert_eq!(rewrite(StatusCode::BAD_REQUEST, &json!("text")), None);
    }
}
