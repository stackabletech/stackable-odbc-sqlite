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
| `suites/harness.py` | PASS/FAIL accounting and connection-string parsing, shared by the suites |
| `suites/odbc_abi.py` | The raw ODBC C ABI declared for `ctypes`, for the suites that skip the Driver Manager |
| `suites/test_integration.py` | The pyodbc suite, run once per connection style |
| `suites/test_transactions.py` | Manual-commit transactions, run once per connection style |
| `suites/test_sql_surface.py` | The SQL a BI tool emits, the ODBC escapes and the catalog functions, run once per connection style |
| `suites/test_c_abi.py` | The C ABI pen test, run once |
| `suites/test_type_matrix.py` | Type-transform fuzz and column description, run once |
| `perf/test_stress.py` | BI query patterns over a generated star schema, run once |
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

`test_transactions.py` runs the same two ways, because manual-commit mode is
set on the connection. Three of its scenarios assert the *opposite* of the
Trino driver's equivalents, which is why it could not simply be copied across:
a failed statement leaves a SQLite transaction usable rather than aborting it,
a commit preserves an open cursor rather than closing it, and serializable is
the level that must be accepted rather than refused.

`test_sql_surface.py` runs both ways too. It walks joins, aggregates, window
functions, CTEs, set operations and parameters, and is the only suite that
reaches `escape_dialect.rs`: the `{fn ...}`, `{d ...}`, `{ts ...}` and
`{oj ...}` sequences, including the three date/time forms that are bare
keywords in SQLite and need the whole escape rewritten rather than the name
swapped. Where the Trino driver can only check that a key or index lookup
returns nothing without erroring, this one asserts the rows, because SQLite
publishes all three.

Then `test_c_abi.py`, once. It loads the driver's `.so` with `ctypes` and calls
the exported entry points with **no Driver Manager in the loop**, which is the
point: unixODBC answers a large part of the ODBC state machine itself, so what
the driver does with an out-of-order or malformed call is invisible to anything
going through pyodbc. It covers handle lifecycle and parentage, stale handles
and double frees, cursor state, attribute round-trips, the query timeout, and
transactions. A DSN run would reach the same code by a longer route, so there
is only one.

Because the spec's **(DM)** diagnostics come from the Driver Manager, that suite
never demands one. Where a SQLSTATE is (DM)-annotated it asserts what the driver
does instead, with a comment naming the diagnostic it is not asking for.

Last, `test_type_matrix.py`, also once and also through ctypes. It drives every
(value, C type) pair through `SQLGetData` and checks invariants rather than a
transcribed copy of the ODBC conversion matrix, which would mostly test the
transcription. It then checks what `SQLDescribeCol` *says* each column is, which
is a separate question from what `SQLGetData` will hand over: SQLite gives a
computed column no declared type, so the driver answers from the storage class
of the values, and a tool decides from that whether a column can be summed.

Last, `perf/test_stress.py`, which asks the same shapes of SQL at size:
multi-table joins over a generated star schema, 50,000-row fetches, wide rows
and a prepared statement re-executed two hundred times. That matters more here
than for a client-server driver, because `exec_direct` materialises every row
before returning, so a large result set is where that decision is felt. It
builds its fixture with recursive CTEs and works in a database of its own,
`generated/stress.db`, so the shared one keeps the size the other suites
expect.

There is no counterpart to the Trino driver's `perf/parse_profile.py` and
`perf/profile_stress.sh`. Those split a query's time between the coordinator
and the client, and SQLite has no server-side half to attribute anything to.
`cargo bench` measures fetch throughput instead.

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
libvirt VM over WinRM, registers it, and runs the same suites through the
Windows Driver Manager: the three pyodbc ones DSN-less and then via a DSN, the
two ctypes ones once each, and the stress suite once. The Windows DM is much
stricter than unixODBC and tends to fail silently, so this is measured rather
than assumed.

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
