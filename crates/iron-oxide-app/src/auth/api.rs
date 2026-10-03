//! Sign-in server functions (#5).
//!
//! Every one is a `POST`: they change state (or read the signed-in user, which must never be
//! cached), and the CSRF layer checks the `Origin`/`Sec-Fetch-Site` of every `POST`.
//!
//! Functions that need a signed-in user take the server-only [`AuthUser`] argument. Without a
//! valid session it rejects the call with `401`, which the client sees as
//! `ServerFnError::ServerError { code: 401, .. }` (see [`is_unauthorized`]).
//!
//! [`AuthUser`]: crate::server::auth::AuthUser

use dioxus::prelude::*;
use webauthn_rs_proto::{
    CreationChallengeResponse, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse,
};

use super::types::{GoogleIntent, Me, PasskeyId};
#[cfg(feature = "server")]
use crate::server::auth::{AuthContext, AuthUser, google, passkeys};

/// Whether a server function failed because the user is not signed in (HTTP 401). The 401 can
/// arrive as a decoded server error or as a bare HTTP status, depending on the client path.
#[must_use]
pub fn is_unauthorized(error: &ServerFnError) -> bool {
    matches!(
        error,
        ServerFnError::ServerError { code: 401, .. }
            | ServerFnError::Request(dioxus::fullstack::RequestError::Status(_, 401))
    )
}

/// The signed-in user. Fails with 401 when signed out.
#[post("/api/auth/me", ctx: AuthContext, user: AuthUser)]
pub async fn me() -> Result<Me, ServerFnError> {
    Ok(passkeys::me(&ctx, user.user_id()).await?)
}

/// Signs out: deletes the session server-side and clears the cookie. Succeeds when already
/// signed out.
#[post("/api/auth/sign-out", ctx: AuthContext)]
pub async fn sign_out() -> Result<(), ServerFnError> {
    Ok(ctx.sign_out().await?)
}

/// Signs out on every device: deletes all of the user's sessions, this one included, and clears
/// this device's cookie. It only takes access away, so it needs no further check.
#[post("/api/auth/sign-out-everywhere", ctx: AuthContext, user: AuthUser)]
pub async fn sign_out_everywhere() -> Result<(), ServerFnError> {
    Ok(ctx.sign_out_everywhere(user.user_id()).await?)
}

/// Renames the account. `display_name` is trimmed, must not be blank, at most
/// [`MAX_NAME_CHARS`](super::types::MAX_NAME_CHARS) characters and without control characters
/// (`400` otherwise). Returns the account as renamed.
#[post("/api/auth/rename", ctx: AuthContext, user: AuthUser)]
pub async fn rename_account(display_name: String) -> Result<Me, ServerFnError> {
    Ok(passkeys::rename(&ctx, user.user_id(), &display_name).await?)
}

/// Starts creating an account with a passkey. Pass it to `navigator.credentials.create()`.
///
/// `display_name` (optional, at most 64 characters) names the account in the passkey manager.
#[post("/api/auth/passkey/sign-up/begin", ctx: AuthContext)]
pub async fn passkey_sign_up_begin(
    display_name: String,
) -> Result<CreationChallengeResponse, ServerFnError> {
    Ok(passkeys::sign_up_begin(&ctx, &display_name).await?)
}

/// Finishes creating the account: creates the user, stores the passkey and signs in.
#[post("/api/auth/passkey/sign-up/finish", ctx: AuthContext)]
pub async fn passkey_sign_up_finish(
    credential: RegisterPublicKeyCredential,
) -> Result<Me, ServerFnError> {
    Ok(passkeys::sign_up_finish(&ctx, &credential).await?)
}

/// Starts a username-less passkey sign-in. Pass it to `navigator.credentials.get()`.
#[post("/api/auth/passkey/sign-in/begin", ctx: AuthContext)]
pub async fn passkey_sign_in_begin() -> Result<RequestChallengeResponse, ServerFnError> {
    Ok(passkeys::sign_in_begin(&ctx).await?)
}

/// Finishes a passkey sign-in.
#[post("/api/auth/passkey/sign-in/finish", ctx: AuthContext)]
pub async fn passkey_sign_in_finish(credential: PublicKeyCredential) -> Result<Me, ServerFnError> {
    Ok(passkeys::sign_in_finish(&ctx, &credential).await?)
}

/// Starts adding another passkey to the signed-in account.
#[post("/api/auth/passkey/add/begin", ctx: AuthContext, user: AuthUser)]
pub async fn passkey_add_begin() -> Result<CreationChallengeResponse, ServerFnError> {
    Ok(passkeys::add_begin(&ctx, user.user_id()).await?)
}

/// Finishes adding a passkey. `nickname` (optional, at most 64 characters) labels it in the list.
#[post("/api/auth/passkey/add/finish", ctx: AuthContext, user: AuthUser)]
pub async fn passkey_add_finish(
    credential: RegisterPublicKeyCredential,
    nickname: String,
) -> Result<Me, ServerFnError> {
    Ok(passkeys::add_finish(&ctx, user.user_id(), &credential, &nickname).await?)
}

/// Removes one of the signed-in user's passkeys. Refused if it is their last way to sign in.
#[post("/api/auth/passkey/remove", ctx: AuthContext, user: AuthUser)]
pub async fn passkey_remove(passkey_id: PasskeyId) -> Result<Me, ServerFnError> {
    Ok(passkeys::remove(&ctx, user.user_id(), passkey_id).await?)
}

/// Starts Sign in with Google and returns the Google URL to open. Signed in, it always links
/// Google to the current account (whatever `intent` says); signed out, an unknown Google
/// account creates a new account. `popup`: whether it opens in a popup (the callback page then closes itself) or
/// in the current window (the callback page then goes back to `/`).
#[post("/api/auth/google/begin", ctx: AuthContext)]
pub async fn google_begin(intent: GoogleIntent, popup: bool) -> Result<String, ServerFnError> {
    Ok(google::begin(&ctx, intent, popup).await?)
}

/// Unlinks Google from the signed-in account. Refused if it is their last way to sign in.
#[post("/api/auth/google/unlink", ctx: AuthContext, user: AuthUser)]
pub async fn google_unlink() -> Result<Me, ServerFnError> {
    Ok(google::unlink(&ctx, user.user_id()).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_unauthorized_matches_only_401() {
        let error = |code| ServerFnError::ServerError {
            message: "x".to_owned(),
            code,
            details: None,
        };
        assert!(is_unauthorized(&error(401)));
        assert!(!is_unauthorized(&error(403)));
        assert!(!is_unauthorized(&error(500)));
        assert!(!is_unauthorized(&ServerFnError::new("x")));
        let status = |code| {
            ServerFnError::Request(dioxus::fullstack::RequestError::Status(
                "x".to_owned(),
                code,
            ))
        };
        assert!(is_unauthorized(&status(401)));
        assert!(!is_unauthorized(&status(500)));
    }
}
