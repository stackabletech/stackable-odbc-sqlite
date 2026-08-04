<!-- markdownlint-disable MD041 MD033 -->

<p align="center">
  <img width="150" src="./.readme/static/borrowed/Icon_Stackable.svg" alt="Stackable Logo"/>
</p>

<h1 align="center">Stackable ODBC Driver for SQLite</h1>

<p align="center"><em>Open a SQLite file from Excel, DBeaver, LibreOffice or Python, with no server to run.</em></p>

[![Build and Test](https://github.com/stackabletech/stackable-odbc-sqlite/actions/workflows/build.yaml/badge.svg)](https://github.com/stackabletech/stackable-odbc-sqlite/actions/workflows/build.yaml)
[![Security Audit](https://github.com/stackabletech/stackable-odbc-sqlite/actions/workflows/security_audit.yaml/badge.svg)](https://github.com/stackabletech/stackable-odbc-sqlite/actions/workflows/security_audit.yaml)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/stackabletech/stackable-odbc-sqlite/badge)](https://scorecard.dev/viewer/?uri=github.com/stackabletech/stackable-odbc-sqlite)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-green.svg)](https://docs.stackable.tech/home/stable/contributor/index.html)
[![Apache License 2.0](https://img.shields.io/badge/license-Apache--2.0-green)](./LICENSE)
[![ODBC 3.80](https://img.shields.io/badge/ODBC-3.80-blue)](#compatibility)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20Windows-blue)](#compatibility)
[![SQLite bundled](https://img.shields.io/badge/SQLite-3.53.2%20bundled-blue)](https://sqlite.org)

[Stackable Data Platform](https://stackable.tech/) | [Platform Docs](https://docs.stackable.tech/) | [Discussions](https://github.com/orgs/stackabletech/discussions) | [Discord](https://discord.gg/7kZ3BNnCAF)

## What is this?

[SQLite](https://sqlite.org) is a database that lives in a single file. There
is nothing to install and nothing to start, because the whole database is one
`.db` file you can copy onto a USB stick. Your phone is running several of them
right now.

Most desktop tools cannot open one of those files directly, but nearly all of
them speak **ODBC**, a standard that lets any tool load a small library, called
a driver, and talk to a database through it.

This is the ODBC driver for SQLite. Install it, and Excel, LibreOffice Base,
DBeaver, `isql` and Python's `pyodbc` can query a SQLite file as if it were a
full database server. Linux and Windows are both first-class targets.

SQLite itself is compiled into the driver, so there is nothing else to install
and no second copy on the machine that could disagree with it.

## Quick start

Download an archive from the
[releases page](https://github.com/stackabletech/stackable-odbc-sqlite/releases).

### Windows

1. Unzip `stackable-odbc-sqlite-<version>-windows-x64.zip`.
2. Right-click `install.bat` and choose **Run as administrator**. This registers
   the driver with Windows.
3. Open **ODBC Data Sources (64-bit)** from the Start menu, click **Add**, and
   pick `stackable_odbc_sqlite` from the list. Name the data source, browse to
   your `.db` file, and click **Test connection** before saving.

Step 3 creates a *DSN*: a saved connection with a name. Once it exists, every
tool on the machine can pick it from a list instead of asking you to type a
connection string.

### Linux

You need unixODBC (the `unixodbc` package). Installing the driver registers it
system-wide, so it needs root.

```bash
mkdir /tmp/sqlite-odbc
tar xzf stackable-odbc-sqlite-<version>-linux-x64.tar.gz -C /tmp/sqlite-odbc
cd /tmp/sqlite-odbc
sudo ./install.sh
```

Check it worked with `odbcinst -q -d`, which should list
`[stackable_odbc_sqlite]`.

### Your first query

```python
import pyodbc

conn = pyodbc.connect("Driver=stackable_odbc_sqlite;Database=/path/to/your.db")
for row in conn.cursor().execute("SELECT name FROM sqlite_master WHERE type = 'table'"):
    print(row.name)
```

For the full install and uninstall reference, see
[`packaging/README.md`](packaging/README.md).

## Connecting

Connection strings are `Key=Value` pairs joined by `;`. Keys are
case-insensitive. There is exactly one key.

| Key | Required | Meaning |
|-----|----------|---------|
| `Database` | Yes | Path to the SQLite file, or `:memory:` for a throwaway in-memory database |

```text
Driver=stackable_odbc_sqlite;Database=/path/to/your.db
```

Instead of typing that every time you can save it as a DSN, which is a named,
stored connection much like a browser bookmark. On Linux, add a section to
`~/.odbc.ini`:

```ini
[SQLite Test]
Driver = stackable_odbc_sqlite
Database = /path/to/your.db
```

On Windows the **Add** button in the ODBC Data Source Administrator writes one
for you. See [`packaging/README.md`](packaging/README.md) for that and for the
scripted alternatives.

## What you get

- **The stop button stops the query.** Cancelling from your tool calls SQLite's
  `sqlite3_interrupt` on the connection, so a runaway query really stops rather
  than running to the end while your tool reports it as cancelled. The
  statement can be run again afterwards. Query timeouts work the same way, so
  "give up after 30 seconds" is a promise the driver keeps.

- **Real transactions.** Turn autocommit off and the driver opens a transaction
  for you, then commits or rolls back when you say so and immediately opens the
  next one. Your open result sets survive both, because the driver has already
  read every row into memory by the time you commit.

- **Foreign keys are switched on.** SQLite ships with foreign-key enforcement
  *off* for backwards compatibility, which surprises almost everyone. This
  driver turns it on for every connection, so a `REFERENCES` clause in your
  schema is a rule the database keeps.

- **Your tool can browse the database.** Tables, views, columns, primary keys,
  foreign keys, indexes and row identifiers all show up in the object browser,
  read from SQLite's own `PRAGMA` introspection, so you can click through what
  is there instead of guessing table names.

- **Columns get sensible types even though SQLite has almost none.** SQLite is
  dynamically typed. Any value can go in any column, and there is no `DATE` or
  `BOOLEAN` type at all. The driver reads each column's declared type together
  with the storage class of its values and maps the pair onto a proper ODBC
  type. That covers the three ways people store a timestamp in SQLite: ISO
  text, Unix seconds and Julian day numbers.

- **Your tool gets accurate answers about what SQLite supports.** Applications
  choose which SQL to send based on what the driver reports about itself, so
  those answers are measured against the bundled library rather than copied
  from documentation. The `ALTER TABLE` clauses are checked by executing each
  one, and the reserved-word list is read out of the library at runtime.

- **Windows gets its own installer and setup dialog**, so the ODBC
  administrator's **Add** button behaves the way it does for a commercial
  driver. The DLL is cross-compiled, export-checked and unit-tested on every
  pull request, and the integration suite can also be run through the Windows
  Driver Manager in a VM.

- **Every release says what is inside it.** Both archives carry a CycloneDX
  SBOM generated from the binary's own embedded dependency list rather than
  from `Cargo.toml`, so it describes what was actually linked, including the
  bundled SQLite. The release page also carries SPDX documents, checksums and
  build provenance attestations.

## Limits

Each of these is reported to your tool as unsupported rather than quietly
faked, so the tool can react instead of trusting a wrong answer.

- **No catalogs and no schemas.** SQLite has neither, so the driver says so
  rather than inventing a one-level hierarchy for the sake of looking familiar.
- **No stored procedures.** SQLite has none, so those lookups return nothing.
- **Rows arrive one at a time.** There are no block cursors and no parameter
  arrays.
- **Result sets are read into memory in one go.** That is what lets cursors
  survive a commit or rollback, but a `SELECT` over a table larger than your
  RAM will not work.
- **One isolation level.** SQLite gives you serializable transactions, so that
  is the only level offered, and asking for a weaker one is refused up front.
- **No setup dialog on Linux.** unixODBC has no convention for a driver to put
  a window on the screen, so on Linux a DSN is defined by a section in `odbc.ini`.

## Compatibility

| Component | Support |
|---|---|
| ODBC | 3.80 |
| Platforms | Linux x86-64, Windows x86-64 |
| Driver Managers | unixODBC, and the Windows Driver Manager |
| SQLite | 3.53.2, compiled into the driver |
| Tested with | `pyodbc`, `isql` |

## Troubleshooting

**Turn on logging first.** The driver logs to a file when you ask it to, and
that is usually enough to see what a tool is really sending:

```bash
export ODBC_LOG_LEVEL=debug          # trace, debug, info, warn, error
export ODBC_LOG_FILE=/tmp/sqlite-odbc.log
```

On Windows, set the same two as environment variables. The log may contain your
SQL, so check it before sharing.

**You connected fine but the database is empty.** SQLite creates a file that
does not exist yet rather than refusing, so a typo in the path connects
successfully and finds nothing. Check the path. The Windows dialog's **Test
connection** button reports the table count for exactly this reason.

**The driver does not appear in the list.** On Linux, run `odbcinst -q -d`; if
`[stackable_odbc_sqlite]` is missing, the install did not complete. On Windows,
make sure you opened **ODBC Data Sources (64-bit)**: a 64-bit driver is
invisible to the 32-bit Administrator, and both are in the Start menu under
similar names.

**A `REFERENCES` clause is being enforced that was not before.** That is
deliberate. The driver turns foreign-key enforcement on for every connection,
which most other SQLite tooling leaves off.

## Getting help

- [GitHub Discussions](https://github.com/orgs/stackabletech/discussions) for
  questions
- [Discord](https://discord.gg/7kZ3BNnCAF) to talk to us
- [Issues](https://github.com/stackabletech/stackable-odbc-sqlite/issues) for
  bugs, and [SECURITY.md](SECURITY.md) for anything security-related

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for building from source, running the
tests, and how the repository is laid out. [AGENTS.md](AGENTS.md) has the
architecture and the ODBC design rationale behind what the driver reports.
[CHANGELOG.md](CHANGELOG.md) records what changed in each release.

## License

Apache-2.0
