# Stackable SQLite ODBC Driver

ODBC 3.x driver for [SQLite](https://sqlite.org), for Linux and Windows.
SQLite is compiled into the driver, so there is no separate SQLite to install.

This file ships inside both release archives. If you have just extracted one,
start at [Installation](#installation).

## What is in the archive

`stackable-odbc-sqlite-<version>-linux-x64.tar.gz`:

| File | Purpose |
|------|---------|
| `libstackable_odbc_sqlite.so` | The driver |
| `install.sh`, `uninstall.sh` | Registration with unixODBC |
| `libstackable_odbc_sqlite.so.cdx.json` | CycloneDX SBOM for the driver |
| `README.md`, `LICENSE` | This file, and Apache-2.0 |

`stackable-odbc-sqlite-<version>-windows-x64.zip`:

| File | Purpose |
|------|---------|
| `stackable_odbc_sqlite.dll` | The driver |
| `install.bat`, `uninstall.bat` | Registration with the Windows Driver Manager |
| `configure-dsn.ps1` | The data source dialog |
| `stackable_odbc_sqlite.dll.cdx.json` | CycloneDX SBOM for the driver |
| `README.md`, `LICENSE` | This file, and Apache-2.0 |

The release page carries `sha256sums.txt` over every published file. Verify a
download with `sha256sum -c sha256sums.txt`, run from the directory you
downloaded into.

## Installation

### Linux (x86_64)

Requires `unixODBC` (the `unixodbc` package) and root privileges for
`odbcinst` registration.

```bash
mkdir /tmp/sqlite-odbc
tar xzf stackable-odbc-sqlite-<version>-linux-x64.tar.gz -C /tmp/sqlite-odbc
cd /tmp/sqlite-odbc
sudo ./install.sh
```

Verify with `odbcinst -q -d`; the output should include
`[stackable_odbc_sqlite]`.

To uninstall:

```bash
sudo ./uninstall.sh
```

If you created any DSNs, also remove them from `/etc/odbc.ini` (or
`~/.odbc.ini`). Your database files are untouched either way: a data source
only points at one.

### Windows (x86_64)

Extract the `.zip`, open an **Administrator** Command Prompt (`cmd.exe`) in the
extracted folder, then:

```cmd
install.bat
```

`install.bat` installs `configure-dsn.ps1` next to the DLL and refuses to run
without it, because that script is the dialog the ODBC Administrator's
**Add…** button displays.

Verify with the ODBC Data Source Administrator
(`%SystemRoot%\System32\odbcad32.exe`); the Drivers tab should list
`stackable_odbc_sqlite`, with a version and `Stackable GmbH` rather than
`Not marked`.

To uninstall:

```cmd
uninstall.bat
```

## Creating a data source

A data source (DSN) stores the connection settings under a name, so an
application can ask for `sales_db` instead of a full connection string. It is
optional: the DSN-less connection strings below work without one.

### Windows: the dialog

The ODBC Data Source Administrator's **Add…** button, and **Configure…** on an
existing data source, both display this driver's dialog. It asks for the data
source name and the database file, and its **Test connection** button opens the
file and reports the SQLite version and the number of tables it found before
anything is written.

The same dialog runs on its own, without going through the Administrator:

```cmd
powershell -ExecutionPolicy Bypass -File "%ProgramFiles%\Stackable\ODBC\configure-dsn.ps1"
```

```cmd
rem Edit an existing data source
powershell -ExecutionPolicy Bypass -File "...\configure-dsn.ps1" -Dsn sales_db
```

### Windows: without a dialog

For a scripted install, the same script writes a data source with no GUI:

```cmd
powershell -ExecutionPolicy Bypass -File "...\configure-dsn.ps1" ^
  -NoGui -Set @{ DSN='sales_db'; Database='C:\data\sales.db' }
```

Or through `odbcconf` directly:

```cmd
odbcconf.exe /A {CONFIGDSN "stackable_odbc_sqlite" "DSN=sales_db|Database=C:\data\sales.db|"}
```

> **PowerShell users:** `odbcconf.exe` commands with `{...}` use `cmd.exe`
> syntax. In PowerShell, wrap the argument in single quotes:
> `odbcconf.exe /A '{CONFIGDSN ...}'`.

A **System** data source (visible to every user, stored in `HKLM`) needs an
elevated session. The dialog disables the System option when it does not have
one, and `-NoGui -System` fails rather than writing a User data source
silently.

### Linux

Add a section to `/etc/odbc.ini` (or `~/.odbc.ini` for a per-user DSN):

```ini
[sales_db]
Driver = stackable_odbc_sqlite
Database = /path/to/sales.db
```

## Connection string

There is exactly one key. Keys are case-insensitive.

| Key | Required | Meaning |
|-----|----------|---------|
| `Database` | Yes | Path to the SQLite file, or `:memory:` for a throwaway in-memory database |

DSN-less:

```text
Driver=stackable_odbc_sqlite;Database=/path/to/sales.db
```

A file that does not exist yet is created on first connect, because that is
what SQLite does. A typo in the path therefore connects successfully and finds
an empty database rather than failing, which is why the dialog's **Test
connection** reports the table count.

## Support

- [Issues](https://github.com/stackabletech/stackable-odbc-sqlite/issues) for
  bugs
- [GitHub Discussions](https://github.com/orgs/stackabletech/discussions) for
  questions
- [Discord](https://discord.gg/7kZ3BNnCAF) to talk to us

A driver log is the most useful thing to attach to a report. Set two
environment variables before starting your application:

```bash
export ODBC_LOG_LEVEL=debug          # trace, debug, info, warn, error
export ODBC_LOG_FILE=/tmp/sqlite-odbc.log
```

On Windows, set the same two through **System Properties → Environment
Variables**. The log may contain your SQL, so check it before sharing.

## The SBOM

Each archive carries a CycloneDX software bill of materials next to the driver,
and the release page publishes an SPDX document for every artifact as well.
Both list what the binary actually links, the SQLite compiled inside it
included, so a security advisory can be checked against the driver you
installed rather than against whatever SQLite your system happens to have.

## Building the archives from source

Everything below is for people building the driver themselves. If you
installed from a release archive, you are done.

From the **repository root**:

```bash
# One-time: the Windows cross-compilation target and the two packaging tools
rustup target add x86_64-pc-windows-gnu
cargo install cargo-auditable
# syft: https://github.com/anchore/syft

# Build both binaries. `cargo auditable`, not plain `cargo`: it embeds the
# dependency list that the SBOM is generated from, and sbom.sh refuses an
# artifact without it.
cargo auditable build --locked --release
cargo auditable build --locked --release --target x86_64-pc-windows-gnu

VERSION=0.0.1 ./packaging/build-archives.sh
```

That writes both archives, four SBOMs and `sha256sums.txt` to
`packaging/dist/`.

### How the SBOM is generated

`packaging/sbom.sh` produces one CycloneDX and one SPDX document per artifact.
The component list comes from the `.dep-v0` section `cargo auditable` embeds,
so it describes what was **linked** rather than what `Cargo.toml` asked for.
Dev-dependencies are excluded by construction, and a git dependency's purl
names the resolved commit rather than a branch that moves.

Two kinds of component are invisible to cargo and are declared by hand in
`packaging/sbom-native.json`:

- **SQLite itself.** cargo sees `libsqlite3-sys`, the Rust wrapper. The C
  library compiled inside it is what an advisory against SQLite would name, and
  it ships in both artifacts. `the_declared_sqlite_version_is_the_one_linked`
  in `src/lib.rs` fails the build if the declared version drifts from what
  `rusqlite::version()` reports.
- **What each artifact links at load time.** The `.so` links unixODBC; the
  `.dll` imports only Windows' own libraries and carries the mingw runtime
  statically. `./packaging/sbom.sh --check-native <artifact>` verifies both
  claims against the real binary, and CI runs it on every pull request.

`./packaging/test-sbom.sh` is that pipeline's own test suite.
