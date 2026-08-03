# Windows testing

The integration suites, driven through the Windows ODBC Driver Manager over
WinRM. The Windows DM is far stricter than unixODBC and tends to fail silently,
so this is measured rather than assumed. The target is a disposable Windows
Server VM on a host-only libvirt network, created by the Ansible playbook in
`vm/`.

The VM's credentials are `Administrator` / `Asdf1234`, the defaults in
`windows_test.py`. They are not a secret: the machine is local, throwaway, and
reachable only from the host that created it. Pass `--user` and `--password`
for a VM built some other way.

## Quick start: running the tests

If the VM does not exist yet, build it first: [Prerequisites](#prerequisites),
then [Creating the VM](#creating-the-vm).

Start the VM and its networks (skip whatever is already running):

```bash
virsh --connect qemu:///system net-start stackable-odbc-test-hostnet
virsh --connect qemu:///system net-start stackable-odbc-test-internet
virsh --connect qemu:///system start stackable-odbc-test
```

Then run from the Linux host. `uv` installs `pywinrm` itself:

```bash
uv run --with pywinrm python3 integration-tests/windows/windows_test.py
```

This runs the same suites the Linux runner does, in the same shapes:

| Suite | How |
|---|---|
| `test_integration.py`, `test_transactions.py`, `test_sql_surface.py` | Through the Windows Driver Manager, once DSN-less and once via a DSN |
| `test_c_abi.py`, `test_type_matrix.py` | Loading the DLL with `ctypes`, so no Driver Manager is in the loop. Once |
| `perf/test_stress.py` | Once, against a database of its own |

Every suite runs and every result is recorded; the script does not stop at the
first failure, because one Windows-only defect should not hide the next and a VM
round trip is slow enough that finding out costs a second run.

`harness.py` and `odbc_abi.py` are deployed alongside, flat in `C:\odbc_test`,
which is where each suite's own `sys.path` entry looks for them.

Nothing needs to be running on the host: SQLite is compiled into the DLL, and
the script copies a freshly built database to the VM.

**Do not diagnose a Windows failure without rebuilding the DLL first.**
`--skip-build` reuses whatever sits in `target/x86_64-pc-windows-gnu/release/`,
which can predate the feature under test by days.

### Options

`--help` lists them all. The ones that come up:

| Flag | Default | Effect |
|---|---|---|
| `--skip-build` | off | Use the DLL already in `target/`, rather than rebuilding. See the warning above |
| `--target {gnu,msvc}` | `gnu` | Which Windows target to build and deploy. `msvc` needs an MSVC-capable linker on the host; see [Building the DLL](#building-the-dll) |
| `--host <address>` | discovered from the libvirt DHCP leases | VM IP or hostname |
| `--vm-network <name>` | `stackable-odbc-test-hostnet` | The libvirt network that discovery reads leases from |
| `--user`, `--password` | `Administrator`, `Asdf1234` | WinRM credentials |
| `--gateway <ip>` | `$ODBC_TEST_HOST_GATEWAY`, else `192.168.197.1` | The host-only gateway address the VM reaches the host on, to download the DLL and the test files from a short-lived HTTP server |

### The setup dialog

`windows_test.py` deploys `packaging/windows/configure-dsn.ps1` to
`C:\odbc_test\` beside the DLL, which is where the driver looks for it, and
registers the driver with `Setup=` pointing at the DLL. The suite itself
creates its DSN with `odbcconf`, which passes a null *hwndParent*, so it takes
the headless path and never displays anything.

That means the dialog is deployed but not exercised automatically. To check it,
open the Administrator in the VM by hand:

```cmd
%SystemRoot%\System32\odbcad32.exe
```

**Add…** on `stackable_odbc_sqlite`, or **Configure…** on an existing data
source, should display the dialog, and its **Test connection** button should
report the SQLite version and a table count. The table count is the check worth
having, because SQLite creates a missing file rather than refusing, so a typo
in the path connects perfectly well and finds nothing.

### Using a different hypervisor (VirtualBox, Hyper-V, etc.)

The VM lifecycle section below uses QEMU/KVM via libvirt, and the test script
auto-discovers the VM IP from libvirt DHCP leases. A Windows guest in another
hypervisor works too; pass its IP directly:

```bash
uv run --with pywinrm python3 integration-tests/windows/windows_test.py --host <vm-ip>
```

The VM must have WinRM enabled on port 5985 with NTLM auth, and Python 3 plus
pyodbc installed.

### OpenSSL legacy provider

WinRM uses NTLM authentication, which requires MD4, disabled by default in
modern OpenSSL. The test script sets `OPENSSL_CONF` to point at
`integration-tests/windows/openssl_legacy.cnf`, which enables the legacy
provider.

An `unsupported hash type md4` error means that file is missing, or that
`OPENSSL_CONF` is overridden in your environment.

## VM lifecycle

### Prerequisites

QEMU/KVM and libvirt must be installed and working as system services.
`nix-shell` only provides Ansible and the Python bindings, not the
virtualisation stack itself. Verify with:

```bash
virsh --connect qemu:///system list --all
```

You also need a `default` storage pool (`virsh pool-list`), and your user must
be in the `libvirt` group.

QEMU typically runs as a dedicated user (for example `libvirt-qemu`) that
cannot read files under your home directory. If the playbook fails with a
permission error on the ISO or the virtio drivers, grant read access with ACLs
(`setfacl -m u:libvirt-qemu:r /path/to/file.iso`, and
`setfacl -m u:libvirt-qemu:x` on each parent directory).

On Ubuntu 24.04 the following was enough; package names differ elsewhere:

```bash
sudo apt install -y qemu-system-x86 qemu-utils libvirt-daemon-system \
    libvirt-clients virtinst bridge-utils virt-viewer virt-manager acl
sudo adduser $USER libvirt
sudo adduser $USER kvm
# log out and back in, then verify:
virsh --connect qemu:///system list --all
# uv (Python tool runner, used by the test script):
pipx install uv
```

### Creating the VM

```bash
# Set once, pointing at your Windows Server 2022 evaluation ISO.
export WINDOWS_ISO=~/Downloads/SERVER_EVAL_x64FRE_en-us.iso

cd integration-tests/windows/vm
nix-shell                # loads Ansible + libvirt Python bindings
ansible-playbook start.yaml -i inventory.ini
```

The playbook creates a QEMU/KVM VM with two networks (host-only and NAT), boots
the Windows ISO, and waits for the guest agent. `Autounattend.xml` installs
Python and pyodbc automatically.

First run takes around 30 minutes, most of it the Windows install and the
downloads. Watch progress with:

```bash
virt-viewer --connect qemu:///system stackable-odbc-test
```

### What the current VM image was built with

A snapshot of the image in use, not a set of requirements. Each pin and the
paths derived from it have to move together, which is why they are collected
here.

| Thing | Value | Set in |
|---|---|---|
| Guest OS | Windows Server 2022 evaluation, [from the evalcenter](https://www.microsoft.com/en-us/evalcenter/evaluate-windows-server-2022) | `$WINDOWS_ISO`, checked by `vm/start.yaml` |
| Guest Python | 3.12, at `C:\Program Files\Python312\python.exe` | `vm/files/windows-install-config/Autounattend.xml`, and `REMOTE_PYTHON` in `windows_test.py` |
| virtio-win drivers | 0.1.248 | `vm/start.yaml`, downloaded and checksummed |
| LLVM for the MSVC cross build | `llvmPackages_18` | the `nix-shell` line under [Building the DLL](#building-the-dll) |

### Shutting down

```bash
virsh --connect qemu:///system shutdown stackable-odbc-test
```

The VM definition and disk persist, so the next `start` is fast.

### Tearing down completely

Remove the VM, its disk, and the virtual networks:

```bash
virsh --connect qemu:///system destroy stackable-odbc-test
virsh --connect qemu:///system undefine stackable-odbc-test
virsh --connect qemu:///system vol-delete --pool default stackable-odbc-test.qcow2

virsh --connect qemu:///system net-destroy stackable-odbc-test-hostnet
virsh --connect qemu:///system net-destroy stackable-odbc-test-internet
virsh --connect qemu:///system net-undefine stackable-odbc-test-hostnet
virsh --connect qemu:///system net-undefine stackable-odbc-test-internet
```

## Reference: driver and DSN management

Everything below is what `windows_test.py` does for you, written out for when
you are working in the VM by hand.

### Building the DLL

The mingw cross-compiler needs no extra tooling:

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

Output: `target/x86_64-pc-windows-gnu/release/stackable_odbc_sqlite.dll`

MSVC cross-compilation works through `cargo-xwin` (`cargo install cargo-xwin`,
plus nix for LLVM), and is what `--target msvc` builds:

```bash
nix-shell -p llvmPackages_18.clang llvmPackages_18.lld llvmPackages_18.llvm --run \
  "cargo xwin build --release --target x86_64-pc-windows-msvc"
```

Output: `target/x86_64-pc-windows-msvc/release/stackable_odbc_sqlite.dll`

Both work with the Windows Driver Manager. Prefer mingw; use MSVC to match a
target environment exactly. Note that the harness builds with plain
`cargo build`, while release DLLs are built with `cargo auditable`, which
embeds the dependency list `packaging/sbom.sh` refuses an artifact without.

### Registering the driver

All commands below run in `cmd.exe` as Administrator. Adjust the DLL path as
needed.

```cmd
odbcconf.exe /A {INSTALLDRIVER "stackable_odbc_sqlite|Driver=C:\odbc_test\stackable_odbc_sqlite.dll|Setup=C:\odbc_test\stackable_odbc_sqlite.dll|"}
```

Both `Driver=` and `Setup=` must point to the same DLL, which exports both the
ODBC API functions and the `ConfigDSNW` setup entry point.

### Creating a DSN

Two ways.

**The ODBC Data Source Administrator**, `odbcad32.exe` → **Add…**, which
displays the driver's dialog. `ConfigDSN` reaches
`SqliteBackend::configure_dsn`, which runs `configure-dsn.ps1` and hands the
keywords back for core to write. The script must sit beside the DLL.

**`odbcconf`**, which is what the test harness uses. It passes a null
*hwndParent*, so no dialog is displayed and the keywords on the command line
are written as given:

```cmd
odbcconf.exe /A {CONFIGDSN "stackable_odbc_sqlite" "DSN=test_sqlite|Database=C:\odbc_test\test.db|"}
```

### Connection string parameters

`Database` is the only key. The full table is in the
[root README](../../README.md#connecting), and the authoritative list is
`src/backend/types/connect_params.rs`.

### Verifying registration

Open `%SystemRoot%\System32\odbcad32.exe` (64-bit) and confirm:

- **Drivers tab**: `stackable_odbc_sqlite` is listed, with a version and
  `Stackable GmbH` rather than `Not marked`.
- **User DSN tab**: `test_sqlite` (or whatever name you chose) is listed.
- **Add…** on the driver displays the setup dialog. See
  [The setup dialog](#the-setup-dialog).

### Unregistering

Remove a DSN (User DSN entries live under `HKCU`):

```cmd
reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\test_sqlite" /f
reg delete "HKCU\SOFTWARE\ODBC\ODBC.INI\ODBC Data Sources" /v "test_sqlite" /f
```

Remove the driver (through the registry, since `odbcconf` has no
`REMOVEDRIVER`):

```cmd
reg delete "HKLM\SOFTWARE\ODBC\ODBCINST.INI\stackable_odbc_sqlite" /f
reg delete "HKLM\SOFTWARE\ODBC\ODBCINST.INI\ODBC Drivers" /v "stackable_odbc_sqlite" /f
```

## Reference: manual testing

### PowerShell smoke test

PowerShell's `System.Data.Odbc` is built into .NET, so no extra tools are
needed. This example is self-contained: it creates its own table, queries it,
and cleans up. The driver must be registered first, which the test script does.

```powershell
$conn = New-Object System.Data.Odbc.OdbcConnection("Driver=stackable_odbc_sqlite;Database=C:\odbc_test\manual_test.db")
$conn.Open()
Write-Host "Connected: $($conn.State)"

$cmd = $conn.CreateCommand()
$cmd.CommandText = "CREATE TABLE IF NOT EXISTS demo (id INTEGER PRIMARY KEY, name TEXT, value REAL)"
$cmd.ExecuteNonQuery() | Out-Null
$cmd.CommandText = "DELETE FROM demo"
$cmd.ExecuteNonQuery() | Out-Null
$cmd.CommandText = "INSERT INTO demo VALUES (1, 'Alice', 75000.50)"
$cmd.ExecuteNonQuery() | Out-Null
$cmd.CommandText = "INSERT INTO demo VALUES (2, 'Bob', 62000.00)"
$cmd.ExecuteNonQuery() | Out-Null

$cmd.CommandText = "SELECT * FROM demo"
$reader = $cmd.ExecuteReader()
while ($reader.Read()) {
    Write-Host "$($reader[0]) | $($reader[1]) | $($reader[2])"
}
$reader.Close()

$cmd.CommandText = "DROP TABLE demo"
$cmd.ExecuteNonQuery() | Out-Null
$conn.Close()
Write-Host "Done"
```

Expected output:

```text
Connected: Open
1 | Alice | 75000.5
2 | Bob | 62000
Done
```

To connect through the DSN the test script registers instead:

```powershell
$c = New-Object System.Data.Odbc.OdbcConnection("DSN=test_sqlite"); $c.Open(); Write-Host "Connected: $($c.State)"; $c.Close()
```

### Running test_integration.py manually

To run the suite without the wrapper script, from a PowerShell session on the
VM:

```powershell
& "C:\Program Files\Python312\python.exe" C:\odbc_test\test_integration.py "Driver=stackable_odbc_sqlite;Database=C:\odbc_test\test.db"
```
