# stackable-odbc-sqlite

ODBC 3.x driver for [SQLite](https://sqlite.org), built on the
[stackable-odbc-core](https://github.com/stackabletech/stackable-odbc-core)
framework.

The driver compiles to a C dynamic library that an ODBC Driver Manager
(unixODBC on Linux, the built-in Driver Manager on Windows) loads at runtime.
It opens a local SQLite database file through `rusqlite` with the bundled
SQLite library, so it needs no server and no external SQLite installation.

## Requirements

- Rust 1.95.0+ (pinned in `rust-toolchain.toml`)
- Linux: `unixODBC` and `isql` (`pacman -S unixodbc-dev` / `apt install unixodbc-dev`)
- `sqlite3` CLI for creating the test database (`pacman -S sqlite` / `apt install sqlite3`)

## Building

```bash
cargo build
```

Linux output: `target/debug/libstackable_odbc_sqlite.so`.

## Connection string parameters

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| Database  | Yes      | --      | Path to the SQLite database file (`:memory:` for an in-memory database) |

## Testing

Run all commands from the repository root.

```bash
# Build the driver, create the test database, write the ODBC config
./test/setup.sh

# Connect interactively
export ODBCSYSINI=$(pwd)/test
export ODBCINI=$(pwd)/test/odbc.ini
isql -3 test_sqlite -v
```

Or with a DSN-less connection string:

```bash
isql -3 -k "Driver=$(pwd)/target/debug/libstackable_odbc_sqlite.so;Database=$(pwd)/test/test.db" -v
```

The test database (`test/test.db`) has a `types_test` table with integer, text,
real, boolean, blob, and text-based datetime columns (see
`test/create_test_db.sql`). The full integration suite runs via
`./test/run-tests.sh` (add `--windows` for the VM suite); see
[AGENTS.md](AGENTS.md#testing) for the complete matrix.

### Logging

```bash
# Log to stderr at debug level
ODBC_LOG_LEVEL=debug isql -3 test_sqlite -v

# Log to a file (levels: trace, debug, info, warn, error)
ODBC_LOG_LEVEL=debug ODBC_LOG_FILE=/tmp/odbc.log isql -3 test_sqlite -v
```

This is invaluable for seeing which ODBC functions are called, and in what order.

## Releasing

See [packaging/README.md](packaging/README.md) for building release archives,
and `release.toml` for the `cargo-release` configuration.

## License

Apache-2.0
