#!/usr/bin/env python3
"""
Run SQLite integration tests on a Windows VM over WinRM.

Builds the ODBC driver DLL, discovers the VM, deploys everything, registers
the driver, and runs test_integration.py through the Windows Driver Manager.

Usage:
    uv run --with pywinrm python3 integration-tests/windows/windows_test.py
    uv run --with pywinrm python3 integration-tests/windows/windows_test.py --skip-build
    uv run --with pywinrm python3 integration-tests/windows/windows_test.py --host 192.168.197.138

Requires: pywinrm (pip install pywinrm)
"""

import argparse
import base64
import functools
import http.server
import os
import re
import socket
import subprocess
import sys
import threading
from pathlib import Path

WINDOWS_DIR = Path(__file__).resolve().parent
TEST_DIR = WINDOWS_DIR.parent
PROJECT_DIR = TEST_DIR.parent
SUITES_DIR = TEST_DIR / "suites"
OPENSSL_CNF = WINDOWS_DIR / "openssl_legacy.cnf"

REMOTE_DIR = r"C:\odbc_test"
REMOTE_DLL = rf"{REMOTE_DIR}\stackable_odbc_sqlite.dll"
REMOTE_TEST = rf"{REMOTE_DIR}\test_integration.py"
REMOTE_DB = rf"{REMOTE_DIR}\test.db"

DRIVER_NAME = "stackable_odbc_sqlite"
DSN_NAME = "test_sqlite"

# Absolute path to Python on the VM. The Autounattend installs Python 3.12
# with InstallAllUsers=1 which always uses this location. We use the absolute
# path because WinRM sessions may not have an up-to-date PATH.
REMOTE_PYTHON = r'"C:\Program Files\Python312\python.exe"'

# The host-only network gateway (host side) — the VM can reach this IP to
# download files from our HTTP server. Override with --gateway or
# ODBC_TEST_HOST_GATEWAY for non-default libvirt subnets.
DEFAULT_HOST_GATEWAY = "192.168.197.1"
HTTP_PORT = 8080


def main():
    args = parse_args()
    setup_openssl()

    if not args.skip_build:
        build_dll(args.target)

    dll_path = resolve_dll_path(args.target)
    test_path = SUITES_DIR / "test_integration.py"

    host = args.host or discover_vm_ip(args.vm_network)

    import winrm

    print(f"=== Connecting to {host} via WinRM ===")
    session = winrm.Session(
        f"http://{host}:5985/wsman",
        auth=(args.user, args.password),
        transport="ntlm",
    )

    # Verify connectivity
    r = session.run_ps("hostname")
    hostname = r.std_out.decode().strip()
    if r.status_code != 0:
        print(f"ERROR: WinRM connection failed: {r.std_err.decode()}", file=sys.stderr)
        sys.exit(1)
    print(f"Connected to {hostname}")

    # Wait for VM setup to complete (Python, pyodbc installed).
    # The Autounattend.xml FirstLogonCommands create C:\setup_complete.txt
    # as the final step. WinRM may be available before this finishes.
    wait_for_setup(session)

    # Create remote directory
    session.run_ps(rf"New-Item -ItemType Directory -Force -Path {REMOTE_DIR} | Out-Null")

    # Serve files via HTTP and have the VM download them. This avoids WinRM's
    # command-length and envelope-size limits for large file transfers.
    print(f"=== Deploying files via HTTP ===")
    files_to_serve = {
        dll_path.name: dll_path,
        "test_integration.py": test_path,
    }
    with http_file_server(files_to_serve) as port:
        base_url = f"http://{args.gateway}:{port}"
        download_ps = (
            f'$ProgressPreference = "SilentlyContinue"; '
            f'Invoke-WebRequest -Uri "{base_url}/{dll_path.name}" '
            f'-OutFile "{REMOTE_DLL}"; '
            f'Invoke-WebRequest -Uri "{base_url}/test_integration.py" '
            f'-OutFile "{REMOTE_TEST}"'
        )
        r = session.run_ps(download_ps)
        if r.status_code != 0:
            stderr = r.std_err.decode()
            print(f"ERROR: file download failed:\n{stderr}", file=sys.stderr)
            sys.exit(1)
    print(f"  DLL: {dll_path.stat().st_size / 1024:.0f} KB")
    print(f"  test_integration.py: {test_path.stat().st_size / 1024:.0f} KB")

    print("=== Registering ODBC driver ===")
    register_driver(session)

    # --- Run 1: DSN-less connection string ---
    print("=== Running integration tests (DSN-less) ===")
    conn_str = f"Driver={DRIVER_NAME};Database={REMOTE_DB}"
    exit_code = run_tests(session, conn_str)
    if exit_code != 0:
        sys.exit(exit_code)

    # --- Run 2: DSN-based connection ---
    print("=== Registering DSN ===")
    register_dsn(session)

    print("=== Running integration tests (via DSN) ===")
    dsn_conn_str = f"DSN={DSN_NAME}"
    exit_code = run_tests(session, dsn_conn_str)
    sys.exit(exit_code)


def parse_args():
    p = argparse.ArgumentParser(
        description="Run SQLite ODBC integration tests on a Windows VM.",
    )
    p.add_argument(
        "--skip-build",
        action="store_true",
        help="skip cargo build, use existing DLL",
    )
    p.add_argument(
        "--target",
        choices=["gnu", "msvc"],
        default="gnu",
        help="cross-compilation target (default: gnu)",
    )
    p.add_argument(
        "--host",
        help="VM IP or hostname (default: auto-discover from DHCP leases)",
    )
    p.add_argument(
        "--vm-network",
        default="stackable-odbc-test-hostnet",
        help="libvirt network for IP discovery (default: stackable-odbc-test-hostnet)",
    )
    p.add_argument(
        "--user",
        default="Administrator",
        help="WinRM username (default: Administrator)",
    )
    p.add_argument(
        "--password",
        default="Asdf1234",
        help="WinRM password (default: Asdf1234)",
    )
    p.add_argument(
        "--gateway",
        default=os.environ.get("ODBC_TEST_HOST_GATEWAY", DEFAULT_HOST_GATEWAY),
        help=(
            "host-only network gateway IP the VM uses to reach the host "
            f"(default: $ODBC_TEST_HOST_GATEWAY or {DEFAULT_HOST_GATEWAY})"
        ),
    )
    return p.parse_args()


def setup_openssl():
    """Point OPENSSL_CONF at the legacy provider config for NTLM/MD4."""
    if "OPENSSL_CONF" in os.environ:
        return  # user already set it
    if OPENSSL_CNF.exists():
        os.environ["OPENSSL_CONF"] = str(OPENSSL_CNF)
    else:
        print(
            f"WARNING: {OPENSSL_CNF} not found. WinRM NTLM auth may fail\n"
            "if your OpenSSL does not have the legacy provider enabled.\n"
            "See integration-tests/windows/WINDOWS.md for details.",
            file=sys.stderr,
        )


def build_dll(target: str):
    """Cross-compile the SQLite ODBC driver for Windows."""
    rust_target = (
        "x86_64-pc-windows-msvc" if target == "msvc" else "x86_64-pc-windows-gnu"
    )
    cmd = [
        "cargo", "build", "--release",
        "--target", rust_target,
        "-p", "stackable-odbc-sqlite",
    ]
    print(f"=== Building DLL ({rust_target}) ===")
    result = subprocess.run(cmd, cwd=PROJECT_DIR)
    if result.returncode != 0:
        print("ERROR: cargo build failed", file=sys.stderr)
        sys.exit(1)


def wait_for_setup(session):
    """Wait for VM FirstLogonCommands to finish (sentinel file)."""
    import time

    r = session.run_ps(r"Test-Path C:\setup_complete.txt")
    if r.std_out.decode().strip() == "True":
        return

    print("=== Waiting for VM setup to complete ===")
    for i in range(60):
        time.sleep(10)
        r = session.run_ps(r"Test-Path C:\setup_complete.txt")
        if r.std_out.decode().strip() == "True":
            print("  Setup complete")
            return
        print(f"  still waiting... ({(i + 1) * 10}s)", end="\r")

    print(
        "\nERROR: timed out waiting for VM setup (C:\\setup_complete.txt).\n"
        "The FirstLogonCommands in Autounattend.xml may have failed.\n"
        "Check the VM with: virt-viewer --connect qemu:///system stackable-odbc-test",
        file=sys.stderr,
    )
    sys.exit(1)


def resolve_dll_path(target: str) -> Path:
    """Return the path to the built DLL."""
    rust_target = (
        "x86_64-pc-windows-msvc" if target == "msvc" else "x86_64-pc-windows-gnu"
    )
    dll = PROJECT_DIR / "target" / rust_target / "release" / "stackable_odbc_sqlite.dll"
    if not dll.exists():
        print(f"ERROR: DLL not found at {dll}", file=sys.stderr)
        print("Run without --skip-build, or check your build output.", file=sys.stderr)
        sys.exit(1)
    return dll


def discover_vm_ip(network: str) -> str:
    """Get the VM IP from libvirt DHCP leases."""
    print(f"=== Discovering VM IP from network {network} ===")
    try:
        result = subprocess.run(
            ["virsh", "--connect", "qemu:///system", "net-dhcp-leases", network],
            capture_output=True, text=True, check=True,
        )
    except FileNotFoundError:
        print("ERROR: virsh not found. Install libvirt or use --host.", file=sys.stderr)
        sys.exit(1)
    except subprocess.CalledProcessError as e:
        print(
            f"ERROR: could not query DHCP leases for network '{network}'.\n"
            "Is the VM running? See integration-tests/windows/WINDOWS.md for setup.\n"
            f"virsh output: {e.stderr}",
            file=sys.stderr,
        )
        sys.exit(1)

    # Parse lines like: 2026-04-01 ... ipv4 192.168.197.138/24 sble-addc ...
    ips = re.findall(r"ipv4\s+([\d.]+)/", result.stdout)
    if not ips:
        print(
            f"ERROR: no DHCP leases found on network '{network}'.\n"
            "Is the VM running? See integration-tests/windows/WINDOWS.md for setup.",
            file=sys.stderr,
        )
        sys.exit(1)

    ip = ips[-1]  # take the most recent lease
    print(f"Found VM at {ip}")
    return ip


class _FileServer(http.server.SimpleHTTPRequestHandler):
    """HTTP handler that serves specific files from a lookup table."""

    file_map: dict[str, Path] = {}

    def do_GET(self):
        name = self.path.lstrip("/")
        path = self.file_map.get(name)
        if path is None or not path.exists():
            self.send_error(404, f"Not found: {name}")
            return
        data = path.read_bytes()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, format, *args):
        pass  # silence request logging


class http_file_server:
    """Context manager that runs a temporary HTTP server in a background thread.

    Serves only the files in ``file_map`` (name → local Path). The server
    binds to all interfaces on an available port (or HTTP_PORT if free) and
    shuts down when the context exits.
    """

    def __init__(self, file_map: dict[str, Path]):
        self.file_map = file_map
        self.server = None
        self.thread = None

    def __enter__(self) -> int:
        handler = type(
            "_Handler",
            (_FileServer,),
            {"file_map": self.file_map},
        )
        port = HTTP_PORT if _port_available(HTTP_PORT) else 0
        self.server = http.server.HTTPServer(("0.0.0.0", port), handler)
        if port == 0:
            port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        return port

    def __exit__(self, *_):
        if self.server:
            self.server.shutdown()
        if self.thread:
            self.thread.join(timeout=5)


def _port_available(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        try:
            s.bind(("0.0.0.0", port))
            return True
        except OSError:
            return False


def run_tests(session, conn_str: str) -> int:
    """Run test_integration.py on the VM and return the exit code."""
    r = session.run_ps(
        f'& {REMOTE_PYTHON} {REMOTE_TEST} "{conn_str}"'
    )
    stdout = r.std_out.decode("utf-8", errors="replace")
    print(stdout, end="")

    if r.std_err:
        stderr = r.std_err.decode("utf-8", errors="replace")
        if "CLIXML" not in stderr:
            print(stderr, end="", file=sys.stderr)

    return r.status_code


def register_driver(session):
    """Register the ODBC driver via registry + odbcconf.exe.

    INSTALLDRIVER alone won't update the DLL path if the driver is already
    registered (it only increments UsageCount). Force-update via the registry
    to ensure the freshly deployed DLL is always used.
    """
    # Must use run_cmd (cmd.exe), not run_ps — PowerShell mangles odbcconf arguments.
    cmd = (
        f'odbcconf.exe /A {{INSTALLDRIVER '
        f'"{DRIVER_NAME}|Driver={REMOTE_DLL}|Setup={REMOTE_DLL}|"}}'
    )
    r = session.run_cmd("cmd.exe", ["/c", cmd])
    if r.status_code != 0:
        print(f"ERROR: driver registration failed: {r.std_err.decode()}", file=sys.stderr)
        sys.exit(1)
    session.run_ps(
        f'Set-ItemProperty '
        f'"HKLM:\\SOFTWARE\\ODBC\\ODBCINST.INI\\{DRIVER_NAME}" '
        f'-Name "Driver" -Value "{REMOTE_DLL}"; '
        f'Set-ItemProperty '
        f'"HKLM:\\SOFTWARE\\ODBC\\ODBCINST.INI\\{DRIVER_NAME}" '
        f'-Name "Setup" -Value "{REMOTE_DLL}"'
    )
    print(f"Driver '{DRIVER_NAME}' registered")


def register_dsn(session):
    """Register a DSN pointing at the test database."""
    cmd = (
        f'odbcconf.exe /A {{CONFIGDSN "{DRIVER_NAME}" '
        f'"DSN={DSN_NAME}|Database={REMOTE_DB}|"}}'
    )
    r = session.run_cmd("cmd.exe", ["/c", cmd])
    if r.status_code != 0:
        print(f"ERROR: DSN registration failed: {r.std_err.decode()}", file=sys.stderr)
        sys.exit(1)
    print(f"DSN '{DSN_NAME}' registered")


if __name__ == "__main__":
    main()
