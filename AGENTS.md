# Agent Guide

Implementation details for AI agents working on `stackable-odbc-sqlite`.

This crate is an ODBC driver for [SQLite](https://sqlite.org). It contains
**only** SQLite-specific code: the `Backend` and `StatementBackend`
implementations, connection-string parsing, SQLite-to-ODBC type conversion, ODBC
escape-sequence translation, and the catalog and metadata functions. Everything
generic (handle management, UTF-16 marshalling, diagnostics, panic safety, and
the C ABI entry points) lives in
[`stackable-odbc-core`](https://github.com/stackabletech/stackable-odbc-core).

## Quick Reference

| Topic | When to Read |
|-------|-------------|
| [Relationship to core](#relationship-to-stackable-odbc-core) | Deciding where a change belongs |
| [Conventions](#conventions) | Any code change |
| [Backend error mapping](#backend-error-mapping) | Touching an error path |
| [Declaring capabilities](#declaring-capabilities) | Adding or changing any `SQLGetInfo` value |
| [Transactions](#transactions) | Touching `SQLEndTran`, autocommit or cursor behaviour |
| [Cancellation](#cancellation) | Touching `SQLCancel` or `SQL_ATTR_QUERY_TIMEOUT` |
| [`row_count` has three answers](#row_count-has-three-answers-not-two) | Touching `SQLRowCount` or the execute path |
| [Catalog functions](#catalog-functions) | Touching anything in `metadata.rs` |
| [Architecture](#architecture-of-this-crate) | Understanding the module layout |
| [Connection string keys](#connection-string-keys) | Adding or changing a parameter |
| [Testing](#testing) | Writing or running tests |
| [Packaging](#packaging) | Cutting a release |

```bash
cargo build                                  # needs unixodbc-dev
cargo test                                   # unit + FFI tests; needs no server
cargo clippy --all-targets -- -D warnings
pre-commit run --all-files                   # the gate; run before every commit

./integration-tests/setup.sh                 # build driver, create the DB, write ODBC config
./integration-tests/run-tests.sh             # run the integration suite
```

## Relationship to stackable-odbc-core

`stackable-odbc-core` is a path dependency on a sibling checkout until it is
published:

```toml
stackable-odbc-core = { path = "../stackable-odbc-core" }
```

There is a matching `TODO` in `Cargo.toml`. Until it is resolved, CI cannot
pass, because a path dependency does not resolve on a runner. This crate is not published to
crates.io; releases are GitHub Release archives built by
`.github/workflows/release.yaml`.

| Concern | Owner |
|---------|-------|
| Handle allocation, tag validation, `panic_safe` | core |
| UTF-16 marshalling, diagnostics, `SQLGetDiagRec` | core |
| The exported C ABI entry points (`forward_ffi!`): 60 `SQL*` functions, plus `ConfigDSNW` on Windows | core |
| `SQLGetInfo` marshalling and shape checking, cursor-state tracking | core |
| Every `SQLGetInfo` value that describes SQLite | this crate, see [Declaring capabilities](#declaring-capabilities) |
| `Backend` / `StatementBackend` trait definitions | core |
| Opening the database, executing, fetching | this crate |
| SQLite storage class → SQL type mapping, value conversion | this crate |
| Querying SQLite for catalog metadata | this crate, see [Catalog functions](#catalog-functions) |
| Catalog column layout, sort order, the `SQL_ALL_*` enumerations | core |
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

Every change an application can observe (a reported `SQLGetInfo` value, a
SQLSTATE, a type mapping) gets an entry in `CHANGELOG.md` under
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
// BAD: the value is unchecked and the name is only a comment
sql_bind_parameter::<B>(stmt, 1, 1 /* SQL_PARAM_INPUT */, ..., -5 /* SQL_BIGINT */, ...);

// GOOD: the compiler validates both
sql_bind_parameter::<B>(stmt, 1, ParamType::Input as i16, ..., SqlDataType::EXT_BIG_INT.0, ...);
```

Prefer the `odbc-sys` type over defining a new constant when one exists. Most
spec values are already modelled:

| Value | Use |
|-------|-----|
| `SQL_PARAM_INPUT`, `SQL_PARAM_OUTPUT`, … | `ParamType::Input as i16` |
| `SQL_BIGINT`, `SQL_VARCHAR`, `SQL_INTEGER`, … | `SqlDataType::EXT_BIG_INT.0` (note the `.0`) |
| `SQL_C_SBIGINT`, `SQL_C_WCHAR`, … | `CDataType::SBigInt as i16` |
| `SQL_ATTR_*` | `StatementAttribute::*` / `ConnectionAttribute::*` |
| `SQL_HANDLE_*` | `HandleType::*` |

All are re-exported from `stackable_odbc_core::types`. **This crate takes no
direct `odbc-sys` dependency**, reaching those types only through core's
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
`SQLITE_CONSTRAINT_NOTNULL` from `SQLITE_CONSTRAINT_FOREIGNKEY`, as the SQLSTATE
cannot) and `with_source` (the causal chain). A new classified variant must
carry `cause` too, or it silently reports native code `0`.

**One error type, both directions.** Every `Backend` and `StatementBackend`
method returns `Result<_, SqliteError>`, because core requires
`Into<OdbcError> + From<OdbcError> + Error + Send + Sync + 'static`. The
`From<OdbcError>` direction is what lets a defaulted trait body construct an
error and still name `Self::Error`, and `SqliteError::Odbc` is where such an
error lands. Return `OdbcError::NoResultSet` and friends through `.into()`
rather than reclassifying them: the round trip is lossless, and reclassifying
would discard the SQLSTATE core chose.

Convert raw integers to typed enums at the boundary with the `xxx_from_raw()`
functions from core, never `transmute`.

### 08001 versus 08S01

`08001` ("client unable to establish connection") is only valid from the
connection functions. Once a connection exists, a failing link is `08S01`
("communication link failure"). That is the code the diagnostics tables of
`SQLExecute`, `SQLFetch`, `SQLGetInfo` and the rest actually list.

For this driver `connect` is where real I/O happens:
`rusqlite::Connection::open` touches the filesystem, so a missing or unreadable
database file is `08001`. Failures after that point are `08S01`.

### Declaring capabilities

`Backend` has around thirty **required** methods that state what SQLite can
do: `alter_table_support`, `outer_join_capabilities`, `subqueries`,
`sql_conformance`, `supports_catalogs`, `identifier_case`,
`quoted_identifier_case`, `txn_capable`, `txn_isolation_options`, `integrity`,
`multiple_active_txn`, `special_characters`, `accessible_procedures`,
`dbms_name`, `dbms_version`, `table_types` and the rest. They are required,
with no default, deliberately: a defaulted capability is a claim no backend
ever made, and every one of them was a bug here before core made it a compile
error. `table_types` is required for the same reason and one of its own: an
empty table-type list is an *answer* ("this data source has no table types"),
not "unknown", and unlike catalogs and schemas there is no `supports_*` method
for core to derive it from. `special_characters` is required on that same
principle: `""` asserts that nothing beyond the alphanumerics and underscore
is legal unquoted, which is a claim, not an absence, and inheriting it as a
default is how this driver came to under-report `$`.

They all take `&Self::Connection`, because `SQLGetInfo` is a per-connection
call and a data source's capabilities can differ by server. Every one this
driver declares is a property of the SQLite `rusqlite` links, not of the file
opened, so each ignores the argument, but the answer must still be read
through a connection, and the tests do that via `info::tests::test_connection`
rather than calling the hook as a free function. `cursor_commit_behavior`,
`cursor_rollback_behavior`, `catalog_result_column_widths`, `driver_name` and
`driver_version` are the exceptions and take none: `SQLGetInfo` must answer the
first three before a connection exists, and the Windows Driver Manager asks for
driver identity before `SQLDriverConnectW`. Note the split within the identity
group: `driver_name`/`driver_version` describe the driver and take no
connection, while `dbms_name`/`dbms_version` describe what was connected to and
take one.

The same split runs through `get_info`. `sqlite_get_info` takes
`Option<&SqliteConnection>` (`None` on the pre-connect path) and hands it to
`default_get_info` / `common_get_info_raw`, which answer only what is knowable
without a data source and leave the rest. An arm that consults a capability
hook must therefore be guarded on the connection being present, which is why
`SQL_MAX_CATALOG_NAME_LEN` and `SQL_MAX_SCHEMA_NAME_LEN` only report `0` once
one is open.

Four rules, all learned the hard way:

**Declare it once.** A capability with a hook is answered *only* through the
hook, never also in `get_info_raw`. Core derives the info type from the hook,
so a second answer is a value that can disagree with itself, and the one an
application sees depends on which core consults first. `SQL_IDENTIFIER_CASE`
was stated in both places; so was `SQL_GETDATA_EXTENSIONS`, which is not even a
fact about SQLite: it describes core's own fetch path, and belongs to core for
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
about the same thing, and assert the relationship.
`catalog_and_schema_info_types_agree_with_each_other` and
`transaction_isolation_offers_only_the_level_sqlite_implements` are that check,
and they assert the spec's rule rather than today's values, so they keep
holding if the answer changes.

### Transactions

`connect` issues `PRAGMA foreign_keys = ON`. SQLite leaves it off for backward
compatibility, and the bundled library only happens to compile with
`SQLITE_DEFAULT_FOREIGN_KEYS`, so without the pragma `SQL_INTEGRITY = "Y"`
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
`rusqlite::Statement` is live when `end_tran` runs. Raw SQLite is stricter: a
ROLLBACK aborts pending statements with `SQLITE_ABORT` (>= 3.7.11), which would
be `SQL_CB_CLOSE`, and a COMMIT with pending writes fails with `SQLITE_BUSY`.

If result sets ever become lazily streamed, both hooks must be revisited, and
`SQL_CB_CLOSE` would additionally require a real
`StatementBackend::close_cursor`, which is fallible now (`Result<(),
Self::Error>`), because under `SQL_CB_CLOSE` it is the only thing that closes
the cursor during `SQLEndTran`, and a failure has to reach the statement's
diagnostic queue rather than be swallowed. Here it only resets an index into an
already-materialised `Vec`, so it cannot fail.
`end_tran_cursor_behaviour_is_preserve_for_commit_and_rollback` pins the
reported values through the FFI entry point.

`SQL_ATTR_TXN_ISOLATION` is validated by core against `txn_isolation_options`,
which this driver answers with `SQL_TXN_SERIALIZABLE` alone. Setting any other
level on an open connection is refused with `HY024` rather than stored and
echoed back. See `txn_isolation_accepts_only_the_level_sqlite_implements`.
Because `txn_isolation_options` is a per-connection hook, a level set *before*
connecting is only checked for naming exactly one level; the comparison against
the hook happens at connect time, so an unsupported level fails the connect.

### Cancellation

`SQLCancel` is real: `Backend::CancelToken` is `Arc<rusqlite::InterruptHandle>`
and `cancel` calls `sqlite3_interrupt`, which stops the in-flight
`sqlite3_step` on that connection.

This is the **aliasing** token shape of the two `Backend::CancelToken`'s doc
comment describes (the token refers to the same connection the statement is
executing on), and it is sound only because SQLite documents
`sqlite3_interrupt` as safe to call from another thread. The `Arc` is core's
requirement for that shape: core clones the token out of its registry before
touching anything else, so the token has to survive a concurrent
`SQLDisconnect`. `rusqlite` already satisfies the underlying rule: its
`InterruptHandle` holds an `Arc<Mutex<*mut sqlite3>>` shared with the
connection, and `InnerConnection::close` nulls that pointer while holding the
same mutex, so a racing `interrupt()` either finds a live handle or finds null
and does nothing.

Three things this depends on, in order:

- **The handle is captured in `connect`,** not fetched on demand.
  `cancel_token` can neither block nor fail, and the `rusqlite::Connection`
  lives behind a `Mutex`, so reaching through it would mean waiting on whatever
  thread is executing. Core's own doc asks for the same thing for a different
  reason: assemble the token with the connection in hand, never lazily inside
  `cancel`.
- **`cancel` takes no lock this driver owns.** On `SQLCancel`'s idle path core
  holds the connection's group lock across the call, so anything that waited on
  it would deadlock. `interrupt()` takes only `rusqlite`'s own short-lived
  interrupt lock, which no ODBC entry point holds.
- **`SQLITE_INTERRUPT` maps to `HY008`.** `map_sqlite_error` classifies
  `ErrorCode::OperationInterrupted` as `SqliteError::OperationCanceled`, which
  is the SQLSTATE the spec's diagnostics tables list for a statement stopped by
  `SQLCancel`. Without that arm a cancelled statement would report `HY000`.

`sql_cancel_from_another_thread_stops_a_running_statement` drives the real
entry points across two threads. It was verified by mutation: with
`token.interrupt()` removed the query runs to completion and the test fails on
the return code. Note the gate it holds: `SQLCancel`'s idle branch clears the
statement's diagnostic queue, so a cancel landing after `SQLExecDirectW`
returns would wipe the `HY008` the test is reading.

`SQL_ATTR_QUERY_TIMEOUT` is still substituted with `0` and reported as `01S02`,
because this driver does not override `Backend::set_query_timeout` and the
default answers `NotImplemented`.

**That is now a gap rather than an impossibility.** The original reason (a
synchronous execute path with no deadline to arm) no longer holds: core owns
the timer (`query_timer.rs`), and `Ok(QueryTimeout::CoreCancels)` asks it to arm
one and call `Backend::cancel` when the deadline passes. `cancel` is real here,
which is exactly the precondition `CoreCancels` documents. Closing the gap means
overriding `set_query_timeout` to return `CoreCancels`, and overriding
`is_cancelled` alongside it, since that is what turns the interrupted statement's
own symptom into the `HYT00` the application is waiting for rather than the
`HY008` a user-initiated `SQLCancel` produces. `SQL_ATTR_QUERY_TIMEOUT` is a
*statement* attribute while the hook receives only the connection, so read
core's scope caveat on `set_query_timeout` before doing it.

## Architecture of this crate

| Path | Responsibility |
|------|----------------|
| `src/lib.rs` | The `forward_ffi!` invocation and the crate docs |
| `src/backend.rs` | `SqliteBackend`, `SqliteConnection`, `SqliteStatement`, `SqliteError`, `map_sqlite_error` |
| `src/backend/execute.rs` | `exec_direct`, `prepare`, `execute`, and the `StatementBackend` impl |
| `src/backend/info.rs` | `SQLGetInfo` answers and the capability bitmaps, plus the snapshot test |
| `src/backend/metadata.rs` | The catalog row producers: tables, columns, primary keys, statistics, special columns |
| `src/backend/params.rs` | Deliberately empty. Parameter binding is inline in `execute.rs`; the entry points are core's |
| `src/backend/types/connect_params.rs` | `SqliteConnectParams` |
| `src/escape_dialect.rs` | ODBC escape-sequence translation for SQLite's dialect |
| `src/type_conversion.rs` | SQLite storage classes and declared types → ODBC SQL types |
| `src/ffi_integration_tests.rs` | Tests that drive the real C ABI entry points |

### Result sets are materialised eagerly

`SqliteStatement` holds `rows: Vec<Vec<ColumnValue>>` and `cursor: i64`, an
index into an in-memory snapshot, not a live SQLite cursor. `exec_direct`
collects every row before returning and the `rusqlite::Statement` is finalized
at that point.

This is load-bearing well beyond memory use. It is why the cursor-behaviour
hooks report `Preserve`, why `SQLEndTran` cannot disturb a cursor, and why
concurrency is a non-issue. Changing it is not a local optimisation.

### `row_count` has three answers, not two

`StatementBackend::row_count` returns `Option<i64>`, and core reads all three
possibilities differently:

| Answer | Means | Here |
|--------|-------|------|
| `Some(n)` | the backend counted | a searched INSERT / UPDATE / DELETE, or a materialised result set |
| `Some(-1)` | `SQL_NO_TOTAL`, cannot determine | a count exceeding `i64`; unreachable in practice |
| `None` | not applicable to this statement | DDL, transaction control, `PRAGMA`, an unexecuted prepared statement |

The distinction between the last two is not cosmetic. Core turns a statement
with **zero columns** reporting **`Some(0)`** into `SQL_NO_DATA`, which is
`SQLExecDirect`'s documented behaviour for "a searched update, insert, or
delete statement that doesn't affect any rows". Answering `Some(0)` for DDL
therefore made every `CREATE TABLE` return `SQL_NO_DATA`.

SQLite offers no predicate for "is this DML" (`sqlite3_stmt_readonly` is false
for DDL too), so `execute::is_searched_dml` decides it from the statement's
leading keyword, past whitespace and both comment forms. `REPLACE` and `WITH`
count alongside the obvious three: the first is an `INSERT OR REPLACE` alias,
and the second fronts a CTE, which is only ever consulted for a zero-column
statement, so a `WITH` that declared no columns cannot be a `WITH ... SELECT`.
Being wrong is not symmetric, so an unrecognised keyword answers "no count":
withholding a count leaves `SQLRowCount` at -1, while inventing one fabricates
`SQL_NO_DATA`.

Do **not** replace this with the number `rusqlite`'s `execute()` returns.
`sqlite3_changes()` reports the rows touched by the *most recently completed*
INSERT, UPDATE or DELETE, so a `CREATE TABLE` run after a three-row `INSERT` is
handed that `3`. `ddl_after_dml_does_not_inherit_the_dml_row_count` pins it.

### Catalog functions

The six catalog methods take a **typed query object** (`&TablesQuery`,
`&ColumnsQuery`, `&PrimaryKeysQuery`, `&ForeignKeysQuery`, `&StatisticsQuery`,
`&SpecialColumnsQuery`) and return **typed row vectors** (`Vec<TableRow>`,
`Vec<ColumnRow>`, `Vec<PrimaryKeyRow>`, `Vec<ForeignKeyRow>`,
`Vec<StatisticsRow>`, `Vec<SpecialColumnRow>`), not a `Self::Statement`. Core
converts each row to the spec's column layout, sorts the set into the order
that function's spec page mandates, and serves it.

Both sides are core's types and both are sealed, which is what a change in
`metadata.rs` has to work with:

- **Neither has a struct expression here.** Every row type is
  `#[non_exhaustive]`, so a row is built from `Default` and the consuming
  setter per column: `TableRow::default().name(n).table_type(t)`. Each setter
  takes `impl Into<T>`, so an `Option<String>` column accepts a bare `String`.
  A column a driver does not populate is simply not named, which is the point,
  since it makes a column added to a spec result set a core-only change instead
  of a break in every driver. The query types are sealed the same way, with
  crate-private fields, an accessor and a `with_*` setter per field, and a
  `new()` for the arguments that have no honest default (`StatisticsQuery`'s
  `unique_only`, `SpecialColumnsQuery`'s `identifier_type`/`scope`/`nullable`).
- **Read the filters off the query, do not destructure it.** The run of
  same-typed `Option<&str>` arguments these hooks used to take is exactly what
  the query types exist to remove: `SQLForeignKeys` took six in a row, where
  swapping a primary-key argument for its foreign-key counterpart compiled
  without complaint. Unpacking a query back into positional arguments at the
  trait boundary reintroduces that hazard one layer down, so the query travels
  all the way into `metadata.rs`.
- **`TablesQuery::table_types()` is already parsed.** Core splits `TableType`
  on commas and strips the optional single quotes (it is a value list, not a
  pattern, and `SQL_ATTR_METADATA_ID` never applies to it), so a backend gets a
  `&[String]` and never parses it. Empty means no filter. A lone `"%"` does
  still arrive, because the `SQL_ALL_TABLE_TYPES` enumeration core answers
  itself additionally requires the other three arguments to be empty strings;
  `metadata::tables` reads that as no filter.

Three further consequences for anything changed in `metadata.rs`:

- **Do not sort, and do not add an `ORDER BY` for ODBC's sake.** Core sorts,
  stably, on the spec's keys. A second ordering in the backend is one more
  place for it to be wrong, and it silently overrides nothing: core re-sorts
  regardless. The one thing to keep in mind is that the sort takes NULL
  placement from `Backend::null_collation`, which is why `SQLStatistics`'
  table-stat row (NULL `NON_UNIQUE`) still comes first: this driver reports
  `SQL_NC_LOW`.
- **Do not handle the `SQL_ALL_*` enumerations.** `SQL_ALL_CATALOGS`,
  `SQL_ALL_SCHEMAS` and `SQL_ALL_TABLE_TYPES` are all the same `"%"` sentinel,
  distinguished by which argument carries it while the others are empty
  strings. Core detects them on the *raw* arguments before calling `tables`,
  and answers from `supports_catalogs`, `supports_schemas` and `table_types`.
  `catalogs` and `schemas` are left defaulted here because the first two hooks
  say SQLite has neither, so core never asks.
- **A non-`Option` field is a column the spec marks "not NULL".** The types
  enforce it, which is how `SQLForeignKeys`' `PKCOLUMN_NAME` stopped being
  reported as NULL for a `REFERENCES parent` with no column list. SQLite
  defines that as the parent's primary key, so `parent_pk_column` resolves the
  name rather than dropping it.

Because ordering is core's, an ordering assertion belongs in
`ffi_integration_tests.rs`, where core's sort has actually run. The unit tests
in `metadata.rs` assert only which rows exist and what each field holds. See
`sql_statistics_w_orders_table_stat_row_first_then_unique_before_non_unique`.

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

Needs no database file: the FFI tests connect to `:memory:`. `cargo test` runs
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
`rusqlite::Connection` inside it are no longer reachable from here, so use the
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
./integration-tests/setup.sh          # build, create the database, write the ODBC config
./integration-tests/run-tests.sh      # pyodbc suite through real unixODBC, then cargo test
./integration-tests/run-tests.sh --windows          # also run the Windows VM suite
./integration-tests/run-tests.sh --skip-cargo-test  # pyodbc only; what CI passes
```

Both are wrappers; the logic is in `integration-tests/scripts/`, with the paths
and helpers they share in `scripts/lib.sh`. Everything `setup.sh` writes lands
in `integration-tests/generated/`, which is gitignored wholesale because all of
it embeds absolute paths. See
[integration-tests/README.md](integration-tests/README.md) for the layout and
why the pyodbc suite is run twice.

### Windows VM tests

See [integration-tests/windows/WINDOWS.md](integration-tests/windows/WINDOWS.md).
Requires a provisioned libvirt VM; `integration-tests/windows/windows_test.py`
runs the same pyodbc suite over WinRM, DSN-less and then via DSN.

### Benchmarks

```bash
cargo bench
```

`benches/fetch_sqlite.rs` drives the full FFI fetch path against `:memory:` and
exists to catch regressions in the eager-materialise and per-call clone costs of
the `SqliteBackend` → `ColumnValue` → `write_column_value` pipeline.

### What runs in core, not here

Do not reintroduce these; they moved with the framework:

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
