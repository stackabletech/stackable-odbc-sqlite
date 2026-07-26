#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TEST_DIR="$SCRIPT_DIR"

echo "=== Building stackable-odbc-sqlite ==="
cd "$PROJECT_DIR"
cargo build

DRIVER_PATH="$PROJECT_DIR/target/debug/libstackable_odbc_sqlite.so"
DB_PATH="$TEST_DIR/test.db"

echo "=== Creating test database ==="
rm -f "$DB_PATH"
sqlite3 "$DB_PATH" < "$TEST_DIR/create_test_db.sql"

echo "=== Writing ODBC configuration ==="
cat > "$TEST_DIR/odbcinst.ini" << EOF
[stackable_odbc_sqlite]
Driver = $DRIVER_PATH
EOF

cat > "$TEST_DIR/odbc.ini" << EOF
[test_sqlite]
Driver = stackable_odbc_sqlite
Database = $DB_PATH
EOF

echo ""
echo "=== Setup complete ==="
echo ""
echo "Run tests:    ./test/run-tests.sh [--windows]"
echo ""
echo "To test interactively:"
echo "  export ODBCSYSINI=$TEST_DIR"
echo "  export ODBCINI=$TEST_DIR/odbc.ini"
echo "  isql -3 test_sqlite -v"
