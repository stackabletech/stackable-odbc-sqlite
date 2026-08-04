#!/usr/bin/env python3
"""Shared machinery for the integration suites.

Standard library only, and `pyodbc` is imported lazily inside `Target.connect`.
`test_c_abi.py` loads the driver's `.so` with `ctypes` and depends on neither a
Driver Manager, nor `uv`, nor pyodbc. A module-scope pyodbc import here would
give it all three silently.
"""

import os
import time


class Results:
    """PASS/FAIL/NOTE/SKIP accounting for one suite run.

    `bad` rather than `fail` so a suite is free to define its own module-level
    `fail()` with different semantics; a silent collision is worse than an
    unlovely name.
    """

    def __init__(self, title):
        self.title = title
        self.passed = 0
        self.failed = 0
        self.notes = 0
        self.skipped = 0

    def ok(self, label, detail=""):
        self.passed += 1
        print(f"PASS  {label}{': ' + detail if detail else ''}")

    def bad(self, label, detail=""):
        self.failed += 1
        print(f"FAIL  {label}{': ' + detail if detail else ''}")

    def check(self, label, cond, detail=""):
        """Record a boolean assertion. Returns the condition, so a caller can
        skip dependent work without re-evaluating it."""
        if cond:
            self.ok(label, detail)
        else:
            self.bad(label, detail)
        return bool(cond)

    def run(self, label, fn):
        """Run a callable, recording an exception as a failure with its
        message. Prints elapsed time: a suite that slows down is a finding."""
        t0 = time.monotonic()
        try:
            fn()
            print(f"PASS  {label}  ({time.monotonic() - t0:.1f}s)")
            self.passed += 1
        except Exception as e:
            print(f"FAIL  {label}  ({time.monotonic() - t0:.1f}s): {e}")
            self.failed += 1

    def note(self, label, text):
        """An observation the driver is entitled to make either way. Not a
        gap, and never counted as a pass."""
        self.notes += 1
        print(f"NOTE  {label}: {text}")

    def skip(self, label, reason):
        """A test that did not run. The reason is mandatory: an unrun test must
        never be indistinguishable from a passing one."""
        self.skipped += 1
        print(f"SKIP  {label}: {reason}")

    def summary(self):
        parts = [f"{self.passed} passed", f"{self.failed} failed"]
        if self.skipped:
            parts.append(f"{self.skipped} skipped")
        if self.notes:
            parts.append(f"{self.notes} notes")
        print(f"\n{', '.join(parts)}")
        return 1 if self.failed else 0


class Target:
    """What a suite was pointed at, parsed from its connection-string argument.

    SQLite needs no running stack, so unlike the Trino driver's harness there is
    no environment file to read: the connection string carries everything, and
    `setup.sh` is what produced it. Both forms are accepted, because
    `run-tests.sh` runs the pyodbc suites once per connection style:

    - `Driver=/path/to/libstackable_odbc_sqlite.so;Database=/path/to/test.db`
    - `DSN=test_sqlite`

    `driver_path` and `database` are only recoverable from the first. A suite
    that loads the `.so` itself has to say so by calling `require_driver_path`,
    which fails loudly rather than letting a DSN run reach ctypes and crash on
    a `None` path.
    """

    def __init__(self, conn_str):
        self.conn_str_value = conn_str
        self._keys = {}
        for pair in conn_str.split(";"):
            key, sep, value = pair.partition("=")
            if sep:
                self._keys[key.strip().lower()] = value.strip()

    @classmethod
    def from_argv(cls, argv, usage):
        if len(argv) < 2:
            raise SystemExit(usage)
        return cls(argv[1])

    def get(self, key, default=None):
        return self._keys.get(key.lower(), default)

    @property
    def is_dsn(self):
        return "dsn" in self._keys and "driver" not in self._keys

    @property
    def driver_path(self):
        return self.get("driver")

    @property
    def database(self):
        return self.get("database")

    def require_driver_path(self):
        """The driver `.so`, for a suite that loads it directly.

        A `DSN=` connection string names a data source the Driver Manager
        resolves, so the library path is not in it. Exiting here beats letting
        `ctypes.CDLL(None)` load the running process and fail somewhere far
        less obvious.
        """
        path = self.driver_path
        if not path:
            raise SystemExit(
                "this suite loads the driver directly and needs a DSN-less "
                "connection string carrying Driver=<path to the .so>\n"
                f"got: {self.conn_str_value}"
            )
        if not os.path.exists(path):
            raise SystemExit(f"driver not found: {path}\nrun: cargo build")
        return path

    def conn_str(self, **overrides):
        """The connection string, with keys replaced or removed.

        An override of `None` drops the key, which is how a suite tests
        connecting with, say, no `Database` at all.
        """
        if not overrides:
            return self.conn_str_value
        merged = dict(self._keys)
        for key, value in overrides.items():
            merged[key.lower()] = value
        return ";".join(f"{k}={v}" for k, v in merged.items() if v is not None)

    def connect(self, **overrides):
        """Connect through the Driver Manager. pyodbc is imported here rather
        than at module scope so the ctypes suites keep their zero dependencies.
        """
        import pyodbc

        return pyodbc.connect(self.conn_str(**overrides), autocommit=True)
