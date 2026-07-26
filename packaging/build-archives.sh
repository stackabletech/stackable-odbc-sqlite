#!/usr/bin/env bash
# Assemble release archives for stackable-odbc-sqlite.
#
# Preconditions:
#   - $VERSION environment variable set (e.g. "1.0.0-beta.1")
#   - target/release/libstackable_odbc_sqlite.so exists
#   - target/x86_64-pc-windows-gnu/release/stackable_odbc_sqlite.dll exists
#
# Output (written to dist/):
#   - stackable-odbc-sqlite-<version>-linux-x64.tar.gz
#   - stackable-odbc-sqlite-<version>-windows-x64.zip
set -euo pipefail

: "${VERSION:?VERSION environment variable must be set}"

PACKAGING_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$PACKAGING_DIR/.." && pwd)"
DIST_DIR="$PACKAGING_DIR/dist"
LINUX_SO="$REPO_ROOT/target/release/libstackable_odbc_sqlite.so"
WINDOWS_DLL="$REPO_ROOT/target/x86_64-pc-windows-gnu/release/stackable_odbc_sqlite.dll"
LICENSE_FILE="$REPO_ROOT/LICENSE"

if [ ! -f "$LINUX_SO" ]; then
  echo "ERROR: $LINUX_SO not found. Run 'cargo build --release' first." >&2
  exit 1
fi
if [ ! -f "$WINDOWS_DLL" ]; then
  echo "ERROR: $WINDOWS_DLL not found. Run 'cargo build --release --target x86_64-pc-windows-gnu' first." >&2
  exit 1
fi
if [ ! -f "$LICENSE_FILE" ]; then
  echo "ERROR: LICENSE file not found at $LICENSE_FILE" >&2
  exit 1
fi

mkdir -p "$DIST_DIR"

# --- Linux archive ---
LINUX_STAGING="$DIST_DIR/staging-linux"
rm -rf "$LINUX_STAGING"
mkdir -p "$LINUX_STAGING"
cp "$LINUX_SO" "$LINUX_STAGING/"
cp "$PACKAGING_DIR/linux/install.sh" "$LINUX_STAGING/"
cp "$PACKAGING_DIR/linux/uninstall.sh" "$LINUX_STAGING/"
cp "$PACKAGING_DIR/README.md" "$LINUX_STAGING/"
cp "$LICENSE_FILE" "$LINUX_STAGING/"
chmod +x "$LINUX_STAGING/install.sh" "$LINUX_STAGING/uninstall.sh"

LINUX_ARCHIVE="stackable-odbc-sqlite-${VERSION}-linux-x64.tar.gz"
tar -czf "$DIST_DIR/$LINUX_ARCHIVE" -C "$LINUX_STAGING" .
rm -rf "$LINUX_STAGING"

# --- Windows archive ---
WINDOWS_STAGING="$DIST_DIR/staging-windows"
rm -rf "$WINDOWS_STAGING"
mkdir -p "$WINDOWS_STAGING"
cp "$WINDOWS_DLL" "$WINDOWS_STAGING/"
cp "$PACKAGING_DIR/windows/install.bat" "$WINDOWS_STAGING/"
cp "$PACKAGING_DIR/windows/uninstall.bat" "$WINDOWS_STAGING/"
cp "$PACKAGING_DIR/README.md" "$WINDOWS_STAGING/"
cp "$LICENSE_FILE" "$WINDOWS_STAGING/"

WINDOWS_ARCHIVE="stackable-odbc-sqlite-${VERSION}-windows-x64.zip"
(cd "$WINDOWS_STAGING" && zip -r "$DIST_DIR/$WINDOWS_ARCHIVE" .)
rm -rf "$WINDOWS_STAGING"

echo "Built:"
echo "  $DIST_DIR/$LINUX_ARCHIVE"
echo "  $DIST_DIR/$WINDOWS_ARCHIVE"
