# Contributing

Thanks for considering a contribution. Bug reports, connection strings that
fail, and reports of a tool that will not talk to the driver are all useful.

This driver exists to exercise
[`stackable-odbc-core`](https://github.com/stackabletech/stackable-odbc-core) on
a backend that needs no server, so a change that makes it a better test of core
is as welcome as one that makes it a better SQLite driver.

- **Questions and ideas:** [GitHub Discussions](https://github.com/orgs/stackabletech/discussions)
  or [Discord](https://discord.gg/7kZ3BNnCAF).
- **Bugs:** open an issue. Please say which platform, which Driver Manager
  (unixODBC or the Windows one), and which application. A driver log helps most
  of all: set `ODBC_LOG_FILE` and `ODBC_LOG_LEVEL=debug` and attach the result.
- **Security problems:** do not open an issue. See [SECURITY.md](SECURITY.md).

## Building

You need the unixODBC development libraries, because the ODBC bindings link
against them. SQLite itself is compiled into the driver, so there is nothing
else to install, and no database or ODBC configuration is needed to build and
run the unit tests.

```bash
sudo apt-get install unixodbc-dev   # Debian/Ubuntu
sudo pacman -S unixodbc             # Arch
```

```bash
git clone https://github.com/stackabletech/stackable-odbc-sqlite
cd stackable-odbc-sqlite
cargo build --release
```

That produces `target/release/libstackable_odbc_sqlite.so`.

Everything generic about being an ODBC driver lives in
[`stackable-odbc-core`](https://github.com/stackabletech/stackable-odbc-core):
handle management, UTF-16 marshalling, diagnostics, panic safety and the
exported C entry points. This repository holds only the SQLite-specific half.
Cargo fetches core for you, so there is nothing to clone by hand.

### Working on core at the same time

This is the common case here, because this driver is where core's changes are
tried out. To build against a local checkout of core rather than the fetched
one, add a `[patch]` to your own `.cargo/config.toml`, which is not checked in:

```toml
[patch."https://github.com/stackabletech/stackable-odbc-core.git"]
stackable-odbc-core = { path = "../stackable-odbc-core" }
```

`.cargo/` is gitignored, so the override cannot be committed. **`Cargo.lock`
can**: cargo rewrites core's entry to the local path while the patch is active,
so check `git status` before committing. Remove the override, or push your core
changes, before you rely on a build.

The toolchain version is pinned in `rust-toolchain.toml`, so rustup will fetch
the right one on first build.

### Windows

Cross-compile with MinGW (`gcc-mingw-w64-x86-64`):

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

That produces `target/x86_64-pc-windows-gnu/release/stackable_odbc_sqlite.dll`.
`build.rs` embeds a version resource into it with `windres`, which comes with
that same package; a build without it fails rather than shipping a DLL the ODBC
Data Source Administrator lists as `Not marked`.

Anything destined for a release archive is built with
[`cargo auditable`](https://github.com/rust-secure-code/cargo-auditable), which
embeds the dependency list the SBOM is generated from. See
[`packaging/README.md`](packaging/README.md).

## Testing

```bash
cargo test                                   # unit and FFI tests; no setup needed
cargo clippy --all-targets -- -D warnings
```

`cargo test` must produce zero warnings. It drives the real exported C entry
points against real handles, so it catches marshalling bugs an ordinary Rust
test cannot.

The integration suite goes one layer further out, through real unixODBC using
Python's `pyodbc`. It needs no server, so it runs on every pull request:

```bash
./integration-tests/setup.sh       # build the driver, create the database, write the ODBC config
./integration-tests/run-tests.sh   # run the pyodbc suite, then cargo test
```

See [`integration-tests/README.md`](integration-tests/README.md) for the flags.
The Windows suite runs the same tests through the Windows Driver Manager in a
VM; see
[`integration-tests/windows/WINDOWS.md`](integration-tests/windows/WINDOWS.md).

## Before you commit

```bash
pre-commit run --all-files
```

That is the gate, and it is the single source of truth for what must pass. It
runs rustfmt, clippy, `cargo test`, rustdoc, cargo-deny, cargo-sort, shellcheck
and markdownlint.

Two more things a change usually needs:

- **A changelog entry**, under `## [Unreleased]` in
  [`CHANGELOG.md`](CHANGELOG.md), if an ODBC application can observe the
  difference. A changed SQLSTATE, a changed `SQLGetInfo` value, a new
  connection-string key or a different type mapping all count.
- **A new connection-string key touches four places**: the parser in
  `src/backend/types/connect_params.rs`, the key tables in
  [`README.md`](README.md) and [`packaging/README.md`](packaging/README.md), and
  the `$Fields` table in `packaging/windows/configure-dsn.ps1`.
  `dsn_keys_match_the_connection_string_parser` in `src/lib.rs` fails the build
  if the parser and the dialog disagree. See
  [Connection string keys](AGENTS.md#connection-string-keys).

## Where things live

[`AGENTS.md`](AGENTS.md) is the working reference: module layout, the split
against `stackable-odbc-core`, the error-mapping rules, and the measured SQLite
and Driver Manager behaviour behind the design decisions. Read the section that
covers whatever you are about to change. It is written for AI coding agents and
human contributors alike.

Two rules are worth stating here, because they are the ones most easily broken
by a reasonable-looking change:

- **Read the ODBC spec page for any function whose behaviour you change.** What
  the driver returns from `SQLGetInfo`, from the catalog functions and from the
  type-conversion paths is directly observable by applications, and each has a
  spec-defined shape and value range.
- **Route every client error through `map_sqlite_error`.** It is the single
  place that decides the SQLSTATE and carries SQLite's own extended result code
  through to `SQLGetDiagRec`. Building an error at the call site quietly
  degrades it.

## License

By contributing you agree that your contribution is licensed under
[Apache-2.0](LICENSE).
