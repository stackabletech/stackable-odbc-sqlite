#!/usr/bin/env bash
# Assemble release archives for stackable-odbc-sqlite.
#
# Preconditions:
#   - $VERSION environment variable set (e.g. "1.0.0-beta.1")
#   - target/release/libstackable_odbc_sqlite.so exists
#   - target/x86_64-pc-windows-gnu/release/stackable_odbc_sqlite.dll exists
#   - both built with `cargo auditable`, which embeds the .dep-v0 section the
#     SBOM is generated from. sbom.sh refuses an artifact without it.
#   - syft on PATH
#
# Output (written to packaging/dist/):
#   - stackable-odbc-sqlite-<version>-linux-x64.tar.gz
#   - stackable-odbc-sqlite-<version>-windows-x64.zip
#   - a CycloneDX and an SPDX SBOM per artifact, four files
#   - sha256sums.txt over everything above
#
# Each archive also carries the CycloneDX SBOM for what it contains, so an
# offline or air-gapped install has it without going back to the release page.
set -euo pipefail

: "${VERSION:?VERSION environment variable must be set}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGING_DIR="$REPO_ROOT/packaging"
DIST_DIR="$PACKAGING_DIR/dist"
LINUX_SO="$REPO_ROOT/target/release/libstackable_odbc_sqlite.so"
WINDOWS_DLL="$REPO_ROOT/target/x86_64-pc-windows-gnu/release/stackable_odbc_sqlite.dll"
LICENSE_FILE="$REPO_ROOT/LICENSE"

if [ ! -f "$LINUX_SO" ]; then
  echo "ERROR: $LINUX_SO not found. Run 'cargo auditable build --release' first." >&2
  exit 1
fi
if [ ! -f "$WINDOWS_DLL" ]; then
  echo "ERROR: $WINDOWS_DLL not found. Run 'cargo auditable build --release --target x86_64-pc-windows-gnu' first." >&2
  exit 1
fi
if [ ! -f "$LICENSE_FILE" ]; then
  echo "ERROR: LICENSE file not found at $LICENSE_FILE" >&2
  exit 1
fi

mkdir -p "$DIST_DIR"

# --- SBOMs ---
# Generated first, because each archive carries the one describing its contents.
# sbom.sh writes <basename>.cdx.json and <basename>.spdx.json.
SBOM_DIR="$DIST_DIR/sbom"
rm -rf "$SBOM_DIR"
mkdir -p "$SBOM_DIR"

"$PACKAGING_DIR/sbom.sh" "$LINUX_SO" "$SBOM_DIR"
"$PACKAGING_DIR/sbom.sh" "$WINDOWS_DLL" "$SBOM_DIR"

LINUX_SBOM="$SBOM_DIR/$(basename "$LINUX_SO").cdx.json"
WINDOWS_SBOM="$SBOM_DIR/$(basename "$WINDOWS_DLL").cdx.json"

# --- Linux archive ---
LINUX_STAGING="$DIST_DIR/staging-linux"
rm -rf "$LINUX_STAGING"
mkdir -p "$LINUX_STAGING"
cp "$LINUX_SO" "$LINUX_STAGING/"
cp "$PACKAGING_DIR/linux/install.sh" "$LINUX_STAGING/"
cp "$PACKAGING_DIR/linux/uninstall.sh" "$LINUX_STAGING/"
cp "$PACKAGING_DIR/README.md" "$LINUX_STAGING/"
cp "$LICENSE_FILE" "$LINUX_STAGING/"
cp "$LINUX_SBOM" "$LINUX_STAGING/"
chmod +x "$LINUX_STAGING/install.sh" "$LINUX_STAGING/uninstall.sh"

LINUX_ARCHIVE="stackable-odbc-sqlite-${VERSION}-linux-x64.tar.gz"
tar -czf "$DIST_DIR/$LINUX_ARCHIVE" -C "$LINUX_STAGING" .
rm -rf "$LINUX_STAGING"

# --- Windows archive ---
# configure-dsn.ps1 is not an extra: install.bat refuses to register the driver
# without it, because it is the dialog the ODBC Administrator's "Add..." button
# displays.
WINDOWS_STAGING="$DIST_DIR/staging-windows"
rm -rf "$WINDOWS_STAGING"
mkdir -p "$WINDOWS_STAGING"
cp "$WINDOWS_DLL" "$WINDOWS_STAGING/"
cp "$PACKAGING_DIR/windows/install.bat" "$WINDOWS_STAGING/"
cp "$PACKAGING_DIR/windows/uninstall.bat" "$WINDOWS_STAGING/"
cp "$PACKAGING_DIR/windows/configure-dsn.ps1" "$WINDOWS_STAGING/"
cp "$PACKAGING_DIR/README.md" "$WINDOWS_STAGING/"
cp "$LICENSE_FILE" "$WINDOWS_STAGING/"
cp "$WINDOWS_SBOM" "$WINDOWS_STAGING/"

WINDOWS_ARCHIVE="stackable-odbc-sqlite-${VERSION}-windows-x64.zip"
(cd "$WINDOWS_STAGING" && zip -r "$DIST_DIR/$WINDOWS_ARCHIVE" .)
rm -rf "$WINDOWS_STAGING"

# --- SBOMs as release assets ---
# Named with the version, so an asset downloaded on its own still says which
# release it describes.
for fmt in cdx spdx; do
  cp "$SBOM_DIR/$(basename "$LINUX_SO").$fmt.json" \
     "$DIST_DIR/stackable-odbc-sqlite-${VERSION}-linux-x64.$fmt.json"
  cp "$SBOM_DIR/$(basename "$WINDOWS_DLL").$fmt.json" \
     "$DIST_DIR/stackable-odbc-sqlite-${VERSION}-windows-x64.$fmt.json"
done
rm -rf "$SBOM_DIR"

# --- Checksums ---
# Over every published file, generated last so it covers the SBOMs too. Paths
# are relative, so `sha256sum -c sha256sums.txt` works from the download
# directory.
(cd "$DIST_DIR" && sha256sum ./*.tar.gz ./*.zip ./*.json > sha256sums.txt)

echo "Built:"
echo "  $DIST_DIR/$LINUX_ARCHIVE"
echo "  $DIST_DIR/$WINDOWS_ARCHIVE"
echo "  $DIST_DIR/sha256sums.txt  ($(wc -l < "$DIST_DIR/sha256sums.txt") entries)"
