//! Passkey ceremonies (WebAuthn, through `webauthn-rs`) and the account queries around them.
//!
//! - Sign-up creates the user and registers their first passkey. The user id (UUIDv7) and the
//!   WebAuthn user handle are drawn when the ceremony starts, and the rows are only inserted once
//!   the passkey verifies.
//! - The user handle is random (UUIDv4 from the OS CSPRNG), one per user, stored in
//!   `webauthn_user_handles`, and never the user id: authenticators keep it, and a UUIDv7 would
//!   tell them when the account was created.
//! - Sign-in is discoverable (username-less): the browser picks the credential and returns the
//!   user handle, and only the passkey with that credential id belonging to the user with that
//!   handle is accepted.
//! - Every passkey is created with user verification required and as a discoverable ("resident")
//!   credential.

use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;
use webauthn_rs::prelude::{
    CreationChallengeResponse, DiscoverableAuthentication, DiscoverableKey, Passkey,
    PasskeyRegistration, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse,
};
use webauthn_rs_proto::ResidentKeyRequirement;

use super::{
    AuthContext,
    ceremony::{self, CeremonyKind},
    error::AuthError,
};
use crate::auth::types::{Me, PasskeyId, PasskeyInfo, UserId, normalize_name};

/// The account name shown in the passkey manager when the user gives none.
const DEFAULT_ACCOUNT_NAME: &str = "Iron Oxide";
/// The nickname of a user's first passkey.
const FIRST_PASSKEY_NICKNAME: &str = "First passkey";
/// A user may register at most this many passkeys.
pub const MAX_PASSKEYS_PER_USER: i64 = 20;

/// What the sign-up ceremony remembers between begin and finish.
#[derive(serde::Serialize, serde::Deserialize)]
struct SignUpState {
    user_id: Uuid,
    user_handle: Uuid,
    display_name: Option<String>,
    registration: PasskeyRegistration,
}

/// Makes the created credential discoverable, required for username-less sign-in.
/// `start_passkey_registration` leaves `residentKey` unset (non-discoverable is allowed); the
/// registration check itself does not depend on it.
fn require_discoverable(mut ccr: CreationChallengeResponse) -> CreationChallengeResponse {
    if let Some(selection) = ccr.public_key.authenticator_selection.as_mut() {
        selection.resident_key = Some(ResidentKeyRequirement::Required);
        selection.require_resident_key = true;
    }
    ccr
}

/// Rejects a credential the browser says is not discoverable. `credProps` is unsigned, so this
/// only catches honest clients; a lying one only locks itself out.
fn check_discoverable(credential: &RegisterPublicKeyCredential) -> Result<(), AuthError> {
    match credential
        .extensions
        .cred_props
        .as_ref()
        .and_then(|props| props.rk)
    {
        Some(false) => Err(AuthError::NotDiscoverable),
        Some(true) | None => Ok(()),
    }
}

/// The counter and backup flags of a passkey, read from its serialized form (the fields are
/// private in `webauthn-rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PasskeyFlags {
    sign_count: i64,
    backup_eligible: bool,
    backup_state: bool,
}

fn passkey_flags(passkey: &serde_json::Value) -> PasskeyFlags {
    let cred = &passkey["cred"];
    PasskeyFlags {
        sign_count: cred["counter"].as_i64().unwrap_or(0),
        backup_eligible: cred["backup_eligible"].as_bool().unwrap_or(false),
        backup_state: cred["backup_state"].as_bool().unwrap_or(false),
    }
}

fn nickname_or(raw: &str, default: impl FnOnce() -> String) -> Result<String, AuthError> {
    normalize_name(raw)
        .map(|name| name.unwrap_or_else(default))
        .map_err(|reason| AuthError::Invalid(format!("The name {reason}.")))
}

/// Starts sign-up. The account is only created by [`sign_up_finish`].
pub async fn sign_up_begin(
    ctx: &AuthContext,
    display_name: &str,
) -> Result<CreationChallengeResponse, AuthError> {
    let display_name = normalize_name(display_name)
        .map_err(|reason| AuthError::Invalid(format!("The name {reason}.")))?;
    // The domain generator keeps ids in order within the process (#67).
    let user_id = iron_oxide_domain::UserId::new_v7().as_uuid();
    let user_handle = new_user_handle();
    let label = display_name.as_deref().unwrap_or(DEFAULT_ACCOUNT_NAME);
    let (ccr, registration) =
        ctx.auth
            .webauthn()
            .start_passkey_registration(user_handle, label, label, None)?;
    let state = SignUpState {
        user_id,
        user_handle,
        display_name,
        registration,
    };
    ceremony::start(
        ctx.db(),
        &ctx.session,
        CeremonyKind::PasskeySignUp,
        None,
        &state,
    )
    .await?;
    Ok(require_discoverable(ccr))
}

/// Finishes sign-up: verifies the new passkey, creates the user with it, and signs in. The
/// ceremony is taken in the same transaction ([`ceremony::complete`]): a `503` leaves it usable.
pub async fn sign_up_finish(
    ctx: &AuthContext,
    credential: &RegisterPublicKeyCredential,
) -> Result<Me, AuthError> {
    let me = ceremony::complete(
        ctx.db(),
        &ctx.session,
        CeremonyKind::PasskeySignUp,
        None,
        async |tx, state: SignUpState| {
            check_discoverable(credential)?;
            let passkey = ctx
                .auth
                .webauthn()
                .finish_passkey_registration(credential, &state.registration)?;
            let user = UserId::from_uuid(state.user_id);
            sqlx::query!(
                "INSERT INTO users (id, display_name) VALUES ($1, $2)",
                user.as_uuid(),
                state.display_name,
            )
            .execute(&mut **tx)
            .await?;
            sqlx::query!(
                "INSERT INTO webauthn_user_handles (user_id, user_handle) VALUES ($1, $2)",
                user.as_uuid(),
                state.user_handle,
            )
            .execute(&mut **tx)
            .await?;
            insert_passkey(tx, user, &passkey, FIRST_PASSKEY_NICKNAME).await?;
            account(tx, user).await
        },
    )
    .await?;
    ctx.sign_in(me.user_id).await?;
    Ok(me)
}

/// Starts a username-less sign-in with a modal passkey prompt.
pub async fn sign_in_begin(ctx: &AuthContext) -> Result<RequestChallengeResponse, AuthError> {
    let (mut rcr, authentication) = ctx.auth.webauthn().start_discoverable_authentication()?;
    // webauthn-rs sets conditional mediation (autofill). The button opens the modal prompt.
    rcr.mediation = None;
    ceremony::start(
        ctx.db(),
        &ctx.session,
        CeremonyKind::PasskeySignIn,
        None,
        &authentication,
    )
    .await?;
    Ok(rcr)
}

/// Finishes a passkey sign-in: verifies the assertion against the stored passkey (signature,
/// challenge, origin, RP ID, user verification, counter), records its new counter and backup
/// state, and signs in. The ceremony is taken in the same transaction
/// ([`ceremony::complete`]): a `503` leaves it usable.
pub async fn sign_in_finish(
    ctx: &AuthContext,
    credential: &PublicKeyCredential,
) -> Result<Me, AuthError> {
    let me = ceremony::complete(
        ctx.db(),
        &ctx.session,
        CeremonyKind::PasskeySignIn,
        None,
        async |tx, authentication: DiscoverableAuthentication| {
            verify_sign_in(ctx, tx, credential, authentication).await
        },
    )
    .await?;
    ctx.sign_in(me.user_id).await?;
    Ok(me)
}

/// The work of [`sign_in_finish`], in its transaction.
async fn verify_sign_in(
    ctx: &AuthContext,
    tx: &mut Transaction<'_, Postgres>,
    credential: &PublicKeyCredential,
    authentication: DiscoverableAuthentication,
) -> Result<Me, AuthError> {
    let webauthn = ctx.auth.webauthn();
    let (user_handle, credential_id) = webauthn.identify_discoverable_authentication(credential)?;

    // Lock the passkey row: concurrent sign-ins with the same credential see each other's
    // counter update.
    let row = sqlx::query!(
        "SELECT p.id, p.user_id, p.passkey
         FROM passkeys p JOIN webauthn_user_handles h ON h.user_id = p.user_id
         WHERE p.credential_id = $1 AND h.user_handle = $2
         FOR UPDATE OF p",
        credential_id,
        user_handle,
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AuthError::UnknownPasskey)?;

    let mut passkey: Passkey = serde_json::from_value(row.passkey)?;
    let result = webauthn.finish_discoverable_authentication(
        credential,
        authentication,
        &[DiscoverableKey::from(&passkey)],
    )?;
    passkey.update_credential(&result);
    let json = serde_json::to_value(&passkey)?;
    let flags = passkey_flags(&json);
    sqlx::query!(
        "UPDATE passkeys
         SET passkey = $2, sign_count = $3, backup_eligible = $4, backup_state = $5,
             last_used_at = now()
         WHERE id = $1",
        row.id,
        json,
        flags.sign_count,
        flags.backup_eligible,
        flags.backup_state,
    )
    .execute(&mut **tx)
    .await?;
    account(tx, UserId::from_uuid(row.user_id)).await
}

/// Starts adding a passkey to the signed-in user's account. Their existing passkeys are
/// excluded, so the same authenticator is not registered twice.
pub async fn add_begin(
    ctx: &AuthContext,
    user: UserId,
) -> Result<CreationChallengeResponse, AuthError> {
    // A new sign-in method needs a recent sign-in (#22): else a stale session could add its own
    // passkey and sign in afresh with it.
    ctx.require_recent_sign_in().await?;
    let existing = user_passkeys(ctx.db(), user).await?;
    if i64::try_from(existing.len()).unwrap_or(i64::MAX) >= MAX_PASSKEYS_PER_USER {
        return Err(AuthError::Invalid(format!(
            "You can have at most {MAX_PASSKEYS_PER_USER} passkeys."
        )));
    }
    let display_name = sqlx::query_scalar!(
        "SELECT display_name FROM users WHERE id = $1",
        user.as_uuid()
    )
    .fetch_optional(ctx.db())
    .await?
    .ok_or(AuthError::Unauthenticated)?;
    let label = display_name.as_deref().unwrap_or(DEFAULT_ACCOUNT_NAME);
    let exclude = existing.iter().map(|p| p.cred_id().clone()).collect();
    let user_handle = user_handle(ctx.db(), user).await?;
    let (ccr, registration) =
        ctx.auth
            .webauthn()
            .start_passkey_registration(user_handle, label, label, Some(exclude))?;
    ceremony::start(
        ctx.db(),
        &ctx.session,
        CeremonyKind::PasskeyAdd,
        Some(user),
        &registration,
    )
    .await?;
    Ok(require_discoverable(ccr))
}

/// Finishes adding a passkey to the signed-in user's account. The ceremony is taken in the same
/// transaction ([`ceremony::complete`]): a `503` leaves it usable.
pub async fn add_finish(
    ctx: &AuthContext,
    user: UserId,
    credential: &RegisterPublicKeyCredential,
    nickname: &str,
) -> Result<Me, AuthError> {
    ceremony::complete(
        ctx.db(),
        &ctx.session,
        CeremonyKind::PasskeyAdd,
        Some(user),
        async |tx, registration: PasskeyRegistration| {
            // Checked again: the sign-in may have aged past the window since `add_begin`.
            ctx.require_recent_sign_in().await?;
            check_discoverable(credential)?;
            let passkey = ctx
                .auth
                .webauthn()
                .finish_passkey_registration(credential, &registration)?;
            let count = lock_user_and_count_passkeys(tx, user).await?;
            if count >= MAX_PASSKEYS_PER_USER {
                return Err(AuthError::Invalid(format!(
                    "You can have at most {MAX_PASSKEYS_PER_USER} passkeys."
                )));
            }
            let nickname =
                nickname_or(nickname, || format!("Passkey {}", count.saturating_add(1)))?;
            insert_passkey(tx, user, &passkey, &nickname).await?;
            account(tx, user).await
        },
    )
    .await
}

/// A new random WebAuthn user handle (UUIDv4: 122 bits from the OS CSPRNG).
fn new_user_handle() -> Uuid {
    Uuid::new_v4()
}

/// The user's WebAuthn user handle, created on first use (a Google-only user adding a passkey).
async fn user_handle(pool: &PgPool, user: UserId) -> Result<Uuid, AuthError> {
    sqlx::query!(
        "INSERT INTO webauthn_user_handles (user_id, user_handle) VALUES ($1, $2)
         ON CONFLICT (user_id) DO NOTHING",
        user.as_uuid(),
        new_user_handle(),
    )
    .execute(pool)
    .await?;
    Ok(sqlx::query_scalar!(
        "SELECT user_handle FROM webauthn_user_handles WHERE user_id = $1",
        user.as_uuid()
    )
    .fetch_one(pool)
    .await?)
}

/// Removes one of the user's passkeys, unless it is their last way to sign in.
pub async fn remove(ctx: &AuthContext, user: UserId, passkey: PasskeyId) -> Result<Me, AuthError> {
    let mut tx = ctx.db().begin().await?;
    let methods = lock_user_and_count_methods(&mut tx, user).await?;
    let deleted = sqlx::query!(
        "DELETE FROM passkeys WHERE id = $1 AND user_id = $2",
        passkey.as_uuid(),
        user.as_uuid(),
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(AuthError::NotFound);
    }
    if methods <= 1 {
        // Rolled back when `tx` drops.
        return Err(AuthError::LastSignInMethod);
    }
    tx.commit().await?;
    me(ctx, user).await
}

/// Locks the user's row (serializing changes to their sign-in methods) and counts them.
/// Fails with [`AuthError::Unauthenticated`] if the user no longer exists.
pub(super) async fn lock_user_and_count_methods(
    tx: &mut Transaction<'_, Postgres>,
    user: UserId,
) -> Result<i64, AuthError> {
    let passkeys = lock_user_and_count_passkeys(tx, user).await?;
    let identities = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM oauth_identities WHERE user_id = $1"#,
        user.as_uuid()
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(passkeys.saturating_add(identities))
}

async fn lock_user_and_count_passkeys(
    tx: &mut Transaction<'_, Postgres>,
    user: UserId,
) -> Result<i64, AuthError> {
    sqlx::query_scalar!(
        "SELECT id FROM users WHERE id = $1 FOR UPDATE",
        user.as_uuid()
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AuthError::Unauthenticated)?;
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM passkeys WHERE user_id = $1"#,
        user.as_uuid()
    )
    .fetch_one(&mut **tx)
    .await?)
}

async fn insert_passkey(
    tx: &mut Transaction<'_, Postgres>,
    user: UserId,
    passkey: &Passkey,
    nickname: &str,
) -> Result<(), AuthError> {
    let json = serde_json::to_value(passkey)?;
    let flags = passkey_flags(&json);
    let credential_id: &[u8] = passkey.cred_id().as_ref();
    sqlx::query!(
        "INSERT INTO passkeys
             (user_id, credential_id, passkey, sign_count, backup_eligible, backup_state, nickname)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        user.as_uuid(),
        credential_id,
        json,
        flags.sign_count,
        flags.backup_eligible,
        flags.backup_state,
        nickname,
    )
    .execute(&mut **tx)
    .await
    .map_err(|error| match &error {
        sqlx::Error::Database(db) if db.constraint() == Some("passkeys_credential_id_key") => {
            AuthError::PasskeyAlreadyRegistered
        }
        _ => AuthError::Database(error),
    })?;
    Ok(())
}

async fn user_passkeys(pool: &PgPool, user: UserId) -> Result<Vec<Passkey>, AuthError> {
    sqlx::query_scalar!(
        "SELECT passkey FROM passkeys WHERE user_id = $1",
        user.as_uuid()
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|json| serde_json::from_value(json).map_err(AuthError::from))
    .collect()
}

fn rfc3339(time: OffsetDateTime) -> String {
    time.format(&Rfc3339).unwrap_or_else(|_| time.to_string())
}

/// The signed-in user's account: name, passkeys and linked Google account.
pub async fn me(ctx: &AuthContext, user: UserId) -> Result<Me, AuthError> {
    account(&mut *ctx.db().acquire().await?, user).await
}

/// [`me`], read on `conn` (inside a transaction: what it is about to commit).
async fn account(conn: &mut PgConnection, user: UserId) -> Result<Me, AuthError> {
    let display_name = sqlx::query_scalar!(
        "SELECT display_name FROM users WHERE id = $1",
        user.as_uuid()
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AuthError::Unauthenticated)?;
    let passkeys = sqlx::query!(
        "SELECT id, nickname, created_at, last_used_at, backup_state FROM passkeys
         WHERE user_id = $1 ORDER BY created_at, id",
        user.as_uuid()
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| PasskeyInfo {
        id: PasskeyId::from_uuid(row.id),
        nickname: row.nickname,
        created_at: rfc3339(row.created_at),
        last_used_at: row.last_used_at.map(rfc3339),
        backed_up: row.backup_state,
    })
    .collect();
    let google_linked = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM oauth_identities WHERE user_id = $1 AND provider = 'google')
           AS "linked!""#,
        user.as_uuid()
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(Me {
        user_id: user,
        display_name,
        passkeys,
        google_linked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use webauthn_rs_proto::{AuthenticatorSelectionCriteria, CredProps, UserVerificationPolicy};

    fn ccr() -> CreationChallengeResponse {
        let webauthn = super::super::tests::webauthn();
        let (ccr, _) = webauthn
            .start_passkey_registration(Uuid::now_v7(), "a", "a", None)
            .unwrap();
        ccr
    }

    #[test]
    fn registration_requires_a_discoverable_credential_and_user_verification() {
        let ccr = require_discoverable(ccr());
        let selection = ccr.public_key.authenticator_selection.unwrap();
        assert_eq!(
            selection.resident_key,
            Some(ResidentKeyRequirement::Required)
        );
        assert!(selection.require_resident_key);
        assert_eq!(
            selection.user_verification,
            UserVerificationPolicy::Required
        );
    }

    #[test]
    fn require_discoverable_leaves_a_missing_selection_alone() {
        let mut ccr = ccr();
        ccr.public_key.authenticator_selection = None;
        assert!(
            require_discoverable(ccr)
                .public_key
                .authenticator_selection
                .is_none()
        );
        let _unused: Option<AuthenticatorSelectionCriteria> = None;
    }

    fn registration_with_rk(rk: Option<bool>) -> RegisterPublicKeyCredential {
        let mut credential: RegisterPublicKeyCredential =
            serde_json::from_value(serde_json::json!({
                "id": "AA",
                "rawId": "AA",
                "response": { "attestationObject": "AA", "clientDataJSON": "AA" },
                "type": "public-key"
            }))
            .unwrap();
        credential.extensions.cred_props = rk.map(|rk| CredProps { rk: Some(rk) });
        credential
    }

    #[test]
    fn a_credential_reported_non_discoverable_is_rejected() {
        assert!(matches!(
            check_discoverable(&registration_with_rk(Some(false))),
            Err(AuthError::NotDiscoverable)
        ));
        assert!(check_discoverable(&registration_with_rk(Some(true))).is_ok());
        assert!(check_discoverable(&registration_with_rk(None)).is_ok());
    }

    #[test]
    fn passkey_flags_are_read_from_the_serialized_passkey() {
        let json = serde_json::json!({
            "cred": { "counter": 7, "backup_eligible": true, "backup_state": false }
        });
        assert_eq!(
            passkey_flags(&json),
            PasskeyFlags {
                sign_count: 7,
                backup_eligible: true,
                backup_state: false
            }
        );
        assert_eq!(
            passkey_flags(&serde_json::json!({})),
            PasskeyFlags {
                sign_count: 0,
                backup_eligible: false,
                backup_state: false
            }
        );
    }

    #[test]
    fn nickname_defaults_and_validation() {
        assert_eq!(nickname_or("  Phone ", || "x".to_owned()).unwrap(), "Phone");
        assert_eq!(
            nickname_or("", || "Passkey 2".to_owned()).unwrap(),
            "Passkey 2"
        );
        assert!(matches!(
            nickname_or(&"a".repeat(65), || "x".to_owned()),
            Err(AuthError::Invalid(_))
        ));
    }

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(OffsetDateTime::UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }
}
