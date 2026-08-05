# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-08-05

First release, so this section describes what the driver offers rather than
what changed.

### Added

**Querying.** An ODBC 3.80 driver for [SQLite](https://sqlite.org) on Linux and
Windows. Queries, result sets fetched a row at a time, bound parameters, and
the ODBC escape sequences `{fn ...}`, `{d ...}` and `{oj ...}` translated into
SQLite SQL. SQLite is compiled into the driver, so there is no separate library
to install and no second copy on the machine that could disagree with it.

**Types.** SQLite is dynamically typed and has no date, time or boolean type at
all. The driver reads each column's declared type alongside the storage class
of the values in it, and maps the pair onto a proper ODBC type. That covers the
three ways SQLite users store a timestamp: ISO 8601 text, Unix epoch seconds,
and Julian day numbers.

**Metadata.** Tables, views, columns, primary keys, foreign keys, indexes and
row identifiers, read from SQLite's own `PRAGMA` introspection and
`sqlite_master`. A tool can browse the database instead of asking you to type
table names.

**Transactions.** Turn autocommit off and the driver opens a transaction, then
commits or rolls back on request and opens the next one. Open result sets
survive both, because every row has already been read into memory by the time
you commit.

**Foreign keys.** SQLite enforces foreign keys only when asked, which surprises
most people who assume a `REFERENCES` clause is a rule the database keeps. The
driver issues `PRAGMA foreign_keys = ON` for every connection.

**Cancellation and timeouts.** `SQLCancel` from another thread calls
`sqlite3_interrupt` on the connection, so a runaway query stops instead of
running to completion while the application believes it was cancelled. The
statement reports `HY008` and can be run again. `SQL_ATTR_QUERY_TIMEOUT` is
enforced the same way, reporting `HYT00` when the deadline passes, and it
covers execution, which is where a SQLite query spends its time.

**Reported capabilities.** What a driver says about itself is how applications
decide which SQL to send, so the values here are measured rather than
transcribed. The `ALTER TABLE` clauses are established by executing each one
against the linked library, and the reserved-word list is read out of it at
runtime through `sqlite3_keyword_name`.

**Packaging.** Installers for Linux and Windows, and a Windows dialog for
creating a data source, reachable from the ODBC Data Source Administrator's
**Add…** button. Every release artifact ships with a CycloneDX SBOM, is
published alongside an SPDX document, and is covered by `sha256sums.txt`. The
SBOM is generated from the binary's own embedded dependency list, so it
describes what was linked, including the bundled SQLite.

### Known limitations

- SQLite has no catalogs and no schemas, so the driver reports none rather than
  inventing a one-level hierarchy.
- SQLite has no stored procedures, so those lookups return no rows.
- `SQL_C_NUMERIC` cannot be used to *retrieve* a value. A `DECIMAL` column is
  described as `SQL_DECIMAL` and reads correctly as `SQL_C_CHAR` or
  `SQL_C_DOUBLE`, but `SQLGetData` and a column bound to `SQL_C_NUMERIC` both
  report `07006`. It works as a parameter type, so the restriction is on the
  retrieval side only.
- Rows are fetched one at a time. `SQL_ATTR_ROW_ARRAY_SIZE` and
  `SQL_ATTR_PARAMSET_SIZE` are both pinned at 1, so there are no block cursors
  and no parameter arrays.
- Result sets are read into memory in full, which is what lets cursors survive
  a commit or rollback. A `SELECT` larger than available memory will not work.
- Only the serializable isolation level is offered, because it is the only one
  SQLite provides. Asking for a weaker one is refused rather than silently
  ignored.
- Linux has no setup dialog. unixODBC has no convention for a driver to display
  one, so a data source there is a section in `odbc.ini`.

[Unreleased]: https://github.com/stackabletech/stackable-odbc-sqlite/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/stackabletech/stackable-odbc-sqlite/releases/tag/v0.1.0
