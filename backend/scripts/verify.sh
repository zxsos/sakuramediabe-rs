#!/usr/bin/env bash
# Pre-commit gates, Linux/macOS counterpart of scripts/verify.ps1.
#
# Run this instead of trusting that you remembered:
#
#   bash scripts/verify.sh
#   bash scripts/verify.sh --skip-tests
#
# Why it exists: the same three failures that produced verify.ps1 -- a green
# local run shipping red CI -- are environment-specific, not PowerShell-
# specific. A gate you cannot run on the machine you develop on is not a gate.
#
# Every gate captures the exit code immediately after the command, with no
# pipeline in between. Pipelines reset $?, which is easy to reintroduce by
# accident and produces a blank "exit" code that reads like a pass.
#
# Labels are ASCII for parity with verify.ps1 (PowerShell 5.1 reads a BOM-less
# file as ANSI and turns CJK into mojibake).

set -u -o pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$REPO_ROOT/Cargo.toml"
PARITY="$REPO_ROOT/parity"

# The Rust toolchain is not always on PATH in non-login shells (rustup appends
# to .profile / .zshrc, which only login shells read).
export PATH="$HOME/.cargo/bin:$PATH"

# Integration tests need a real database. Without this they fail loudly rather
# than skip (TestDb::require panics), but the message is a panic backtrace
# instead of one actionable line, so set it here.
export SMDB_TEST_DATABASE_URL="${SMDB_TEST_DATABASE_URL:-postgres://sakuramedia:sakuramedia@127.0.0.1:5433/sakuramedia_test}"
export DATABASE_URL="${DATABASE_URL:-$SMDB_TEST_DATABASE_URL}"

SKIP_TESTS=0
for arg in "$@"; do
  case "$arg" in
    --skip-tests) SKIP_TESTS=1 ;;
    -h | --help)
      sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

failed=()

step() {
  local name="$1"
  shift
  local out
  out="$("$@" 2>&1)"
  local code=$?
  if [ "$code" -eq 0 ]; then
    printf '  PASS  %s\n' "$name"
  else
    printf '  FAIL  %s  (exit %s)\n' "$name" "$code"
    failed+=("$name")
    # Errors live at the end; the head is progress bars and cargo chatter.
    printf '%s\n' "$out" | tail -12 | sed 's/^/        /'
  fi
}

# --- preflight -------------------------------------------------------------
# Cheap checks first: every gate below fails in a confusing way when the
# toolchain or the database is missing, so name the real problem up front.
echo 'preflight'
if ! command -v cargo > /dev/null 2>&1; then
  echo '  FAIL  cargo not found. Install Rust from https://rustup.rs (>= 1.85).'
  exit 1
fi
printf '  PASS  cargo %s\n' "$(cargo --version | awk '{print $2}')"
if ! command -v cargo-clippy > /dev/null 2>&1 && ! cargo clippy --version > /dev/null 2>&1; then
  echo '  FAIL  clippy component missing. Run: rustup component add clippy'
  exit 1
fi
printf '  PASS  %s\n' "$(cargo clippy --version)"
if [ "$SKIP_TESTS" -eq 0 ]; then
  if command -v docker > /dev/null 2>&1 &&
    docker inspect -f '{{.State.Health.Status}}' sakuramedia-rs-pg > /dev/null 2>&1; then
    printf '  PASS  postgres container %s\n' \
      "$(docker inspect -f '{{.State.Health.Status}}' sakuramedia-rs-pg)"
  else
    echo '  WARN  container sakuramedia-rs-pg not found -- run: docker compose up -d'
    echo '        (integration tests will fail, not skip; that is intentional)'
  fi
fi

# --- gates -----------------------------------------------------------------
echo 'fmt'
step 'cargo fmt --all -- --check' cargo fmt --manifest-path "$MANIFEST" --all -- --check

echo 'doc'
RUSTDOCFLAGS='-D warnings' step 'cargo doc --workspace -D warnings' \
  cargo doc --manifest-path "$MANIFEST" --workspace --no-deps --offline

echo 'lint'
step 'clippy --all-targets --all-features' \
  cargo clippy --manifest-path "$MANIFEST" --workspace --all-targets --all-features --offline -- -D warnings

if [ "$SKIP_TESTS" -eq 0 ]; then
  echo 'test'
  step 'cargo test --workspace' cargo test --manifest-path "$MANIFEST" --workspace --offline
fi

echo 'parity'
# compare.py and compare_core.py drive prebuilt binaries; build them here so a
# stale binary can never be the thing under test.
step 'build parity-cli' cargo build --manifest-path "$MANIFEST" --release -p parity-cli --offline
step 'build core_parity' cargo build --manifest-path "$MANIFEST" --release -p sm-core --bin core_parity --offline
step 'schema' python3 "$PARITY/compare_schema.py"
step 'compare' python3 "$PARITY/compare.py"
step 'core' python3 "$PARITY/compare_core.py"
# The seventh gate. See parity/check_paged_wrappers.py for why: paged_list!
# generates the whole method, so a hand-written wrapper around it returns ().
# The compiler catches it, but points at the macro expansion, not the mistake.
step 'paged wrappers' python3 "$PARITY/check_paged_wrappers.py"

# The eighth gate: progress-baseline drift. Edit the code without re-running
# `scripts/progress.ps1 -Write` and this fails -- same reasoning as the header
# of this file: a check you have to remember to run is not a check.
#
# The generator is PowerShell, so this needs pwsh. When it is missing we WARN
# rather than fail (CI images without PowerShell still have every other gate),
# but the warning names what did not run -- a silent skip would read as a pass.
echo 'progress'
if command -v pwsh > /dev/null 2>&1; then
  step 'progress baseline (-Diff)' pwsh -NoProfile -File "$REPO_ROOT/scripts/progress.ps1" -Diff
else
  echo '  WARN  pwsh not found -- progress baseline drift NOT checked'
  echo '        (run: pwsh -File scripts/progress.ps1 -Write, then commit the file)'
fi

echo
if [ "${#failed[@]}" -gt 0 ]; then
  printf 'FAILED: %s\n' "$(IFS=', '; echo "${failed[*]}")"
  exit 1
fi
echo 'all gates passed'
