# Integration tests

Everything that exercises the driver from outside the Rust crate: through real
unixODBC on Linux, and through the real Windows Driver Manager in a VM.

There is no service to start. SQLite is a file, and `rusqlite` links its own
copy of it into the driver, so the whole suite runs on a bare runner in seconds.
That is exactly why it gates every pull request while the Trino driver's
equivalent cannot.

```bash
./integration-tests/setup.sh       # build the driver, create the database, write the ODBC config
./integration-tests/run-tests.sh   # pyodbc through unixODBC, then cargo test
```

Both take `--help`.

## Layout

| Path | What it holds |
|------|---------------|
| `setup.sh`, `run-tests.sh` | Wrappers. The logic is in `scripts/` |
| `scripts/lib.sh` | The paths and helpers both scripts share. Sourced, never executed |
| `scripts/setup.sh` | Builds the driver, creates `test.db`, writes `odbc.ini` / `odbcinst.ini` |
| `scripts/run-tests.sh` | Runs the suites |
| `suites/create_test_db.sql` | The schema and rows every suite reads |
| `suites/test_integration.py` | The pyodbc suite, run once per connection style |
| `generated/` | Everything `setup.sh` writes. Gitignored |
| `windows/` | The VM suite, its libvirt definitions, and [WINDOWS.md](windows/WINDOWS.md) |

`generated/` is ignored rather than committed because the ODBC config it holds
names absolute paths. `odbcinst.ini` points at the driver's `.so` and
`odbc.ini` at the database, so neither survives being moved to another
checkout, and a committed copy would be wrong for everyone but its author.

## What gets run

`run-tests.sh` runs the pyodbc suite **twice**, against the same database:

- **DSN-less**, `Driver=...;Database=...`, which exercises this driver's own
  connection-string parsing.
- **Via a DSN**, `DSN=test_sqlite`, where the Driver Manager resolves the
  keywords out of `odbc.ini` first.

They are separate runs because they fail separately. A driver that reads its
parameters correctly can still be unreachable through a DSN, and that is a
configuration most applications actually use.

It then runs `cargo test`, so that one command gives a developer the whole
suite. CI passes `--skip-cargo-test`, since its pre-commit job has already run
exactly that via the `cargo-test` hook.

## Options

| Flag | Effect |
|------|--------|
| `--skip-build` | Reuse the driver already built. Forwarded to `windows_test.py`, whose build is a separate cross-compile |
| `--skip-cargo-test` | Run the pyodbc suites only. What CI passes |
| `--windows` | Additionally run the suite inside the Windows VM |

Any other argument is forwarded to `windows_test.py` (`--target`, `--host`,
`--vm-network`, `--user`, `--password`, `--gateway`) and so is rejected without
`--windows`, since a flag forwarded to a script that never runs would be
silently ignored.

## Windows

`windows/windows_test.py` deploys the cross-compiled DLL to a provisioned
libvirt VM over WinRM, registers it, and runs the same
`suites/test_integration.py` through the Windows Driver Manager, DSN-less and
then via a DSN. The Windows DM is much stricter than unixODBC and tends to fail
silently, so this is measured rather than assumed.

See [windows/WINDOWS.md](windows/WINDOWS.md) for provisioning the VM.

## Interactively

`setup.sh` prints these at the end, with absolute paths filled in:

```bash
export ODBCSYSINI=$(pwd)/integration-tests/generated
export ODBCINI=$(pwd)/integration-tests/generated/odbc.ini
isql -3 test_sqlite -v
```

Prefix either with `ODBC_LOG_LEVEL=debug` to see which ODBC functions your
client calls, and in what order.
