# Stackable SQLite ODBC Driver

ODBC 3.x driver for SQLite, primarily intended for testing and for
exercising the Stackable ODBC framework on a lightweight backend.

## Building from source

To produce the release archives yourself, run the following from the
**repository root**:

```bash
# One-time: add the Windows cross-compilation target
rustup target add x86_64-pc-windows-gnu

# Build the Linux and Windows binaries
cargo build --release
cargo build --release --target x86_64-pc-windows-gnu

# Package into release archives (replace the version as appropriate)
VERSION=0.0.1 ./packaging/build-archives.sh
```

This produces two files in `packaging/dist/`:

- `stackable-odbc-sqlite-<version>-linux-x64.tar.gz`
- `stackable-odbc-sqlite-<version>-windows-x64.zip`

To install on Linux, extract and run the install script:

```bash
mkdir /tmp/sqlite-odbc
tar xzf stackable-odbc-sqlite-0.0.1-linux-x64.tar.gz -C /tmp/sqlite-odbc
cd /tmp/sqlite-odbc
sudo ./install.sh
```

On Windows, extract the `.zip` and run `install.bat` from an Administrator
Command Prompt. See the installation instructions below for details.

## Installation

> **Note:** These instructions assume you are working from an extracted
> release archive, where the driver binary sits alongside the install
> scripts. If you are working from a source checkout, build the archives
> first (see above).

### Linux (x86_64)

Requires `unixODBC` (`unixodbc` package) and root privileges for
`odbcinst` registration.

```bash
sudo ./install.sh
```

Verify with `odbcinst -q -d`; the output should include
`[stackable_odbc_sqlite]`.

To uninstall:

```bash
sudo ./uninstall.sh
```

If you created any DSNs, also remove them from `/etc/odbc.ini` (or
`~/.odbc.ini`).

### Windows (x86_64)

Open an **Administrator** Command Prompt (`cmd.exe`), then:

```cmd
install.bat
```

Verify with the ODBC Data Source Administrator
(`%SystemRoot%\System32\odbcad32.exe`); the Drivers tab should list
`stackable_odbc_sqlite`.

To uninstall:

```cmd
uninstall.bat
```

If you created any DSNs, also remove them via the registry:

```cmd
reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\YourDsnName" /f
reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\ODBC Data Sources" /v "YourDsnName" /f
```

## Create a DSN (optional)

A DSN stores connection parameters so that users don't need the full
connection string each time. This step is optional, since DSN-less connection
strings (shown below) work without it.

On Windows (`cmd.exe`):

```cmd
odbcconf.exe /A {CONFIGDSN "stackable_odbc_sqlite" "DSN=SQLite Test|Database=C:\data\test.db|"}
```

> **PowerShell users:** `odbcconf.exe` commands with `{...}` use `cmd.exe`
> syntax. In PowerShell, wrap the argument in single quotes:
> `odbcconf.exe /A '{CONFIGDSN ...}'`.

The DSN will appear under the **User DSN** tab in ODBC Data Source
Administrator.

> **Note:** the driver registers itself as its own `Setup` library and
> implements `ConfigDSNW`, but headlessly: it never displays a dialog. The
> **Add** button therefore does not fail, it silently writes a data source
> from whatever attributes the Driver Manager passed it, which will not
> include `Database`. Create DSNs with `odbcconf` or the registry so that
> every key is set.

On Linux, add a section to `/etc/odbc.ini` (or `~/.odbc.ini` for a
per-user DSN):

```ini
[SQLite Test]
Driver = stackable_odbc_sqlite
Database = /path/to/your.db
```

## Connection string

DSN-less:

```
Driver=stackable_odbc_sqlite;Database=/path/to/your.db
```

## Support

<https://github.com/stackabletech/stackable-odbc-sqlite>
