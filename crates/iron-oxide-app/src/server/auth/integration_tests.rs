//! Sign-in integration tests: the real router and server functions against Postgres
//! (`#[ignore = "needs Postgres"]`, run by the CI integration job), a software passkey and a
//! local mock of Google. Each `sqlx::test` gets a fresh, migrated database.

use dioxus::server::axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use webauthn_rs_proto::{
    CreationChallengeResponse, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse,
};

use super::test_support::{Browser, CallError, Grant, Passkey, TestApp, query_param, sign_up};
use crate::auth::types::Me;

const SIGN_UP_BEGIN: &str = "/api/auth/passkey/sign-up/begin";
const SIGN_UP_FINISH: &str = "/api/auth/passkey/sign-up/finish";
const SIGN_IN_BEGIN: &str = "/api/auth/passkey/sign-in/begin";
const SIGN_IN_FINISH: &str = "/api/auth/passkey/sign-in/finish";
const ADD_BEGIN: &str = "/api/auth/passkey/add/begin";
const ADD_FINISH: &str = "/api/auth/passkey/add/finish";
const REMOVE: &str = "/api/auth/passkey/remove";
const ME: &str = "/api/auth/me";
const SIGN_OUT: &str = "/api/auth/sign-out";
const GOOGLE_BEGIN: &str = "/api/auth/google/begin";
const GOOGLE_UNLINK: &str = "/api/auth/google/unlink";

async fn sign_in_assertion(
    browser: &mut Browser,
    passkey: &mut Passkey,
    credential_id: &[u8],
) -> PublicKeyCredential {
    let rcr: RequestChallengeResponse = browser.call(SIGN_IN_BEGIN, json!({})).await.unwrap();
    passkey.sign_in(rcr, credential_id)
}

async fn me(browser: &mut Browser) -> Result<Me, CallError> {
    browser.call(ME, json!({})).await
}

async fn session_rows(db: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions")
        .fetch_one(db)
        .await
        .unwrap()
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn sign_up_then_sign_out_then_sign_in_with_the_passkey(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let mut passkey = Passkey::new();

    let (me1, credential_id) = sign_up(&mut browser, &mut passkey, " Jules ").await;
    assert_eq!(me1.display_name.as_deref(), Some("Jules"));
    assert_eq!(
        me1.user_id.as_uuid().get_version_num(),
        7,
        "UUIDv7 ids (#65)"
    );
    assert_eq!(me1.passkeys[0].id.as_uuid().get_version_num(), 7);
    assert_eq!(me1.passkeys.len(), 1);
    assert!(!me1.google_linked);
    assert_eq!(me(&mut browser).await.unwrap(), me1);

    let () = browser.call(SIGN_OUT, json!({})).await.unwrap();
    assert!(browser.cookie.is_none(), "sign-out clears the cookie");
    assert_eq!(
        me(&mut browser).await.unwrap_err().status,
        StatusCode::UNAUTHORIZED
    );

    let assertion = sign_in_assertion(&mut browser, &mut passkey, &credential_id).await;
    let me2: Me = browser
        .call(SIGN_IN_FINISH, json!({ "credential": assertion }))
        .await
        .unwrap();
    assert_eq!(me2.user_id, me1.user_id);
    assert!(me2.passkeys[0].last_used_at.is_some());

    // The counter moved forward and was stored.
    let count: i64 = sqlx::query_scalar("SELECT sign_count FROM passkeys")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(count >= 1, "{count}");
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn the_session_id_changes_on_sign_in(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let mut passkey = Passkey::new();

    // Sign-up begin creates an anonymous session: the id an attacker could have planted.
    let ccr: CreationChallengeResponse = browser
        .call(SIGN_UP_BEGIN, json!({ "display_name": "" }))
        .await
        .unwrap();
    let planted = browser.cookie.clone().expect("anonymous session cookie");
    let credential = passkey.register(ccr);
    let _: Me = browser
        .call(SIGN_UP_FINISH, json!({ "credential": credential }))
        .await
        .unwrap();
    let signed_in = browser.cookie.clone().expect("signed-in cookie");
    assert_ne!(planted, signed_in, "sign-in must rotate the session id");

    // The planted id is dead: it neither authenticates nor exists any more.
    let mut attacker = app.browser();
    attacker.cookie = Some(planted);
    assert_eq!(
        me(&mut attacker).await.unwrap_err().status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(session_rows(&db).await, 1);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn session_ids_are_stored_hashed(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    sign_up(&mut browser, &mut Passkey::new(), "a").await;
    let cookie = browser.cookie.clone().unwrap();
    let rows: Vec<(Vec<u8>, Value)> = sqlx::query_as("SELECT id_hash, data FROM sessions")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0.len(), 32);
    let value = cookie.split_once('=').unwrap().1;
    assert!(!String::from_utf8_lossy(&rows[0].0).contains(value));
    assert!(!rows[0].1.to_string().contains(value));
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_replayed_sign_in_is_rejected(db: PgPool) {
    let app = TestApp::new(db).await;
    let mut browser = app.browser();
    let mut passkey = Passkey::new();
    let (_, credential_id) = sign_up(&mut browser, &mut passkey, "a").await;
    let () = browser.call(SIGN_OUT, json!({})).await.unwrap();

    let assertion = sign_in_assertion(&mut browser, &mut passkey, &credential_id).await;
    let anonymous = browser.clone();
    let _: Me = browser
        .call(SIGN_IN_FINISH, json!({ "credential": assertion }))
        .await
        .unwrap();

    // Same assertion again, in the now signed-in session: the ceremony is gone.
    let error = browser
        .call::<Me>(SIGN_IN_FINISH, json!({ "credential": assertion }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);

    // Same assertion with the pre-sign-in cookie: that session was deleted on rotation.
    let mut replay = anonymous;
    let error = replay
        .call::<Me>(SIGN_IN_FINISH, json!({ "credential": assertion }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);

    // Same assertion against a fresh challenge: the signed challenge does not match.
    let mut other = app.browser();
    let _: RequestChallengeResponse = other.call(SIGN_IN_BEGIN, json!({})).await.unwrap();
    let error = other
        .call::<Me>(SIGN_IN_FINISH, json!({ "credential": assertion }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        me(&mut other).await.unwrap_err().status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn two_concurrent_finishes_of_one_ceremony_cannot_both_succeed(db: PgPool) {
    let app = TestApp::new(db).await;
    let mut browser = app.browser();
    let mut passkey = Passkey::new();
    let (_, credential_id) = sign_up(&mut browser, &mut passkey, "a").await;
    let () = browser.call(SIGN_OUT, json!({})).await.unwrap();

    let assertion = sign_in_assertion(&mut browser, &mut passkey, &credential_id).await;
    let (mut a, mut b) = (browser.clone(), browser.clone());
    let body = json!({ "credential": assertion });
    let (ra, rb) = tokio::join!(
        a.call::<Me>(SIGN_IN_FINISH, body.clone()),
        b.call::<Me>(SIGN_IN_FINISH, body.clone())
    );
    assert_eq!(usize::from(ra.is_ok()) + usize::from(rb.is_ok()), 1);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_sign_up_ceremony_is_single_use(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let mut passkey = Passkey::new();
    let ccr: CreationChallengeResponse = browser
        .call(SIGN_UP_BEGIN, json!({ "display_name": "a" }))
        .await
        .unwrap();
    let credential = passkey.register(ccr);
    let before = browser.clone();
    let _: Me = browser
        .call(SIGN_UP_FINISH, json!({ "credential": credential }))
        .await
        .unwrap();
    let mut replay = before;
    let error = replay
        .call::<Me>(SIGN_UP_FINISH, json!({ "credential": credential }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(users, 1);
}

// --- A retryable failure leaves the ceremony usable (#104) ---------------------------------

/// Makes every `operation` (`INSERT`, `UPDATE`) on `table` fail with a serialization failure
/// (`40001`, a retryable `503`) until [`heal`].
async fn fail_transiently(db: &PgPool, operation: &str, table: &str) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION test_fail_transiently() RETURNS trigger LANGUAGE plpgsql AS
         $$ BEGIN RAISE EXCEPTION 'injected' USING ERRCODE = '40001'; END $$",
    )
    .execute(db)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER test_fail_transiently BEFORE {operation} ON {table}
         FOR EACH ROW EXECUTE FUNCTION test_fail_transiently()"
    ))
    .execute(db)
    .await
    .unwrap();
}

async fn heal(db: &PgPool, table: &str) {
    sqlx::query(&format!("DROP TRIGGER test_fail_transiently ON {table}"))
        .execute(db)
        .await
        .unwrap();
}

async fn count(db: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(db)
        .await
        .unwrap()
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_503_sign_up_finish_can_be_replayed(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let mut passkey = Passkey::new();
    let ccr: CreationChallengeResponse = browser
        .call(SIGN_UP_BEGIN, json!({ "display_name": "a" }))
        .await
        .unwrap();
    let body = json!({ "credential": passkey.register(ccr) });

    fail_transiently(&db, "INSERT", "users").await;
    let error = browser
        .call::<Me>(SIGN_UP_FINISH, body.clone())
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE, "{error:?}");
    assert_eq!(count(&db, "users").await, 0);
    assert_eq!(count(&db, "auth_ceremonies").await, 1, "rolled back");

    heal(&db, "users").await;
    let me1: Me = browser.call(SIGN_UP_FINISH, body.clone()).await.unwrap();
    assert_eq!(me(&mut browser).await.unwrap(), me1);
    assert_eq!(count(&db, "auth_ceremonies").await, 0);
    // Still single use.
    assert_eq!(
        browser
            .call::<Me>(SIGN_UP_FINISH, body)
            .await
            .unwrap_err()
            .status,
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_503_sign_in_finish_can_be_replayed(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let mut passkey = Passkey::new();
    let (me1, credential_id) = sign_up(&mut browser, &mut passkey, "a").await;
    let () = browser.call(SIGN_OUT, json!({})).await.unwrap();
    let assertion = sign_in_assertion(&mut browser, &mut passkey, &credential_id).await;
    let body = json!({ "credential": assertion });

    fail_transiently(&db, "UPDATE", "passkeys").await;
    let error = browser
        .call::<Me>(SIGN_IN_FINISH, body.clone())
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE, "{error:?}");
    assert_eq!(
        me(&mut browser).await.unwrap_err().status,
        StatusCode::UNAUTHORIZED
    );

    heal(&db, "passkeys").await;
    let me2: Me = browser.call(SIGN_IN_FINISH, body).await.unwrap();
    assert_eq!(me2.user_id, me1.user_id);
    assert!(me2.passkeys[0].last_used_at.is_some());
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_503_add_passkey_finish_can_be_replayed(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    sign_up(&mut browser, &mut Passkey::new(), "a").await;
    let ccr: CreationChallengeResponse = browser.call(ADD_BEGIN, json!({})).await.unwrap();
    let body = json!({ "credential": Passkey::new().register(ccr), "nickname": "Laptop" });

    fail_transiently(&db, "INSERT", "passkeys").await;
    let error = browser
        .call::<Me>(ADD_FINISH, body.clone())
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE, "{error:?}");

    heal(&db, "passkeys").await;
    let me2: Me = browser.call(ADD_FINISH, body).await.unwrap();
    assert_eq!(me2.passkeys.len(), 2);
    assert_eq!(me2.passkeys[1].nickname, "Laptop");
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_failed_verification_still_uses_up_the_ceremony(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (mut mine, mut other) = (app.browser(), app.browser());
    let mut passkey = Passkey::new();
    let ccr: CreationChallengeResponse = mine
        .call(SIGN_UP_BEGIN, json!({ "display_name": "a" }))
        .await
        .unwrap();
    let own = passkey.register(ccr);
    let ccr: CreationChallengeResponse = other
        .call(SIGN_UP_BEGIN, json!({ "display_name": "b" }))
        .await
        .unwrap();
    let foreign = Passkey::new().register(ccr);

    // Signed over another challenge: refused, and the ceremony is gone with it.
    let error = mine
        .call::<Me>(SIGN_UP_FINISH, json!({ "credential": foreign }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    let error = mine
        .call::<Me>(SIGN_UP_FINISH, json!({ "credential": own }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert_eq!(count(&db, "users").await, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn an_expired_ceremony_is_rejected(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let mut passkey = Passkey::new();
    let ccr: CreationChallengeResponse = browser
        .call(SIGN_UP_BEGIN, json!({ "display_name": "a" }))
        .await
        .unwrap();
    sqlx::query("UPDATE auth_ceremonies SET expires_at = now() - interval '1 second'")
        .execute(&db)
        .await
        .unwrap();
    let credential = passkey.register(ccr);
    let error = browser
        .call::<Me>(SIGN_UP_FINISH, json!({ "credential": credential }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn user_verification_is_required(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let mut passkey = Passkey::with_uv(false);
    let ccr: CreationChallengeResponse = browser
        .call(SIGN_UP_BEGIN, json!({ "display_name": "a" }))
        .await
        .unwrap();
    let credential = passkey.register(ccr);
    let error = browser
        .call::<Me>(SIGN_UP_FINISH, json!({ "credential": credential }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(users, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_user_handle_pointing_at_another_account_is_rejected(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (mut alice, mut bob) = (app.browser(), app.browser());
    let mut bob_key = Passkey::new();
    let (alice_me, _) = sign_up(&mut alice, &mut Passkey::new(), "alice").await;
    let (_, bob_cred) = sign_up(&mut bob, &mut bob_key, "bob").await;
    let () = bob.call(SIGN_OUT, json!({})).await.unwrap();

    // Bob signs with his own passkey but claims Alice's user handle, or her user id.
    let alice_handle: uuid::Uuid =
        sqlx::query_scalar("SELECT user_handle FROM webauthn_user_handles WHERE user_id = $1")
            .bind(alice_me.user_id.as_uuid())
            .fetch_one(&db)
            .await
            .unwrap();
    for claimed in [alice_handle, alice_me.user_id.as_uuid()] {
        let mut assertion = sign_in_assertion(&mut bob, &mut bob_key, &bob_cred).await;
        assertion.response.user_handle = Some(claimed.as_bytes().to_vec().into());
        let error = bob
            .call::<Me>(SIGN_IN_FINISH, json!({ "credential": assertion }))
            .await
            .unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        me(&mut bob).await.unwrap_err().status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn an_expired_session_is_401(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    sign_up(&mut browser, &mut Passkey::new(), "a").await;
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'")
        .execute(&db)
        .await
        .unwrap();
    let error = me(&mut browser).await.unwrap_err();
    assert_eq!(error.status, StatusCode::UNAUTHORIZED);
    assert_eq!(error.message, "Please sign in.");
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_session_past_the_absolute_timeout_is_401_and_deleted(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    sign_up(&mut browser, &mut Passkey::new(), "a").await;
    let long_ago = super::session::now_unix() - 31 * 24 * 60 * 60;
    sqlx::query(
        "UPDATE sessions SET data = jsonb_set(data, '{auth.signed_in_at}', to_jsonb($1::bigint))",
    )
    .bind(long_ago)
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        me(&mut browser).await.unwrap_err().status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(session_rows(&db).await, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn activity_pushes_back_the_idle_expiry(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    sign_up(&mut browser, &mut Passkey::new(), "a").await;
    let two_hours_ago = super::session::now_unix() - 2 * 60 * 60;
    sqlx::query(
        "UPDATE sessions SET expires_at = now() + interval '1 hour',
         data = jsonb_set(data, '{auth.last_seen_at}', to_jsonb($1::bigint))",
    )
    .bind(two_hours_ago)
    .execute(&db)
    .await
    .unwrap();
    me(&mut browser).await.unwrap();
    let expires_in_days: f64 = sqlx::query_scalar(
        "SELECT (extract(epoch FROM expires_at - now()) / 86400)::float8 FROM sessions",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(expires_in_days > 13.9, "{expires_in_days}");
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn sign_out_deletes_the_session_server_side(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    sign_up(&mut browser, &mut Passkey::new(), "a").await;
    let stolen = browser.cookie.clone();
    assert_eq!(session_rows(&db).await, 1);
    let () = browser.call(SIGN_OUT, json!({})).await.unwrap();
    assert_eq!(session_rows(&db).await, 0);
    // A copy of the old cookie is worthless.
    let mut thief = app.browser();
    thief.cookie = stolen;
    assert_eq!(
        me(&mut thief).await.unwrap_err().status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn saving_a_deleted_session_does_not_resurrect_it(db: PgPool) {
    // A request that loaded the session before a sign-out in another tab saves it afterwards
    // (e.g. the idle-expiry touch): the store must not recreate it.
    use tower_sessions::{
        SessionStore,
        session::{Id, Record},
    };
    let store = super::session::PgSessionStore::new(db.clone());
    let mut record = Record {
        id: Id::default(),
        data: Default::default(),
        expiry_date: time::OffsetDateTime::now_utc() + time::Duration::hours(1),
    };
    store.create(&mut record).await.unwrap();
    assert!(store.load(&record.id).await.unwrap().is_some());
    store.delete(&record.id).await.unwrap();
    store.save(&record).await.unwrap();
    assert!(store.load(&record.id).await.unwrap().is_none());
    assert_eq!(session_rows(&db).await, 0);

    // An expired session is not loaded, nor extended by a save.
    let mut expired = Record {
        id: Id::default(),
        data: Default::default(),
        expiry_date: time::OffsetDateTime::now_utc() - time::Duration::seconds(1),
    };
    store.create(&mut expired).await.unwrap();
    assert!(store.load(&expired.id).await.unwrap().is_none());
    expired.expiry_date = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    store.save(&expired).await.unwrap();
    assert!(store.load(&expired.id).await.unwrap().is_none());
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn concurrent_google_finishes_of_one_ceremony_cannot_both_succeed(db: PgPool) {
    // Both callbacks carry the same session (and so the same ceremony id): only the database
    // delete makes the ceremony single-use here.
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let url = google_begin(&mut browser, "SignIn").await;
    let state = query_param(&url, "state");
    for (code, subject) in [("c1", "sub-a"), ("c2", "sub-b")] {
        app.google.grant(
            code,
            Grant::new(
                query_param(&url, "code_challenge"),
                app.google.claims(&url, subject),
            ),
        );
    }
    let (mut a, mut b) = (browser.clone(), browser.clone());
    let (pa, pb) = (callback_path("c1", &state), callback_path("c2", &state));
    let ((_, _, ba), (_, _, bb)) = tokio::join!(a.get(&pa), b.get(&pb));
    let done = [ba, bb]
        .iter()
        .filter(|body| body.contains(r#""type":"done""#))
        .count();
    assert_eq!(done, 1);
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(users, 1);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn cross_site_posts_are_refused_without_side_effects(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    sign_up(&mut browser, &mut Passkey::new(), "a").await;

    for (origin, site) in [
        (Some("https://evil.example"), Some("cross-site")),
        (Some("https://evil.example"), None),
        (None, Some("cross-site")),
        (None, Some("same-site")),
        (None, None),
        (Some("null"), None),
    ] {
        let mut attacker = browser.clone();
        attacker.origin = origin.map(str::to_owned);
        attacker.fetch_site = site.map(str::to_owned);
        let error = attacker.call::<()>(SIGN_OUT, json!({})).await.unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN, "{origin:?} {site:?}");
        assert_eq!(session_rows(&db).await, 1, "{origin:?} {site:?}");
    }
    me(&mut browser).await.unwrap();
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn add_list_and_remove_passkeys_but_never_the_last_way_in(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let mut phone = Passkey::new();
    let (me1, _) = sign_up(&mut browser, &mut phone, "a").await;

    // The only passkey cannot be removed.
    let error = browser
        .call::<Me>(REMOVE, json!({ "passkey_id": me1.passkeys[0].id }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::CONFLICT);

    // Add a second one.
    let mut laptop = Passkey::new();
    let ccr: CreationChallengeResponse = browser.call(ADD_BEGIN, json!({})).await.unwrap();
    assert_eq!(
        ccr.public_key.exclude_credentials.as_ref().map(Vec::len),
        Some(1),
        "the existing passkey is excluded"
    );
    let credential: RegisterPublicKeyCredential = laptop.register(ccr);
    let me2: Me = browser
        .call(
            ADD_FINISH,
            json!({ "credential": credential, "nickname": "Laptop" }),
        )
        .await
        .unwrap();
    assert_eq!(me2.passkeys.len(), 2);
    assert_eq!(me2.passkeys[1].nickname, "Laptop");

    // Now the first can go, then the second cannot.
    let me3: Me = browser
        .call(REMOVE, json!({ "passkey_id": me2.passkeys[0].id }))
        .await
        .unwrap();
    assert_eq!(me3.passkeys.len(), 1);
    let error = browser
        .call::<Me>(REMOVE, json!({ "passkey_id": me3.passkeys[0].id }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::CONFLICT);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_user_cannot_remove_someone_elses_passkey(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (mut alice, mut bob) = (app.browser(), app.browser());
    let (alice_me, _) = sign_up(&mut alice, &mut Passkey::new(), "alice").await;
    sign_up(&mut bob, &mut Passkey::new(), "bob").await;
    // Give Alice a second passkey so the last-method rule is not what stops Bob.
    sqlx::query(
        "INSERT INTO passkeys (user_id, credential_id, passkey, backup_eligible, backup_state, nickname)
         SELECT user_id, '\\x01'::bytea, passkey, false, false, 'copy' FROM passkeys WHERE user_id = $1",
    )
    .bind(alice_me.user_id.as_uuid())
    .execute(&db)
    .await
    .unwrap();
    let error = bob
        .call::<Me>(REMOVE, json!({ "passkey_id": alice_me.passkeys[0].id }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::NOT_FOUND);
    assert_eq!(me(&mut alice).await.unwrap().passkeys.len(), 2);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn signed_out_calls_to_protected_functions_are_401(db: PgPool) {
    let app = TestApp::new(db).await;
    let mut browser = app.browser();
    for path in [ME, ADD_BEGIN, GOOGLE_UNLINK] {
        let error = browser.call::<Value>(path, json!({})).await.unwrap_err();
        assert_eq!(error.status, StatusCode::UNAUTHORIZED, "{path}");
    }
    let error = browser
        .call::<Value>(REMOVE, json!({ "passkey_id": uuid::Uuid::nil() }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::UNAUTHORIZED);
    let error = browser
        .call::<Value>(GOOGLE_BEGIN, json!({ "intent": "Link", "popup": true }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn deleting_a_user_deletes_their_auth_rows(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let (me1, _) = sign_up(&mut browser, &mut Passkey::new(), "a").await;
    google_sign_in_or_link(&app, &mut browser, "Link", "sub-cascade")
        .await
        .unwrap();
    let _: RequestChallengeResponse = browser.call(SIGN_IN_BEGIN, json!({})).await.unwrap();
    let _: CreationChallengeResponse = browser.call(ADD_BEGIN, json!({})).await.unwrap();

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(me1.user_id.as_uuid())
        .execute(&db)
        .await
        .unwrap();
    for table in [
        "passkeys",
        "oauth_identities",
        "sessions",
        "webauthn_user_handles",
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }
    let bound: i64 =
        sqlx::query_scalar("SELECT count(*) FROM auth_ceremonies WHERE user_id IS NOT NULL")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(bound, 0);
    assert_eq!(
        me(&mut browser).await.unwrap_err().status,
        StatusCode::UNAUTHORIZED
    );
}

// --- Google --------------------------------------------------------------------------------

/// Starts a Google flow and returns the authorization URL.
async fn google_begin(browser: &mut Browser, intent: &str) -> String {
    browser
        .call(GOOGLE_BEGIN, json!({ "intent": intent, "popup": true }))
        .await
        .unwrap()
}

fn callback_path(code: &str, state: &str) -> String {
    format!(
        "/auth/google/callback?code={}&state={}",
        url::form_urlencoded::byte_serialize(code.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(state.as_bytes()).collect::<String>(),
    )
}

/// Runs a whole Google flow (callback in the same browser) for `subject`; returns the page's
/// message type.
async fn google_sign_in_or_link(
    app: &TestApp,
    browser: &mut Browser,
    intent: &str,
    subject: &str,
) -> Result<(), String> {
    let url = google_begin(browser, intent).await;
    let claims = app.google.claims(&url, subject);
    app.google.grant(
        "code-1",
        Grant {
            code_challenge: query_param(&url, "code_challenge"),
            claims,
            sign_with_unpublished_key: false,
            sign_hs256_with_client_secret: false,
        },
    );
    let (status, _, body) = browser
        .get(&callback_path("code-1", &query_param(&url, "state")))
        .await;
    assert_eq!(status, StatusCode::OK);
    if body.contains(r#""type":"done""#) {
        Ok(())
    } else {
        Err(body)
    }
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn google_authorization_url_uses_pkce_state_and_nonce(db: PgPool) {
    let app = TestApp::new(db).await;
    let mut browser = app.browser();
    let url = google_begin(&mut browser, "SignIn").await;
    assert!(
        url.starts_with(&format!("{}/auth?", app.google.issuer)),
        "{url}"
    );
    assert_eq!(query_param(&url, "response_type"), "code");
    assert_eq!(query_param(&url, "code_challenge_method"), "S256");
    assert_eq!(
        query_param(&url, "client_id"),
        super::test_support::CLIENT_ID
    );
    assert_eq!(
        query_param(&url, "redirect_uri"),
        "http://localhost:8080/auth/google/callback"
    );
    assert!(query_param(&url, "scope").split(' ').any(|s| s == "openid"));
    assert!(query_param(&url, "state").len() >= 20);
    assert!(query_param(&url, "nonce").len() >= 20);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn google_sign_in_creates_then_finds_the_account_by_sub(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    google_sign_in_or_link(&app, &mut browser, "SignIn", "sub-1")
        .await
        .unwrap();
    let first = me(&mut browser).await.unwrap();
    assert_eq!(
        first.user_id.as_uuid().get_version_num(),
        7,
        "UUIDv7 ids (#65)"
    );
    assert!(first.google_linked);
    assert!(first.passkeys.is_empty());

    let () = browser.call(SIGN_OUT, json!({})).await.unwrap();
    let before = browser.cookie.clone();
    google_sign_in_or_link(&app, &mut browser, "SignIn", "sub-1")
        .await
        .unwrap();
    assert_ne!(browser.cookie, before, "the session id rotates on sign-in");
    assert_eq!(me(&mut browser).await.unwrap().user_id, first.user_id);

    // Another subject with the same email is another account: never linked by email.
    let mut other = app.browser();
    google_sign_in_or_link(&app, &mut other, "SignIn", "sub-2")
        .await
        .unwrap();
    assert_ne!(me(&mut other).await.unwrap().user_id, first.user_id);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_forwarded_sign_in_url_cannot_sign_the_attacker_in_as_the_victim(db: PgPool) {
    // The attacker starts a flow, sends its genuine Google URL to the victim, and the victim
    // signs in at Google. The victim's browser lands on our callback with the victim's code and
    // the attacker's `state`, but holds no ceremony for it: nothing completes, anywhere.
    let app = TestApp::new(db.clone()).await;
    let mut victim_app = app.browser();
    google_sign_in_or_link(&app, &mut victim_app, "SignIn", "victim-sub")
        .await
        .unwrap();
    for victim_signed_in in [false, true] {
        let mut attacker = app.browser();
        let url = google_begin(&mut attacker, "SignIn").await;
        let code = format!("victim-code-{victim_signed_in}");
        app.google.grant(
            &code,
            Grant::new(
                query_param(&url, "code_challenge"),
                app.google.claims(&url, "victim-sub"),
            ),
        );
        let mut victim = if victim_signed_in {
            victim_app.clone()
        } else {
            app.browser()
        };
        victim.origin = None;
        victim.fetch_site = Some("cross-site".to_owned());
        let (_, _, body) = victim
            .get(&callback_path(&code, &query_param(&url, "state")))
            .await;
        assert!(body.contains(r#""type":"error""#), "{body}");
        assert!(!body.contains(&code), "the code is not echoed: {body}");
        // The attacker is not signed in, and there is no endpoint that could hand it the code.
        assert_eq!(
            me(&mut attacker).await.unwrap_err().status,
            StatusCode::UNAUTHORIZED
        );
        // The forwarded flow is dead: even if the victim's code leaks later, the attacker's
        // own callback with it finds no ceremony.
        let google: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM auth_ceremonies WHERE kind IN ('google_sign_in', 'google_link')",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(google, 0);
        let (_, _, body) = attacker
            .get(&callback_path(&code, &query_param(&url, "state")))
            .await;
        assert!(body.contains(r#""type":"error""#), "{body}");
        assert_eq!(
            me(&mut attacker).await.unwrap_err().status,
            StatusCode::UNAUTHORIZED
        );
    }
    // No account other than the victim's was created or reached.
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(users, 1);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_forwarded_link_url_cannot_bind_the_victims_google_to_the_attacker(db: PgPool) {
    let app = TestApp::new(db).await;
    let mut attacker = app.browser();
    let (attacker_me, _) = sign_up(&mut attacker, &mut Passkey::new(), "attacker").await;
    let url = google_begin(&mut attacker, "Link").await;
    app.google.grant(
        "victim-link-code",
        Grant::new(
            query_param(&url, "code_challenge"),
            app.google.claims(&url, "victim-new-sub"),
        ),
    );
    let mut victim = app.browser();
    victim.origin = None;
    victim.fetch_site = Some("cross-site".to_owned());
    let (_, _, body) = victim
        .get(&callback_path(
            "victim-link-code",
            &query_param(&url, "state"),
        ))
        .await;
    assert!(body.contains(r#""type":"error""#), "{body}");
    assert!(!me(&mut attacker).await.unwrap().google_linked);

    // Later, the victim's own "Continue with Google" gets a fresh account of their own.
    let mut victim_later = app.browser();
    google_sign_in_or_link(&app, &mut victim_later, "SignIn", "victim-new-sub")
        .await
        .unwrap();
    assert_ne!(
        me(&mut victim_later).await.unwrap().user_id,
        attacker_me.user_id
    );
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_forged_callback_cannot_cancel_a_flow_in_progress(db: PgPool) {
    // The session cookie is SameSite=Lax, so any site can navigate the victim to our callback.
    let app = TestApp::new(db).await;
    let mut victim = app.browser();
    let url = google_begin(&mut victim, "SignIn").await;
    let state = query_param(&url, "state");
    for forged in [
        "/auth/google/callback?error=access_denied".to_owned(),
        "/auth/google/callback?error=access_denied&state=junk".to_owned(),
        callback_path("junk-code", "junk-state"),
        "/auth/google/callback?code=junk".to_owned(),
    ] {
        let mut navigated = victim.clone();
        navigated.origin = None;
        navigated.fetch_site = Some("cross-site".to_owned());
        let (_, _, body) = navigated.get(&forged).await;
        assert!(body.contains(r#""type":"error""#), "{forged}: {body}");
    }
    // The real callback still completes.
    app.google.grant(
        "real-code",
        Grant::new(
            query_param(&url, "code_challenge"),
            app.google.claims(&url, "sub-real"),
        ),
    );
    let (_, _, body) = victim.get(&callback_path("real-code", &state)).await;
    assert!(body.contains(r#""type":"done""#), "{body}");
    assert!(me(&mut victim).await.unwrap().google_linked);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn googles_error_with_the_right_state_ends_the_flow(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let url = google_begin(&mut browser, "SignIn").await;
    let state = query_param(&url, "state");
    let (_, _, body) = browser
        .get(&format!(
            "/auth/google/callback?error=access_denied&state={state}"
        ))
        .await;
    assert!(body.contains(r#""type":"error""#), "{body}");
    let ceremonies: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_ceremonies")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(ceremonies, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_new_begin_replaces_the_previous_ceremony_row(db: PgPool) {
    // Repeated begins on one cookie must not pile up rows, even without the per-IP limit (#23).
    let mut rate_limit = crate::server::rate_limit::RateLimitConfig::default();
    rate_limit.limits.auth_begin.per_ip = None;
    let app = TestApp::with_rate_limit(db.clone(), rate_limit).await;
    let mut browser = app.browser();
    for _ in 0..20 {
        let _: RequestChallengeResponse = browser.call(SIGN_IN_BEGIN, json!({})).await.unwrap();
        let _: CreationChallengeResponse = browser
            .call(SIGN_UP_BEGIN, json!({ "display_name": "" }))
            .await
            .unwrap();
        let _ = google_begin(&mut browser, "SignIn").await;
    }
    let ceremonies: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_ceremonies")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(ceremonies, 3, "one per kind");
    assert_eq!(session_rows(&db).await, 1);
    // A fresh begin and finish still work.
    let mut passkey = Passkey::new();
    let ccr: CreationChallengeResponse = browser
        .call(SIGN_UP_BEGIN, json!({ "display_name": "" }))
        .await
        .unwrap();
    let credential = passkey.register(ccr);
    let _: Me = browser
        .call(SIGN_UP_FINISH, json!({ "credential": credential }))
        .await
        .unwrap();
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn continue_with_google_while_signed_in_links_instead_of_creating_an_account(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let (me1, _) = sign_up(&mut browser, &mut Passkey::new(), "a").await;
    google_sign_in_or_link(&app, &mut browser, "SignIn", "sub-while-signed-in")
        .await
        .unwrap();
    let me2 = me(&mut browser).await.unwrap();
    assert_eq!(me2.user_id, me1.user_id);
    assert!(me2.google_linked);
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(users, 1);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_redirect_flow_goes_back_home_and_a_popup_closes(db: PgPool) {
    let app = TestApp::new(db).await;
    for (popup, after) in [(false, "home"), (true, "close")] {
        let mut browser = app.browser();
        let url: String = browser
            .call(GOOGLE_BEGIN, json!({ "intent": "SignIn", "popup": popup }))
            .await
            .unwrap();
        app.google.grant(
            "code-r",
            Grant::new(
                query_param(&url, "code_challenge"),
                app.google.claims(&url, "sub-r"),
            ),
        );
        let (_, _, body) = browser
            .get(&callback_path("code-r", &query_param(&url, "state")))
            .await;
        assert!(body.contains(r#""type":"done""#), "{body}");
        assert!(body.contains(&format!(r#""after":"{after}""#)), "{body}");
        // The app, sharing the session, sees the flow as finished.
        assert!(me(&mut browser).await.unwrap().google_linked);
    }
}

/// Begins a flow and grants a code with `tamper` applied; returns the finish error status.
async fn google_finish_with(
    app: &TestApp,
    tamper: impl FnOnce(&mut Grant, &mut String),
) -> StatusCode {
    let mut browser = app.browser();
    let url = google_begin(&mut browser, "SignIn").await;
    let mut state = query_param(&url, "state");
    let mut grant = Grant::new(
        query_param(&url, "code_challenge"),
        app.google.claims(&url, "sub-tamper"),
    );
    tamper(&mut grant, &mut state);
    app.google.grant("code-t", grant);
    let (_, _, body) = browser.get(&callback_path("code-t", &state)).await;
    let status = if body.contains(r#""type":"done""#) {
        StatusCode::OK
    } else {
        assert!(body.contains(r#""type":"error""#), "{body}");
        StatusCode::BAD_REQUEST
    };
    if status != StatusCode::OK {
        assert_eq!(
            me(&mut browser).await.unwrap_err().status,
            StatusCode::UNAUTHORIZED
        );
    }
    status
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn google_rejects_a_state_mismatch(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let status = google_finish_with(&app, |_, state| state.push('x')).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(users, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn google_rejects_a_nonce_mismatch(db: PgPool) {
    let app = TestApp::new(db).await;
    let status = google_finish_with(&app, |grant, _| {
        grant.claims["nonce"] = json!("another-nonce");
    })
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn google_rejects_a_token_for_another_client(db: PgPool) {
    let app = TestApp::new(db).await;
    let status = google_finish_with(&app, |grant, _| {
        grant.claims["aud"] = json!("someone-else.apps.googleusercontent.com");
    })
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn google_rejects_a_token_shared_with_another_audience(db: PgPool) {
    let app = TestApp::new(db).await;
    let status = google_finish_with(&app, |grant, _| {
        grant.claims["aud"] = json!([
            super::test_support::CLIENT_ID,
            "other.apps.googleusercontent.com"
        ]);
    })
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let status = google_finish_with(&app, |grant, _| {
        grant.claims["azp"] = json!("other.apps.googleusercontent.com");
    })
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn google_rejects_a_token_from_another_issuer(db: PgPool) {
    let app = TestApp::new(db).await;
    let status = google_finish_with(&app, |grant, _| {
        grant.claims["iss"] = json!("https://evil.example");
    })
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn google_rejects_an_expired_token(db: PgPool) {
    let app = TestApp::new(db).await;
    let status = google_finish_with(&app, |grant, _| {
        let now = super::session::now_unix();
        grant.claims["iat"] = json!(now - 7200);
        grant.claims["exp"] = json!(now - 3600);
    })
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn google_rejects_a_token_signed_with_an_unknown_key(db: PgPool) {
    let app = TestApp::new(db).await;
    let status = google_finish_with(&app, |grant, _| grant.sign_with_unpublished_key = true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn google_rejects_an_hs256_token_keyed_with_the_client_secret(db: PgPool) {
    let app = TestApp::new(db).await;
    let status =
        google_finish_with(&app, |grant, _| grant.sign_hs256_with_client_secret = true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn google_rejects_a_code_bound_to_another_pkce_challenge(db: PgPool) {
    let app = TestApp::new(db).await;
    let status = google_finish_with(&app, |grant, _| {
        grant.code_challenge = "not-our-challenge".to_owned();
    })
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_google_ceremony_is_single_use(db: PgPool) {
    let app = TestApp::new(db).await;
    let mut browser = app.browser();
    let url = google_begin(&mut browser, "SignIn").await;
    let state = query_param(&url, "state");
    for code in ["c1", "c2"] {
        app.google.grant(
            code,
            Grant::new(
                query_param(&url, "code_challenge"),
                app.google.claims(&url, "sub-once"),
            ),
        );
    }
    let before = browser.clone();
    let (_, _, body) = browser.get(&callback_path("c1", &state)).await;
    assert!(body.contains(r#""type":"done""#), "{body}");
    // The same state again, with the pre-sign-in cookie or with none: nothing left to use.
    for mut replay in [before, app.browser()] {
        let (_, _, body) = replay.get(&callback_path("c2", &state)).await;
        assert!(body.contains(r#""type":"error""#), "{body}");
        assert_eq!(
            me(&mut replay).await.unwrap_err().status,
            StatusCode::UNAUTHORIZED
        );
    }
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_forged_callback_cannot_log_the_victim_into_the_attackers_account(db: PgPool) {
    // Login CSRF: the attacker gets a code for their own Google account and sends the victim
    // to the callback with it.
    let app = TestApp::new(db).await;
    let mut attacker = app.browser();
    let url = google_begin(&mut attacker, "SignIn").await;
    app.google.grant(
        "attacker-code",
        Grant {
            code_challenge: query_param(&url, "code_challenge"),
            claims: app.google.claims(&url, "attacker-sub"),
            sign_with_unpublished_key: false,
            sign_hs256_with_client_secret: false,
        },
    );
    let mut victim = app.browser();
    // The victim has a Google flow of their own in flight (worst case).
    let _ = google_begin(&mut victim, "SignIn").await;
    let (status, _, body) = victim
        .get(&callback_path("attacker-code", &query_param(&url, "state")))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#""type":"error""#), "{body}");
    assert_eq!(
        me(&mut victim).await.unwrap_err().status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn linking_cannot_take_over_another_accounts_google(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    // Alice signs up with Google.
    let mut alice = app.browser();
    google_sign_in_or_link(&app, &mut alice, "SignIn", "alice-sub")
        .await
        .unwrap();
    let alice_me = me(&mut alice).await.unwrap();

    // Bob (passkey account) tries to link Alice's Google account.
    let mut bob = app.browser();
    let (bob_me, _) = sign_up(&mut bob, &mut Passkey::new(), "bob").await;
    let body = google_sign_in_or_link(&app, &mut bob, "Link", "alice-sub")
        .await
        .unwrap_err();
    assert!(body.contains("already used by another account"), "{body}");
    assert!(!me(&mut bob).await.unwrap().google_linked);

    // Signing in with that Google account is still Alice, never Bob.
    let mut again = app.browser();
    google_sign_in_or_link(&app, &mut again, "SignIn", "alice-sub")
        .await
        .unwrap();
    let who = me(&mut again).await.unwrap().user_id;
    assert_eq!(who, alice_me.user_id);
    assert_ne!(who, bob_me.user_id);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn link_then_sign_in_with_google_and_unlink_rules(db: PgPool) {
    let app = TestApp::new(db).await;
    let mut browser = app.browser();
    let (me1, _) = sign_up(&mut browser, &mut Passkey::new(), "a").await;
    google_sign_in_or_link(&app, &mut browser, "Link", "sub-link")
        .await
        .unwrap();
    let linked = me(&mut browser).await.unwrap();
    assert!(linked.google_linked);
    assert_eq!(linked.sign_in_methods(), 2);

    // Linking a second, different Google account is refused.
    let body = google_sign_in_or_link(&app, &mut browser, "Link", "sub-other")
        .await
        .unwrap_err();
    assert!(body.contains("different Google account"), "{body}");

    // Sign in with the linked Google account: same user.
    let mut phone = app.browser();
    google_sign_in_or_link(&app, &mut phone, "SignIn", "sub-link")
        .await
        .unwrap();
    assert_eq!(me(&mut phone).await.unwrap().user_id, me1.user_id);

    // Unlink, then removing the last passkey is refused.
    let unlinked: Me = browser.call(GOOGLE_UNLINK, json!({})).await.unwrap();
    assert!(!unlinked.google_linked);
    let error = browser
        .call::<Me>(REMOVE, json!({ "passkey_id": unlinked.passkeys[0].id }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::CONFLICT);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn google_as_the_only_method_cannot_be_unlinked(db: PgPool) {
    let app = TestApp::new(db).await;
    let mut browser = app.browser();
    google_sign_in_or_link(&app, &mut browser, "SignIn", "sub-only")
        .await
        .unwrap();
    let error = browser
        .call::<Me>(GOOGLE_UNLINK, json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::CONFLICT);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_link_ceremony_cannot_be_finished_by_another_user(db: PgPool) {
    let app = TestApp::new(db).await;
    let mut alice = app.browser();
    sign_up(&mut alice, &mut Passkey::new(), "alice").await;
    let url = google_begin(&mut alice, "Link").await;
    let state = query_param(&url, "state");
    app.google.grant(
        "code-l",
        Grant::new(
            query_param(&url, "code_challenge"),
            app.google.claims(&url, "sub-l"),
        ),
    );
    // Alice signs out and Bob signs in on the same browser before the callback.
    let () = alice.call(SIGN_OUT, json!({})).await.unwrap();
    assert!(alice.cookie.is_none());
    let mut bob = alice.clone();
    sign_up(&mut bob, &mut Passkey::new(), "bob").await;
    let _ = bob.get(&callback_path("code-l", &state)).await;
    assert!(!me(&mut bob).await.unwrap().google_linked);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn cleanup_deletes_expired_sessions_and_ceremonies(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    sign_up(&mut browser, &mut Passkey::new(), "a").await;
    let _: RequestChallengeResponse = app.browser().call(SIGN_IN_BEGIN, json!({})).await.unwrap();
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE auth_ceremonies SET expires_at = now() - interval '1 second'")
        .execute(&db)
        .await
        .unwrap();
    let deleted = super::session::PgSessionStore::new(db.clone())
        .delete_expired()
        .await
        .unwrap();
    assert_eq!(deleted, 3, "two sessions and one ceremony");
    assert_eq!(session_rows(&db).await, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn auth_rows_never_change_owner(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let (me1, _) = sign_up(&mut browser, &mut Passkey::new(), "a").await;
    google_sign_in_or_link(&app, &mut browser, "Link", "sub-owner")
        .await
        .unwrap();
    let _: CreationChallengeResponse = browser.call(ADD_BEGIN, json!({})).await.unwrap();
    let other: uuid::Uuid = sqlx::query_scalar("INSERT INTO users DEFAULT VALUES RETURNING id")
        .fetch_one(&db)
        .await
        .unwrap();
    for table in [
        "passkeys",
        "oauth_identities",
        "sessions",
        "auth_ceremonies",
    ] {
        let error = sqlx::query(&format!(
            "UPDATE {table} SET user_id = $2 WHERE user_id = $1"
        ))
        .bind(me1.user_id.as_uuid())
        .bind(other)
        .execute(&db)
        .await
        .unwrap_err();
        let db_error = error.as_database_error().unwrap();
        assert_eq!(
            db_error.code().as_deref(),
            Some("23000"),
            "{table}: {error}"
        );
    }
    me(&mut browser).await.unwrap();
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn the_webauthn_user_handle_is_random_stable_and_not_the_user_id(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let ccr: CreationChallengeResponse = browser
        .call(SIGN_UP_BEGIN, json!({ "display_name": "a" }))
        .await
        .unwrap();
    let sign_up_handle = ccr.public_key.user.id.to_vec();
    let mut phone = Passkey::new();
    let credential = phone.register(ccr);
    let me1: Me = browser
        .call(SIGN_UP_FINISH, json!({ "credential": credential }))
        .await
        .unwrap();
    let stored: uuid::Uuid =
        sqlx::query_scalar("SELECT user_handle FROM webauthn_user_handles WHERE user_id = $1")
            .bind(me1.user_id.as_uuid())
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(sign_up_handle, stored.as_bytes().to_vec());
    assert_ne!(stored, me1.user_id.as_uuid());
    assert_eq!(stored.get_version_num(), 4, "random, not time-based");

    // A second passkey gets the same handle.
    let ccr: CreationChallengeResponse = browser.call(ADD_BEGIN, json!({})).await.unwrap();
    assert_eq!(ccr.public_key.user.id.to_vec(), stored.as_bytes().to_vec());

    // A Google-only account gets its own handle on its first passkey.
    let mut other = app.browser();
    google_sign_in_or_link(&app, &mut other, "SignIn", "sub-handle")
        .await
        .unwrap();
    let ccr: CreationChallengeResponse = other.call(ADD_BEGIN, json!({})).await.unwrap();
    let other_handle = ccr.public_key.user.id.to_vec();
    assert_ne!(other_handle, stored.as_bytes().to_vec());
    let other_id = me(&mut other).await.unwrap().user_id;
    assert_ne!(other_handle, other_id.as_uuid().as_bytes().to_vec());
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn new_users_get_uuidv7_ids_by_default(db: PgPool) {
    let id: uuid::Uuid = sqlx::query_scalar("INSERT INTO users DEFAULT VALUES RETURNING id")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(id.get_version_num(), 7);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_callback_with_repeated_parameters_gets_the_error_page(db: PgPool) {
    let app = TestApp::new(db).await;
    let mut browser = app.browser();
    let url = google_begin(&mut browser, "SignIn").await;
    let state = query_param(&url, "state");
    let (status, _, body) = browser
        .get(&format!(
            "/auth/google/callback?code=a&state={state}&state={state}"
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#""type":"error""#), "{body}");
}

// --- Step-up for new sign-in methods (#22) -------------------------------------------------

/// Moves the sign-in of every session of `user` `seconds` into the past (whole seconds, floored,
/// so the age is exact).
async fn age_sign_in(db: &PgPool, user: &Me, seconds: i64) {
    sqlx::query(
        "UPDATE sessions
         SET data = jsonb_set(data, '{auth.signed_in_at}',
                              to_jsonb(floor(extract(epoch FROM now()))::bigint - $2))
         WHERE user_id = $1",
    )
    .bind(user.user_id.as_uuid())
    .bind(seconds)
    .execute(db)
    .await
    .unwrap();
}

// Ages with a margin: the database (which computes them) and the server may disagree on the
// clock by a second or so (Docker's VM), so neither is put at the window's edge.
const STALE: i64 = 10 * 60 + 60;
const FRESH: i64 = 60;
const STEP_UP: &str = "For your security, sign in again first, then try again.";

async fn passkey_count(db: &PgPool, user: &Me) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM passkeys WHERE user_id = $1")
        .bind(user.user_id.as_uuid())
        .fetch_one(db)
        .await
        .unwrap()
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn adding_a_passkey_needs_a_recent_sign_in(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let mut passkey = Passkey::new();
    let (me1, credential_id) = sign_up(&mut browser, &mut passkey, "a").await;

    // A stale session cannot even start.
    age_sign_in(&db, &me1, STALE).await;
    let error = browser
        .call::<CreationChallengeResponse>(ADD_BEGIN, json!({}))
        .await
        .unwrap_err();
    assert_eq!(
        (error.status, error.message.as_str()),
        (StatusCode::FORBIDDEN, STEP_UP)
    );

    // Started while fresh, finished once stale: refused at the finish too.
    age_sign_in(&db, &me1, FRESH).await;
    let ccr: CreationChallengeResponse = browser.call(ADD_BEGIN, json!({})).await.unwrap();
    let credential = Passkey::new().register(ccr);
    age_sign_in(&db, &me1, STALE).await;
    let error = browser
        .call::<Me>(
            ADD_FINISH,
            json!({ "credential": credential, "nickname": "late" }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::FORBIDDEN);
    assert_eq!(passkey_count(&db, &me1).await, 1);

    // Signing in again with the account's own passkey (a new session) allows it.
    let assertion = sign_in_assertion(&mut browser, &mut passkey, &credential_id).await;
    let _: Me = browser
        .call(SIGN_IN_FINISH, json!({ "credential": assertion }))
        .await
        .unwrap();
    let ccr: CreationChallengeResponse = browser.call(ADD_BEGIN, json!({})).await.unwrap();
    let credential = Passkey::new().register(ccr);
    let _: Me = browser
        .call(
            ADD_FINISH,
            json!({ "credential": credential, "nickname": "second" }),
        )
        .await
        .unwrap();
    assert_eq!(passkey_count(&db, &me1).await, 2);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn linking_google_needs_a_recent_sign_in(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let (me1, _) = sign_up(&mut browser, &mut Passkey::new(), "a").await;

    // A stale session cannot start a link.
    age_sign_in(&db, &me1, STALE).await;
    let error = browser
        .call::<String>(GOOGLE_BEGIN, json!({ "intent": "Link", "popup": true }))
        .await
        .unwrap_err();
    assert_eq!(
        (error.status, error.message.as_str()),
        (StatusCode::FORBIDDEN, STEP_UP)
    );
    // "Continue with Google" while signed in links too: the same refusal.
    let error = browser
        .call::<String>(GOOGLE_BEGIN, json!({ "intent": "SignIn", "popup": true }))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::FORBIDDEN);

    // Started while fresh, completed once stale: the callback refuses to link.
    age_sign_in(&db, &me1, FRESH).await;
    let url = google_begin(&mut browser, "Link").await;
    age_sign_in(&db, &me1, STALE).await;
    let claims = app.google.claims(&url, "sub-late");
    app.google.grant(
        "code-1",
        Grant {
            code_challenge: query_param(&url, "code_challenge"),
            claims,
            sign_with_unpublished_key: false,
            sign_hs256_with_client_secret: false,
        },
    );
    let (status, _, body) = browser
        .get(&callback_path("code-1", &query_param(&url, "state")))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains(r#""type":"done""#), "{body}");
    assert!(!me(&mut browser).await.unwrap().google_linked);

    // Fresh, it links.
    age_sign_in(&db, &me1, FRESH).await;
    google_sign_in_or_link(&app, &mut browser, "Link", "sub-fresh")
        .await
        .unwrap();
    assert!(me(&mut browser).await.unwrap().google_linked);
}

#[sqlx::test]
#[ignore = "needs Postgres"]
async fn a_method_added_in_a_session_does_not_refresh_its_sign_in(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut browser = app.browser();
    let (me1, _) = sign_up(&mut browser, &mut Passkey::new(), "a").await;
    let ccr: CreationChallengeResponse = browser.call(ADD_BEGIN, json!({})).await.unwrap();
    let credential = Passkey::new().register(ccr);
    let _: Me = browser
        .call(
            ADD_FINISH,
            json!({ "credential": credential, "nickname": "new" }),
        )
        .await
        .unwrap();
    google_sign_in_or_link(&app, &mut browser, "Link", "sub-new")
        .await
        .unwrap();
    // The session's step-up is still the sign-up's: once that is old, neither the passkey nor
    // the Google account added in this session counts as a fresh sign-in.
    age_sign_in(&db, &me1, STALE).await;
    let error = browser
        .call::<CreationChallengeResponse>(ADD_BEGIN, json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::FORBIDDEN);
    let error = browser
        .call::<()>("/api/account/delete", json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::FORBIDDEN);
}
