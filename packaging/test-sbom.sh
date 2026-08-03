#!/usr/bin/env bash
# Assertions for packaging/sbom.sh, run against the real release artifacts.
#
# Needs syft and cargo-auditable. Builds the .so if absent; the Windows checks
# are skipped unless the DLL has been cross-compiled too.
# Run from anywhere: ./packaging/test-sbom.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SO="$REPO_ROOT/target/release/libstackable_odbc_sqlite.so"
DLL="$REPO_ROOT/target/x86_64-pc-windows-gnu/release/stackable_odbc_sqlite.dll"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILURES=0
check() {
  local label="$1" actual="$2" expected="$3"
  if [ "$actual" = "$expected" ]; then
    echo "PASS  $label"
  else
    echo "FAIL  $label: expected '$expected', got '$actual'"
    FAILURES=$((FAILURES + 1))
  fi
}

if [ ! -f "$SO" ]; then
  echo "Building the release artifact with cargo auditable..."
  (cd "$REPO_ROOT" && cargo auditable build --locked --release)
fi

"$REPO_ROOT/packaging/sbom.sh" "$SO" "$WORK"
SBOM="$WORK/libstackable_odbc_sqlite.so.cdx.json"
SPDX="$WORK/libstackable_odbc_sqlite.so.spdx.json"

check "SBOM file is written" "$([ -f "$SBOM" ] && echo yes || echo no)" "yes"
check "SPDX file is written" "$([ -f "$SPDX" ] && echo yes || echo no)" "yes"

# The whole point of reading .dep-v0 rather than Cargo.toml is that the list is
# the linked graph, so it has to be a graph rather than a handful of entries.
# A bound rather than an exact count: an exact one turns every dependency bump
# into a failing test that says nothing about the change.
check "the component list is the whole graph" \
  "$(jq '.components | length >= 30' "$SBOM")" "true"

check "every component is licensed" \
  "$(jq '[.components[] | select((.licenses // []) | length == 0)] | length' "$SBOM")" "0"

check "syft cpe23 noise is stripped" \
  "$(jq '[.components[].properties[]? | select(.name | startswith("syft:cpe23"))] | length' "$SBOM")" "0"

check "dev-dependencies are absent" \
  "$(jq '[.components[] | select(.name | test("^(criterion|proptest)$"))] | length' "$SBOM")" "0"

check "the artifact is the SBOM subject" \
  "$(jq -r '.metadata.component.name' "$SBOM")" "libstackable_odbc_sqlite.so"

check "the subject carries a sha256" \
  "$(jq -r '.metadata.component.hashes[]? | select(.alg == "SHA-256") | .content' "$SBOM" | tr -d '\n' | wc -c)" "64"

check "no absolute build path leaks" \
  "$(jq -r '[.. | strings | select(startswith("/home/") or startswith("/build/"))] | length' "$SBOM")" "0"

check "the rust toolchain is recorded" \
  "$(jq '[.metadata.properties[]? | select(.name == "stackable:rustc-version")] | length' "$SBOM")" "1"

# Core is pinned by branch, so its commit moves with every core change. Assert
# the shape rather than the value: a resolved 40-character commit, never the
# branch name, or the purl would name whatever that branch points at today.
check "core's purl names an immutable commit" \
  "$(jq -r '[.components[] | select(.name == "stackable-odbc-core")
             | select(.purl | test("\\?vcs_url=git\\+https://github\\.com/stackabletech/stackable-odbc-core\\.git@[0-9a-f]{40}$"))] | length' "$SBOM")" "1"

# One: stackable-odbc-sqlite, the root package, which is path-local permanently.
# The gate is not "zero path components": it is that the only path-sourced
# component is the root package, which is what catches a developer's local
# `[patch]` override shipping in a release artifact.
check "the only path-sourced component is the root package" \
  "$(jq -r '[.components[]
             | select(.properties[]? | select(.name == "stackable:cargo-source" and .value == "path"))
             | .name] | join(",")' "$SBOM")" \
  "stackable-odbc-sqlite"

# --- the bundled SQLite ----------------------------------------------------
# The reason this repo has a `common` fragment at all. cargo sees the wrapper
# crate, libsqlite3-sys; the C library compiled inside it is what an advisory
# against SQLite names, and only sbom-native.json carries it.
# That the version it carries is the version actually linked is asserted in
# Rust instead, by `the_declared_sqlite_version_is_the_one_linked` in
# src/lib.rs: it compares the fragment against `rusqlite::version()`, which is
# the linked library answering for itself rather than a file path guessed at.
check "SQLite itself is a component, not just its wrapper" \
  "$(jq '[.components[] | select(.name == "sqlite")] | length' "$SBOM")" "1"

check "the native Linux component is merged in" \
  "$(jq '[.components[] | select(.name == "unixodbc")] | length' "$SBOM")" "1"

check "the native component keeps its soname" \
  "$(jq -r '.components[] | select(.name == "unixodbc")
            | .properties[] | select(.name == "stackable:soname") | .value' "$SBOM")" \
  "libodbcinst.so.2"

check "the Windows runtime is not merged into a Linux SBOM" \
  "$(jq '[.components[] | select(.name == "libgcc" or .name == "mingw-w64-runtime")] | length' "$SBOM")" "0"

# --- SPDX ------------------------------------------------------------------
# SPDX is converted from the enriched CycloneDX rather than generated afresh, so
# the enrichment reaches both formats from one implementation. These assert the
# conversion carries it across.

check "SPDX carries the enriched licenses" \
  "$(jq '[.packages[] | select(.externalRefs[]? | .referenceType == "purl")
          | select((.licenseDeclared // "NOASSERTION") == "NOASSERTION")] | length' "$SPDX")" "0"

check "SPDX carries the native components" \
  "$(jq '[.packages[] | select(.name == "unixodbc" or .name == "sqlite")] | length' "$SPDX")" "2"

check "SPDX leaks no build path" \
  "$(jq '[.. | strings | select(startswith("/home/") or startswith("/build/"))] | length' "$SPDX")" "0"

# --- --check-native --------------------------------------------------------

check "--check-native passes on the current fragment" \
  "$("$REPO_ROOT/packaging/sbom.sh" --check-native "$SO" >/dev/null 2>&1 && echo ok || echo failed)" "ok"

# Drift must be detected, not tolerated. Feed it a fragment with the entry
# removed and require a non-zero exit.
jq 'del(.linux[0])' "$REPO_ROOT/packaging/sbom-native.json" > "$WORK/drifted.json"
check "--check-native detects a missing entry" \
  "$(SBOM_NATIVE="$WORK/drifted.json" "$REPO_ROOT/packaging/sbom.sh" --check-native "$SO" >/dev/null 2>&1 && echo ok || echo failed)" "failed"

# The wrong soname must be caught too, which is the mistake of naming libodbc
# where the artifact links libodbcinst.
jq '.linux[0].properties |= map(if .name == "stackable:soname" then .value = "libodbc.so.2" else . end)' \
  "$REPO_ROOT/packaging/sbom-native.json" > "$WORK/wrong-soname.json"
check "--check-native detects a wrong soname" \
  "$(SBOM_NATIVE="$WORK/wrong-soname.json" "$REPO_ROOT/packaging/sbom.sh" --check-native "$SO" >/dev/null 2>&1 && echo ok || echo failed)" "failed"

# --- the Windows artifact --------------------------------------------------
# Generated as well as checked, because the two artifact formats take different
# branches through augment and finalize. Syft emits a second self-entry of type
# "application" for the PE artifact, which the Linux run never exercises.
if [ -f "$DLL" ]; then
  check "--check-native passes on the Windows DLL" \
    "$("$REPO_ROOT/packaging/sbom.sh" --check-native "$DLL" >/dev/null 2>&1 && echo ok || echo failed)" "ok"

  "$REPO_ROOT/packaging/sbom.sh" "$DLL" "$WORK" >/dev/null
  WSBOM="$WORK/stackable_odbc_sqlite.dll.cdx.json"

  check "Windows: every component is licensed" \
    "$(jq '[.components[] | select((.licenses // []) | length == 0)] | length' "$WSBOM")" "0"

  check "Windows: no self-entry survives" \
    "$(jq '[.components[] | select((.purl // "") == "")] | length' "$WSBOM")" "0"

  check "Windows: the toolchain runtime is declared" \
    "$(jq -r '[.components[] | select(.name == "mingw-w64-runtime" or .name == "libgcc") | .name] | sort | join(",")' "$WSBOM")" \
    "libgcc,mingw-w64-runtime"

  check "Windows: SQLite is declared there too" \
    "$(jq '[.components[] | select(.name == "sqlite")] | length' "$WSBOM")" "1"

  check "Windows: unixODBC is not merged in" \
    "$(jq '[.components[] | select(.name == "unixodbc")] | length' "$WSBOM")" "0"

  check "Windows: no absolute build path leaks" \
    "$(jq '[.. | strings | select(startswith("/home/") or startswith("/build/"))] | length' "$WSBOM")" "0"

  check "Windows: the artifact is the SBOM subject" \
    "$(jq -r '.metadata.component.name' "$WSBOM")" "stackable_odbc_sqlite.dll"
else
  echo "SKIP  Windows checks: DLL not built (cargo auditable build --release --target x86_64-pc-windows-gnu)"
fi

# An artifact built without cargo auditable must be refused, not silently turned
# into a near-empty SBOM. Strip the section to prove the guard fires.
cp "$SO" "$WORK/no-audit.so"
objcopy --remove-section=.dep-v0 "$WORK/no-audit.so" 2>/dev/null || true
check "an artifact without .dep-v0 is refused" \
  "$("$REPO_ROOT/packaging/sbom.sh" "$WORK/no-audit.so" "$WORK/refused" >/dev/null 2>&1 && echo ok || echo refused)" "refused"

echo
if [ "$FAILURES" -eq 0 ]; then
  echo "All checks passed."
else
  echo "$FAILURES check(s) failed."
  exit 1
fi
