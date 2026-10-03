//! One-time ceremony state: the WebAuthn challenge state, or Google's state, nonce and PKCE
//! verifier.
//!
//! The state itself is stored in `auth_ceremonies`; the session only holds the row's random id,
//! under one key per kind. Taking a ceremony deletes the row in the same statement that reads it
//! (`DELETE ... RETURNING`), so it can be used once, even by two concurrent requests with the
//! same cookie. Session data alone could not guarantee that: each request works on its own copy,
//! saved after the handler returns.
//!
//! A passkey ceremony is completed with [`complete`]: the delete runs in the same transaction as
//! the work it allows, so a retryable failure (`503`: no pooled connection in time, a statement
//! deadline, a serialization failure) rolls the delete back too, and replaying the same request
//! can succeed. Any other failure consumes it.

use std::time::Duration;

use serde::{Serialize, de::DeserializeOwned};
use sqlx::{Connection as _, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tower_sessions::Session;
use uuid::Uuid;

use super::{
    error::AuthError,
    session::{self, keys},
};
use crate::auth::types::UserId;

/// What a ceremony is for. Stored as `auth_ceremony_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "auth_ceremony_kind", rename_all = "snake_case")]
pub enum CeremonyKind {
    PasskeySignUp,
    PasskeySignIn,
    PasskeyAdd,
    GoogleSignIn,
    GoogleLink,
}

impl CeremonyKind {
    /// The session key holding this kind's ceremony id. Both Google kinds share one: the
    /// callback cannot tell them apart, and starting one replaces the other.
    #[must_use]
    pub const fn session_key(self) -> &'static str {
        match self {
            Self::PasskeySignUp => "auth.ceremony.passkey_sign_up",
            Self::PasskeySignIn => "auth.ceremony.passkey_sign_in",
            Self::PasskeyAdd => "auth.ceremony.passkey_add",
            Self::GoogleSignIn | Self::GoogleLink => "auth.ceremony.google",
        }
    }

    /// How long the user has to complete it.
    #[must_use]
    pub const fn ttl(self) -> Duration {
        match self {
            // The WebAuthn prompt times out after 5 minutes (webauthn-rs default).
            Self::PasskeySignUp | Self::PasskeySignIn | Self::PasskeyAdd => {
                Duration::from_secs(5 * 60)
            }
            // Google's consent screen can take longer (account chooser, 2FA).
            Self::GoogleSignIn | Self::GoogleLink => Duration::from_secs(10 * 60),
        }
    }

    /// Whether it is started by, and bound to, a signed-in user.
    #[must_use]
    pub const fn needs_user(self) -> bool {
        matches!(self, Self::PasskeyAdd | Self::GoogleLink)
    }
}

/// Stores a new ceremony of `kind` and points the session at it. The ceremony it replaces (same
/// session key) is deleted, so repeated begins on one cookie do not pile up rows.
///
/// `user` must be `Some` exactly for the kinds that [`CeremonyKind::needs_user`].
pub async fn start<T: Serialize>(
    pool: &PgPool,
    session: &Session,
    kind: CeremonyKind,
    user: Option<UserId>,
    state: &T,
) -> Result<(), AuthError> {
    if kind.needs_user() != user.is_some() {
        return Err(AuthError::Internal(format!(
            "ceremony {kind:?} started with the wrong user binding"
        )));
    }
    if let Some(replaced) = session.get::<Uuid>(kind.session_key()).await? {
        sqlx::query!("DELETE FROM auth_ceremonies WHERE id = $1", replaced)
            .execute(pool)
            .await?;
    }
    let id = Uuid::now_v7();
    let expires_at = OffsetDateTime::now_utc() + kind.ttl();
    sqlx::query!(
        "INSERT INTO auth_ceremonies (id, kind, user_id, state, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
        id,
        kind as CeremonyKind,
        user.map(|user| user.as_uuid()),
        serde_json::to_value(state)?,
        expires_at,
    )
    .execute(pool)
    .await?;

    session.insert(kind.session_key(), id).await?;
    // A signed-out visitor only needs the session for the ceremony: keep it short-lived.
    if session.get::<UserId>(keys::USER_ID).await?.is_none() {
        session.set_expiry(Some(session::anonymous_expiry()));
    }
    Ok(())
}

/// A ceremony taken out of the database.
pub struct Taken {
    pub kind: CeremonyKind,
    /// The user who started it (only for [`CeremonyKind::needs_user`] kinds).
    pub user: Option<UserId>,
    state: serde_json::Value,
}

impl Taken {
    /// The stored state.
    pub fn state<T: DeserializeOwned>(self) -> Result<T, AuthError> {
        Ok(serde_json::from_value(self.state)?)
    }
}

/// Completes the session's ceremony of `kind` in one transaction: takes (deletes) it, runs
/// `finish` with its state, and commits both together.
///
/// - Success: committed, and the ceremony is removed from the session.
/// - A retryable failure ([`AuthError::is_retryable`], `503`), from the take, `finish` or the
///   commit: everything is rolled back, the ceremony included, and the session is left as it was.
///   Replaying the same request can succeed.
/// - Any other failure (verification, an unknown passkey, a limit): `finish`'s work is rolled
///   back (a savepoint) but the ceremony stays consumed, as a one-time ceremony must.
///
/// Fails with [`AuthError::Ceremony`] if the session has none, it was already used, it expired,
/// or it is bound to a different user than `user` (see [`CeremonyKind::needs_user`]).
pub async fn complete<T, R>(
    pool: &PgPool,
    session: &Session,
    kind: CeremonyKind,
    user: Option<UserId>,
    finish: impl AsyncFnOnce(&mut Transaction<'_, Postgres>, T) -> Result<R, AuthError>,
) -> Result<R, AuthError>
where
    T: DeserializeOwned,
{
    let Some(id) = session.get::<Uuid>(kind.session_key()).await? else {
        shorten_if_signed_out(session).await?;
        return Err(AuthError::Ceremony("none in this session"));
    };
    let mut tx = pool.begin().await?;
    let Some(row) = sqlx::query!(
        r#"DELETE FROM auth_ceremonies WHERE id = $1 AND kind = $2
           RETURNING kind AS "kind: CeremonyKind", user_id, state, expires_at > now() AS "live!""#,
        id,
        kind as CeremonyKind,
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        forget(session, kind).await?;
        return Err(AuthError::Ceremony("unknown or already used"));
    };
    let state = match checked(row.kind, row.user_id, row.state, row.live, user)
        .and_then(Taken::state::<T>)
    {
        Ok(state) => state,
        Err(error) => return consumed(tx, session, kind, error).await,
    };

    let mut work = tx.begin().await?;
    match finish(&mut work, state).await {
        Ok(result) => {
            work.commit().await?;
            tx.commit().await?;
            forget(session, kind).await?;
            Ok(result)
        }
        Err(error) if error.is_retryable() => {
            // Explicitly, rather than on drop: the connection goes back to the pool clean.
            let _ = work.rollback().await;
            let _ = tx.rollback().await;
            Err(error)
        }
        Err(error) => {
            work.rollback().await?;
            consumed(tx, session, kind, error).await
        }
    }
}

/// Commits the take alone (the ceremony is used up) and returns `error`.
async fn consumed<R>(
    tx: Transaction<'_, Postgres>,
    session: &Session,
    kind: CeremonyKind,
    error: AuthError,
) -> Result<R, AuthError> {
    tx.commit().await?;
    forget(session, kind).await?;
    Err(error)
}

/// Removes the ceremony's id from the session.
async fn forget(session: &Session, kind: CeremonyKind) -> Result<(), AuthError> {
    session.remove::<Uuid>(kind.session_key()).await?;
    shorten_if_signed_out(session).await
}

const GOOGLE_KINDS: [CeremonyKind; 2] = [CeremonyKind::GoogleSignIn, CeremonyKind::GoogleLink];

/// Takes (and deletes) the session's Google ceremony, whichever of sign-in or link it is, but
/// only if `state` is the one it sent to Google. A callback with another `state` (forged, or
/// from someone else's flow) leaves the ceremony untouched, so it can neither use nor cancel it.
/// `current` is the signed-in user, if any: a link ceremony must have been started by them.
pub async fn take_google(
    pool: &PgPool,
    session: &Session,
    current: Option<UserId>,
    state: &str,
) -> Result<Taken, AuthError> {
    let key = CeremonyKind::GoogleSignIn.session_key();
    let id = session
        .get::<Uuid>(key)
        .await?
        .ok_or(AuthError::Ceremony("none in this session"))?;
    let row = sqlx::query!(
        r#"DELETE FROM auth_ceremonies
           WHERE id = $1 AND kind = ANY($2) AND state ->> 'state' = $3
           RETURNING kind AS "kind: CeremonyKind", user_id, state, expires_at > now() AS "live!""#,
        id,
        &GOOGLE_KINDS as &[CeremonyKind],
        state,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(AuthError::Ceremony("no ceremony with this state"))?;
    session.remove::<Uuid>(key).await?;
    shorten_if_signed_out(session).await?;
    checked(row.kind, row.user_id, row.state, row.live, current)
}

/// Deletes any Google ceremony that sent `state` to Google, whichever session holds it.
pub async fn discard_google_by_state(pool: &PgPool, state: &str) -> Result<u64, AuthError> {
    Ok(sqlx::query!(
        "DELETE FROM auth_ceremonies
         WHERE kind IN ('google_sign_in', 'google_link') AND state ->> 'state' = $1",
        state,
    )
    .execute(pool)
    .await?
    .rows_affected())
}

/// Keeps a signed-out session short-lived once its ceremony is gone, even if it failed.
async fn shorten_if_signed_out(session: &Session) -> Result<(), AuthError> {
    if session.get::<UserId>(keys::USER_ID).await?.is_none() {
        session.set_expiry(Some(session::anonymous_expiry()));
    }
    Ok(())
}

/// Checks a deleted ceremony row: unexpired, and bound to `current` exactly when its kind is.
fn checked(
    kind: CeremonyKind,
    user_id: Option<Uuid>,
    state: serde_json::Value,
    live: bool,
    current: Option<UserId>,
) -> Result<Taken, AuthError> {
    if !live {
        return Err(AuthError::Ceremony("expired"));
    }
    let owner = user_id.map(UserId::from_uuid);
    let bound_ok = if kind.needs_user() {
        owner.is_some() && owner == current
    } else {
        owner.is_none()
    };
    if !bound_ok {
        return Err(AuthError::Ceremony("bound to another user"));
    }
    Ok(Taken {
        kind,
        user: owner,
        state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [CeremonyKind; 5] = [
        CeremonyKind::PasskeySignUp,
        CeremonyKind::PasskeySignIn,
        CeremonyKind::PasskeyAdd,
        CeremonyKind::GoogleSignIn,
        CeremonyKind::GoogleLink,
    ];

    #[test]
    fn session_keys_are_distinct_except_google() {
        let mut keys: Vec<&str> = ALL.iter().map(|kind| kind.session_key()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), ALL.len() - 1);
        assert!(keys.iter().all(|key| key.starts_with("auth.ceremony.")));
        assert_eq!(
            CeremonyKind::GoogleSignIn.session_key(),
            CeremonyKind::GoogleLink.session_key()
        );
    }

    #[test]
    fn ceremonies_are_short_lived() {
        for kind in ALL {
            assert!(kind.ttl() <= Duration::from_secs(10 * 60), "{kind:?}");
            assert!(kind.ttl() <= session::ANONYMOUS_TIMEOUT, "{kind:?}");
        }
    }

    #[test]
    fn only_add_and_link_are_bound_to_a_user() {
        let bound: Vec<CeremonyKind> = ALL.into_iter().filter(|k| k.needs_user()).collect();
        assert_eq!(bound, [CeremonyKind::PasskeyAdd, CeremonyKind::GoogleLink]);
    }
}
