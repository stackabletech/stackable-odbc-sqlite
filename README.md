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
[![ODBC 3.80](https://img.shields.io/badge/ODBC-3.80-blue)](#what-it-deliberately-does-not-do)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20Windows-blue)](#quick-start)
[![SQLite bundled](https://img.shields.io/badge/SQLite-3.53.2%20bundled-blue)](https://sqlite.org)

[Stackable Data Platform](https://stackable.tech/) | [Platform Docs](https://docs.stackable.tech/) | [Discussions](https://github.com/orgs/stackabletech/discussions) | [Discord](https://discord.gg/7kZ3BNnCAF)

## What is this?

[SQLite](https://sqlite.org) is a database that lives in a single file. There
is nothing to install and nothing to start: the whole database is one `.db`
file you can copy onto a USB stick. Your phone is running several of them right
now.

Most desktop tools cannot open one of those files directly, but nearly all of
them speak **ODBC**. ODBC is a widely supported standard: a tool loads a small library called a *driver*, calls a fixed set of functions on it, and the driver translates those calls into whatever the actual database understands. Write one driver, and every ODBC-speaking tool on the machine can talk to that database.

This repository is that driver for SQLite. Install it, and Excel, LibreOffice
Base, DBeaver, `isql` and Python's `pyodbc` can query a SQLite file as if it
were a full database server. Linux and Windows are both supported.

Two things make it unusual:

- **It carries its own SQLite.** Version 3.53.2 is compiled straight into the
  driver, so there is no separate SQLite to install and no version of it on the
  machine that could disagree with the one the driver actually uses.
- **It is a testbed.** Everything generic about being an ODBC driver lives in
  [`stackable-odbc-core`](https://github.com/stackabletech/stackable-odbc-core),
  which also powers the
  [Trino driver](https://github.com/stackabletech/stackable-odbc-trino). SQLite
  is small, fast and needs no server, which makes it the ideal backend for
  proving that shared framework behaves.

## Quick start

No release has been cut yet, so build the driver yourself. You need Rust (the
version in `rust-toolchain.toml` is installed automatically by `rustup`) and
the unixODBC development headers, because the ODBC bindings link against them:

```bash
sudo apt-get install unixodbc-dev   # Debian/Ubuntu
sudo pacman -S unixodbc             # Arch
```

Clone this repository:

```bash
git clone https://github.com/stackabletech/stackable-odbc-sqlite
cd stackable-odbc-sqlite
cargo build --release
```

Output: `target/release/libstackable_odbc_sqlite.so`.

For Windows, cross-compile with MinGW (`gcc-mingw-w64-x86-64`):

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

Output: `target/x86_64-pc-windows-gnu/release/stackable_odbc_sqlite.dll`.

### Installing it

`packaging/build-archives.sh` turns those binaries into the same release
archives CI publishes, each with an installer inside:

```bash
VERSION=0.0.1 ./packaging/build-archives.sh
```

On Linux, unpack `stackable-odbc-sqlite-<version>-linux-x64.tar.gz` and run
`sudo ./install.sh`. It copies the library into place and registers it with
unixODBC; check it worked with `odbcinst -q -d`, which should list
`[stackable_odbc_sqlite]`.

On Windows, unpack the `.zip` and run `install.bat` from an Administrator
Command Prompt, then look for `stackable_odbc_sqlite` on the Drivers tab of
**ODBC Data Sources (64-bit)**. From there, **Add…** opens the driver's own
dialog: name the data source, browse to a `.db` file, and press **Test
connection** to check it before saving.

The full install, uninstall and DSN reference is in
[`packaging/README.md`](packaging/README.md).

### Then use it

```python
import pyodbc

conn = pyodbc.connect("Driver=stackable_odbc_sqlite;Database=/path/to/your.db")
for row in conn.cursor().execute("SELECT name FROM sqlite_master WHERE type = 'table'"):
    print(row.name)
```

Or straight from a source checkout, without installing anything at all:

```bash
isql -3 -k "Driver=$(pwd)/target/release/libstackable_odbc_sqlite.so;Database=$(pwd)/test/test.db" -v
```

## Highlights

- **The stop button actually stops the query.** Cancelling from your tool calls
  SQLite's `sqlite3_interrupt` on the connection, so a runaway query really
  stops instead of quietly running to the end while your tool pretends it was
  cancelled. The statement reports "operation canceled" and can be re-run.

- **Real transactions.** Turn autocommit off and the driver opens a transaction
  for you, then commits or rolls back when you say so and immediately opens the
  next one. Your open result sets survive both, because the driver has already
  read every row into memory by the time you commit.

- **Foreign keys are switched on.** SQLite ships with foreign-key enforcement
  *off* for backwards compatibility, which surprises almost everyone. This
  driver turns it on for every connection, so a `REFERENCES` clause in your
  schema is a rule the database enforces rather than a comment.

- **Your tool can browse the database.** Tables, views, columns, primary keys,
  foreign keys, indexes and row identifiers all show up in the object browser,
  read out of SQLite's own `PRAGMA` introspection. So you can click through what
  is there instead of guessing table names.

- **Columns get sensible types even though SQLite has almost none.** SQLite is
  dynamically typed: any value can go in any column, and there is no `DATE` or
  `BOOLEAN` type at all. The driver reads each column's declared type and its
  actual storage class and maps them onto proper ODBC types, including the
  three different ways SQLite people store a timestamp (ISO text, Unix seconds,
  Julian day numbers).

- **Nothing is claimed that was not measured.** What a driver reports about
  itself is how tools decide which SQL to send, so guessing wrong there breaks
  things in confusing ways. The tests here run the actual SQL to check: the list
  of `ALTER TABLE` clauses is verified by executing each one, and the list of
  reserved words is read out of the linked SQLite library at runtime instead of
  being copied from documentation that can drift.

- **Windows is a real target, not an afterthought.** It gets its own installer
  and its own setup dialog, so the ODBC administrator's **Add…** button works
  the way it does for a commercial driver. The DLL is cross-compiled,
  export-checked and unit-tested on every pull request, and the integration
  suite can be run through the Windows Driver Manager in a VM, which is far
  stricter than unixODBC and tends to fail silently rather than loudly.

- **Every release says what is inside it.** Both archives carry a CycloneDX
  SBOM generated from the binary's own embedded dependency list rather than
  from `Cargo.toml`, so it describes what was linked. That includes the
  bundled SQLite and the Driver Manager the library loads, neither of which
  cargo can see. The release page also carries SPDX, checksums and build
  provenance attestations.

## Connecting

Connection strings are `Key=Value` pairs joined by `;`. Keys are
case-insensitive. There is exactly one key:

| Key | Required | Meaning |
|-----|----------|---------|
| `Database` | Yes | Path to the SQLite file, or `:memory:` for a throwaway in-memory database |

```text
Driver=stackable_odbc_sqlite;Database=/path/to/your.db
```

Instead of typing that every time you can save it as a **DSN**, which is just a
named, stored connection, like a browser bookmark. On Linux, add a section to
`~/.odbc.ini`:

```ini
[SQLite Test]
Driver = stackable_odbc_sqlite
Database = /path/to/your.db
```

On Windows, the **Add…** button in the ODBC Data Source Administrator writes
one for you; see [`packaging/README.md`](packaging/README.md) for that and for
the scripted alternatives.

### Logging

Two environment variables turn on tracing, which is by far the fastest way to
see which ODBC functions your tool actually calls, and in what order:

```bash
# Levels: trace, debug, info, warn, error
ODBC_LOG_LEVEL=debug isql -3 test_sqlite -v

# Or send it to a file instead of stderr
ODBC_LOG_LEVEL=debug ODBC_LOG_FILE=/tmp/odbc.log isql -3 test_sqlite -v
```

## What it deliberately does not do

Every one of these is reported to the application as unsupported rather than
quietly faked, so a tool can react to it instead of trusting a wrong answer.

- **No catalogs and no schemas.** SQLite has neither, so the driver says so
  rather than inventing a fake one-level hierarchy for the sake of looking
  familiar.
- **No stored procedures.** SQLite has none, so those lookups return nothing.
- **No query timeout.** You can cancel a running statement from another thread,
  but asking for "give up after 30 seconds" is answered with "you have no
  timeout" and a warning, instead of a promise that would never be kept.
- **Result sets are read into memory in one go.** Simple, and it is what makes
  cursors survive a commit or rollback, but a `SELECT` over a table larger than
  your RAM is not going to work.
- **One isolation level.** SQLite gives you serializable transactions, so that
  is the only level offered, and asking for a weaker one is refused up front
  rather than accepted and silently ignored.
- **No setup dialog on Linux.** Windows gets one, from the **Add** button in
  the ODBC administrator. unixODBC has no equivalent convention for a driver to
  put a window on the screen, so on Linux a DSN is a section in `odbc.ini`.

## Testing

```bash
cargo test    # unit and FFI tests; needs no database file and no setup
cargo bench   # Criterion fetch-throughput benchmark against :memory:
```

`cargo test` drives the real exported C entry points against real handles, so
it catches the marshalling bugs that ordinary Rust tests cannot.

The integration suite goes one layer further out and runs through real
unixODBC, using Python's `pyodbc` exactly like a normal application would:

```bash
./integration-tests/setup.sh       # build the driver, create the database, write the ODBC config
./integration-tests/run-tests.sh   # run the pyodbc suite, then cargo test
```

Both are run on every pull request. `run-tests.sh --windows` additionally runs
the same suite inside a Windows VM; see
[integration-tests/README.md](integration-tests/README.md) for what is covered
and [integration-tests/windows/WINDOWS.md](integration-tests/windows/WINDOWS.md)
for how to provision one.

For the architecture, the conventions and the full testing reference, see
[AGENTS.md](AGENTS.md). For building it, the `[patch]` that points core at a
sibling checkout, and what has to pass before a commit, see
[CONTRIBUTING.md](CONTRIBUTING.md).

## Releasing

See [packaging/README.md](packaging/README.md) for building the release
archives and how the SBOM is produced, and `release.toml` for the
`cargo-release` configuration.

## Security

Please report vulnerabilities privately; see [SECURITY.md](SECURITY.md).

## License

Apache-2.0
