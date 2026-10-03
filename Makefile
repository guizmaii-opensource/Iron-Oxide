# Iron Oxide: one entry point for the dev, test, build and deploy tasks.
#
#   make              list the targets (same as `make help`)
#   make test         unit tests, no database
#   make check        everything CI runs, in CI order
#
# Works with GNU make 3.81 (macOS's /usr/bin/make) and 4.x (Linux, CI).
# Variables are set on the command line: `make test-db PG_PORT=5444`, `make build V=1`.
# Versions are never written here: they are read from rust-toolchain.toml and Cargo.lock.

.DEFAULT_GOAL := help
# Targets run one after the other, even under `make -j`: cargo parallelises on its own, and the
# composite targets (`check`, `dev`) rely on their order.
.NOTPARALLEL:

# --- Shell: fail fast ---------------------------------------------------------------------------
# bash with -e (stop at the first failing command), -u (an unset variable is an error) and
# -o pipefail. make 3.81 ignores .SHELLFLAGS, so there the flags go into SHELL itself.
ifneq ($(filter 3.%,$(MAKE_VERSION)),)
SHELL := bash -eu -o pipefail
else
SHELL := bash
.SHELLFLAGS := -eu -o pipefail -c
endif

# Quiet by default; `V=1` echoes every command. Secrets are never on a command line (recipes pass
# them through environment variables), so V=1 shows variable names, not values.
V ?= 0
ifeq ($(V),1)
Q :=
else
Q := @
endif
MAKEFLAGS += --no-print-directory

# --- Machine quirks -----------------------------------------------------------------------------
# rustup's cargo must win over Homebrew's: only the rustup proxy honours rust-toolchain.toml.
CARGO_BIN := $(or $(CARGO_HOME),$(HOME)/.cargo)/bin
export PATH := $(CARGO_BIN):$(PATH)

# macOS: while the Xcode license is not accepted, git, cc and the linker fail through the Xcode
# toolchain. The Command Line Tools work regardless, so use them until the license is accepted.
# Once it is, the probe passes and nothing is changed. Probed once; sub-makes inherit the result.
ifeq ($(origin DEVELOPER_DIR)$(IRON_OXIDE_XCODE_PROBED),undefined)
ifeq ($(shell uname -s),Darwin)
ifneq ($(shell xcodebuild -license check >/dev/null 2>&1 || echo failed),)
ifneq ($(wildcard /Library/Developer/CommandLineTools),)
export DEVELOPER_DIR := /Library/Developer/CommandLineTools
endif
endif
endif
export IRON_OXIDE_XCODE_PROBED := 1
endif

# RUSTFLAGS replaces the rustflags of .cargo/config.toml instead of adding to them.
ifneq ($(strip $(RUSTFLAGS)$(CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS)),)
$(warning RUSTFLAGS is set: wasm builds need --cfg=web_sys_unstable_apis in it (see README, "Toolchain"))
endif

# --- Versions, read from their single sources ---------------------------------------------------
# $(call lock-version,crate): the version of `crate` in Cargo.lock (empty if it is not a dependency).
lock-version = $(shell awk '$$0 == "name = \"$(1)\"" { getline; gsub(/^version = "|"$$/, ""); print; exit }' Cargo.lock 2>/dev/null)
RUST_TOOLCHAIN = $(shell sed -n 's/^channel *= *"\([^"]*\)".*/\1/p' rust-toolchain.toml)
# dx must be exactly the version of the dioxus crates.
DX_VERSION = $(call lock-version,dioxus)
# sqlx-cli must match the sqlx crate.
SQLX_VERSION = $(call lock-version,sqlx)

# --- Project layout -----------------------------------------------------------------------------
APP := iron-oxide-app
DOMAIN := iron-oxide-domain
WASM_TARGET := wasm32-unknown-unknown
APP_DIR := crates/$(APP)
MIGRATIONS := $(APP_DIR)/migrations
SW_TESTS := $(wildcard $(APP_DIR)/tests/sw/*.test.mjs)
TARGET_DIR = $(abspath $(or $(CARGO_TARGET_DIR),$(CURDIR)/target))
BUNDLE_DIR = $(TARGET_DIR)/dx/$(APP)/release/web

# `make test LOCKED=` to let cargo update Cargo.lock; CI always passes --locked.
LOCKED ?= --locked
CARGO ?= cargo
DX ?= dx
SQLX ?= sqlx
FLY ?= fly
GH ?= gh
GITLEAKS ?= gitleaks
# Extra arguments for `dx serve` (`make dev DX_ARGS="--port 8081"`).
DX_ARGS ?=
# Port the app listens on locally (dx serve, the release server, the phone helpers).
APP_PORT ?= 8080

# sqlx macros compile from the committed `.sqlx/` metadata, as in CI, so that compiling never
# needs a running database (not even when `.env` sets DATABASE_URL). Only `dev`, `sqlx-prepare`
# and `test-db` (its `prepare --check` step) compile against the live database.
export SQLX_OFFLINE ?= true

# The CI-like targets build without incremental compilation, as CI does: incremental caches take
# many GB and only help the edit-compile loop (`compile`, `dev`), which keeps them. Sub-makes of
# `check` inherit it (including its `sqlx-check`, which `compile` also runs, incrementally). Set
# CARGO_INCREMENTAL yourself to override.
check fmt-check lint test test-db smoke build deploy: export CARGO_INCREMENTAL ?= 0

# --- Local Postgres (docker compose) ------------------------------------------------------------
# Use another project name and port to run a second, independent database:
# `make test-db COMPOSE_PROJECT=my-branch PG_PORT=5444`.
COMPOSE_PROJECT ?= iron-oxide
PG_PORT ?= $(or $(IRON_OXIDE_PG_PORT),5433)
export IRON_OXIDE_PG_PORT := $(PG_PORT)
COMPOSE = docker compose -p $(COMPOSE_PROJECT)
# Throwaway local credentials from docker-compose.yml, not a secret.
LOCAL_PG = postgres://iron_oxide:iron_oxide@localhost:$(PG_PORT)
# Database of `test-db` and of `smoke`/`docker-run`. When one is given from outside (CI's Postgres
# service), no compose database is started for it. The decision is taken by the top-level make and
# passed down, because exported variables look "given from outside" to a sub-make.
ifndef IRON_OXIDE_START_TEST_DB
export IRON_OXIDE_START_TEST_DB := $(if $(filter undefined,$(origin TEST_DATABASE_URL)),1,0)
endif
ifndef IRON_OXIDE_START_SMOKE_DB
export IRON_OXIDE_START_SMOKE_DB := $(if $(filter undefined,$(origin SMOKE_DATABASE_URL)),1,0)
endif
TEST_DATABASE_URL ?= $(LOCAL_PG)/iron_oxide_test
export TEST_DATABASE_URL
# The server applies the migrations itself at startup.
SMOKE_DATABASE_URL ?= $(LOCAL_PG)/iron_oxide
export SMOKE_DATABASE_URL
# Database for `db-psql`.
DB ?= iron_oxide

# Docker image
IMAGE ?= iron-oxide
DOCKER_BUILD_ARGS ?=
# Host port for `docker-run`.
DOCKER_PORT ?= 8080

# --- Helpers ------------------------------------------------------------------------------------
# $(call step,message)
step = printf '==> %s\n' "$(1)"
# $(call need-file,path,where it comes from): fail with a clear message if `path` does not exist.
need-file = test -e $(1) || { echo "make $@: needs $(1)$(if $(2), from $(2)$(comma) which is not on this branch yet)." >&2; exit 1; }
comma := ,
# $(call need-cmd,command,hint)
need-cmd = command -v $(1) >/dev/null 2>&1 || { echo "make $@: '$(1)' is not installed. $(2)" >&2; exit 1; }
# $(call need-confirm,what): refuse unless CONFIRM=1.
need-confirm = test "$(CONFIRM)" = 1 || { echo "make $@: this $(1). Re-run with CONFIRM=1 to go ahead." >&2; exit 1; }
CONFIRM ?= 0
CONFIRM_SHARED ?= 0
SKIP_SECRETS ?= 0
# dx at exactly the dioxus version.
need-dx = $(call need-cmd,$(DX),Run 'make setup'.); \
	have="$$($(DX) --version | awk '{ print $$2 }')"; \
	test "$$have" = "$(DX_VERSION)" || { echo "make $@: dx $$have is installed, but Cargo.lock pins dioxus $(DX_VERSION). Run 'make setup'." >&2; exit 1; }
# sqlx-cli at the sqlx version.
need-sqlx = $(call need-cmd,$(SQLX),Run 'make setup'.); \
	have="$$($(SQLX) --version | awk '{ print $$2 }')"; \
	test "$$have" = "$(SQLX_VERSION)" || { echo "make $@: sqlx-cli $$have is installed, but Cargo.lock has sqlx $(SQLX_VERSION). Run 'make setup'." >&2; exit 1; }
need-docker = $(call need-cmd,docker,See 'make setup'.); \
	docker info >/dev/null 2>&1 || { echo "make $@: Docker is not running. Start Docker Desktop, or 'colima start'." >&2; exit 1; }
need-compose = $(need-docker)

.PHONY: help setup env versions \
	dev run run-release db-up db-down db-reset db-psql migrate sqlx-prepare schema icons \
	compile test test-make test-db test-all fmt fmt-check lint sqlx-check smoke secrets check \
	build docker-build docker-run deploy logs \
	adb-reverse android-open ios-open tailscale-serve tailscale-reset \
	landing landing-build \
	clean clean-all prune

##@ Setup

help: ## List the targets
	@echo 'Usage: make <target> [VAR=value ...]   (V=1 echoes the commands)'
	@awk 'BEGIN { FS = ":.*## " } \
		/^##@ / { printf "\n%s\n", substr($$0, 5); next } \
		/^[a-zA-Z0-9_-]+:.*## / { printf "  %-16s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

setup: ## Check and install the pinned toolchain, dx and sqlx-cli (DRY_RUN=1: only check)
	$(Q)RUST_TOOLCHAIN='$(RUST_TOOLCHAIN)' DX_VERSION='$(DX_VERSION)' SQLX_VERSION='$(SQLX_VERSION)' \
		WASM_TARGET='$(WASM_TARGET)' CARGO_BIN='$(CARGO_BIN)' DRY_RUN='$(DRY_RUN)' scripts/setup.sh
DRY_RUN ?= 0

# A `SESSION_KEY=replace-me` line gets a freshly generated key (never printed).
env: ## Create .env from .env.example if missing, with a new SESSION_KEY (PRESET=localhost|lan|tailscale)
	$(Q)src=.env$(if $(PRESET),.$(PRESET)).example; \
	$(call need-file,$$src,$(if $(PRESET),#62)); \
	if [ -e .env ] && [ "$(FORCE)" != 1 ]; then \
		$(if $(PRESET),echo "make $@: .env exists. FORCE=1 replaces it (the old one is kept as .env.bak)." >&2; exit 1,echo ".env exists: kept as it is."); \
	else \
		if [ -e .env ]; then mv .env .env.bak; echo "Previous .env moved to .env.bak."; fi; \
		umask 077; \
		SESSION_KEY="$$(openssl rand 64 | openssl base64 -A)" \
			awk '$$0 == "SESSION_KEY=replace-me" { print "SESSION_KEY=" ENVIRON["SESSION_KEY"]; next } { print }' \
			"$$src" >.env; \
		echo "Created .env from $$src, with a generated SESSION_KEY. Fill in the other placeholders"; \
		echo "(the Google client: see docs/auth.md); never commit it."; \
	fi
FORCE ?= 0
PRESET ?=

versions: ## Show the pinned versions and the installed ones
	$(Q)printf '%-10s %-12s %s\n' tool pinned installed; \
	printf '%-10s %-12s %s\n' rust '$(RUST_TOOLCHAIN)' "$$($(CARGO) --version 2>/dev/null | awk '{ print $$2 }')"; \
	printf '%-10s %-12s %s\n' dx '$(DX_VERSION)' "$$($(DX) --version 2>/dev/null | awk '{ print $$2 }')"; \
	printf '%-10s %-12s %s\n' sqlx-cli '$(or $(SQLX_VERSION),-)' "$$($(SQLX) --version 2>/dev/null | awk '{ print $$2 }')"

##@ Dev

dev: ## Start Postgres, apply the migrations and run `dx serve` with hot reload
	$(Q)$(MAKE) env db-up migrate
	$(Q)$(need-dx)
	$(Q)$(call step,dx serve: open http://localhost:$(APP_PORT) (passkeys need localhost$(comma) not 127.0.0.1))
	$(Q)SQLX_OFFLINE=false $(DX) serve --web -p $(APP) --port $(APP_PORT) $(DX_ARGS)

run: dev ## Same as dev

run-release: build ## Build the release bundle and run its server (service worker on) with ./.env
	$(Q)$(call step,release server on http://127.0.0.1:$(APP_PORT))
	$(Q)PORT=$(APP_PORT) "$(BUNDLE_DIR)/server"

db-up: ## Start the local Postgres (compose project COMPOSE_PROJECT, port PG_PORT)
	$(Q)$(need-compose)
	$(Q)$(call step,Postgres '$(COMPOSE_PROJECT)' on localhost:$(PG_PORT))
	$(Q)$(COMPOSE) up -d --wait

db-down: ## Stop the local Postgres, keeping its data
	$(Q)$(need-compose)
	$(Q)$(COMPOSE) down

db-reset: ## Delete the local Postgres data and start afresh (CONFIRM=1)
	$(Q)$(need-compose)
	$(Q)$(call need-confirm,deletes every database of compose project '$(COMPOSE_PROJECT)')
	$(Q)$(COMPOSE) down -v
	$(Q)$(COMPOSE) up -d --wait

db-psql: ## Open psql in the Postgres container (DB=iron_oxide_test for the test database)
	$(Q)$(need-compose)
	$(Q)$(COMPOSE) exec postgres psql -U iron_oxide -d $(DB)

migrate: ## Apply the migrations to DATABASE_URL (from the environment, else .env)
	$(Q)$(need-sqlx)
	$(Q)test -n "$${DATABASE_URL:-}" || test -e .env || { echo "make $@: set DATABASE_URL, or run 'make env'." >&2; exit 1; }
	$(Q)$(call step,sqlx migrate run)
	$(Q)$(SQLX) migrate run --source $(MIGRATIONS)

sqlx-prepare: db-up migrate ## Regenerate the offline query data in .sqlx/ (commit it)
	$(Q)$(call step,cargo sqlx prepare)
	$(Q)SQLX_OFFLINE=false $(CARGO) sqlx prepare --workspace -- --all-targets --features $(APP)/server

schema: ## Regenerate schemas/program.schema.json from the domain types (commit it)
	$(Q)UPDATE_SCHEMA=1 $(CARGO) test -p $(DOMAIN) $(LOCKED) program::schema

icons: ## Regenerate the PNG icons from the SVG sources (rsvg-convert, ImageMagick)
	$(Q)$(call need-cmd,rsvg-convert,brew install librsvg)
	$(Q)$(call need-cmd,magick,brew install imagemagick)
	$(Q)sh $(APP_DIR)/icons/render.sh

##@ Quality

compile: ## Type-check everything, fast: the workspace, the server and the wasm client
	$(Q)$(MAKE) sqlx-check
	$(Q)$(call step,cargo check (web on wasm32))
	$(Q)$(CARGO) check -p $(APP) --target $(WASM_TARGET) --features web $(LOCKED)

test: ## Run the unit tests (no database)
	$(Q)$(call step,cargo test (workspace))
	$(Q)$(CARGO) test --workspace $(LOCKED)
	$(Q)$(call step,cargo test (app with the server feature))
	$(Q)$(CARGO) test -p $(APP) --features server $(LOCKED)
	$(Q)$(call step,cargo test (domain crate on its own))
	$(Q)$(CARGO) test -p $(DOMAIN) $(LOCKED)
	$(Q)$(call step,service worker tests (node --test))
	$(Q)test -n "$(SW_TESTS)" || { echo "make $@: no service worker tests found ($(APP_DIR)/tests/sw/*.test.mjs)." >&2; exit 1; }
	$(Q)$(call need-cmd,node,Install Node.js: brew install node)
	$(Q)node --test $(SW_TESTS)

test-make: ## Test the Makefile's own safety guards (deploy, clean, check, test)
	$(Q)$(call step,Makefile guard tests)
	$(Q)scripts/test-make-guards.sh

test-db: ## Run the Postgres tests and check .sqlx/ (starts the compose database if needed)
	$(Q)$(need-sqlx)
ifeq ($(IRON_OXIDE_START_TEST_DB),1)
	$(Q)$(MAKE) db-up
endif
	$(Q)$(call step,sqlx migrate run (test database))
	$(Q)DATABASE_URL="$$TEST_DATABASE_URL" $(SQLX) migrate run --source $(MIGRATIONS)
	$(Q)$(call step,cargo sqlx prepare --check)
	$(Q)DATABASE_URL="$$TEST_DATABASE_URL" SQLX_OFFLINE=false \
		$(CARGO) sqlx prepare --workspace --check -- --all-targets --features $(APP)/server
	$(Q)$(call step,cargo test -- --ignored (tests that need Postgres))
	$(Q)DATABASE_URL="$$TEST_DATABASE_URL" $(CARGO) test --workspace $(LOCKED) -- --ignored
	$(Q)log="$$(mktemp)"; trap 'rm -f "$$log"' EXIT; \
		DATABASE_URL="$$TEST_DATABASE_URL" $(CARGO) test -p $(APP) --features server $(LOCKED) -- --ignored 2>&1 \
		| tee "$$log"; \
		$(call step,every Postgres test ran); \
		scripts/check-postgres-tests.sh "$$log"

test-all: test test-db ## Run all the tests

fmt: ## Format the code
	$(Q)$(CARGO) fmt --all

fmt-check: ## Check the formatting, as CI does
	$(Q)$(call step,cargo fmt --check)
	$(Q)$(CARGO) fmt --all --check

lint: ## Run clippy exactly as CI does (warnings are errors)
	$(Q)$(call step,clippy (no features))
	$(Q)$(CARGO) clippy --workspace --all-targets $(LOCKED) -- -D warnings
	$(Q)$(call step,clippy (app, server))
	$(Q)$(CARGO) clippy -p $(APP) --all-targets --features server $(LOCKED) -- -D warnings
	$(Q)$(call step,clippy (app, web on wasm32))
	$(Q)$(CARGO) clippy -p $(APP) --target $(WASM_TARGET) --features web $(LOCKED) -- -D warnings

sqlx-check: ## Build with SQLX_OFFLINE=true: fails if .sqlx/ is missing a query
	$(Q)$(call step,cargo check (workspace))
	$(Q)SQLX_OFFLINE=true $(CARGO) check --workspace --all-targets $(LOCKED)
	$(Q)$(call step,cargo check (app with the server feature))
	$(Q)SQLX_OFFLINE=true $(CARGO) check -p $(APP) --all-targets --features server $(LOCKED)

smoke: ## Build the release bundle, run it against Postgres and check it over HTTP
	$(Q)$(call need-cmd,psql,brew install libpq (the smoke test checks the migrations ran))
ifeq ($(IRON_OXIDE_START_SMOKE_DB),1)
	$(Q)$(MAKE) db-up
endif
	$(Q)$(MAKE) build
	$(Q)$(call step,smoke test on 127.0.0.1:$(APP_PORT))
	$(Q)DATABASE_URL="$$SMOKE_DATABASE_URL" PORT=$(APP_PORT) scripts/smoke-test.sh "$(BUNDLE_DIR)"

secrets: ## Scan the commits not on origin/main for secrets (needs gitleaks)
	$(Q)$(call need-cmd,$(GITLEAKS),brew install gitleaks (see 'make setup'))
	$(Q)$(GITLEAKS) git --redact --exit-code 1 --log-opts="--remerge-diff $(SECRETS_RANGE)" .
SECRETS_RANGE ?= origin/main..HEAD

check: ## Run everything CI runs, in CI order (starts the compose database)
ifeq ($(SKIP_SECRETS),1)
	$(Q)echo "==> secret scan skipped (SKIP_SECRETS=1)"
else
	$(Q)command -v $(GITLEAKS) >/dev/null 2>&1 || { echo "make $@: gitleaks is not installed, so the secret scan cannot run. Install it (see 'make setup'), or pass SKIP_SECRETS=1 to skip it." >&2; exit 1; }
	$(Q)$(MAKE) secrets
endif
	$(Q)$(MAKE) fmt-check lint test test-make sqlx-check
	$(Q)$(MAKE) test-db smoke
	$(Q)echo "==> check: all green"

##@ Build & deploy

build: ## Build the release bundle (dx bundle --web --release)
	$(Q)$(need-dx)
	$(Q)$(call step,dx bundle --web --release)
	$(Q)$(DX) bundle --web --release -p $(APP)
	$(Q)echo "Bundle: $(BUNDLE_DIR)"

docker-build: ## Build the production Docker image (IMAGE=iron-oxide)
	$(Q)$(need-docker)
	$(Q)docker build -t $(IMAGE) $(DOCKER_BUILD_ARGS) .

# Local sign-in settings: a random session key per run and a placeholder Google client.
docker-run: db-up ## Run the Docker image against the local Postgres, on DOCKER_PORT
	$(Q)$(call step,$(IMAGE) on http://localhost:$(DOCKER_PORT))
	$(Q)DATABASE_URL="$$(printf '%s' "$$SMOKE_DATABASE_URL" | sed 's/@localhost:/@host.docker.internal:/')" \
		SESSION_KEY="$$(openssl rand 64 | openssl base64 -A)" \
		docker run --rm --init -p 127.0.0.1:$(DOCKER_PORT):8080 \
		--add-host=host.docker.internal:host-gateway \
		-e APP_BASE_URL=http://localhost:$(DOCKER_PORT) -e DATABASE_URL \
		-e WEBAUTHN_RP_ID=localhost -e WEBAUTHN_ORIGIN=http://localhost:$(DOCKER_PORT) \
		-e GOOGLE_CLIENT_ID=placeholder.apps.googleusercontent.com -e GOOGLE_CLIENT_SECRET=placeholder \
		-e GOOGLE_REDIRECT_URL=http://localhost:$(DOCKER_PORT)/auth/google/callback \
		-e SESSION_KEY \
		$(IMAGE)

# Deploys only what CI has seen: a clean checkout of `main` at exactly `origin/main`, whose CI run
# passed. `fly deploy` runs from a pristine `git archive` of that commit in a temporary directory,
# never from this working directory, so ignored, excluded or skip-worktree files cannot reach the
# build. The image is built on Fly's builder, not taken from CI. Pushes to main deploy through
# GitHub Actions; this target is for a manual redeploy.
deploy: ## Redeploy origin/main to Fly.io after `make check` (CONFIRM=1)
	$(Q)$(call need-cmd,$(FLY),brew install flyctl)
	$(Q)$(call need-cmd,$(GH),brew install gh (checks that CI passed for the commit))
	$(Q)$(call need-cmd,$(GITLEAKS),brew install gitleaks (see 'make setup'))
	$(Q)$(call need-confirm,deploys to production)
	$(Q)test "$(SKIP_SECRETS)" != 1 || { echo "make $@: SKIP_SECRETS=1 is not allowed for a deploy." >&2; exit 1; }
	$(Q)$(deploy-guard); $(deploy-ci-passed)
	$(Q)$(MAKE) check SECRETS_RANGE=HEAD
	$(Q)$(deploy-guard); $(deploy-ci-passed); \
		src="$$(mktemp -d)"; trap 'rm -rf "$$src"' EXIT; \
		git archive "$$sha" | tar -x -C "$$src"; \
		$(call step,fly deploy of $$sha (pristine export)); \
		cd "$$src" && $(FLY) deploy
# CI's workflow passed on a push of this exact commit. Sets `sha`.
deploy-ci-passed = sha="$$(git rev-parse HEAD)"; \
	ok="$$($(GH) run list --commit "$$sha" --workflow ci.yml --event push --status success --json databaseId --jq length)"; \
	test "$$ok" -gt 0 || { echo "make $@: CI has not passed for $$sha on main yet." >&2; exit 1; }
# On main, with a clean tree, at exactly origin/main (not ahead, behind or diverged). Run before
# and again after `make check`, which takes minutes.
deploy-guard = test "$$(git rev-parse --abbrev-ref HEAD)" = main || { echo "make $@: deploy from main only." >&2; exit 1; }; \
	test -z "$$(git status --porcelain)" || { echo "make $@: the working tree has uncommitted changes." >&2; exit 1; }; \
	git fetch --quiet origin main || { echo "make $@: cannot fetch origin/main." >&2; exit 1; }; \
	test "$$(git rev-parse HEAD)" = "$$(git rev-parse origin/main)" || { echo "make $@: HEAD is not origin/main (ahead, behind or diverged): deploy exactly what is on origin/main." >&2; exit 1; }

logs: ## Tail the production logs on Fly.io
	$(Q)$(call need-cmd,$(FLY),brew install flyctl)
	$(Q)$(FLY) logs

##@ Mobile

adb-reverse: ## Forward the Android device's localhost:APP_PORT here (SERIAL=... picks a device)
	$(Q)$(call need-cmd,adb,Install Android Studio and put platform-tools on the PATH.)
	$(Q)adb $(if $(SERIAL),-s $(SERIAL)) reverse tcp:$(APP_PORT) tcp:$(APP_PORT)
	$(Q)adb $(if $(SERIAL),-s $(SERIAL)) reverse --list
SERIAL ?=

android-open: ## Open the app in the Android device's browser (after adb-reverse)
	$(Q)$(call need-cmd,adb,Install Android Studio and put platform-tools on the PATH.)
	$(Q)adb $(if $(SERIAL),-s $(SERIAL)) shell am start -a android.intent.action.VIEW -d http://localhost:$(APP_PORT)

ios-open: ## Open the app in the booted iOS Simulator
	$(Q)$(call need-cmd,xcrun,Install Xcode.)
	$(Q)xcrun simctl openurl booted http://localhost:$(APP_PORT)

tailscale-serve: ## Publish the app over HTTPS to your tailnet (tailscale serve)
	$(Q)$(call need-cmd,tailscale,brew install --cask tailscale-app)
	$(Q)tailscale serve --bg --https=443 localhost:$(APP_PORT)
	$(Q)tailscale serve status

tailscale-reset: ## Stop publishing the app to your tailnet
	$(Q)$(call need-cmd,tailscale,brew install --cask tailscale-app)
	$(Q)tailscale serve reset

##@ Landing page

# The static site of iron-oxyde.com: landing/ plus the program JSON Schema, which the page's prompt
# points AI assistants at (https://iron-oxyde.com/program.schema.json). The Pages workflow
# (.github/workflows/landing.yml) deploys the same LANDING_OUT.
LANDING_OUT ?= dist/landing
# Port of the local preview (`make landing`).
LANDING_PORT ?= 8000

# LANDING_OUT reaches the recipes through the environment only (never pasted into a shell line):
# scripts/landing-build.py refuses anything but a plain directory strictly under dist/.
landing-build landing: export LANDING_OUT := $(LANDING_OUT)

landing-build: ## Assemble the landing page into LANDING_OUT (dist/landing)
	$(Q)$(call need-cmd,python3,Install Python 3: brew install python)
	$(Q)python3 scripts/landing-build.py

landing: landing-build ## Preview the landing page on http://localhost:LANDING_PORT (8000)
	$(Q)$(call step,landing page on http://localhost:$(LANDING_PORT) (Ctrl-C stops it))
	$(Q)python3 -m http.server $(LANDING_PORT) --bind 127.0.0.1 --directory "$$LANDING_OUT"

##@ Misc

clean: ## Delete the build output: cargo's target dir (the one in use) and dx's output
	$(Q)case "$(TARGET_DIR)/" in \
		"$(CURDIR)/"*) ;; \
		*) test "$(CONFIRM_SHARED)" = 1 || { echo "make $@: this deletes $(TARGET_DIR)$(comma) which is outside this checkout and may be shared. Re-run with CONFIRM_SHARED=1 to go ahead." >&2; exit 1; } ;; \
	esac
	$(Q)$(call step,cargo clean ($(TARGET_DIR)))
	$(Q)$(CARGO) clean
	$(Q)rm -rf dist

# See scripts/prune.sh: it resolves the target dir cargo really uses, refuses anything that is not
# a cargo target dir (or is /, $$HOME, this checkout or a parent), and never touches sources.
prune: ## Delete build artefacts older than PRUNE_DAYS (14) or of removed toolchains (PRUNE_MAXSIZE=10GB caps)
	$(Q)CARGO='$(CARGO)' PRUNE_DAYS='$(PRUNE_DAYS)' PRUNE_MAXSIZE='$(PRUNE_MAXSIZE)' DRY_RUN='$(DRY_RUN)' \
		scripts/prune.sh
PRUNE_DAYS ?= 14
# Optional size cap, e.g. PRUNE_MAXSIZE=10GB: then the oldest artefacts go until the dir fits.
PRUNE_MAXSIZE ?=

clean-all: ## clean, and delete the local Postgres data (CONFIRM=1)
	$(Q)$(call need-confirm,deletes the build output and every database of compose project '$(COMPOSE_PROJECT)')
	$(Q)$(MAKE) clean CONFIRM=0
	$(Q)$(COMPOSE) down -v
