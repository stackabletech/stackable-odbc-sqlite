#!/usr/bin/env bash
# Runs the pyodbc suite through real unixODBC, the Rust FFI tests, and
# optionally the same pyodbc suite inside a Windows VM.
#
# Requires setup.sh to have been run first.
#
# Usage:
#   ./integration-tests/run-tests.sh                   # Linux only
#   ./integration-tests/run-tests.sh --windows         # Linux, then the Windows VM
#   ./integration-tests/run-tests.sh --skip-build      # reuse the driver already built
#   ./integration-tests/run-tests.sh --skip-cargo-test # pyodbc only; what CI runs
#
# Any other argument is forwarded to windows_test.py (--host, --gateway, --user,
# --password), and is therefore only accepted alongside --windows.
set -euo pipefail

# shellcheck source=integration-tests/scripts/lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

RUN_WINDOWS=false
SKIP_BUILD=false
SKIP_CARGO_TEST=false
WINDOWS_ARGS=()

for arg in "$@"; do
    case "$arg" in
        --windows) RUN_WINDOWS=true ;;
        # Forwarded as well as acted on: the VM build is a separate
        # cross-compile, and skipping one without the other would be a surprise.
        --skip-build)
            SKIP_BUILD=true
            WINDOWS_ARGS+=("$arg")
            ;;
        --skip-cargo-test) SKIP_CARGO_TEST=true ;;
        -h | --help)
            usage "${BASH_SOURCE[0]}"
            exit 0
            ;;
        *) WINDOWS_ARGS+=("$arg") ;;
    esac
done

# Silently forwarding to a script that is never invoked is how a typo'd flag
# becomes a test run that ignored it.
if [[ "$RUN_WINDOWS" == false && ${#WINDOWS_ARGS[@]} -gt 0 ]]; then
    echo "ERROR: ${WINDOWS_ARGS[*]} only applies with --windows. Try --help." >&2
    exit 2
fi

require_setup

if [[ "$SKIP_BUILD" == false ]]; then
    build_driver
fi

# Two configurations, matching what the Windows suite runs: a DSN-less
# connection string exercises the driver's own parsing, while the DSN goes
# through the Driver Manager's lookup first.
use_generated_odbc_config

echo "=== Running Linux pyodbc integration tests (DSN-less) ==="
uv run --with pyodbc python3 "$SUITES_DIR/test_integration.py" \
    "Driver=$DRIVER_PATH;Database=$DB_PATH"

echo "=== Running Linux pyodbc integration tests (DSN) ==="
uv run --with pyodbc python3 "$SUITES_DIR/test_integration.py" "DSN=$DSN_NAME"

# Run by default so that a developer invoking this script gets the whole suite
# in one command. CI passes --skip-cargo-test, because its pre-commit job has
# already run exactly this via the cargo-test hook, and repeating it there means
# rebuilding the test harness on a second runner for no added coverage.
if [[ "$SKIP_CARGO_TEST" == false ]]; then
    echo "=== Running SQLite FFI integration tests ==="
    (cd "$PROJECT_DIR" && cargo test)
fi

if [[ "$RUN_WINDOWS" == true ]]; then
    echo "=== Running Windows VM integration tests ==="
    uv run --with pywinrm python3 "$WINDOWS_DIR/windows_test.py" \
        "${WINDOWS_ARGS[@]+"${WINDOWS_ARGS[@]}"}"
fi
