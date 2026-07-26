#!/usr/bin/env bash
# Uninstall the Stackable SQLite ODBC driver on Linux.
# Must be run as root (or via sudo).
set -euo pipefail

INSTALL_DIR="${INSTALL_DIR:-/usr/local/lib/stackable-odbc}"
DRIVER_LIB="libstackable_odbc_sqlite.so"

if [ "$EUID" -ne 0 ]; then
  echo "This script must be run as root (or via sudo)." >&2
  exit 1
fi

odbcinst -u -d -n "stackable_odbc_sqlite" || true
rm -f "$INSTALL_DIR/$DRIVER_LIB"
rmdir --ignore-fail-on-non-empty "$INSTALL_DIR" 2>/dev/null || true

echo "Stackable SQLite ODBC driver uninstalled."
echo "If you created any DSNs, remove them from /etc/odbc.ini (or ~/.odbc.ini)."
