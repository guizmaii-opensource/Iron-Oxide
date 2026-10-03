# Sign-in

Iron Oxide has no passwords and no email. There are two ways to sign in (#5, decided in #37):

- **Passkeys** (WebAuthn), the primary method, through [`webauthn-rs`](https://docs.rs/webauthn-rs).
- **Sign in with Google** (OpenID Connect), through [`openidconnect`](https://docs.rs/openidconnect).

Sessions are server-side, stored in Postgres, and identified by a signed cookie.

Code map:

| Where | What |
|---|---|
| `crates/iron-oxide-app/src/auth/api.rs` | The sign-in server functions (all `POST`) |
| `crates/iron-oxide-app/src/auth/types.rs` | Types shared with the client (`Me`, `UserId`, …) |
| `crates/iron-oxide-app/src/auth/browser.rs` | Browser side: `navigator.credentials`, the Google popup |
| `crates/iron-oxide-app/src/server/auth/mod.rs` | `AuthState`, `AuthContext`, the `AuthUser` extractor, router wiring |
| `crates/iron-oxide-app/src/server/auth/passkeys.rs` | Passkey ceremonies, account queries |
| `crates/iron-oxide-app/src/server/auth/google.rs` | Google flow and the `/auth/google/callback` page |
| `crates/iron-oxide-app/src/server/auth/session.rs` | Session store, cookie settings, timeouts, cleanup |
| `crates/iron-oxide-app/src/server/auth/ceremony.rs` | One-time ceremony state |
| `crates/iron-oxide-app/src/server/auth/csrf.rs` | The CSRF layer |
| `crates/iron-oxide-app/src/server/rate_limit.rs` | Per-IP and per-user rate limits ([rate-limiting.md](rate-limiting.md)) |
| `crates/iron-oxide-app/migrations/20260928153000_create_auth_tables.sql` | Tables |

## Accounts and identities

A `users` row holds no identifier from outside: no email, no Google subject. Identities live in
their own tables, all `ON DELETE CASCADE` from `users`, so deleting a user (#22) removes their
passkeys, Google link, sessions and in-flight ceremonies. As for every user-owned table (#17),
`user_id` is indexed and a trigger (`forbid_owner_change`) forbids changing it, so no row can move
to another user:

- `passkeys`: one row per WebAuthn credential. `credential_id` is unique across all users. The
  serialized `webauthn_rs::Passkey` (public key, algorithm, signature counter, backup flags) is the
  source of truth. `sign_count`, `backup_eligible` and `backup_state` mirror it for display.
  Each row also has a nickname, `created_at` and `last_used_at`.
- `oauth_identities`: `(provider, subject)` is unique, so one Google account belongs to at most one
  user, and `(user_id, provider)` is unique, so a user has at most one Google account.
- `sessions`: see [Sessions](#sessions).
- `auth_ceremonies`: see [Ceremony state](#ceremony-state).

Row ids, user ids included, are UUIDv7 (#65). Nothing secret or shown to an authenticator is:
session ids, WebAuthn user handles, and Google's `state`, nonce and PKCE verifier are
cryptographically random.

An account always keeps at least one way in. Removing a passkey or unlinking Google is refused when
it is the last one. The check locks the user row (`SELECT … FOR UPDATE`), so two concurrent
removals cannot both pass.

## Passkeys

The relying party ID is `WEBAUTHN_RP_ID`. In production the app is served at
`https://app.iron-oxyde.com` (the apex is the landing page, #70), and the RP ID is the **parent**
domain `iron-oxyde.com`, not the app's host: passkeys are bound to the RP ID, so they keep working
if the app moves to another subdomain. The origin is `WEBAUTHN_ORIGIN`, which must equal
`APP_BASE_URL`'s origin; the RP ID must be that origin's host or a parent domain of it. Every ceremony requires user verification (Face ID,
Touch ID, device PIN).

**Sign-up** (`passkey_sign_up_begin` → `navigator.credentials.create()` → `passkey_sign_up_finish`):

1. Begin draws the new user's id (UUIDv7) and a separate WebAuthn **user handle**: a random
   UUIDv4 from the OS CSPRNG, kept in `webauthn_user_handles`, one per user and the same for all
   their passkeys. The handle is never the user id: authenticators store it, and a UUIDv7 would
   tell them when the account was created. The optional name given by the user labels the
   account in the passkey manager.
2. The creation options require a discoverable ("resident") credential and user verification.
   `webauthn-rs` leaves `residentKey` unset, so we set it to `required` before sending.
3. Finish verifies the attestation: challenge, origin, RP ID hash and the UV flag. It rejects a
   credential the browser reports as non-discoverable (`credProps.rk == false`). It then creates
   the user and stores the passkey in one transaction, and signs in.

**Sign-in** is discoverable, so no username is needed (`passkey_sign_in_begin` →
`navigator.credentials.get()` → `passkey_sign_in_finish`):

1. Begin asks for any credential of our RP (an empty `allowCredentials`). It uses a modal prompt:
   `webauthn-rs`' discoverable API defaults to conditional (autofill) mediation, which we unset.
2. The browser returns the credential id and the user handle. Finish loads the one passkey with
   *that* credential id *and* that user handle, locked with `FOR UPDATE`. It verifies the
   signature, challenge, origin, RP ID, UV flag and signature counter. A counter that does not
   increase, when non-zero, is rejected as a possible clone. Finish then stores the new counter
   and backup state, sets `last_used_at` and signs in.

**Adding a passkey** works like sign-up, bound to the signed-in user. The user's existing
credentials go in `excludeCredentials`, so the same authenticator is not registered twice. There
are at most 20 passkeys per user.

**Step-up (#22).** Adding a sign-in method (a passkey: begin and finish; Google: begin of a link and
its callback) and deleting the account need a sign-in from the last 10 minutes
(`session::STEP_UP_WINDOW`, the session's `auth.signed_in_at`). Otherwise `403 For your security,
sign in again first, then try again.` Only a sign-in sets that time, never adding a method, so a
stale session cannot add its own credential and sign in afresh with it. Tests:
`adding_a_passkey_needs_a_recent_sign_in`, `linking_google_needs_a_recent_sign_in`,
`a_method_added_in_a_session_does_not_refresh_its_sign_in`,
`a_stale_session_cannot_add_a_passkey_to_delete_the_account`.

The `webauthn-rs` feature flags used are:

- `danger-allow-state-serialisation`: the ceremony state is serialized into `auth_ceremonies`, on
  the server only.
- `conditional-ui`: enables the discoverable authentication API.

## Sign in with Google

This is the authorization code flow with PKCE (S256), `state` and `nonce`. It asks only for the
`openid` scope, so we never receive an email or a name. Accounts are linked by the ID token's
`sub` claim only, never by email.

1. `google_begin(intent, popup)` is a `POST`, so the CSRF check applies. It discovers Google's endpoints,
   stores a ceremony holding a random `state`, `nonce` and PKCE verifier, and returns the
   authorization URL. The client opens it:
   - **in a popup** (`window.open`) opened synchronously in the tap handler, which iOS requires
     for an installed PWA. The popup is opened blank first, then pointed at Google once the URL
     comes back;
   - **in the current window**, if popups are blocked (a full redirect).
2. Google redirects to `GET /auth/google/callback?code=…&state=…`. This is the standard web
   flow: the callback completes sign-in **only in the browser context that holds the session
   that started it**, and only if `state` matches that session's ceremony.
   - A full-page redirect, or a popup that shares the app's cookies, carries that session. The
     callback finishes the flow and rotates the session. In a popup the page announces
     `{"type":"done"}` on a same-origin `BroadcastChannel` (and to `window.opener` with
     `postMessage(…, <our origin>)` if it still has one; Google's pages send
     `Cross-Origin-Opener-Policy: same-origin`, so usually it does not) and closes itself. The
     app then re-checks `me()`; it also re-checks every 2 s while it waits, in case the message
     does not arrive. After a full redirect the page goes back to `/`.
   - A request without that session, or with another `state`, gets an error page and changes
     nothing: the ceremony is only consumed when its own `state` comes back. So a forged
     navigation to the callback (`?error=…`, or junk `code`/`state`; any site can trigger one,
     since the cookie is `SameSite=Lax`) cannot cancel a sign-in in progress.
   - **Installed iOS web app:** if the popup gets its own cookie jar, the callback there has no
     session and shows an error. While waiting, the app offers "Continue in this window", which
     restarts the flow as a full-page redirect (top-level navigation) in the app's own window. If
     neither shares the app's cookies there, sign-in in the installed app uses passkeys. To be
     confirmed on a real iPhone.
   - The page is sent with `Cache-Control: no-store`, `Referrer-Policy: no-referrer` (its URL
     holds the code) and a CSP that only allows its own inline script (by hash). It cannot be
     framed, never echoes the code, and data it shows is escaped.

   **Why nothing may complete a flow outside the session that started it (the forwarded-link
   attack).** Anyone can call `google_begin` and get a genuine authorization URL (Google's
   domain, our client id, our redirect URI) that carries *their* `state`, nonce and PKCE
   challenge. If they send it to a victim ("Sign in to Iron Oxide with Google") and the victim
   signs in at Google, the victim's browser arrives at our callback with the victim's code and
   the attacker's `state`. An earlier design let such a callback leave the code on the server for
   the session owning that `state` to redeem: the attacker would have been signed in as the
   victim. `state` is not a secret of the victim's browser; it is only a secret of whoever
   started the flow. With the standard flow the victim's browser holds no ceremony for that
   `state`, the callback shows an error, and the attacker never sees the code. The callback also
   **deletes the attacker's ceremony** (found by its `state`, which only its creator and this
   browser know), so the flow can never complete: without that, a code leaking later by another
   channel (history sync, a screenshot of the URL, a proxy log) could still be redeemed by the
   attacker, who holds the matching PKCE verifier. PKCE does not protect a flow the attacker
   started; this deletion does. Tested by
   `a_forwarded_sign_in_url_cannot_sign_the_attacker_in_as_the_victim` and
   `a_forwarded_link_url_cannot_bind_the_victims_google_to_the_attacker`.
3. Finishing takes the ceremony whose `state` matches (checked again in constant time). It then exchanges the code
   together with the PKCE verifier, over an HTTP client that follows no redirects. It verifies the
   ID token:
   - the signature, against Google's JWKS fetched on each sign-in, so key rotation needs no
     restart;
   - the issuer, the audience (exactly our client id, and `azp`, if present, equal to it),
     expiry and issue time;
   - the nonce.
4. Then:
   - **Signed out** ("Continue with Google"): the user linked to `sub` is signed in. If there is
     none, **a new account is created** with that identity.
   - **Signed in**, whichever button: `sub` is linked to the current user; a signed-in session
     never switches to, or creates, another account through Google. This is refused if
     `sub` already belongs to another account, or if the user already has a different Google
     account. Linking an identity to itself again is a no-op.

## Sessions

Sessions use [`tower-sessions`](https://docs.rs/tower-sessions) 0.15 with our own Postgres store.
The published `tower-sessions-sqlx-store` still targets `tower-sessions-core` 0.14, and it creates
its own table with no user column.

- **Cookie:** `__Host-iron_oxide_session` (`iron_oxide_session` on local http), with
  `HttpOnly; SameSite=Lax; Path=/`. It is `Secure` whenever `APP_BASE_URL` is `https`.
  - Plain http is only accepted for `localhost`/loopback, so `Secure` cannot be turned off in
    production.
  - The `__Host-` prefix stops a sibling subdomain from setting or overwriting the cookie.
  - The value is signed with `SESSION_KEY` (HMAC), so a forged or truncated id is rejected before
    any database lookup.
- **Storage:** the table stores the SHA-256 of the session id, never the id itself, so a copy of
  the table cannot be replayed as cookies. `user_id` is kept in a column, so deleting a user
  deletes their sessions.
- **Rotation:** every sign-in deletes the old session, issues a new random id and carries no data
  over. An id planted before sign-in is worthless.
- **Expiry:**
  - **Idle:** 14 days without activity. Activity pushes the expiry back, at most once an hour,
    since each push is a write.
  - **Absolute:** 30 days after sign-in, whatever the activity.
  - **Signed-out sessions:** 15 minutes. They only exist to hold an in-flight ceremony.
  - The store never loads an expired session. The absolute limit is checked on every
    authenticated request, and an expired session is deleted.
- **Sign-out** deletes the session row and clears the cookie. Sign-out and account deletion are
  final: the store's `save` only updates an existing, unexpired row, so a request that loaded the
  session earlier cannot recreate it.
- **Cleanup:** a background task deletes expired sessions and ceremonies every 6 hours. It runs
  rarely on purpose: every run wakes the scale-to-zero Neon compute (#41).

## Ceremony state

A ceremony's state is the WebAuthn challenge state, or Google's `state`, nonce and PKCE verifier.
It is stored in `auth_ceremonies`, and the session holds only the row's random id. Taking a
ceremony removes the id from the session and deletes the row in the same statement that reads it
(`DELETE … RETURNING`). It is therefore single-use even when two requests with the same cookie race.
The session alone could not guarantee that, since each request works on its own copy of the
session data.

A passkey finish takes its ceremony inside the transaction that does the work
(`ceremony::complete`). A retryable failure (`503`: no pooled connection in time, a statement
deadline, a serialization failure) rolls back the take too, so the client can replay the same
request. Any other failure (verification, an unknown passkey, the passkey limit) still uses the
ceremony up. What cannot be replayed is a failure once the commit has happened: saving the
session afterwards, or the connection dropping during `COMMIT` itself (the server answers `503`,
but the transaction may have committed, ceremony take included). The retry then gets `400`, and
the user starts again, as on every click of the sign-in buttons; a sign-up that did commit signs
in with the new passkey.

Ceremonies expire after 5 minutes (passkeys) or 10 minutes (Google). A ceremony started by a
signed-in user (adding a passkey, linking Google) is bound to that user, and only that user's
session can finish it.

## CSRF

- The session cookie is `SameSite=Lax`, so browsers do not send it on cross-site `POST`s.
- A layer in front of every route refuses any request with an unsafe method (anything but `GET`,
  `HEAD`, `OPTIONS` and `TRACE`) unless:
  - `Sec-Fetch-Site`, if present, is `same-origin` (`same-site` is refused too);
  - `Origin`, if present, is exactly `APP_BASE_URL`'s origin. It is never compared with `Host`,
    which `dx serve` and proxies rewrite;
  - at least one of the two headers is present.
- Every server function that changes state is a `POST`. `GET` endpoints must never change state.
  The one cross-site `GET` that does, the Google callback, is protected by its one-time `state`.
- **One exemption: `POST /webhooks/stripe`** (billing, see `docs/billing.md`). Stripe's deliveries
  are server-to-server `POST`s with no `Origin`, no `Sec-Fetch-Site` and no cookie, so the check
  would refuse them. The route is merged into the router *after* `auth::install`, so it sits
  outside the session and CSRF layers: it has no session and never sees the user's cookie. It is
  authenticated by Stripe's signature instead (HMAC-SHA256 with the endpoint secret, 5-minute
  replay window, deduplication by event id). Until that is implemented it only answers `501`,
  after reading at most 256 KiB of body. Any other path, including `/webhooks/stripe/…`, stays
  behind the check.

## `AuthUser` in server functions

```rust
#[cfg(feature = "server")]
use crate::server::auth::AuthUser;

#[post("/api/sets", user: AuthUser)]
pub async fn save_set(set: NewSet) -> Result<(), ServerFnError> {
    let user_id = user.user_id(); // from the server-side session; scope every query by it
    // ...
}
```

Without a valid session, the call fails with HTTP 401 before the body runs. The client sees that
as `ServerFnError::ServerError { code: 401, .. }`, and `auth::api::is_unauthorized` detects it, so
the UI can route to the sign-in screen. No server function takes a user id from the client.

Errors reaching the client carry only a status and a short generic message. Details are logged
server-side: which check failed, and database errors.

## Threat model

| Threat | Mitigation | Tested by |
|---|---|---|
| **Session fixation** | New random session id on every sign-in; old session deleted; unknown ids never adopted (the store draws a fresh id); `__Host-` cookie cannot be set by subdomains | `the_session_id_changes_on_sign_in`, `google_sign_in_creates_then_finds_the_account_by_sub` |
| **Session theft (DB copy, XSS)** | Only SHA-256 of ids stored; `HttpOnly`; signed cookie; idle 14 d and absolute 30 d expiry; server-side sign-out | `session_ids_are_stored_hashed`, `sign_out_deletes_the_session_server_side`, `an_expired_session_is_401`, `a_session_past_the_absolute_timeout_is_401_and_deleted` |
| **A lost or shared device still signed in** | `sign_out_everywhere` (`POST /api/auth/sign-out-everywhere`, #103) deletes every session of the user, this one included, and clears this device's cookie. It only takes access away, so it asks for no step-up. | `sign_out_everywhere_ends_every_session_of_the_user_and_only_theirs` |
| **CSRF** | `SameSite=Lax` + `Sec-Fetch-Site`/`Origin` check on every non-safe method; state changes only via `POST` | `csrf::tests`, `cross_site_posts_are_refused_without_side_effects` |
| **Forged billing events** (a cross-site or scripted `POST /webhooks/stripe`) | The route is outside the CSRF and session layers by design, so it must authenticate every request itself: Stripe signature (HMAC-SHA256, constant-time compare, 5-minute tolerance) and event-id deduplication, see `docs/billing.md`. Today it is a stub: it answers `501` and changes nothing, with a 256 KiB body limit | `billing::tests::stripe_deliveries_are_not_blocked_by_the_csrf_check`, `billing::tests::the_exemption_is_only_the_webhook_route`, `billing::tests::the_body_limit_is_enforced` |
| **Login CSRF** (victim signed into the attacker's account) | Google: `state` bound to the victim's session, single-use; passkeys: the challenge lives in the victim's session | `a_forged_callback_cannot_log_the_victim_into_the_attackers_account` |
| **Forwarded authorization URL** (attacker starts a flow, victim completes it at Google) | The callback completes only with the ceremony of its own session and matching `state`; no code is ever stored or handed to another session | `a_forwarded_sign_in_url_cannot_sign_the_attacker_in_as_the_victim`, `a_forwarded_link_url_cannot_bind_the_victims_google_to_the_attacker` |
| **Forged callback cancelling a flow** (cross-site navigation to the callback) | The ceremony is consumed only when its own `state` comes back, `?error=` included | `a_forged_callback_cannot_cancel_a_flow_in_progress`, `googles_error_with_the_right_state_ends_the_flow` |
| **Challenge / ceremony replay** | Ceremony consumed with `DELETE … RETURNING`, 5–10 min TTL, bound to the session (and user); WebAuthn signs the challenge; signature counter checked | `a_replayed_sign_in_is_rejected`, `two_concurrent_finishes_of_one_ceremony_cannot_both_succeed`, `concurrent_google_finishes_of_one_ceremony_cannot_both_succeed`, `a_sign_up_ceremony_is_single_use`, `a_google_ceremony_is_single_use`, `an_expired_ceremony_is_rejected`, `a_failed_verification_still_uses_up_the_ceremony` |
| **Passkey without user verification** | UV required at registration and sign-in | `user_verification_is_required` |
| **Account creation time leaking to authenticators** | The WebAuthn user handle is a random UUIDv4 per user, not the (UUIDv7) user id | `the_webauthn_user_handle_is_random_stable_and_not_the_user_id` |
| **Credential/user mismatch** (assertion with another user's handle) | Lookup by credential id *and* user handle | `a_user_handle_pointing_at_another_account_is_rejected` |
| **Account linking hijack** | Linked by `sub` only, never email; `(provider, subject)` unique; linking refuses a `sub` owned by another account; link ceremonies bound to the initiating user | `linking_cannot_take_over_another_accounts_google`, `a_link_ceremony_cannot_be_finished_by_another_user`, `google_sign_in_creates_then_finds_the_account_by_sub` |
| **Token substitution** (ID token for another client, issuer or user) | `aud` = exactly our client id (and `azp` if present), `iss` = Google, RS256 signature against Google's JWKS (no HMAC algorithms), `nonce` bound to the ceremony, `exp`/`iat`; code bound to our PKCE verifier | `google_rejects_a_token_for_another_client`, `…_shared_with_another_audience`, `…_from_another_issuer`, `…_signed_with_an_unknown_key`, `google_rejects_an_hs256_token_keyed_with_the_client_secret`, `…_an_expired_token`, `google_rejects_a_nonce_mismatch`, `google_rejects_a_code_bound_to_another_pkce_challenge` |
| **Authorization code interception** (logs, referrer, history) | Our flows: PKCE (the verifier never leaves the server). A flow an attacker started and forwarded: its ceremony is deleted when the victim's callback arrives, so a later leak of that code is useless. `Referrer-Policy: no-referrer`, `no-store`; the code is never put in a page nor stored | `google_rejects_a_code_bound_to_another_pkce_challenge`, `a_forwarded_sign_in_url_cannot_sign_the_attacker_in_as_the_victim`, `google::tests` |
| **Open redirect** | No return-URL parameter anywhere; the callback only ever goes to `/`; the Google redirect URL is fixed by config and validated | `config::tests::google_redirect_url_must_be_the_app_callback` |
| **XSS via the callback page** | Query data JSON-escaped for `<script>` and HTML-escaped; CSP allows only the page's own script by hash | `google::tests::callback_page_is_locked_down`, `script_safe_json_cannot_close_the_script_element` |
| **Locking yourself out** | Last sign-in method cannot be removed (row lock against races) | `add_list_and_remove_passkeys_but_never_the_last_way_in`, `google_as_the_only_method_cannot_be_unlinked` |
| **Deleted user keeps access** | Sessions, identities and ceremonies cascade from `users`; a deleted session is never resurrected | `deleting_a_user_deletes_their_auth_rows`, `saving_a_deleted_session_does_not_resurrect_it` |
| **Secrets in logs** | `secrecy` wrappers; generic client errors; a per-event filter drops every `webauthn_rs*` event below `info`, after and independently of `RUST_LOG` (they log credential ids and public keys at `debug`, challenges and registrations at `trace`) | `a_nasty_database_password_never_reaches_the_logs`, `error::tests`, `logging::tests` |
| **Credential stuffing / password spraying** | **Not applicable**: there are no passwords. Passkeys are phishing-resistant and origin-bound | — |
| **Brute force / resource exhaustion** on the begin endpoints | Per-IP limits on every begin and finish function and the callback, checked after the CSRF check (so a cross-site page cannot spend a shared IP's limits) and before the session or the database is touched, plus per-user limits when signed in ([rate-limiting.md](rate-limiting.md)): each IP can create at most 30 sessions and ceremonies at once, then one every 2 s. A new begin deletes the ceremony it replaces (one row per kind per session for sequential requests; concurrent begins on one cookie can each leave a row until cleanup, within the same per-IP limit); ceremonies and signed-out sessions are short-lived and cleaned up | `a_limited_begin_creates_no_session_and_no_ceremony`, `a_new_begin_replaces_the_previous_ceremony_row`, `the_sign_in_limit_is_per_client_behind_the_proxy` |
| **Credential id existence oracle** | **Accepted.** `credential_id` is unique across all accounts, so registering an id that exists gets 409. The WebAuthn spec says a relying party should reject a credential id already registered to any user; ids are random, chosen by the authenticator, and only ever sent to their owner (in `excludeCredentials`) | `add_list_and_remove_passkeys_but_never_the_last_way_in` |

## Creating the Google OAuth client

In the [Google Cloud console](https://console.cloud.google.com/):

1. Create a project, e.g. "Iron Oxide".
2. **Google Auth Platform → Branding** (the OAuth consent screen): app name "Iron Oxide", a
   support email, and `iron-oxyde.com` under authorized domains (the registrable domain; it
   covers `app.iron-oxyde.com`). **Audience**: External.
   **Data access**: no scopes need adding. The app asks for `openid` only, which needs no Google
   verification. Publish the app ("In production") when it goes live. In "Testing", only the
   listed test users can sign in.
3. **Clients → Create client → Web application**. Use one client per environment, so the
   production secret never sits on a laptop:
   - **Local:** name "Iron Oxide (local)". Authorized redirect URI:
     `http://localhost:8080/auth/google/callback`.
   - **Production:** name "Iron Oxide". Authorized redirect URI:
     `https://app.iron-oxyde.com/auth/google/callback`.
   - No "Authorized JavaScript origins" are needed: the flow runs on the server.
4. Copy the client ID and secret into `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET`. Set
   `GOOGLE_REDIRECT_URL` to the exact redirect URI registered for that client. The server refuses
   to start if it is not `APP_BASE_URL`'s origin followed by `/auth/google/callback`.

## Running it locally

```sh
make env   # .env from .env.example, with a freshly generated SESSION_KEY
# In .env: set GOOGLE_CLIENT_ID / GOOGLE_CLIENT_SECRET (the local client above)
make dev   # Postgres, migrations, dx serve
```

Open **http://localhost:8080**, not 127.0.0.1: the RP ID is `localhost`, and WebAuthn requires the
page's host to match it.
- Browsers treat `localhost` as a secure context, so passkeys work over plain http there.
- The session cookie is not `Secure` locally, since not every browser accepts `Secure` cookies
  over plain http.
- Passkeys created on `localhost` only work on `localhost`.

To try the installed-PWA flow on a phone you need https on the real domain. Passkeys registered
on a temporary host (e.g. the Fly default hostname) will not carry over to `iron-oxyde.com`.
Passkeys made on `app.iron-oxyde.com` (RP ID `iron-oxyde.com`) keep working on any other
`*.iron-oxyde.com` host the app may move to.

## Production settings

| Variable | Value |
|---|---|
| `APP_BASE_URL` | `https://app.iron-oxyde.com` |
| `WEBAUTHN_RP_ID` | `iron-oxyde.com` (the parent domain, not the app's host) |
| `WEBAUTHN_ORIGIN` | `https://app.iron-oxyde.com` |
| `GOOGLE_REDIRECT_URL` | `https://app.iron-oxyde.com/auth/google/callback` |
| `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET` | the production client (secret) |
| `SESSION_KEY` | `openssl rand 64 \| openssl base64 -A`, generated for production only (secret) |

Set the secrets as platform secrets (e.g. `fly secrets set`), never in a file in the repository.
Rotating `SESSION_KEY` signs everyone out.

The server links the system OpenSSL through `webauthn-rs`. A build image needs `libssl-dev` and
`pkg-config`. The runtime needs `libssl3`, which the distroless `cc` image includes.
