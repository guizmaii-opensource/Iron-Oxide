#!/usr/bin/env bash
# Tests the Makefile's safety guards in a scratch git repository, with stub tools: nothing is
# deployed, built or deleted outside the scratch directory. Run by `make test-make` (CI: Unit tests).
#
# Covered: `make test` fails without service worker tests; `make check` fails without gitleaks
# (unless SKIP_SECRETS=1); `make deploy` refuses unless CONFIRM=1, fly, gh, gitleaks, a green CI
# run, a successful fetch and a clean `main` at exactly origin/main (not ahead, behind or
# diverged), both before and after its `make check`, and deploys from a pristine export of the
# commit; `make clean` and `make clean-all CONFIRM=1` refuse a target dir outside the checkout
# without CONFIRM_SHARED=1; `make prune` refuses a target dir holding tracked files; `make
# landing-build` only replaces a directory strictly under dist/, inserts the prompt, strips comments and
# refuses GitHub/open-source/licence mentions. And the suite itself passes when its caller sets CONFIRM=1 or
# SKIP_SECRETS=1.
set -euo pipefail

# Every case starts from a neutral environment: nothing the calling make (or shell) set may leak in.
unset MAKEFLAGS MFLAGS MAKEOVERRIDES GNUMAKEFLAGS MAKELEVEL MAKEFILES \
  CONFIRM CONFIRM_SHARED SKIP_SECRETS SECRETS_RANGE CARGO_TARGET_DIR FLY GH GITLEAKS CARGO \
  IRON_OXIDE_START_TEST_DB IRON_OXIDE_START_SMOKE_DB

root=$(cd "$(dirname "$0")/.." && pwd)
scratch=$(cd "$(mktemp -d)" && pwd -P)
trap 'rm -rf "$scratch"' EXIT

export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1

# Stub tools: they only say what they would do. `gh` answers "1 successful CI run", `gh-no-ci` "0".
# `fly` also records the directory it runs in and that directory's content. `make` stands for the
# nested `make check` of deploy; `make-moves-head` also commits during it.
stubs=$scratch/stubs
mkdir -p "$stubs"
export FLY_RECORD=$scratch/fly-record
printf '#!/bin/sh\n{ pwd; ls -A; cat Makefile; } >"$FLY_RECORD"\necho "STUB fly $*"\n' >"$stubs/fly"
printf '#!/bin/sh\necho "STUB make $*"\n' >"$stubs/make"
printf '#!/bin/sh\necho "STUB make $*"\ngit commit --quiet --allow-empty -m moved\n' >"$stubs/make-moves-head"
printf '#!/bin/sh\necho "STUB cargo $*"\n' >"$stubs/cargo"
printf '#!/bin/sh\necho "STUB gitleaks $*"\n' >"$stubs/gitleaks"
printf '#!/bin/sh\necho 1\n' >"$stubs/gh"
printf '#!/bin/sh\necho 0\n' >"$stubs/gh-no-ci"
chmod +x "$stubs"/*

# A bare origin and a checkout of main holding the Makefile.
git init --quiet --bare "$scratch/origin.git"
git init --quiet -b main "$scratch/work"
work=$scratch/work
cp "$root/Makefile" "$work/Makefile"
git -C "$work" add Makefile
git -C "$work" commit --quiet -m init
git -C "$work" remote add origin "$scratch/origin.git"
git -C "$work" push --quiet -u origin main

failures=0
# run <expect: ok|fail> <description> <pattern the output must contain> -- make arguments...
run() {
  local expect=$1 what=$2 pattern=$3
  shift 4
  local out status=0
  out=$(cd "$work" && make "$@" 2>&1) || status=$?
  local verdict=pass
  if [ "$expect" = ok ] && [ "$status" -ne 0 ]; then verdict="FAIL (exit $status)"; fi
  if [ "$expect" = fail ] && [ "$status" -eq 0 ]; then verdict="FAIL (exit 0)"; fi
  if ! grep -qF -- "$pattern" <<<"$out"; then verdict="FAIL (no \"$pattern\")"; fi
  if [ "$verdict" = pass ]; then
    echo "  ok    $what"
  else
    echo "  $verdict  $what"
    sed 's/^/        | /' <<<"$out"
    failures=$((failures + 1))
  fi
}
deploy=(deploy FLY="$stubs/fly" GH="$stubs/gh" GITLEAKS="$stubs/gitleaks" MAKE="$stubs/make")

echo "make test"
run fail "fails when no service worker test exists" "no service worker tests found" -- \
  test CARGO="$stubs/cargo"
mkdir -p "$work/crates/iron-oxide-app/tests/sw"
printf "import test from 'node:test';\ntest('stub', () => {});\n" \
  >"$work/crates/iron-oxide-app/tests/sw/stub.test.mjs"
run ok "runs the service worker tests when they exist" "service worker tests" -- \
  test CARGO="$stubs/cargo"
rm -r "$work/crates"

echo "make check"
run fail "fails when gitleaks is missing" "gitleaks is not installed" -- \
  check GITLEAKS="$scratch/missing" MAKE=true
run ok "skips the scan only with SKIP_SECRETS=1" "secret scan skipped (SKIP_SECRETS=1)" -- \
  check GITLEAKS="$scratch/missing" SKIP_SECRETS=1 MAKE=true

echo "make deploy"
run fail "refuses without CONFIRM=1" "CONFIRM=1" -- "${deploy[@]}"
run fail "refuses without fly" "'$scratch/missing' is not installed" -- \
  "${deploy[@]}" CONFIRM=1 FLY="$scratch/missing"
run fail "refuses without gh" "'$scratch/missing' is not installed" -- \
  "${deploy[@]}" CONFIRM=1 GH="$scratch/missing"
run fail "refuses without gitleaks" "'$scratch/missing' is not installed" -- \
  "${deploy[@]}" CONFIRM=1 GITLEAKS="$scratch/missing"
run fail "refuses SKIP_SECRETS=1" "SKIP_SECRETS=1 is not allowed" -- "${deploy[@]}" CONFIRM=1 SKIP_SECRETS=1
run fail "refuses when CI has not passed" "CI has not passed" -- \
  "${deploy[@]}" CONFIRM=1 GH="$stubs/gh-no-ci"
run ok "runs make check (full-history secret scan) before deploying" "STUB make check SECRETS_RANGE=HEAD" -- \
  "${deploy[@]}" CONFIRM=1
run ok "deploys a clean main at origin/main" "STUB fly deploy" -- "${deploy[@]}" CONFIRM=1

# Local state that `git status` does not show must not reach the upload: a skip-worktree edit of a
# tracked file, a file excluded by .git/info/exclude, and an excluded secret-looking file.
git -C "$work" update-index --skip-worktree Makefile
echo '# LOCAL EDIT' >>"$work/Makefile"
printf 'extra.rs\n.envrc\n' >>"$work/.git/info/exclude"
echo 'fn not_reviewed() {}' >"$work/extra.rs"
echo 'export SECRET=x' >"$work/.envrc"
rm -f "$FLY_RECORD"
run ok "deploys even with local-only state (it is not uploaded)" "STUB fly deploy" -- "${deploy[@]}" CONFIRM=1
fly_dir=$(head -1 "$FLY_RECORD" 2>/dev/null || true)
if [ -n "$fly_dir" ] && [ "$fly_dir" != "$(cd "$work" && pwd -P)" ] && [ "$fly_dir" != "$work" ] &&
  ! grep -qxE 'extra\.rs|\.envrc|\.git' "$FLY_RECORD" && ! grep -q 'LOCAL EDIT' "$FLY_RECORD" &&
  [ ! -e "$fly_dir" ]; then
  echo "  ok    fly runs in a pristine, temporary export of the commit"
else
  echo "  FAIL  fly runs in a pristine, temporary export of the commit"
  sed 's/^/        | /' "$FLY_RECORD" 2>/dev/null || true
  failures=$((failures + 1))
fi
git -C "$work" update-index --no-skip-worktree Makefile
git -C "$work" checkout --quiet -- Makefile
rm "$work/extra.rs" "$work/.envrc"

rm -f "$FLY_RECORD"
run fail "refuses when HEAD moves during make check" "HEAD is not origin/main" -- \
  "${deploy[@]}" CONFIRM=1 MAKE="$stubs/make-moves-head"
if [ -e "$FLY_RECORD" ]; then
  echo "  FAIL  fly did not run after HEAD moved"
  failures=$((failures + 1))
fi
git -C "$work" reset --quiet --hard origin/main

git -C "$work" remote set-url origin "$scratch/no-such-origin.git"
run fail "refuses when origin cannot be fetched" "cannot fetch origin/main" -- "${deploy[@]}" CONFIRM=1
git -C "$work" remote set-url origin "$scratch/origin.git"

echo untracked >"$work/untracked.txt"
run fail "refuses uncommitted changes" "uncommitted changes" -- "${deploy[@]}" CONFIRM=1
rm "$work/untracked.txt"

git -C "$work" checkout --quiet -b other
run fail "refuses another branch" "deploy from main only" -- "${deploy[@]}" CONFIRM=1
git -C "$work" checkout --quiet main

git -C "$work" commit --quiet --allow-empty -m ahead
run fail "refuses a main ahead of origin/main" "HEAD is not origin/main" -- "${deploy[@]}" CONFIRM=1

git -C "$work" push --quiet origin main
git -C "$work" reset --quiet --hard HEAD~1
run fail "refuses a main behind origin/main" "HEAD is not origin/main" -- "${deploy[@]}" CONFIRM=1

git -C "$work" commit --quiet --allow-empty -m diverged
run fail "refuses a main diverged from origin/main" "HEAD is not origin/main" -- "${deploy[@]}" CONFIRM=1

echo "make prune (throwaway target dirs only)"
# A tiny crate, so that the real `cargo metadata` resolves the target dir. The stub cargo passes
# everything to the real one except `sweep`, which it only echoes.
real_cargo=$(command -v cargo)
printf '[package]\nname = "scratch"\nversion = "0.1.0"\nedition = "2021"\n' >"$work/Cargo.toml"
mkdir -p "$work/src" && : >"$work/src/lib.rs"
mkdir -p "$work/scripts" && cp "$root/scripts/prune.sh" "$work/scripts/prune.sh"
printf '#!/bin/sh\nif [ "$1" = sweep ]; then echo "STUB sweep $*"; exit 0; fi\nexec "%s" "$@"\n' "$real_cargo" \
  >"$stubs/cargo-sweep-stub"
printf '#!/bin/sh\necho "INC=[${CARGO_INCREMENTAL-unset}] $*"\n' >"$stubs/cargo-inc"
chmod +x "$stubs/cargo-sweep-stub" "$stubs/cargo-inc"
fake_target() { mkdir -p "$1/debug" && : >"$1/.rustc_info.json"; }
# prune_case <ok|fail> <description> <pattern> <env assignments...> -- <make arguments...>
prune_case() {
  local expect=$1 what=$2 pattern=$3
  shift 3
  local envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done
  shift
  local out status=0
  out=$(cd "$work" && env -u CARGO_TARGET_DIR -u CARGO_BUILD_TARGET_DIR ${envs[@]+"${envs[@]}"} \
    make prune CARGO="$stubs/cargo-sweep-stub" "$@" 2>&1) || status=$?
  local verdict=pass
  if [ "$expect" = ok ] && [ "$status" -ne 0 ]; then verdict="FAIL (exit $status)"; fi
  if [ "$expect" = fail ] && [ "$status" -eq 0 ]; then verdict="FAIL (exit 0)"; fi
  if ! grep -qF -- "$pattern" <<<"$out"; then verdict="FAIL (no \"$pattern\")"; fi
  if [ "$expect" = fail ] && grep -q "STUB sweep" <<<"$out"; then verdict="FAIL (swept anyway)"; fi
  if [ "$verdict" = pass ]; then
    echo "  ok    $what"
  else
    echo "  $verdict  $what"
    sed 's/^/        | /' <<<"$out"
    failures=$((failures + 1))
  fi
  prune_out=$out
}
fakehome=$scratch/home && fake_target "$fakehome"
notarget=$scratch/not-a-target && mkdir -p "$notarget"
spaced="$scratch/my target" && fake_target "$spaced"
prune_case fail "refuses the checkout" "this checkout or one of its parents" CARGO_TARGET_DIR="$work" --
prune_case fail "refuses a parent of the checkout" "this checkout or one of its parents" CARGO_TARGET_DIR="$scratch" --
prune_case fail "refuses /" "refusing to sweep /." CARGO_TARGET_DIR=/ --
prune_case fail "refuses \$HOME" "refusing to sweep \$HOME" HOME="$fakehome" CARGO_TARGET_DIR="$fakehome" \
  RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" --
prune_case fail "refuses a dir that is not a cargo target dir" "does not look like a cargo target dir" \
  CARGO_TARGET_DIR="$notarget" --
prune_case fail "resolves CARGO_BUILD_TARGET_DIR (here: the checkout)" "this checkout or one of its parents" \
  CARGO_BUILD_TARGET_DIR=. --
prune_case ok "does nothing when the target dir does not exist" "Nothing to prune" \
  CARGO_TARGET_DIR="$scratch/missing-target" --
prune_case ok "handles a target dir with a space" "$spaced: " CARGO_TARGET_DIR="$spaced" --
prune_case fail "rejects PRUNE_MAXSIZE=10G before deleting anything" "is not a size" \
  CARGO_TARGET_DIR="$spaced" -- PRUNE_MAXSIZE=10G
prune_case fail "rejects PRUNE_MAXSIZE='10 GB' before deleting anything" "is not a size" \
  CARGO_TARGET_DIR="$spaced" -- "PRUNE_MAXSIZE=10 GB"
prune_case ok "accepts PRUNE_MAXSIZE=10GB" "STUB sweep sweep --maxsize 10GB ." \
  CARGO_TARGET_DIR="$spaced" -- PRUNE_MAXSIZE=10GB
mkdir -p "$spaced/locked" && chmod 000 "$spaced/locked"
prune_case ok "reports sizes even when du cannot read everything" " MB -> " CARGO_TARGET_DIR="$spaced" --
chmod 755 "$spaced/locked"

inc="$spaced/debug/incremental"
mkdir -p "$inc/old-session" "$inc/new-session"
touch -t 202001010000 "$inc/old-session"
prune_case ok "dry run lists an old incremental cache" "would delete $inc/old-session" \
  CARGO_TARGET_DIR="$spaced" -- DRY_RUN=1
if [ -d "$inc/old-session" ]; then echo "  ok    dry run deletes nothing"; else
  echo "  FAIL  dry run deletes nothing"; failures=$((failures + 1)); fi
prune_case ok "deletes incremental caches older than PRUNE_DAYS" "STUB sweep" CARGO_TARGET_DIR="$spaced" --
if [ ! -e "$inc/old-session" ] && [ -d "$inc/new-session" ]; then
  echo "  ok    ...and keeps the recent ones"
else
  echo "  FAIL  ...and keeps the recent ones"; failures=$((failures + 1))
fi
prune_case ok "PRUNE_MAXSIZE deletes every incremental cache before the size sweep" "--maxsize 1GB" \
  CARGO_TARGET_DIR="$spaced" -- PRUNE_MAXSIZE=1GB
if [ ! -e "$inc/new-session" ]; then echo "  ok    ...including the recent ones"; else
  echo "  FAIL  ...including the recent ones"; failures=$((failures + 1)); fi

echo "make compile keeps incremental builds; the CI-like targets do not"
out=$(cd "$work" && env -u CARGO_INCREMENTAL make compile CARGO="$stubs/cargo-inc" 2>&1 || true)
if grep -q "INC=\[0\]" <<<"$out" || ! grep -q "INC=\[unset\]" <<<"$out"; then
  echo "  FAIL  compile runs every cargo check incrementally"; sed 's/^/        | /' <<<"$out"; failures=$((failures + 1))
else
  echo "  ok    compile runs every cargo check incrementally"
fi
out=$(cd "$work" && env -u CARGO_INCREMENTAL make lint CARGO="$stubs/cargo-inc" 2>&1 || true)
if grep -q "INC=\[unset\]" <<<"$out" || ! grep -q "INC=\[0\]" <<<"$out"; then
  echo "  FAIL  lint runs with CARGO_INCREMENTAL=0"; sed 's/^/        | /' <<<"$out"; failures=$((failures + 1))
else
  echo "  ok    lint runs with CARGO_INCREMENTAL=0"
fi

echo "make clean / clean-all"
shared=$scratch/shared-target
run fail "clean refuses a target dir outside the checkout" "CONFIRM_SHARED=1" -- \
  clean CARGO="$stubs/cargo" CARGO_TARGET_DIR="$shared"
run fail "clean: CONFIRM=1 is not enough for a shared target dir" "CONFIRM_SHARED=1" -- \
  clean CARGO="$stubs/cargo" CARGO_TARGET_DIR="$shared" CONFIRM=1
run fail "clean-all CONFIRM=1 still refuses a shared target dir" "CONFIRM_SHARED=1" -- \
  clean-all CARGO="$stubs/cargo" CARGO_TARGET_DIR="$shared" CONFIRM=1 COMPOSE=echo
run ok "clean-all goes ahead with CONFIRM=1 CONFIRM_SHARED=1" "STUB cargo clean" -- \
  clean-all CARGO="$stubs/cargo" CARGO_TARGET_DIR="$shared" CONFIRM=1 CONFIRM_SHARED=1 COMPOSE=echo
run ok "clean accepts a relative target dir inside the checkout" "STUB cargo clean" -- \
  clean CARGO="$stubs/cargo" CARGO_TARGET_DIR=target

# `make landing-build` (scripts/landing-build.py) replaces LANDING_OUT: only a plain directory
# strictly under dist/ is accepted. It inserts programs/ai-prompt.md, strips maintainer comments and
# refuses a page that mentions GitHub, open source or a licence.
mkdir -p "$work/landing/fonts" "$work/schemas" "$work/programs" "$work/scripts" "$work/victim"
cp "$root/scripts/landing-build.py" "$work/scripts/"
landing_page() {
  printf '<!-- note for maintainers: #70 -->\n<p>%s</p>\n<pre><!-- prompt:begin -->x<!-- prompt:end --></pre>\n' "$1" \
    >"$work/landing/index.html"
}
landing_page 'Train'
printf '/* copied from the app */\nbody { color: red; }\n' >"$work/landing/styles.css"
echo 'SIL Open Font License' >"$work/landing/fonts/font-OFL.txt"
echo '{"$id": "https://raw.githubusercontent.com/x"}' >"$work/schemas/program.schema.json"
echo 'Ask me & then write <json>' >"$work/programs/ai-prompt.md"
touch "$work/victim/keep"
for out in landing . "$scratch" dist/.. dist/ dist "dist/x victim" "dist/x ~" "dist/*" ../dist/x; do
  run fail "landing-build refuses LANDING_OUT='$out'" "strictly under dist/" -- landing-build LANDING_OUT="$out"
done
if [ -f "$work/landing/index.html" ] && [ -f "$work/victim/keep" ] && [ ! -e "$work/dist/x" ] \
  && [ -d "$scratch/stubs" ]; then
  echo "  ok    landing-build refusals deleted nothing"
else
  echo "  FAIL  landing-build refusals deleted nothing"
  failures=$((failures + 1))
fi
mv "$work/programs/ai-prompt.md" "$work/programs/ai-prompt.md.off"
run fail "landing-build fails without programs/ai-prompt.md" "the landing page needs the AI prompt" -- landing-build
mv "$work/programs/ai-prompt.md.off" "$work/programs/ai-prompt.md"
if [ ! -e "$work/dist/landing" ]; then
  echo "  ok    a failed landing-build leaves no half-built site"
else
  echo "  FAIL  a failed landing-build leaves no half-built site"
  failures=$((failures + 1))
fi
for word in 'Star us on GitHub' 'Open source' 'open-source' 'AGPL licence' 'MIT license'; do
  landing_page "$word"
  run fail "landing-build refuses a page saying '$word'" "must not mention GitHub, open source or a licence" -- \
    landing-build
done
landing_page 'Train'
echo 'See github.com/x' >"$work/programs/ai-prompt.md"
run fail "landing-build refuses a prompt naming GitHub" "must not mention GitHub" -- landing-build
echo 'Ask me & then write <json>' >"$work/programs/ai-prompt.md"
run ok "landing-build assembles dist/landing" "Landing page: dist/landing" -- landing-build
built=$work/dist/landing
if [ -f "$built/program.schema.json" ] && [ -f "$built/fonts/font-OFL.txt" ] \
  && grep -qF '<pre>Ask me &amp; then write &lt;json&gt;</pre>' "$built/index.html" \
  && ! grep -q -e '<!--' -e '#70' "$built/index.html" && ! grep -qF '/*' "$built/styles.css"; then
  echo "  ok    landing-build output: schema, fonts, escaped prompt, no maintainer comments"
else
  echo "  FAIL  landing-build output: schema, fonts, escaped prompt, no maintainer comments"
  failures=$((failures + 1))
fi

# The suite must not depend on its caller: `make test-make` runs inside `make check`, itself inside
# `make deploy CONFIRM=1`, and `make check SKIP_SECRETS=1` is documented.
if [ -z "${IRON_OXIDE_GUARD_TESTS_NESTED:-}" ]; then
  echo "caller variables"
  for how in "command line" environment; do
    status=0
    if [ "$how" = "command line" ]; then
      out=$(cd "$root" && IRON_OXIDE_GUARD_TESTS_NESTED=1 make test-make CONFIRM=1 SKIP_SECRETS=1 CONFIRM_SHARED=1 2>&1) || status=$?
    else
      out=$(cd "$root" && IRON_OXIDE_GUARD_TESTS_NESTED=1 CONFIRM=1 SKIP_SECRETS=1 CONFIRM_SHARED=1 make test-make 2>&1) || status=$?
    fi
    if [ "$status" -eq 0 ]; then
      echo "  ok    passes with CONFIRM=1 SKIP_SECRETS=1 CONFIRM_SHARED=1 set by the caller ($how)"
    else
      echo "  FAIL  passes with CONFIRM=1 SKIP_SECRETS=1 CONFIRM_SHARED=1 set by the caller ($how)"
      sed 's/^/        | /' <<<"$out"
      failures=$((failures + 1))
    fi
  done
fi

echo
if [ "$failures" -gt 0 ]; then
  echo "$failures Makefile guard test(s) failed."
  exit 1
fi
echo "All Makefile guard tests passed."
