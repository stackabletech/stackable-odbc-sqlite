#!/usr/bin/env bash
# Builds the driver, creates the test database, and writes the ODBC
# configuration the suites connect through.
#
# Usage:
#   ./integration-tests/setup.sh              # build, then (re)create everything
#   ./integration-tests/setup.sh --skip-build # reuse the driver already built
set -euo pipefail

# shellcheck source=integration-tests/scripts/lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

SKIP_BUILD=false
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=true ;;
        -h | --help)
            usage "${BASH_SOURCE[0]}"
            exit 0
            ;;
        *)
            echo "ERROR: unknown argument '$arg'. Try --help." >&2
            exit 2
            ;;
    esac
done

if [[ "$SKIP_BUILD" == false ]]; then
    build_driver
fi

if [[ ! -f "$DRIVER_PATH" ]]; then
    echo "ERROR: $DRIVER_PATH does not exist. Drop --skip-build." >&2
    exit 1
fi

echo "=== Creating test database ==="
rm -f "$DB_PATH"
sqlite3 "$DB_PATH" < "$SUITES_DIR/create_test_db.sql"

echo "=== Writing ODBC configuration ==="
cat > "$ODBCINST_INI" << EOF
[stackable_odbc_sqlite]
Driver = $DRIVER_PATH
EOF

cat > "$ODBC_INI" << EOF
[$DSN_NAME]
Driver = stackable_odbc_sqlite
Database = $DB_PATH
EOF

cat << EOF

=== Setup complete ===

Run tests:    ./integration-tests/run-tests.sh [--windows]

To test interactively:
  export ODBCSYSINI=$GENERATED
  export ODBCINI=$ODBC_INI
  isql -3 $DSN_NAME -v
EOF
