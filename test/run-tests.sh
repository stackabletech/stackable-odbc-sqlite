#!/usr/bin/env bash
# Runs SQLite integration tests (Linux) and optionally Windows VM tests.
#
# Requires setup.sh to have been run first.
#
# Usage:
#   ./test/run-tests.sh            # Linux tests only
#   ./test/run-tests.sh --windows  # Linux + Windows VM tests
#   ./test/run-tests.sh --skip-build            # skip the cargo build (also passed to windows_test.py)
#   ./test/run-tests.sh --skip-cargo-test       # skip `cargo test` (CI already runs it via pre-commit)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

DRIVER_PATH="$PROJECT_DIR/target/debug/libstackable_odbc_sqlite.so"
DB_PATH="$SCRIPT_DIR/test.db"

RUN_WINDOWS=false
SKIP_BUILD=false
SKIP_CARGO_TEST=false
WINDOWS_EXTRA_ARGS=()

for arg in "$@"; do
    case "$arg" in
        --windows) RUN_WINDOWS=true ;;
        --skip-build) SKIP_BUILD=true; WINDOWS_EXTRA_ARGS+=("$arg") ;;
        --skip-cargo-test) SKIP_CARGO_TEST=true ;;
        *) WINDOWS_EXTRA_ARGS+=("$arg") ;;
    esac
done

# --- Rebuild the driver ---
# Only setup.sh built the .so, so editing driver source and re-running this
# script would silently test the previous build. `cargo test` below builds the
# test harness, not the cdylib that pyodbc loads, so an explicit build is
# needed. cargo is incremental, so this is a no-op when nothing changed.
if [[ "$SKIP_BUILD" == false ]]; then
    echo "=== Building stackable-odbc-sqlite ==="
    (cd "$PROJECT_DIR" && cargo build)
fi

# --- Linux: pyodbc integration tests (2 configs, matching Windows) ---
export ODBCSYSINI="$SCRIPT_DIR"
export ODBCINI="$SCRIPT_DIR/odbc.ini"

echo "=== Running Linux pyodbc integration tests (DSN-less) ==="
uv run --with pyodbc python3 "$SCRIPT_DIR/test_integration.py" \
    "Driver=$DRIVER_PATH;Database=$DB_PATH"

echo "=== Running Linux pyodbc integration tests (DSN) ==="
uv run --with pyodbc python3 "$SCRIPT_DIR/test_integration.py" "DSN=test_sqlite"

# --- Linux: Rust FFI integration tests ---
# Run by default so that a developer invoking this script gets the whole suite
# in one command. CI passes --skip-cargo-test, because its pre-commit job has
# already run exactly this via the cargo-test hook, and repeating it there
# means rebuilding the test harness on a second runner for no added coverage.
if [[ "$SKIP_CARGO_TEST" == false ]]; then
    echo "=== Running SQLite FFI integration tests ==="
    (cd "$PROJECT_DIR" && cargo test)
fi

# --- Windows VM tests (optional) ---
if [[ "$RUN_WINDOWS" == true ]]; then
    echo "=== Running Windows VM integration tests ==="
    uv run --with pywinrm python3 "$SCRIPT_DIR/windows_test.py" "${WINDOWS_EXTRA_ARGS[@]+"${WINDOWS_EXTRA_ARGS[@]}"}"
fi
