#!/usr/bin/env bash
# Shared paths and helpers. Sourced, never executed.
#
# SC2034: every variable below is consumed by a script that sources this file,
# which shellcheck cannot see from here.
# shellcheck disable=SC2034

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT_DIR="$(cd "$TEST_DIR/.." && pwd)"

SUITES_DIR="$TEST_DIR/suites"
PERF_DIR="$TEST_DIR/perf"
WINDOWS_DIR="$TEST_DIR/windows"

# Everything setup.sh writes lands here, and the whole directory is gitignored.
# odbc.ini names the database and odbcinst.ini names the driver library, both by
# absolute path, so neither survives being moved to another checkout.
GENERATED="$TEST_DIR/generated"
DB_PATH="$GENERATED/test.db"
# The stress suite generates 50k rows. It gets a database of its own so the
# shared one keeps the size the other suites expect, and so a stress run left
# half-finished cannot affect them. SQLite creates the file on first connect.
STRESS_DB_PATH="$GENERATED/stress.db"
ODBC_INI="$GENERATED/odbc.ini"
ODBCINST_INI="$GENERATED/odbcinst.ini"

DRIVER_PATH="$PROJECT_DIR/target/debug/libstackable_odbc_sqlite.so"

# The DSN setup.sh writes into odbc.ini, and the one run-tests.sh connects
# through for its second configuration.
DSN_NAME="test_sqlite"

mkdir -p "$GENERATED"

# Point unixODBC at the generated configuration rather than the system's.
# ODBCSYSINI is a *directory* (unixODBC appends `odbcinst.ini` itself) while
# ODBCINI is a full path, which is why the two are not spelled alike.
use_generated_odbc_config() {
    export ODBCSYSINI="$GENERATED"
    export ODBCINI="$ODBC_INI"
}

# Build the cdylib pyodbc loads. `cargo test` builds the test harness, not this,
# so a run that skips it would silently exercise the previous build. cargo is
# incremental, so repeating it costs nothing when nothing changed.
build_driver() {
    echo "=== Building stackable-odbc-sqlite ==="
    (cd "$PROJECT_DIR" && cargo build)
}

# usage <script>. Prints the contiguous comment block below the shebang, minus
# the leading `# `, as that script's help text. Derived rather than given as a
# line range, which silently truncates the moment a line is added to the header.
usage() {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$1"
}

require_setup() {
    if [[ ! -f "$DB_PATH" || ! -f "$ODBC_INI" ]]; then
        echo "ERROR: $GENERATED is incomplete. Run ./integration-tests/setup.sh first." >&2
        exit 1
    fi
}
