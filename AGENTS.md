# Agent Guide

Implementation details for AI agents working on `stackable-odbc-sqlite`.

This crate is an ODBC driver for [SQLite](https://sqlite.org). It contains
**only** SQLite-specific code: the `Backend` and `StatementBackend`
implementations, connection-string parsing, SQLite-to-ODBC type conversion, ODBC
escape-sequence translation, and the catalog and metadata functions. Everything
generic — handle management, UTF-16 marshalling, diagnostics, panic safety, and
the 73 C ABI entry points — lives in
[`stackable-odbc-core`](https://github.com/stackabletech/stackable-odbc-core).

## Quick Reference

| Topic | When to Read |
|-------|-------------|
| [Relationship to core](#relationship-to-stackable-odbc-core) | Deciding where a change belongs |
| [Conventions](#conventions) | Any code change |
| [Backend error mapping](#backend-error-mapping) | Touching an error path |
| [Declaring capabilities](#declaring-capabilities) | Adding or changing any `SQLGetInfo` value |
| [Transactions](#transactions) | Touching `SQLEndTran`, autocommit or cursor behaviour |
| [Architecture](#architecture-of-this-crate) | Understanding the module layout |
| [Connection string keys](#connection-string-keys) | Adding or changing a parameter |
| [Testing](#testing) | Writing or running tests |
| [Packaging](#packaging) | Cutting a release |

```bash
cargo build                                  # needs unixodbc-dev
cargo test                                   # unit + FFI tests; needs no server
cargo clippy --all-targets -- -D warnings
pre-commit run --all-files                   # the gate; run before every commit

./test/setup.sh                              # build driver, create test.db, write ODBC config
./test/run-tests.sh                          # run the integration suite
```

## Relationship to stackable-odbc-core

`stackable-odbc-core` is a path dependency on a sibling checkout until it is
published:

```toml
stackable-odbc-core = { path = "../stackable-odbc-core" }
```

There is a matching `TODO` in `Cargo.toml`. Until it is resolved, CI cannot pass
— a path dependency does not resolve on a runner. This crate is not published to
crates.io; releases are GitHub Release archives built by
`.github/workflows/release.yaml`.

| Concern | Owner |
|---------|-------|
| Handle allocation, tag validation, `panic_safe` | core |
| UTF-16 marshalling, diagnostics, `SQLGetDiagRec` | core |
| The 73 exported C ABI entry points (`forward_ffi!`) | core |
| `SQLGetInfo` marshalling and shape checking, cursor-state tracking | core |
| Every `SQLGetInfo` value that describes SQLite | this crate — see [Declaring capabilities](#declaring-capabilities) |
| `Backend` / `StatementBackend` trait definitions | core |
| Opening the database, executing, fetching | this crate |
| SQLite storage class → SQL type mapping, value conversion | this crate |
| Catalog and metadata queries | this crate |
| Connection-string parsing | this crate |
| ODBC escape-sequence translation | this crate |

`src/lib.rs` is the whole export surface:

```rust
stackable_odbc_core::forward_ffi!(crate::backend::SqliteBackend);
```

That one line expands to every `#[unsafe(no_mangle)] pub unsafe extern "system"`
entry point. If a new ODBC function needs to be exported, it is added to core's
`forward_ffi!` macro, not here; this crate only implements whatever new trait
method it calls.

## Conventions

### Changelog

Every change an application can observe — a reported `SQLGetInfo` value, a
SQLSTATE, a type mapping — gets an entry in `CHANGELOG.md` under
`## [Unreleased]`, following [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Internal refactoring does not.

### Logging in backend methods

Use `tracing` macros, never `println!`. The FFI entry points in core already log
their own arguments and return codes, so a backend method should log what core
cannot see: the SQL it is about to run, the SQLite error it just mapped, the
number of rows it materialised. `ODBC_LOG_LEVEL` / `ODBC_LOG_FILE` control
output; both are initialised by core.

### Named constants

ODBC attribute values, function IDs, and bitmap constants must use named `const`
definitions. Never write raw integer literals for ODBC-spec-defined values. Name
them after the ODBC spec name (e.g. `SQL_AUTOCOMMIT_ON`, `SQL_CB_PRESERVE`).

**This applies to tests too.** Test code is where raw literals creep back in
most easily, usually with the spec name relegated to a trailing comment. A
comment is not a constant:

```rust
// BAD — the value is unchecked and the name is only a comment
sql_bind_parameter::<B>(stmt, 1, 1 /* SQL_PARAM_INPUT */, ..., -5 /* SQL_BIGINT */, ...);

// GOOD — the compiler validates both
sql_bind_parameter::<B>(stmt, 1, ParamType::Input as i16, ..., SqlDataType::EXT_BIG_INT.0, ...);
```

Prefer the `odbc-sys` type over defining a new constant when one exists — most
spec values are already modelled:

| Value | Use |
|-------|-----|
| `SQL_PARAM_INPUT`, `SQL_PARAM_OUTPUT`, … | `ParamType::Input as i16` |
| `SQL_BIGINT`, `SQL_VARCHAR`, `SQL_INTEGER`, … | `SqlDataType::EXT_BIG_INT.0` (note the `.0`) |
| `SQL_C_SBIGINT`, `SQL_C_WCHAR`, … | `CDataType::SBigInt as i16` |
| `SQL_ATTR_*` | `StatementAttribute::*` / `ConnectionAttribute::*` |
| `SQL_HANDLE_*` | `HandleType::*` |

All are re-exported from `stackable_odbc_core::types`. **This crate takes no
direct `odbc-sys` dependency** — it reaches those types only through core's
re-exports. Do not add `odbc-sys` to `Cargo.toml`.

Core also re-exports the crate wholesale as `stackable_odbc_core::odbc_sys`,
so a type with no `types` re-export of its own is still reachable without a
direct dependency. Reach for that rather than hand-rolling a `#[repr(C)]`
mirror: `src/ffi_integration_tests.rs` used to carry a local `RawTimestamp`
duplicating `SQL_TIMESTAMP_STRUCT`, and a mirror that drifts from the real
struct is two different types to the compiler and one silent ABI mismatch to
the application.

### Type cast safety

Never `as`-cast a value that can exceed the target type. Use `try_into()` and
map the failure to a SQLSTATE, or clamp deliberately with a comment saying why
the clamp is correct. Row counts, column sizes and buffer lengths all cross
between `usize`, `i64`, `u16` and `i16` in this crate.

### Backend error mapping

**Route every `rusqlite` error through `map_sqlite_error`** (`src/backend.rs`).
Never hand-build a `SqliteError` or `OdbcError` from a `rusqlite::Error` at the
call site; that function is the single place that decides the SQLSTATE.

`map_sqlite_error` keeps the `rusqlite::Error` it classified in the variant's
`cause` field, and `From<SqliteError> for OdbcError` turns that into
`with_native_error` (SQLite's *extended* result code, which is what separates
`SQLITE_CONSTRAINT_NOTNULL` from `SQLITE_CONSTRAINT_FOREIGNKEY` — the SQLSTATE
cannot) and `with_source` (the causal chain). A new classified variant must
carry `cause` too, or it silently reports native code `0`.

**One error type, both directions.** Every `Backend` and `StatementBackend`
method returns `Result<_, SqliteError>` — core requires
`Into<OdbcError> + From<OdbcError> + Error + Send + Sync + 'static`. The
`From<OdbcError>` direction is what lets a defaulted trait body construct an
error and still name `Self::Error`, and `SqliteError::Odbc` is where such an
error lands. Return `OdbcError::NoResultSet` and friends through `.into()`
rather than reclassifying them: the round trip is lossless, and reclassifying
would discard the SQLSTATE core chose.

Convert raw integers to typed enums at the boundary with the `xxx_from_raw()`
functions from core — never `transmute`.

### 08001 versus 08S01

`08001` ("client unable to establish connection") is only valid from the
connection functions. Once a connection exists, a failing link is `08S01`
("communication link failure") — that is the code the diagnostics tables of
`SQLExecute`, `SQLFetch`, `SQLGetInfo` and the rest actually list.

For this driver `connect` is where real I/O happens:
`rusqlite::Connection::open` touches the filesystem, so a missing or unreadable
database file is `08001`. Failures after that point are `08S01`.

### Declaring capabilities

`Backend` has around two dozen **required** methods that state what SQLite can
do — `alter_table_support`, `outer_join_capabilities`, `subqueries`,
`sql_conformance`, `supports_catalogs`, `identifier_case`,
`txn_isolation_options` and the rest. They are required, with no default,
deliberately: a defaulted capability is a claim no backend ever made, and every
one of them was a bug here before core made it a compile error.

Four rules, all learned the hard way:

**Declare it once.** A capability with a hook is answered *only* through the
hook — never also in `get_info_raw`. Core derives the info type from the hook,
so a second answer is a value that can disagree with itself, and the one an
application sees depends on which core consults first. `SQL_IDENTIFIER_CASE`
was stated in both places; so was `SQL_GETDATA_EXTENSIONS`, which is not even a
fact about SQLite — it describes core's own fetch path, and belongs to core for
the same reason. The snapshot test (`get_info_snapshot`) pins the value an
application sees regardless of who answers it, which is what makes moving an
answer safe.

**Probe the bundled library, never the documentation or the system CLI.**
`rusqlite` links its own SQLite (3.53.2 via the `bundled` feature); the
`sqlite3` binary on a developer's machine is a different version. Writing the
`ALTER TABLE` bitmap from the system CLI's behaviour got `ADD CONSTRAINT` and
`DROP CONSTRAINT` wrong, because 3.51.3 rejects both and 3.53.2 accepts them.
`alter_table_capabilities_are_each_live_probed`,
`outer_join_capabilities_are_each_live_probed` and
`subqueries_are_each_live_probed` all execute the syntax they describe.

**Probe the bits you do not claim, too.** A test that only checks what a bitmap
claims can overclaim forever, and a bitmap that only grows when someone notices
can understate forever. The negative half of the `ALTER TABLE` probe is what
caught the two bits above. `SQL_KEYWORDS` goes further and reads the list out
of the library through `sqlite3_keyword_count` / `sqlite3_keyword_name`, so it
needs no maintenance at all.

**Values must agree with each other.** Most defects found in this crate were
one capability stated twice, in opposite directions:

| Said one thing | Said the opposite |
|---|---|
| `SQL_CATALOG_NAME = "N"` | `SQL_CATALOG_TERM = "catalog"` |
| `SQL_OUTER_JOINS = "Y"` | `SQL_OUTER_JOIN_CAPABILITIES = 0` |
| `SQL_SQL_CONFORMANCE = SQL_SC_SQL92_ENTRY` | `SQL_GROUP_BY = SQL_GB_NO_RELATION` |
| `SQL_SQL92_PREDICATES` without `SQL_SP_QUANTIFIED_COMPARISON` | `SQL_SUBQUERIES` with `SQL_SQ_QUANTIFIED` |
| `SQL_TXN_ISOLATION_OPTION` with four levels | nothing applying the level an application sets |

When adding or changing a capability, look for the other info type that talks
about the same thing, and assert the relationship —
`catalog_and_schema_info_types_agree_with_each_other` and
`transaction_isolation_offers_only_the_level_sqlite_implements` are that check,
and they assert the spec's rule rather than today's values, so they keep
holding if the answer changes.

### Transactions

`connect` issues `PRAGMA foreign_keys = ON`. SQLite leaves it off for backward
compatibility, and the bundled library only happens to compile with
`SQLITE_DEFAULT_FOREIGN_KEYS` — so without the pragma, `SQL_INTEGRITY = "Y"`
would depend on a dependency's build flags rather than on this driver.
`integrity_enhancement_facility_is_actually_enforced` checks it through
`connect`.

SQLite supports transactions and this driver reports `SQL_TC_DML` for
`SQL_TXN_CAPABLE`, so manual-commit mode is honoured for real:
`set_autocommit(false)` issues `BEGIN`, and `end_tran` issues `COMMIT` or
`ROLLBACK` and then opens the next transaction while still in manual-commit
mode.

Both `cursor_commit_behavior` and `cursor_rollback_behavior` return
`CursorBehavior::Preserve`, and **this depends on an implementation detail**:
`execute::exec_direct` materialises every result set eagerly, so no
`rusqlite::Statement` is live when `end_tran` runs. Raw SQLite is stricter — a
ROLLBACK aborts pending statements with `SQLITE_ABORT` (>= 3.7.11), which would
be `SQL_CB_CLOSE`, and a COMMIT with pending writes fails with `SQLITE_BUSY`.

If result sets ever become lazily streamed, both hooks must be revisited, and
`SQL_CB_CLOSE` would additionally require a real
`StatementBackend::close_cursor` — which is fallible now (`Result<(),
Self::Error>`), because under `SQL_CB_CLOSE` it is the only thing that closes
the cursor during `SQLEndTran`, and a failure has to reach the statement's
diagnostic queue rather than be swallowed. Here it only resets an index into an
already-materialised `Vec`, so it cannot fail.
`end_tran_cursor_behaviour_is_preserve_for_commit_and_rollback` pins the
reported values through the FFI entry point.

`SQL_ATTR_TXN_ISOLATION` is validated by core against `txn_isolation_options`,
which this driver answers with `SQL_TXN_SERIALIZABLE` alone. Setting any other
level is refused with `HY024` rather than stored and echoed back — see
`txn_isolation_accepts_only_the_level_sqlite_implements`.

## Architecture of this crate

| Path | Responsibility |
|------|----------------|
| `src/lib.rs` | The `forward_ffi!` invocation and the crate docs |
| `src/backend.rs` | `SqliteBackend`, `SqliteConnection`, `SqliteStatement`, `SqliteError`, `map_sqlite_error` |
| `src/backend/execute.rs` | `exec_direct`, `prepare`, `execute`, and the `StatementBackend` impl |
| `src/backend/info.rs` | `SQLGetInfo` answers and the capability bitmaps, plus the snapshot test |
| `src/backend/metadata.rs` | The catalog functions: tables, columns, primary keys, statistics, special columns |
| `src/backend/params.rs` | Parameter binding |
| `src/backend/types/connect_params.rs` | `SqliteConnectParams` |
| `src/escape_dialect.rs` | ODBC escape-sequence translation for SQLite's dialect |
| `src/type_conversion.rs` | SQLite storage classes and declared types → ODBC SQL types |
| `src/ffi_integration_tests.rs` | Tests that drive the real C ABI entry points |

### Result sets are materialised eagerly

`SqliteStatement` holds `rows: Vec<Vec<ColumnValue>>` and `cursor: i64` — an
index into an in-memory snapshot, not a live SQLite cursor. `exec_direct`
collects every row before returning and the `rusqlite::Statement` is finalized
at that point.

This is load-bearing well beyond memory use. It is why the cursor-behaviour
hooks report `Preserve`, why `SQLEndTran` cannot disturb a cursor, and why
concurrency is a non-issue. Changing it is not a local optimisation.

## Connection string keys

Keys are matched case-insensitively and stored lowercase by core's
`ConnectParams`.

| Key | Required | Description |
|-----|----------|-------------|
| `Database` | Yes | Path to the database file, or `:memory:` |

Adding a key means adding a `PARAM_*` constant in
`src/backend/types/connect_params.rs`, reading it in the `TryFrom` impl, and
listing it in `Backend::browse_connect_attrs` if `SQLBrowseConnect` should
prompt for it.

## Testing

### Unit and FFI tests

```bash
cargo test
```

Needs no database file — the FFI tests connect to `:memory:`. `cargo test` runs
both the per-module unit tests and `src/ffi_integration_tests.rs`, which drives
the real exported entry points against real handles. Prefer adding to the FFI
tests when the behaviour is observable by an application: they catch the
marshalling and cursor-state bugs that unit tests on the backend cannot.

Core's `conformance` module and its connection attach/detach helpers sit behind
its default-off `test-support` feature, enabled here under `[dev-dependencies]`
so `cargo test` sees it and `cargo build` does not. It is test code that would
otherwise ship inside the driver binary.

**Set up test data through the FFI, not by reaching into the handle.** Core's
`handles` module is `pub(crate)`, so `ConnectionHandle` and the
`rusqlite::Connection` inside it are no longer reachable from here — use the
`setup_sql`, `query_scalar_i64` and `query_row_two_strings` helpers, which go
through `SQLExecDirect`/`SQLFetch`/`SQLGetData`. Each allocates its own
statement handle rather than borrowing the caller's, because the statement a
test is asserting on usually holds live state (a cursor, a prepared statement,
bound parameters) that setup would destroy. Driving setup through the driver
also means a setup path that breaks fails loudly, instead of leaving the test
asserting against an empty table. `test_support::attach_connection` is the
supported route for the different job of exercising core's connected paths
with no data source open.

### Integration tests

```bash
./test/setup.sh          # build, create test/test.db, write odbc.ini/odbcinst.ini
./test/run-tests.sh      # pyodbc suite through real unixODBC, then cargo test
./test/run-tests.sh --windows          # also run the Windows VM suite
./test/run-tests.sh --skip-cargo-test  # pyodbc only; what CI passes
```

`test/setup.sh` and `test/run-tests.sh` regenerate `test/odbc.ini`,
`test/odbcinst.ini` and `test/test.db`; all three are gitignored because they
hold absolute paths.

### Windows VM tests

See [windows/WINDOWS.md](windows/WINDOWS.md). Requires a provisioned libvirt VM;
`test/windows_test.py` runs the same pyodbc suite over WinRM, DSN-less and then
via DSN.

### Benchmarks

```bash
cargo bench
```

`benches/fetch_sqlite.rs` drives the full FFI fetch path against `:memory:` and
exists to catch regressions in the eager-materialise and per-call clone costs of
the `SqliteBackend` → `ColumnValue` → `write_column_value` pipeline.

### What runs in core, not here

Do not reintroduce these — they moved with the framework:

- **Miri.** The driver crates link C libraries (bundled SQLite) that Miri cannot
  execute. Core is pure Rust and holds the raw-pointer marshalling.
- **Fuzzing.** The `utf16` and `column_value` fuzz targets fuzz core's code.
- **Generic FFI entry-point tests.** Handle tags, panic safety and diagnostics
  are core's.

## Packaging

`packaging/build-archives.sh` assembles the Linux and Windows release archives
from binaries already built by `cargo build --release`; see
[packaging/README.md](packaging/README.md).

### Cutting a release

`release.toml` configures `cargo-release`. It bumps the version, rewrites
`CHANGELOG.md` and `packaging/README.md`, commits, tags and pushes; the `v*` tag
triggers `.github/workflows/release.yaml`, which builds both binaries and
publishes the GitHub Release.

```bash
release/release.sh patch              # dry run
release/release.sh patch --execute    # for real, from main only
```

`publish = false`: this crate is not published to crates.io. Tags and commits
are signed by configuration, so a release cut without a signing key fails
loudly rather than producing an unsigned tag.
