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
| Generic `SQLGetInfo` defaults, cursor-state tracking | core |
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
re-exports. That is deliberate: `src/ffi_integration_tests.rs` defines a local
`RawTimestamp` mirroring `SQL_TIMESTAMP_STRUCT` rather than pull the crate in.
Do not add `odbc-sys` to `Cargo.toml`.

### Type cast safety

Never `as`-cast a value that can exceed the target type. Use `try_into()` and
map the failure to a SQLSTATE, or clamp deliberately with a comment saying why
the clamp is correct. Row counts, column sizes and buffer lengths all cross
between `usize`, `i64`, `u16` and `i16` in this crate.

### Backend error mapping

**Route every `rusqlite` error through `map_sqlite_error`** (`src/backend.rs`).
Never hand-build a `SqliteError` or `OdbcError` from a `rusqlite::Error` at the
call site; that function is the single place that decides the SQLSTATE.

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

### Transactions

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
`StatementBackend::close_cursor`. `end_tran_cursor_behaviour_is_preserve_for_commit_and_rollback`
pins the reported values through the FFI entry point.

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

### Integration tests

```bash
./test/setup.sh          # build, create test/test.db, write odbc.ini/odbcinst.ini
./test/run-tests.sh      # pyodbc suite through real unixODBC
./test/run-tests.sh --windows   # also run the Windows VM suite
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
