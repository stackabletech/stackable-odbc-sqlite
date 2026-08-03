# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **A setup dialog on Windows.** The ODBC Data Source Administrator's **Add…**
  and **Configure…** buttons now display a dialog instead of silently writing a
  data source with no `Database` key. It asks for the data source name and the
  database file, offers a file browser, and has a **Test connection** button
  that opens the file and reports the SQLite version and how many tables it
  found — which is the check worth having, because SQLite creates a missing
  file rather than refusing, so a typo in the path connects perfectly well and
  finds nothing.

  This is `stackable-odbc-core`'s new `Backend::configure_dsn` hook: core owns
  all of `ConfigDSN` — validating the request, merging the data source's stored
  keywords in, and writing through `SQLWriteDSNToIni` — and this driver
  supplies only the dialog. The dialog itself is
  `packaging/windows/configure-dsn.ps1`, which also runs standalone for a
  scripted install (`-NoGui -Set @{...}`). `install.bat` installs it beside the
  DLL and refuses to register the driver without it.

  Cancelling the dialog leaves the data source untouched and posts no error.
  A **Remove** never prompts: the Administrator has already confirmed it, and
  removing a data source does not touch the database file it points at.

- **The Windows DLL carries a version resource.** The ODBC Data Source
  Administrator listed the driver as `Not marked` under Version and Company,
  because no Rust `cdylib` emits one. `build.rs` now generates it with
  `windres`, taking every string from `Cargo.toml` through cargo's own
  environment, so it cannot disagree with the package.

- **Every release archive carries an SBOM.** One CycloneDX and one SPDX
  document per artifact, generated from the dependency list `cargo auditable`
  embeds in the binary rather than from `Cargo.toml`, so it describes what was
  linked: dev-dependencies are excluded by construction, and a git dependency's
  purl names the resolved commit rather than a branch that moves.

  Two components cargo cannot see are declared by hand in
  `packaging/sbom-native.json` and verified against the real binaries by
  `packaging/sbom.sh --check-native`, which CI runs on every pull request: the
  bundled SQLite itself, which cargo sees only as the `libsqlite3-sys` wrapper,
  and what each artifact links at load time. The release page also carries
  `sha256sums.txt` and build-provenance attestations.

- `SQLCancel` actually cancels. A statement running on one thread can be
  stopped from another, which is the case the spec singles out: the driver now
  holds `sqlite3_interrupt`'s handle for the connection and calls it, so the
  in-flight query fails with SQLSTATE `HY008` ("operation canceled") instead of
  running to completion. It previously reported "not implemented", which
  `SQLCancel` treats as success — an application that asked to stop a runaway
  query got `SQL_SUCCESS` and then waited for it anyway. Cancelling an idle
  statement is still a no-op and still succeeds, per spec, and a cancelled
  statement can be re-executed.

  `SQL_ATTR_QUERY_TIMEOUT` is unaffected and still substituted with `0`:
  cancellation is a signal from another thread, whereas a timeout would need a
  deadline this driver's synchronous execution path has nothing to arm.

- `SQLTables` answers the `SQL_ALL_CATALOGS`, `SQL_ALL_SCHEMAS` and
  `SQL_ALL_TABLE_TYPES` enumerations, which is how a BI tool's navigator
  browses a data source. `SQL_ALL_TABLE_TYPES` reports `TABLE` and `VIEW`, the
  two values `SQLTables` can put in `TABLE_TYPE`; the other two are empty
  result sets, SQLite having neither catalogs nor schemas. The driver used to
  answer the table-type case itself and now declares the list through the new
  `Backend::table_types` hook, with `stackable-odbc-core` detecting all three
  enumerations and serving them — including the distinction that makes them
  work, since all three sentinels are the same `"%"` and differ only in which
  argument carries it while the others are empty strings.

- `SQL_ATTR_ROWS_FETCHED_PTR`, `SQL_ATTR_ROW_STATUS_PTR` and
  `SQL_ATTR_ROW_BIND_OFFSET_PTR` are honoured instead of accepted and ignored.
  With `SQL_ATTR_ROW_ARRAY_SIZE` pinned at 1 the rowset holds exactly one row,
  so the fetched count is 1 per row and 0 at `SQL_NO_DATA`, the status is
  `SQL_ROW_SUCCESS` (or `SQL_ROW_SUCCESS_WITH_INFO` when the row raised
  `01004`), and the bind offset is added to every bound column and indicator
  address on each fetch. This follows a `stackable-odbc-core` change.

- `SQLGetData` retrieves a long character or binary value in parts, returning
  `SQL_SUCCESS_WITH_INFO` with `01004` and resuming from the read position on
  the next call, rather than restarting from the beginning each time. This
  follows a `stackable-odbc-core` change.

- Diagnostics now carry SQLite's own error code and the failure that caused
  them. `map_sqlite_error` keeps the `rusqlite::Error` it classified rather
  than flattening it into a message, so `SQLGetDiagRec` reports SQLite's
  *extended* result code verbatim through `NativeErrorPtr` and the diagnostic
  message includes the whole causal chain. Every error this driver produced
  previously reached the application as native code `0`. The extended code is
  the one worth having: it separates `SQLITE_CONSTRAINT_NOTNULL` (1299) from
  `SQLITE_CONSTRAINT_FOREIGNKEY` (787), which the primary code and SQLSTATE
  both report identically as a constraint violation.

- `SQLColAttribute` answers `SQL_DESC_BASE_TABLE_NAME` for a column that comes
  from a stored table, read from `sqlite3_table_column_metadata`. The catalog
  and schema stay empty, because this driver reports that it has neither.

- Initial extraction of `stackable-odbc-sqlite` into its own repository, from
  the `stackable-odbc-rs` workspace it was developed in. Provides the ODBC
  driver for SQLite: the `Backend` and `StatementBackend` implementations,
  connection-string parsing, SQLite-to-ODBC type conversion, ODBC
  escape-sequence translation, and the catalog and metadata functions, with the
  C ABI entry points generated by `stackable-odbc-core`'s `forward_ffi!` macro.

### Changed

- `stackable-odbc-core` is taken from git rather than from a sibling checkout,
  so a clean clone and CI can build this driver without one. To work on core at
  the same time, put a `[patch]` in your own `.cargo/config.toml`; see
  [`CONTRIBUTING.md`](CONTRIBUTING.md).

- `SQL_QUOTED_IDENTIFIER_CASE` reports `SQL_IC_MIXED` instead of
  `SQL_IC_SENSITIVE`. In SQLite, double quotes are a *delimiter* — they let a
  keyword or a name with punctuation be used as an identifier — and do not
  switch on case-sensitive matching the way they do in a SQL-92 conformant
  DBMS: a table created as `"MixedCase"` is found by `"mixedcase"`, and the
  catalog stores the name with the case it was written in. The old value told
  an application that `"T"` and `"t"` were different tables. Both halves of the
  new claim — case-insensitive matching and mixed-case storage — are probed
  against the bundled library rather than read off the documentation.

- `SQL_SPECIAL_CHARACTERS` reports `$` instead of the empty string. SQLite's
  tokenizer treats `$` as an identifier character, so `a$b` parses undelimited
  and round-trips through `sqlite_master` unchanged. An application reads this
  info type to decide when it must quote, and the empty string had it quoting a
  name that needs no quoting. The empty string was `stackable-odbc-core`'s
  default rather than a claim this driver ever made; it is now a per-connection
  `Backend` hook, and every candidate character is executed against the bundled
  library, the rejected ones included.

- `SQL_CURSOR_SENSITIVITY` reports `SQL_UNSPECIFIED` instead of
  `SQL_INSENSITIVE`, and `SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2` reports
  `SQL_CA2_READ_ONLY_CONCURRENCY` instead of `0`. Both describe
  `stackable-odbc-core`'s own fetch path rather than SQLite, and both now come
  from core: insensitivity would be a promise that no other cursor's changes
  become visible, which core does not make about rows it has not read yet,
  while `0` for the second denied the one concurrency
  `SQLSetStmtAttr(SQL_ATTR_CONCURRENCY)` actually accepts. This follows a
  `stackable-odbc-core` change.

- `SQLDescribeCol` and `SQLColAttribute` report each result column's real
  nullability instead of claiming every column is nullable. A column declared
  `NOT NULL` is now `SQL_NO_NULLS`, a plain table column `SQL_NULLABLE`, and a
  computed column — an expression, a literal, an aggregate —
  `SQL_NULLABLE_UNKNOWN`. The third is the point: `sqlite3_table_column_metadata`
  reports nothing for a computed column, so the driver genuinely cannot
  determine the answer, and the spec has a value for exactly that rather than
  requiring a guess. Guessing is not harmless in either direction:
  `SQL_NO_NULLS` tells an application it may skip a NULL check it needs, and
  `SQL_NULLABLE` makes it write one it does not. Requires `rusqlite`'s
  `column_metadata` feature.

- `SQLGetFunctions` reports every function the driver actually exports, derived
  from `stackable-odbc-core`'s `CORE_EXPORTED_FUNCTIONS`, rather than a
  hand-written list. The list had drifted to 53 of the 69 exported entry
  points, so sixteen the driver does export were reported as unsupported —
  including `SQLAllocConnect`, `SQLTransact`, `SQLExtendedFetch` and the
  descriptor-field functions. It over-claimed nothing, which is the direction
  that matters: `SQLGetFunctions` is what the Windows Driver Manager builds its
  dispatch table from, so naming a function core does not export would hand it
  a null pointer. A test keeps the historical list checked against core's.

- `SQLSetStmtAttr(SQL_ATTR_QUERY_TIMEOUT)` and
  `SQLSetStmtAttr(SQL_ATTR_MAX_ROWS)` now substitute `0` and return
  `SQL_SUCCESS_WITH_INFO` with SQLSTATE `01S02` for any other value, where both
  were previously stored and echoed back by `SQLGetStmtAttr`. Both are on the
  spec's `01S02` substitution list. Nothing in this driver counts rows or
  enforces a deadline — `Backend` is synchronous and `SQLCancel` is not
  implemented — so an application that set a 30-second timeout and got
  `SQL_SUCCESS` would wait indefinitely on a runaway query. Setting either to
  `0` still succeeds plainly, that being the value the driver honours. This
  follows a `stackable-odbc-core` change.

- An infinite `REAL` read as `SQL_C_CHAR` or `SQL_C_WCHAR` now renders as
  `Infinity` / `-Infinity` rather than `inf` / `-inf`. Both spellings parse
  back into a float, and `Infinity` is what Trino, its JDBC driver and
  PostgreSQL emit; the ODBC spec defines no textual form for a non-finite
  float. `NaN` is unchanged. This follows a `stackable-odbc-core` change to its
  shared coercion path.

- `SQLSetStmtAttr(SQL_ATTR_CURSOR_TYPE)` with an unsupported cursor type now
  substitutes `SQL_CURSOR_FORWARD_ONLY` and returns `SQL_SUCCESS_WITH_INFO`
  with SQLSTATE `01S02` ("option value changed"), where it previously failed
  with `HYC00`. The substituted value is readable back through
  `SQLGetStmtAttr`, which is how an application learns what it was given. This
  follows a `stackable-odbc-core` change; the driver's behaviour is unchanged
  beyond what it reports.

- `SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION)` refuses any level other than
  `SQL_TXN_SERIALIZABLE` with SQLSTATE `HY024`, where it previously stored
  whatever it was given and echoed it back. Serializable is the only level
  SQLite runs at and the only one `SQL_TXN_ISOLATION_OPTION` advertises: READ
  COMMITTED and REPEATABLE READ are not SQLite concepts, and READ UNCOMMITTED
  needs shared-cache mode, which `connect` does not open. An application that
  asked for another level previously got `SQL_SUCCESS` and serializable
  behaviour regardless, with no way to learn its request had not been honoured.

- `SQL_IDENTIFIER_CASE` is now declared through the backend's
  `identifier_case` hook rather than answered directly. The value is unchanged
  (`SQL_IC_MIXED`): SQLite stores an unquoted identifier as written and matches
  it case-insensitively. Answering it in one place removes the possibility of
  the hook and the direct answer disagreeing.

- `SQL_GETDATA_EXTENSIONS` is no longer answered by this driver. The value is
  unchanged, and is now `stackable-odbc-core`'s to state: it describes what
  core's own fetch path supports, not anything about SQLite, and this driver
  could not keep it correct if that path changed.

- `SQL_CURSOR_COMMIT_BEHAVIOR` now reports `SQL_CB_PRESERVE` instead of
  `SQL_CB_DELETE`, and `SQL_CURSOR_ROLLBACK_BEHAVIOR` is now declared rather
  than left to a fallback. Both report `SQL_CB_PRESERVE`. The driver
  materialises every result set eagerly, so no SQLite statement is live when
  `SQLEndTran` runs and neither commit nor rollback can disturb an open cursor.
  The previous `SQL_CB_DELETE` was never accurate: `stackable-odbc-core`
  advertised it and implemented nothing, so the driver reported that it
  destroyed cursors on commit while in fact preserving them.

- Connections now enable foreign key enforcement: `connect` issues
  `PRAGMA foreign_keys = ON`. SQLite leaves it off for backward compatibility,
  and the bundled library only happened to be compiled with
  `SQLITE_DEFAULT_FOREIGN_KEYS` — a property of one dependency's build rather
  than of SQLite. On the current build this changes nothing; it stops
  referential integrity from turning itself off if that dependency ever
  changes.

- `SQL_INTEGRITY` now reports `"Y"` instead of `"N"`. SQLite implements the
  whole Integrity Enhancement Facility — `PRIMARY KEY`, `UNIQUE`, `NOT NULL`,
  `CHECK`, `DEFAULT`, and `FOREIGN KEY` with referential actions — and, with
  the pragma above, the driver enforces all of it. `"N"` was
  `stackable-odbc-core`'s default, and the earlier justification for keeping
  it — that SQLite leaves foreign keys off unless asked — no longer applies now
  that the driver asks. A test exercises each constraint and an
  `ON DELETE CASCADE` through `connect`.

- `SQL_SQL_CONFORMANCE` now reports `0` — no SQL-92 level claimed — instead of
  `SQL_SC_SQL92_ENTRY`. That value came from a `stackable-odbc-core` default
  rather than any assessment of SQLite, and it contradicted this driver's own
  answers: the spec ties entry level to `SQL_GB_GROUP_BY_EQUALS_SELECT`, while
  SQLite accepts a bare non-aggregated column absent from `GROUP BY` and a
  `GROUP BY` column absent from the select list. Raising the claim later means
  auditing entry-level conformance properly.

- `SQL_ALTER_TABLE` additionally reports `SQL_AT_ADD_CONSTRAINT`. Despite its
  name that bit means "`ADD COLUMN` is supported with column constraints", and
  SQLite accepts `NOT NULL`, `CHECK`, `REFERENCES` and a named `CONSTRAINT` on
  an added column; only `UNIQUE` and `PRIMARY KEY` are refused. The bit was
  unavailable when this driver first set the bitmap.

- `SQL_SUBQUERIES`, `SQL_COLUMN_ALIAS`, `SQL_CONCAT_NULL_BEHAVIOR`,
  `SQL_UNION`, `SQL_CONVERT_FUNCTIONS`, `SQL_ORDER_BY_COLUMNS_IN_SELECT`,
  `SQL_ACCESSIBLE_TABLES`, `SQL_DATA_SOURCE_READ_ONLY` and
  `SQL_SEARCH_PATTERN_ESCAPE` are now stated by this driver rather than
  inherited from `stackable-odbc-core`, which had no way to know most of them.
  Every value was verified against the bundled library; only `SQL_SUBQUERIES`
  changed (see `Fixed`).

- `SQL_GROUP_BY`, `SQL_NULL_COLLATION`, `SQL_CORRELATION_NAME`,
  `SQL_NON_NULLABLE_COLUMNS`, `SQL_EXPRESSIONS_IN_ORDERBY`,
  `SQL_TIMEDATE_ADD_INTERVALS` and `SQL_TIMEDATE_DIFF_INTERVALS` are now stated
  by this driver rather than inherited. `SQL_CORRELATION_NAME`
  (`SQL_CN_ANY`), `SQL_NON_NULLABLE_COLUMNS` (`SQL_NNC_NON_NULL`) and
  `SQL_EXPRESSIONS_IN_ORDERBY` (`"Y"`) had never been asserted anywhere; the
  two interval bitmaps report `0`, matching `SQL_TIMEDATE_FUNCTIONS`, which
  does not claim `TIMESTAMPADD` or `TIMESTAMPDIFF`.

- `SQL_TXN_ISOLATION_OPTION` now reports `SQL_TXN_SERIALIZABLE` alone, instead
  of also advertising `SQL_TXN_READ_UNCOMMITTED`, `SQL_TXN_READ_COMMITTED` and
  `SQL_TXN_REPEATABLE_READ`. Transactions in SQLite are serializable; READ
  COMMITTED and REPEATABLE READ are not SQLite concepts, and READ UNCOMMITTED
  additionally requires shared-cache mode, which this driver never enables.
  Nothing applied the level an application set in any case —
  `SQL_ATTR_TXN_ISOLATION` is stored on the connection and read back unchanged
  — so the three extra levels promised behaviour no code path delivered.

- `SQL_ALTER_TABLE` now reports `SQL_AT_ADD_COLUMN_SINGLE`,
  `SQL_AT_ADD_COLUMN_DEFAULT`, `SQL_AT_ADD_COLUMN_COLLATION`,
  `SQL_AT_ADD_TABLE_CONSTRAINT` and `SQL_AT_CONSTRAINT_NAME_DEFINITION` instead
  of `0`, which claimed SQLite cannot alter a table in any way. Each bit is
  verified by executing the clause against the bundled library, and the bits
  that stay off are verified to be rejected by it. SQLite's unqualified
  `DROP COLUMN` and `DROP CONSTRAINT`, and both `RENAME` forms, remain absent
  from the bitmap: the ODBC value has no bit for them, and its `CASCADE` and
  `RESTRICT` variants are syntax errors in SQLite.

- `SQL_OUTER_JOIN_CAPABILITIES` now reports every outer-join form SQLite
  implements — `SQL_OJ_LEFT`, `SQL_OJ_RIGHT`, `SQL_OJ_FULL`, `SQL_OJ_NESTED`,
  `SQL_OJ_NOT_ORDERED`, `SQL_OJ_INNER` and `SQL_OJ_ALL_COMPARISON_OPS` — instead
  of `0`. It previously inherited `stackable-odbc-core`'s default of `0`, which
  said SQLite supports no outer joins at all while this driver's own
  `SQL_OUTER_JOINS` reported `"Y"`. Each bit is verified by executing the join
  it describes against the bundled library, not assumed from release notes.

### Fixed

- `SQLRowCount` reported `0` after a `CREATE TABLE`, `DROP TABLE`, `ALTER
  TABLE`, `BEGIN`, `COMMIT`, `PRAGMA` or `VACUUM`, where the spec's
  affected-row count does not apply at all. The three answers are now distinct:
  a count for a searched INSERT / UPDATE / DELETE, the materialised size of a
  result set, and *no count* for everything else. This matters beyond
  tidiness — `stackable-odbc-core` reads a zero-column statement reporting a
  counted zero as `SQL_NO_DATA`, per `SQLExecDirect`'s Comments, so every DDL
  statement this driver ran returned `SQL_NO_DATA` to the application instead
  of `SQL_SUCCESS`. A searched DELETE that matches nothing still reports `0`,
  which is the case the spec reserves `SQL_NO_DATA` for.

  The same fix removes a stale count: `sqlite3_changes()` reports the rows
  touched by the *most recently completed* INSERT, UPDATE or DELETE, so a
  `CREATE TABLE` run straight after a three-row `INSERT` was handed that `3`
  and reported it.

- `SQLForeignKeys` reported `PKCOLUMN_NAME` as NULL for a foreign key declared
  without an explicit column list (`REFERENCES parent`), a column the spec
  marks "not NULL". SQLite defines the implicit target as the parent table's
  primary key, so the name is now resolved from it — per position, for a
  composite key — rather than dropped. `PRAGMA foreign_key_list` leaves its
  `to` column NULL in that case, which is what the old code passed straight
  through.

- `SQLStatistics` with a null `TableName` returned `SQL_SUCCESS` and an empty
  result set, which an application reads as "that table has no indexes". It is
  now `HY009`. `SQLStatistics` is one of only two catalog functions whose
  null-`TableName` clause carries no **(DM)** marker, so the driver owns it
  rather than the Driver Manager. An empty-string `TableName` is still a legal
  argument naming no table, and still returns no rows.

- Every catalog result set is now sorted into the order its spec page
  mandates, by `stackable-odbc-core`, which holds the rows. `SQLTables`,
  `SQLColumns`, `SQLPrimaryKeys`, `SQLForeignKeys` and `SQLSpecialColumns` were
  previously returned in whatever order the underlying `sqlite_master` or
  `PRAGMA` query produced, which matched the spec only by accident;
  `SQLStatistics` sorted itself. Integer key columns (`KEY_SEQ`,
  `ORDINAL_POSITION`) compare numerically, so a table with more than nine
  columns no longer sorts column 10 before column 2.

- `SQLDescribeCol` reported `2^64 - 4` as the column size of an unbounded
  column instead of `0`, `SQLGetInfoW` wrote four bytes into the two-byte
  buffer an application supplies for four `SQLUSMALLINT` info types, and a
  parameter bound `SQL_PARAM_OUTPUT` had its buffer read as an input value.
  `SQLAllocHandle`, `SQLFreeHandle` and `SQLFreeStmt` now post a diagnostic on
  failure rather than returning a bare `SQL_ERROR` with nothing for
  `SQLGetDiagRec` to report. All follow `stackable-odbc-core` fixes.

- `SQL_MAX_COLUMNS_IN_SELECT`, `_IN_TABLE`, `_IN_GROUP_BY`, `_IN_ORDER_BY`,
  `_IN_INDEX`, `SQL_MAX_STATEMENT_LEN` and `SQL_MAX_ROW_SIZE` now report the
  connection's actual limits instead of `0`. The spec allows `0` for "no
  specified limit or the limit is unknown", and `stackable-odbc-core` answers
  that because it cannot know — but SQLite enforces real limits, and a tool
  deciding whether to chunk a wide `SELECT` or a long `IN` list reads exactly
  these. They are read per connection through `sqlite3_limit` rather than
  hardcoded, because `sqlite3_limit` also *sets* them, so any constant would be
  wrong for a connection that changed one. `SQL_MAX_TABLES_IN_SELECT` stays `0`:
  SQLite's 64-table join cap has no `sqlite3_limit` to read it from, and
  transcribing the constant is what has gone stale twice in this crate.

- `SQL_MAX_CATALOG_NAME_LEN` and `SQL_MAX_SCHEMA_NAME_LEN` now report `0`
  instead of the generic identifier length. This driver supports neither
  catalogs nor schemas, so there is no name for these to bound; they were
  stating a maximum length for something the same driver says does not exist.
  Both answers are derived from `supports_catalogs` / `supports_schemas` rather
  than pinned to `0`, so they stay right if either hook flips — and because
  those hooks are per-connection, the `0` applies once a connection is open.
  Asked before `SQLDriverConnectW`, both fall through to
  `stackable-odbc-core`'s generic identifier length, the same answer it gives
  pre-connect for every other `SQL_MAX_*_NAME_LEN`.

- `SQL_SUBQUERIES` no longer claims `SQL_SQ_QUANTIFIED`. `< ALL`, `< ANY` and
  `< SOME` are all syntax errors in SQLite, which this driver already recorded
  by excluding `SQL_SP_QUANTIFIED_COMPARISON` from `SQL_SQL92_PREDICATES` — so
  the same capability was denied by one info type and advertised by another,
  the advertised half coming from a `stackable-odbc-core` default. A tool
  reading `SQL_SUBQUERIES` would have pushed down a predicate SQLite rejects.

- `SQL_KEYWORDS` now lists SQLite's own keywords instead of an empty string.
  The list is read out of the linked library through `sqlite3_keyword_count` /
  `sqlite3_keyword_name` rather than transcribed from SQLite's documentation,
  so it describes the library the driver links. An empty list claimed SQLite
  has no keywords of its own — it has `AUTOINCREMENT`, `PRAGMA`, `VACUUM`,
  `GLOB`, `REGEXP` and many more, and applications read this to decide which
  identifiers need quoting. The driver reports the raw list through
  `Backend::keywords`; `stackable-odbc-core` subtracts the ODBC reserved words
  the specification defines this value as excluding.

- `{fn CURRENT_DATE()}`, `{fn CURRENT_TIME()}` and `{fn CURRENT_TIMESTAMP()}`
  now execute. `SQL_TIMEDATE_FUNCTIONS` advertised all three, but nothing
  translated them: SQLite spells them as bare keywords, `SELECT CURRENT_DATE();`
  is a syntax error, and a name-only remap cannot drop the trailing `()` the
  ODBC escape always carries — so each reached SQLite as `CURRENT_DATE()` and
  failed to prepare. The driver was advertising three functions an application
  could not use. `stackable-odbc-core`'s new
  `EscapeDialect::rewrite_scalar_fn` replaces the whole escape, which is what
  emitting a bare keyword requires.

- `SQL_CATALOG_TERM`, `SQL_CATALOG_NAME_SEPARATOR` and `SQL_SCHEMA_TERM` now
  report empty strings instead of `"catalog"`, `"."` and `"schema"`. The
  `SQLGetInfo` specification requires an empty string from all three when the
  data source supports neither catalogs nor schemas, which this driver has
  always declared through `SQL_CATALOG_NAME`, `SQL_CATALOG_LOCATION`,
  `SQL_CATALOG_USAGE` and `SQL_SCHEMA_USAGE`. Applications were told catalogs do
  not exist and given their name in the same breath. All seven values now derive
  from a single `SUPPORTS_CATALOGS` / `SUPPORTS_SCHEMAS` pair, and a test asserts
  they agree.

- `SQLCloseCursor` after a statement that produced no result set now returns
  `24000` rather than succeeding. An `INSERT` opens no cursor, so there is
  nothing to close; the call was accepted because cursor state was inferred
  from whether a backend statement existed.

[Unreleased]: https://github.com/stackabletech/stackable-odbc-sqlite/commits/HEAD
