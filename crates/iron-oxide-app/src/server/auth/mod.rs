//! Sign-in on the server (#5): passkeys, Sign in with Google, sessions, CSRF, and the
//! [`AuthUser`] extractor. Design and threat model: `docs/auth.md`.
//!
//! # Getting the signed-in user in a server function
//!
//! Add the server-only [`AuthUser`] argument after the route. It derives the [`UserId`] from the
//! session; without a valid session the call fails with `401` before the body runs. Never accept
//! a user id from the client.
//!
//! ```rust,ignore
//! #[cfg(feature = "server")]
//! use crate::server::{AppState, auth::AuthUser};
//! #[cfg(feature = "server")]
//! use dioxus::server::axum::Extension;
//!
//! #[post("/api/sets", state: Extension<AppState>, user: AuthUser)]
//! pub async fn save_set(set: NewSet) -> Result<(), ServerFnError> {
//!     let user_id = user.user_id(); // scope every query by it
//!     // ...
//! }
//! ```

pub mod ceremony;
pub mod csrf;
pub mod error;
pub mod google;
pub mod passkeys;
pub mod session;

#[cfg(test)]
mod integration_tests;
#[cfg(test)]
pub(crate) mod test_support;

use std::sync::Arc;

use dioxus::prelude::ServerFnError;
use dioxus::server::axum::{
    Extension, Router,
    extract::FromRequestParts,
    http::request::Parts,
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use sqlx::PgPool;
use tower_sessions::{Session, cookie::Key};
use webauthn_rs::prelude::{Webauthn, WebauthnBuilder};

pub use self::error::AuthError;
use self::{
    csrf::CsrfPolicy,
    google::GoogleOidc,
    session::{PgSessionStore, keys},
};
use super::{
    AppState,
    config::Config,
    rate_limit::{self, RateLimiter},
};
use crate::auth::types::UserId;

/// The relying party name shown by some passkey prompts.
const RP_NAME: &str = "Iron Oxide";

/// Sign-in services shared by every request. Cheap to clone.
#[derive(Clone)]
pub struct AuthState {
    inner: Arc<Inner>,
}

struct Inner {
    webauthn: Webauthn,
    google: GoogleOidc,
    csrf: CsrfPolicy,
    session_key: Key,
    cookie_secure: bool,
}

impl std::fmt::Debug for AuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthState")
            .field("origin", &self.inner.csrf.origin())
            .field("cookie_secure", &self.inner.cookie_secure)
            .finish_non_exhaustive()
    }
}

impl AuthState {
    /// Builds the sign-in services from the configuration, talking to Google.
    pub fn new(config: &Config) -> Result<Self, AuthError> {
        Self::with_google_issuer(config, google::GOOGLE_ISSUER)
    }

    /// Like [`AuthState::new`] with another OpenID provider (tests use a local mock of Google).
    pub fn with_google_issuer(config: &Config, issuer: &str) -> Result<Self, AuthError> {
        let auth = &config.auth;
        let webauthn = WebauthnBuilder::new(&auth.webauthn_rp_id, &auth.webauthn_origin)?
            .rp_name(RP_NAME)
            .build()?;
        let google = GoogleOidc::new(
            issuer,
            &auth.google_client_id,
            auth.google_client_secret.clone(),
            &auth.google_redirect_url,
        )?;
        let session_key = Key::try_from(auth.session_key.expose_bytes())
            .map_err(|e| AuthError::Internal(format!("SESSION_KEY: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                webauthn,
                google,
                csrf: CsrfPolicy::new(&config.app_base_url),
                session_key,
                cookie_secure: auth.cookie_secure,
            }),
        })
    }

    #[must_use]
    pub fn webauthn(&self) -> &Webauthn {
        &self.inner.webauthn
    }

    #[must_use]
    pub fn google(&self) -> &GoogleOidc {
        &self.inner.google
    }

    /// The app's origin, e.g. `https://iron-oxyde.com`.
    #[must_use]
    pub fn origin(&self) -> &str {
        self.inner.csrf.origin()
    }
}

/// Adds sign-in to the app router: the Google callback route, then (around everything merged
/// so far, innermost first) the session layer, the per-IP rate limit, the CSRF check and the
/// [`AuthState`] extension.
///
/// The per-IP limit sits inside the CSRF check, so cross-site requests (refused anyway) cannot
/// use up a shared IP's limits, and outside the session layer, so a refused request never loads
/// or creates a session.
///
/// Call it after merging every route that needs a session: layers only wrap routes already on
/// the router.
pub fn install(router: Router, auth: AuthState, db: PgPool, limiter: RateLimiter) -> Router {
    let session_layer = session::layer(
        PgSessionStore::new(db),
        auth.inner.session_key.clone(),
        auth.inner.cookie_secure,
    );
    router
        .route(config_callback_path(), get(google::callback))
        .layer(session_layer)
        .layer(middleware::from_fn_with_state(limiter, rate_limit::per_ip))
        .layer(middleware::from_fn_with_state(
            auth.inner.csrf.clone(),
            csrf::guard,
        ))
        .layer(Extension(auth))
}

fn config_callback_path() -> &'static str {
    super::config::GOOGLE_CALLBACK_PATH
}

/// Starts the periodic deletion of expired sessions and ceremonies.
pub fn spawn_cleanup(db: PgPool) -> tokio::task::JoinHandle<()> {
    session::spawn_cleanup(PgSessionStore::new(db))
}

/// A `500` in the server-function error format, for missing server wiring.
fn wiring_error(what: &str) -> Response {
    dioxus::logger::tracing::error!(what, "request extension missing: check server::router");
    ServerFnError::ServerError {
        message: "Something went wrong. Please try again.".to_owned(),
        code: 500,
        details: None,
    }
    .into_response()
}

/// Everything a sign-in server function needs: the database, the sign-in services and the
/// session. Extracted from the request extensions added by [`install`] and `server::router`.
#[derive(Clone)]
pub struct AuthContext {
    pub app: AppState,
    pub auth: AuthState,
    pub session: Session,
}

impl<S: Send + Sync> FromRequestParts<S> for AuthContext {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let app = parts
            .extensions
            .get::<AppState>()
            .cloned()
            .ok_or_else(|| wiring_error("AppState"))?;
        let auth = parts
            .extensions
            .get::<AuthState>()
            .cloned()
            .ok_or_else(|| wiring_error("AuthState"))?;
        let session = parts
            .extensions
            .get::<Session>()
            .cloned()
            .ok_or_else(|| wiring_error("Session"))?;
        Ok(Self { app, auth, session })
    }
}

impl AuthContext {
    #[must_use]
    pub fn db(&self) -> &PgPool {
        &self.app.db
    }

    /// The signed-in user, or `None`.
    ///
    /// Enforces the absolute timeout (an expired session is deleted) and pushes back the idle
    /// timeout at most every [`session::TOUCH_INTERVAL`]. The idle timeout itself is enforced by
    /// the store, which never loads an expired session.
    pub async fn current_user(&self) -> Result<Option<UserId>, AuthError> {
        let Some(user) = self.session.get::<UserId>(keys::USER_ID).await? else {
            return Ok(None);
        };
        let now = session::now_unix();
        let signed_in_at = self.session.get::<i64>(keys::SIGNED_IN_AT).await?;
        if signed_in_at.is_none_or(|at| session::absolute_expired(at, now)) {
            self.session.flush().await?;
            return Ok(None);
        }
        let last_seen = self.session.get::<i64>(keys::LAST_SEEN_AT).await?;
        if session::needs_touch(last_seen, now) {
            self.session.insert(keys::LAST_SEEN_AT, now).await?;
        }
        Ok(Some(user))
    }

    /// The signed-in user, or [`AuthError::Unauthenticated`].
    pub async fn require_user(&self) -> Result<UserId, AuthError> {
        self.current_user().await?.ok_or(AuthError::Unauthenticated)
    }

    /// Signs `user` in on this session: a new session id (the old one is deleted, so an id
    /// planted before sign-in is worthless), no data carried over, and the signed-in expiry.
    pub async fn sign_in(&self, user: UserId) -> Result<(), AuthError> {
        self.session.cycle_id().await?;
        self.session.clear().await;
        let now = session::now_unix();
        self.session.insert(keys::USER_ID, user).await?;
        self.session.insert(keys::SIGNED_IN_AT, now).await?;
        self.session.insert(keys::LAST_SEEN_AT, now).await?;
        self.session.set_expiry(Some(session::signed_in_expiry()));
        Ok(())
    }

    /// Fails with [`AuthError::ReauthenticationRequired`] unless this session signed in within
    /// [`session::STEP_UP_WINDOW`]: the step-up of adding a sign-in method and of deleting the
    /// account (#22).
    ///
    /// The time is the session's `SIGNED_IN_AT`, which only [`AuthContext::sign_in`] writes, when a
    /// passkey sign-in, a Google sign-in or a sign-up has just verified a credential. Every
    /// sign-in starts a new session, so that credential belonged to the account before the
    /// session started. Adding a passkey or linking Google never signs in again, so a credential
    /// added in this session never refreshes it; and since adding one needs this same step-up, a
    /// stale session (a stolen cookie, a forgotten tab) cannot add its own credential to sign in
    /// afresh with it.
    pub async fn require_recent_sign_in(&self) -> Result<(), AuthError> {
        let signed_in_at = self.session.get::<i64>(keys::SIGNED_IN_AT).await?;
        if session::signed_in_recently(signed_in_at, session::now_unix()) {
            Ok(())
        } else {
            Err(AuthError::ReauthenticationRequired)
        }
    }

    /// Signs out: deletes the session row and clears the cookie.
    pub async fn sign_out(&self) -> Result<(), AuthError> {
        Ok(self.session.flush().await?)
    }

    /// Signs `user` out on every device: this session first (row and cookie), then every other
    /// session of theirs.
    pub async fn sign_out_everywhere(&self, user: UserId) -> Result<(), AuthError> {
        self.session.flush().await?;
        PgSessionStore::new(self.db().clone())
            .delete_all_for_user(user)
            .await?;
        Ok(())
    }
}

/// The signed-in user, for server functions and handlers. Rejects the request with `401`
/// (a `ServerFnError::ServerError { code: 401, .. }` body) when there is no valid session.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    user_id: UserId,
}

impl AuthUser {
    /// The signed-in user's id, taken from the server-side session.
    #[must_use]
    pub fn user_id(&self) -> UserId {
        self.user_id
    }

    /// The same id as the repository's owner key: pass it to every `server::db` call.
    #[must_use]
    pub fn owner(&self) -> super::db::ids::UserId {
        self.user_id.into()
    }

    /// An authenticated user without a request, for tests of code that takes an `AuthUser`.
    #[cfg(test)]
    pub(crate) fn for_tests(user_id: UserId) -> Self {
        Self { user_id }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for AuthUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let ctx = AuthContext::from_request_parts(parts, state).await?;
        let user_id = match ctx.require_user().await {
            Ok(user_id) => user_id,
            Err(error) => return Err(ServerFnError::from(error).into_response()),
        };
        if !expected_user_matches(&parts.headers, user_id) {
            let refused =
                crate::server::api::ApiError::conflict(crate::auth::types::ACCOUNT_CHANGED_MESSAGE);
            return Err(ServerFnError::from(refused).into_response());
        }
        Ok(Self { user_id })
    }
}

/// Whether the request's [`EXPECTED_USER_HEADER`] (if any) names `user`. A header that is not
/// one valid id, or several headers, never match: the outbox always sends exactly one.
///
/// [`EXPECTED_USER_HEADER`]: crate::auth::types::EXPECTED_USER_HEADER
fn expected_user_matches(headers: &dioxus::server::axum::http::HeaderMap, user: UserId) -> bool {
    let mut values = headers
        .get_all(crate::auth::types::EXPECTED_USER_HEADER)
        .iter();
    let Some(value) = values.next() else {
        return true;
    };
    values.next().is_none()
        && value
            .to_str()
            .ok()
            .and_then(|text| uuid::Uuid::parse_str(text.trim()).ok())
            .is_some_and(|expected| expected == user.as_uuid())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A WebAuthn instance for `http://localhost:8080`, as in local development.
    pub(crate) fn webauthn() -> Webauthn {
        let origin = url::Url::parse("http://localhost:8080").unwrap();
        WebauthnBuilder::new("localhost", &origin)
            .unwrap()
            .build()
            .unwrap()
    }

    #[test]
    fn the_expected_user_header_must_name_the_session_user_when_present() {
        use crate::auth::types::EXPECTED_USER_HEADER;
        use dioxus::server::axum::http::{HeaderMap, HeaderValue};
        let user = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let with = |values: &[&str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append(EXPECTED_USER_HEADER, HeaderValue::from_str(value).unwrap());
            }
            headers
        };
        let mine = user.as_uuid().to_string();
        let other = uuid::Uuid::from_u128(2).to_string();
        assert!(expected_user_matches(&HeaderMap::new(), user));
        assert!(expected_user_matches(&with(&[&mine]), user));
        assert!(!expected_user_matches(&with(&[&other]), user));
        assert!(!expected_user_matches(&with(&["not-a-uuid"]), user));
        assert!(!expected_user_matches(&with(&[&mine, &other]), user));
    }

    #[test]
    fn webauthn_accepts_localhost_for_local_development() {
        let _ = webauthn();
    }

    #[test]
    fn webauthn_accepts_the_production_domain() {
        let origin = url::Url::parse("https://iron-oxyde.com").unwrap();
        WebauthnBuilder::new("iron-oxyde.com", &origin)
            .unwrap()
            .build()
            .unwrap();
    }

    #[test]
    fn webauthn_accepts_the_app_subdomain_with_the_parent_rp_id() {
        let origin = url::Url::parse("https://app.iron-oxyde.com").unwrap();
        WebauthnBuilder::new("iron-oxyde.com", &origin)
            .unwrap()
            .build()
            .unwrap();
    }

    #[test]
    fn webauthn_rejects_an_rp_id_foreign_to_the_origin() {
        let origin = url::Url::parse("https://iron-oxyde.com").unwrap();
        assert!(WebauthnBuilder::new("example.com", &origin).is_err());
    }
}
