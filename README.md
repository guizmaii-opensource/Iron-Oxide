# Iron-Oxide
Zero-cost gains. The only overhead is the barbell

A strength-training PWA written in Rust with [Dioxus](https://dioxuslabs.com) fullstack.

How we work (tickets, pull requests, reviews, keeping branches up to date): see [CONTRIBUTING.md](CONTRIBUTING.md).

## Layout

| Path | What |
|---|---|
| `crates/iron-oxide-domain` | Pure domain logic. No Dioxus, web-sys or sqlx dependencies; tests run with plain `cargo test -p iron-oxide-domain`. |
| `crates/iron-oxide-app` | The Dioxus fullstack app. The `web` feature builds the browser client (wasm32); the `server` feature builds the axum server (SSR, server functions, `/healthz`, `/readyz`). |
| `crates/iron-oxide-app/migrations` | SQL migrations, embedded in the server and applied at startup. |
| `.sqlx/` | Offline query metadata for the sqlx macros (see "Database"). |
| `docker-compose.yml` | Local Postgres for development and tests. |
| `programs/` | Built-in training programs (JSON), embedded in the domain crate. |
| `landing/` | The static landing page of iron-oxyde.com (plain HTML and CSS), deployed to GitHub Pages. See "Landing page". |
| `schemas/program.schema.json` | JSON Schema of a program document, generated from the domain types. Regenerate with `make schema`; a test fails when it is stale. |
| `Makefile` | Entry point for every dev, test, build and deploy task (`make` lists them). |
| `scripts/` | Helpers the Makefile and CI call: `setup.sh` (tool check and install), `smoke-test.sh` (release bundle smoke test), `check-postgres-tests.sh` (every Postgres test ran), `test-make-guards.sh` (the Makefile's own guards). |

## Quick start

```sh
make setup   # pinned Rust toolchain + wasm target, dx, sqlx-cli; lists anything else missing (Docker...)
make dev     # .env, local Postgres, migrations, then hot reload on http://localhost:8080
make check   # everything CI runs, before pushing
```

Every task goes through `make`: run `make` alone for the list below. It works with macOS's
`/usr/bin/make` (3.81) and GNU make 4. Commands fail at the first error and are not echoed; `V=1`
echoes them (database URLs are passed through the environment, so they never show).

```text
Setup
  help             List the targets
  setup            Check and install the pinned toolchain, dx and sqlx-cli (DRY_RUN=1: only check)
  env              Create .env from .env.example if missing, with a new SESSION_KEY (PRESET=localhost|lan|tailscale)
  versions         Show the pinned versions and the installed ones

Dev
  dev              Start Postgres, apply the migrations and run `dx serve` with hot reload
  run              Same as dev
  run-release      Build the release bundle and run its server (service worker on) with ./.env
  db-up            Start the local Postgres (compose project COMPOSE_PROJECT, port PG_PORT)
  db-down          Stop the local Postgres, keeping its data
  db-reset         Delete the local Postgres data and start afresh (CONFIRM=1)
  db-psql          Open psql in the Postgres container (DB=iron_oxide_test for the test database)
  migrate          Apply the migrations to DATABASE_URL (from the environment, else .env)
  sqlx-prepare     Regenerate the offline query data in .sqlx/ (commit it)
  schema           Regenerate schemas/program.schema.json from the domain types (commit it)
  icons            Regenerate the PNG icons from the SVG sources (rsvg-convert, ImageMagick)

Quality
  compile          Type-check everything, fast: the workspace, the server and the wasm client
  test             Run the unit tests (no database)
  test-make        Test the Makefile's own safety guards (deploy, clean, check, test)
  test-db          Run the Postgres tests and check .sqlx/ (starts the compose database if needed)
  test-all         Run all the tests
  fmt              Format the code
  fmt-check        Check the formatting, as CI does
  lint             Run clippy exactly as CI does (warnings are errors)
  sqlx-check       Build with SQLX_OFFLINE=true: fails if .sqlx/ is missing a query
  smoke            Build the release bundle, run it against Postgres and check it over HTTP
  secrets          Scan the commits not on origin/main for secrets (needs gitleaks)
  check            Run everything CI runs, in CI order (starts the compose database)

Build & deploy
  build            Build the release bundle (dx bundle --web --release)
  docker-build     Build the production Docker image (IMAGE=iron-oxide)
  docker-run       Run the Docker image against the local Postgres, on DOCKER_PORT
  deploy           Redeploy origin/main to Fly.io after `make check` (CONFIRM=1)
  logs             Tail the production logs on Fly.io

Mobile
  adb-reverse      Forward the Android device's localhost:APP_PORT here (SERIAL=... picks a device)
  android-open     Open the app in the Android device's browser (after adb-reverse)
  ios-open         Open the app in the booted iOS Simulator
  tailscale-serve  Publish the app over HTTPS to your tailnet (tailscale serve)
  tailscale-reset  Stop publishing the app to your tailnet

Landing page
  landing-build    Assemble the landing page into LANDING_OUT (dist/landing)
  landing          Preview the landing page on http://localhost:LANDING_PORT (8000)

Misc
  clean            Delete the build output: cargo's target dir (the one in use) and dx's output
  prune            Delete build artefacts older than PRUNE_DAYS (14) or of removed toolchains (PRUNE_MAXSIZE=10GB caps)
  clean-all        clean, and delete the local Postgres data (CONFIRM=1)
```

Useful variables, set on the command line (`make test-db PG_PORT=5444`):

| Variable | Default | What |
|---|---|---|
| `V` | `0` | `1` echoes every command |
| `CONFIRM` | `0` | `1` is required by `db-reset`, `clean-all` and `deploy` |
| `CONFIRM_SHARED` | `0` | `1` is required by `clean` and `clean-all` when `CARGO_TARGET_DIR` is outside the checkout (it may be shared) |
| `SKIP_SECRETS` | `0` | `1` lets `check` run without gitleaks (never allowed for `deploy`) |
| `COMPOSE_PROJECT`, `PG_PORT` | `iron-oxide`, `5433` | Compose project and host port of the local Postgres: set both to run a second, independent database |
| `APP_PORT` | `8080` | Port of `dev`, `run-release`, `smoke` and the phone helpers |
| `DX_ARGS` | | Extra `dx serve` arguments for `dev` |
| `LOCKED` | `--locked` | Empty to let cargo update `Cargo.lock` |
| `DRY_RUN` | `0` | `1`: `setup` only checks |
| `LANDING_OUT`, `LANDING_PORT` | `dist/landing`, `8000` | Output directory of `landing-build`, port of the `landing` preview |

The Makefile puts `~/.cargo/bin` first on the `PATH` (see "Toolchain"), and on macOS, while the
Xcode license is not accepted, points `DEVELOPER_DIR` at the Command Line Tools so git and the
linker keep working.

## Pinned versions

Everything is pinned to its latest stable release and kept current by Renovate (`renovate.json`).

| Tool | Where it is pinned |
|---|---|
| Rust (stable) + the `wasm32-unknown-unknown` target | `rust-toolchain.toml` |
| Dioxus | `dioxus` in `Cargo.toml`, pinned exactly with `=` |
| `dx` (Dioxus CLI) | `.github/workflows/ci.yml` and the Dockerfile; locally, `make setup` installs the `dioxus` version from `Cargo.lock` |
| `sqlx-cli` | `.github/workflows/ci.yml`; locally, `make setup` installs the `sqlx` version from `Cargo.lock` |

The `dx` version must always equal the `dioxus` crate version, and `sqlx-cli` the `sqlx` one.
Renovate bumps them together in one PR, through the `# renovate: datasource=crate` markers. The
Makefile writes no version: it reads them from `rust-toolchain.toml` and `Cargo.lock`, and `make
versions` compares them with what is installed.

## Toolchain

`rust-toolchain.toml` is only honoured when `cargo` is the **rustup** proxy. If Homebrew's `rust`
is installed, its `cargo` ignores the file and uses whatever version Homebrew ships. Check with:

```sh
which cargo        # should be ~/.cargo/bin/cargo, not /opt/homebrew/bin/cargo
cargo --version    # should print the version pinned in rust-toolchain.toml
```

If it doesn't, put `~/.cargo/bin` first on your `PATH` (or `brew uninstall rust`). The Makefile
already does, and `make setup` installs the pinned toolchain, its components and the wasm target.

### wasm `--cfg=web_sys_unstable_apis`

`.cargo/config.toml` passes `--cfg=web_sys_unstable_apis` to wasm32 builds. `web-sys` only exposes
unstable browser APIs behind this cfg, and the rest timer needs one of them: the
[Screen Wake Lock API](https://developer.mozilla.org/docs/Web/API/Screen_Wake_Lock_API), which keeps
the phone screen on during a session. The app refuses to compile for wasm without it, so a build
that bypasses the config fails loudly.

> **`RUSTFLAGS` replaces `.cargo/config.toml`'s `rustflags`; it does not add to them.** If you set
> `RUSTFLAGS` (or `CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS`) for a build that includes the
> wasm client, for example in a Dockerfile or a CI step, it must also contain
> `--cfg=web_sys_unstable_apis`.

## Configuration

The server reads its configuration from environment variables at startup
(`crates/iron-oxide-app/src/server/config.rs`). For local development, `make env` copies the
template `.env.example` to `.env` if there is none yet (`make dev` does it too), with a freshly
generated `SESSION_KEY`; fill in the Google client (see [docs/auth.md](docs/auth.md)).

`.env` is git-ignored and optional, and only read from the working directory (not its parents);
real environment variables take precedence over it. A malformed `.env` stops the server with the
line number only, never the line's content. Never commit real values: `.env.example` holds
placeholders only.

In `DATABASE_URL`, percent-encode special characters in the user name and password (`/` as
`%2F`, `@` as `%40`, `#` as `%23`, `?` as `%3F`, `%` as `%25`). An unencoded one splits the URL in
the wrong place, so the server rejects such URLs rather than risk logging part of the password.
Logs only ever show `host:port/database`.

Query parameters are limited to the ones sqlx supports: `sslmode` and the TLS file options,
`statement-cache-capacity`, `dbname`, `user`, `password`, `application_name` and `options`, plus
the Neon ones below. Anything else, including `host`, `hostaddr` and `port` (the URL names the only
host), stops the server with "unsupported parameter".

`WEBAUTHN_ORIGIN` and `GOOGLE_REDIRECT_URL` must use `https://`, except on `localhost`,
`127.0.0.1` or `[::1]`.

| Variable | Required | What |
|---|---|---|
| `DATABASE_URL` | yes | Postgres connection URL (secret: it embeds the password) |
| `APP_BASE_URL` | yes | Public URL of the app, e.g. `http://localhost:8080`. `https://` required, except on `localhost`/`127.0.0.1` |
| `IP`, `PORT` | no | Bind address, default `127.0.0.1:8080`. `dx serve` sets them itself |
| `RUST_LOG` | no | Log filter, e.g. `info,sqlx=warn` |
| `SHUTDOWN_GRACE_SECS` | no | Time in-flight requests get after a shutdown signal, 1 to 300, default 20. Keep it below the platform's kill timeout |
| `WEBAUTHN_RP_ID` | yes | Passkeys: the relying party domain (`localhost` locally; production: see docs/auth.md). The app is served at `https://app.iron-oxyde.com` |
| `WEBAUTHN_ORIGIN` | yes | Passkeys: the origin of `APP_BASE_URL` (must be equal) |
| `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET` | yes | Sign in with Google: the OAuth client (secret) |
| `GOOGLE_REDIRECT_URL` | yes | `APP_BASE_URL`'s origin + `/auth/google/callback` (must be equal) |
| `SESSION_KEY` | yes | Session cookie signing key (secret), ≥ 64 random bytes in base64: `openssl rand 64 \| openssl base64 -A` |
| `CLIENT_IP_SOURCE` | no | Client IP for the rate limits: `peer` (default, the TCP peer) or `fly` (`Fly-Client-IP`, behind Fly.io's proxy only). See [docs/rate-limiting.md](docs/rate-limiting.md) |
| `STRIPE_WEBHOOK_SECRET` | no | Stripe webhook signing secret (secret). Unused until billing is implemented, see [docs/billing.md](docs/billing.md) |

Sign-in (passkeys, Google, sessions) is described in [docs/auth.md](docs/auth.md), including how to
create the Google OAuth client. Rate limits (per IP and per user) are described in
[docs/rate-limiting.md](docs/rate-limiting.md). Every value is validated at startup. If anything is missing or invalid, the
server prints one line per problem, naming the variable (never its value), and exits with status 1.
Secrets are redacted from `Debug` output and logs.

## Database

Postgres 18 (the same major version as the Neon project) runs locally with Docker Compose
(Compose 2.23 or newer), with a dev database `iron_oxide` and a test database `iron_oxide_test`:

```sh
make db-up     # start, on localhost:5433
make db-psql   # psql in the container (DB=iron_oxide_test for the test database)
make db-down   # stop, keeping the data
make db-reset CONFIRM=1   # delete the data and start afresh
```

It listens on port 5433 so it does not clash with a local Postgres on 5432. `PG_PORT` picks
another port, and `COMPOSE_PROJECT` another compose project (an independent database); change
`DATABASE_URL` in `.env` to match for `dev` and `migrate`. The make targets only ever delete the
data of their own compose project.

The schema, the repository layer and how users' data is kept apart are described in
[`docs/database.md`](docs/database.md).
Server functions (errors, idempotency, the endpoint test harness and the isolation tests every
endpoint needs) follow [`docs/api.md`](docs/api.md). The account export, import and deletion (GDPR)
are described in [`docs/export-format.md`](docs/export-format.md).

### Migrations

Migrations live in `crates/iron-oxide-app/migrations/` and are embedded in the server binary
(`sqlx::migrate!()`). The server applies any pending ones at startup, before serving requests;
there is no separate migration step to run on deploy. `make migrate` applies them to the database
of `DATABASE_URL` (`make dev` does it first, so the sqlx macros can check queries against it). To
add one (`make setup` installs `sqlx-cli`):

```sh
sqlx migrate add <name> --source crates/iron-oxide-app/migrations
```

### Offline query data (`.sqlx/`)

`sqlx::query!` macros check queries against a real database at compile time. So that CI, the
Docker build and anyone without a running database can still compile, the query metadata is
committed in `.sqlx/`, and CI builds with `SQLX_OFFLINE=true`. The Makefile sets it too, except for
`dev`, `sqlx-prepare` and the `.sqlx/` check of `test-db`, which compile against the live
database. After adding or changing a query:

```sh
make sqlx-prepare   # starts and migrates the database, then `cargo sqlx prepare`
git add .sqlx
```

`make sqlx-check` and CI fail if `.sqlx/` is missing a query; `make test-db` and CI also fail if
it holds a stale one.

Outside make, whenever `DATABASE_URL` is set (including from `.env`), the macros check queries
against that live database instead of `.sqlx/`, so its schema must be migrated (`make migrate`),
or build with `SQLX_OFFLINE=true`.

### Tests that need Postgres

They are marked `#[ignore = "needs Postgres"]`, so `make test` and plain `cargo test` skip them.
`make test-db` starts the compose database, applies the migrations to `iron_oxide_test`, checks
`.sqlx/`, then runs them; each `#[sqlx::test]` creates, and then drops, its own database (see
"Tests" in [`docs/database.md`](docs/database.md)). It then fails if any of them silently did not
run (`scripts/check-postgres-tests.sh`). `make test-all` runs both. To use another database, set
`TEST_DATABASE_URL` (as CI does): no compose database is started then.

### Neon (production)

- Use the **direct** endpoint for `DATABASE_URL`: the host **without** `-pooler` (decision #39).
  The startup migrations hold a session-level advisory lock, which Neon's transaction-mode pooler
  (PgBouncer) cannot keep across transactions. The app's own pool is small (5 connections), so it
  does not need Neon's pooler.
- Keep `?sslmode=require` (or `verify-full`). Neon's connection strings can be pasted as they are.
  - `options=endpoint%3D...` and `application_name` are passed to sqlx.
  - `channel_binding`, `connect_timeout` and `sslnegotiation` are accepted but removed before
    the URL reaches sqlx, which does not support them. TLS still applies through `sslmode`, and
    the app bounds each connection attempt itself.
- The Neon project must run the same Postgres major version as `docker-compose.yml` and CI (18).
- Pool settings follow Neon's advice: at most 5 connections, none kept while idle, idle
  connections closed after 2 minutes, every connection recycled after 5 minutes, and the first
  connection retried with backoff (up to 6 attempts) while a suspended compute wakes up.
- Point the platform's frequent health check at `/healthz`, which never queries the database.
  `/readyz` runs `SELECT 1`, so polling it keeps the Neon compute awake and uses up the Free
  plan's compute hours.

## Develop

```sh
make dev
```

This creates `.env` if needed, starts Postgres, applies the migrations, then runs
`dx serve --web -p iron-oxide-app`: it builds the client and the server, and serves the app with
hot reload on http://localhost:8080 (use `localhost`, not `127.0.0.1`: passkeys are bound to it).
The page has a button that calls the `server_time` server function (`GET /api/server-time`).
Probes:

- `GET /healthz`: liveness, always `200 ok` while the process serves HTTP. It never touches the
  database, so the platform can poll it often without keeping a scale-to-zero Neon compute awake.
- `GET /readyz`: readiness, `200 ok` when Postgres answers `SELECT 1` within 2 seconds, `503`
  otherwise (the cause is logged, not returned).

On SIGINT (Ctrl-C, and Fly's default kill signal) or SIGTERM (Docker's), the server stops accepting
connections and lets in-flight requests finish, then closes the database pool and exits with
status 0. The drain is bounded by `SHUTDOWN_GRACE_SECS` (default 20 s, below the 30 s
`kill_timeout` in `fly.toml`): past it, remaining connections, such as a client that never
finishes its request, are dropped and the exit status is 1. A second signal stops the server at
once.

### Shared server state in server functions

At startup the server loads the config, connects to Postgres and applies the migrations, then
attaches an `AppState` (config and connection pool) to every request as an axum `Extension`.
A server function takes it as an extra, server-only argument after the route:

```rust
#[cfg(feature = "server")]
use {crate::server::AppState, dioxus::server::axum::Extension};

#[get("/api/me", state: Extension<AppState>)]
pub async fn me() -> Result<Profile, ServerFnError> {
    let pool: &sqlx::PgPool = &state.db;
    // ... query with `pool`, scoped to the signed-in user.
}
```

`State<AppState>` does not work there, because Dioxus uses the axum router state for itself.

### The signed-in user in server functions

Take the server-only `AuthUser` argument: it reads the user from the server-side session and
rejects the call with `401` when signed out, before the body runs. Never accept a user id from the
client. Functions that change state must be `#[post]` (the CSRF check covers every non-`GET`).

```rust
#[cfg(feature = "server")]
use {crate::server::{AppState, auth::AuthUser}, dioxus::server::axum::Extension};

#[post("/api/sets", state: Extension<AppState>, user: AuthUser)]
pub async fn save_set(set: NewSet) -> Result<(), ServerFnError> {
    let user_id = user.user_id(); // scope every query by it
    // ...
}
```

On the client, `crate::auth::api::is_unauthorized(&error)` tells a 401 apart from other errors.

### Plan gating in server functions

Gate a feature with `server::entitlements::require(&state.db, user, Feature::…)`: it reads the
user's plan from `users.plan` and fails with `403` when the plan does not include it. A write that
takes a quota slot (creating, copying or unarchiving a program) calls
`server::entitlements::reserve_quota(&mut tx, user, Quota::…)` first, in the transaction that
writes: it locks the user's row, counts and checks under that lock. The policy itself is
`iron_oxide_domain::entitlements`, the only code that decides what a plan may do; never compare a
plan anywhere else. See [docs/billing.md](docs/billing.md).

### Checks

`make check` runs what CI runs, in the same order. Each CI job calls one target:

| CI job | Target | What |
|---|---|---|
| Secret scan (gitleaks) | `make secrets` | gitleaks on the commits not on `origin/main` (`check` fails without gitleaks, unless `SKIP_SECRETS=1`) |
| rustfmt | `make fmt-check` | `cargo fmt --all --check` (`make fmt` formats) |
| clippy | `make lint` | clippy with `-D warnings`: the workspace, the app with `server`, the app with `web` on wasm32 |
| Unit tests | `make test`, `make test-make` | the workspace, the app with `server`, the domain crate alone, the service worker tests (Node.js; fails if there are none), then the Makefile's own guards |
| sqlx offline build | `make sqlx-check` | `cargo check` with `SQLX_OFFLINE=true` |
| Integration tests (Postgres) | `make test-db` | migrations, `cargo sqlx prepare --check`, the `--ignored` tests |
| dx bundle + smoke test | `make smoke` | `make build`, then `scripts/smoke-test.sh` runs the server against Postgres and checks it over HTTP |

`make compile` is the fast type-check while working: the workspace, the server and the wasm client.

`clippy::unwrap_used`, `clippy::expect_used` and `clippy::panic` are denied workspace-wide, but
allowed in tests (`clippy.toml`).

## PWA

The app is an installable Progressive Web App.

| Path (in `crates/iron-oxide-app`) | What |
|---|---|
| `public/manifest.webmanifest` | Web app manifest, served at `/manifest.webmanifest` |
| `public/sw.js` | Service worker, served at `/sw.js` so that its scope is the whole app |
| `public/icons/`, `public/favicon.ico` | Generated icons (192, 512, maskable 512, apple-touch 180, favicon) |
| `icons/` | SVG sources for the icons, plus `render.sh` to regenerate them: `make icons` (needs `rsvg-convert` and ImageMagick) |
| `public/fonts/` | Self-hosted fonts (woff2) with their SIL OFL licences, precached by the service worker |
| `assets/app.css` | The only stylesheet: design tokens (`--io-*`, dark and light), fonts, shell and components; documented in [docs/palette.md](docs/palette.md) |
| `src/pwa.rs` | Manifest link, icons and iOS meta tags in `<head>`, plus service worker registration |

`dx` copies `public/` unchanged to the root of the site. The service worker is only registered in
release builds, because `dx serve` rebuilds constantly and serves an unhashed `/wasm/` folder. To test
the PWA locally, run `make run-release` and open http://localhost:8080. Chrome and
Safari treat `localhost`/`127.0.0.1` as a secure context, so no HTTPS is needed.

The page registers the worker as `/sw.js?build=<id>`, where the id is derived from the hashed asset
URLs. Every deploy that changes the wasm, JS or CSS therefore installs a fresh worker with its own
cache, and the old cache is deleted. Bump `CACHE_VERSION` in `sw.js` when only `sw.js` or an
unhashed file in `public/` (icons, manifest) changes. What the service worker caches:

- Hashed files under `/assets/` (wasm, JS, CSS): cache-first. They never change for a given URL.
- The icons and the manifest are precached at install, and so is an anonymous render of `/`,
  fetched without cookies.
- Page navigations: network-first. When offline, the cached shell is served instead. Navigation
  responses are never cached.
- Anything that is not a same-origin `GET`, and everything under `/api/` or `/auth/`: network-only,
  never cached.
- Only complete, direct responses with a non-HTML `Content-Type` are cached (status 200, not
  redirected; a missing `Content-Type` is not cached). `Range` requests go straight to the network.
  Unit tests (`node:test`, part of `make test`): `crates/iron-oxide-app/tests/sw/`.

The server answers `404` (with `Cache-Control: no-store`) for unknown `/assets/…` paths
(`src/pwa/missing_assets.rs`) instead of the SSR page that Dioxus serves for every other unknown path.

To try the app on the iOS Simulator, the Android Emulator or a real phone, see [docs/dev/mobile-testing.md](docs/dev/mobile-testing.md).

## Release build

```sh
make build         # dx bundle --web --release -p iron-oxide-app
make run-release   # build, then run the server with ./.env
```

The output is `target/dx/iron-oxide-app/release/web/` (under `CARGO_TARGET_DIR` if set): a
`server` binary and the `public/` client assets next to it. The server serves `public/` from next
to its binary, reads its configuration from the environment (and `./.env`), and binds to the `IP`
and `PORT` environment variables:

```sh
IP=0.0.0.0 PORT=8080 DATABASE_URL=... APP_BASE_URL=... target/dx/iron-oxide-app/release/web/server
```

## Docker and deployment

```sh
make docker-build       # the production image, tagged iron-oxide
make docker-run         # run it on http://localhost:8080 against the local Postgres (local sign-in settings)
make deploy CONFIRM=1   # manual redeploy of origin/main (see below)
make logs               # fly logs
```

The production configuration, including the six sign-in variables, lives in Fly secrets; they must
all be set before deploying, or the new machines refuse to start (see
[docs/operations/deploy.md](docs/operations/deploy.md)). Pushes to `main` deploy through GitHub
Actions. `make deploy` is for a manual redeploy: it only
runs on a clean `main` at exactly `origin/main` (it fetches first) whose CI run passed (checked
with `gh`), runs `make check` including the secret scan, checks the checkout again, then runs
`fly deploy` from a pristine `git archive` of that commit in a temporary directory, so no local
file (ignored, excluded or skip-worktree) reaches the build. The image is built on Fly's builder,
not taken from CI: the same commit CI tested, but not the same build. First-time setup (Fly app, secrets, deploy token)
and the custom domain: [docs/operations/deploy.md](docs/operations/deploy.md).

## Testing on phones

The app must run on `127.0.0.1:8080` (`make dev` or `make run-release`); the phone reaches it
through a forward:

```sh
make ios-open                          # iOS Simulator: opens the app in the booted simulator
make adb-reverse && make android-open  # Android Emulator or a USB phone
make env PRESET=tailscale FORCE=1      # real iPhone: .env for the Tailscale host (old one kept in .env.bak)
make tailscale-serve                   # ...then publish it over HTTPS to the tailnet; tailscale-reset stops
```

Step-by-step setups (simulators, real phones over Tailscale or mkcert, passkeys, debugging) are in
docs/dev/mobile-testing.md, added by #63.

## Landing page

`landing/` is the static site served at <https://iron-oxyde.com>: one HTML page, one stylesheet, a
small script for the "Copy the prompt" button, the app's self-hosted fonts (with their SIL Open Font
License files) and optimised screenshots. No framework, no build step and no third-party requests.

- `make landing` assembles the site into `dist/landing` and serves it on <http://localhost:8000>
  (`LANDING_PORT` changes the port). `make landing-build` only assembles it.
- **The prompt has one source: `programs/ai-prompt.md`** (#108), which the app shows too.
  `make landing-build` puts it into the page (`scripts/landing-build.py`, between the
  `prompt:begin`/`prompt:end` markers of `landing/index.html`) and fails if the file is missing.
  Edit the prompt in `programs/ai-prompt.md`, never in the page.
- The site also publishes `schemas/program.schema.json` at `/program.schema.json`.
- The build (`scripts/landing-build.py`) also strips HTML and CSS comments from the shipped files
  and fails if the page mentions GitHub, open source or a licence (the fonts' OFL files and the
  JSON Schema are exempt). `LANDING_OUT` must be a plain directory strictly under `dist/`.
- `.github/workflows/landing.yml` deploys it to GitHub Pages on every push to `main` that touches
  `landing/`, the schema, the prompt, the build script, the `Makefile` or the workflow; pull
  requests only build it. `landing/CNAME` holds the
  domain.
- Colours and fonts follow the app's Forge tokens (`crates/iron-oxide-app/assets/app.css`), copied
  at the top of `landing/styles.css`: change both together.
- Screenshots are in `landing/assets/screens/`, one per screen and theme (`*-dark`, `*-light`), taken
  at 390 × 844 at 2× from `make dev` with seeded data: AVIF and WebP at 780 × 1688, plus a 390 × 844
  PNG fallback. The page shows the light ones to visitors whose system uses the light theme.
  `landing/assets/og.jpg` (1200 × 630) is the social preview image.
- The logo is a typographic wordmark until the app icon (#69) is ready. Then put it at
  `landing/assets/icon.svg` (or `icon.png`, square, at least 96 px), uncomment the `<img>` in the
  header of `landing/index.html`, and replace `favicon.ico`, `favicon.svg` and
  `apple-touch-icon.png` in `landing/` (today copies of the PWA icon).

## Disk usage

Rust build output grows fast, so the defaults keep it lean:

- Dev builds (including dx's `server-dev` and `wasm-dev`) keep only line tables for our crates
  (backtraces still show file:line) and no debug info for dependencies (`[profile.dev]` in
  `Cargo.toml`).
- CI and the CI-like make targets (`check`, `lint`, `test`, `build`, ...) build with
  `CARGO_INCREMENTAL=0`; `compile` and `dev` keep incremental compilation for fast rebuilds.
- `make prune` works on the target dir cargo actually uses (`CARGO_TARGET_DIR`,
  `CARGO_BUILD_TARGET_DIR`, else `./target`) and refuses anything that is not a cargo target dir,
  or is `/`, `$HOME`, the checkout or one of its parents. It deletes incremental caches and build
  artefacts older than 14 days (`PRUNE_DAYS`) and artefacts of toolchains that are no longer
  installed (`cargo sweep`, installed by `make setup`). `PRUNE_MAXSIZE=10GB` also caps the size:
  every incremental cache goes first, then the oldest artefacts. The cap counts the whole dir,
  including `dx/` and `doc/`, which it never deletes. `DRY_RUN=1` only lists. It never touches
  sources or `.sqlx/`; `make clean` deletes everything.

## Security

See [SECURITY.md](SECURITY.md) for how to report a vulnerability and for the secrets policy.
Never commit secrets or real `.env` files: CI scans every push and pull request with gitleaks.
To mark a test fixture that gitleaks flags as a false positive, see "Test fixtures that look like
secrets" in SECURITY.md.

## License

Iron Oxide is licensed under the [GNU Affero General Public License v3.0 only](LICENSE)
(`AGPL-3.0-only`). If you run a modified version as a network service, you must offer its users
the corresponding source code.
