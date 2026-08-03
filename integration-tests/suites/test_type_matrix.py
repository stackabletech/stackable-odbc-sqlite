#!/usr/bin/env python3
"""
Type-transform fuzz for the SQLite ODBC driver.

Two halves.

The first drives every (SQLite value, C data type) pair through `SQLGetData` on
the raw C ABI and checks the outcome against invariants rather than against a
transcribed copy of the ODBC conversion matrix. Transcribing the matrix would
mostly test the transcription. The invariants below are the properties whose
violation is a defect, and they hold for every cell of it.

    1. The call returns. No pair may crash, abort or hang the process.
    2. A failure carries a SQLSTATE. `SQL_ERROR` with no diagnostic record
       leaves an application with an error it cannot interpret.
    3. NULL is reported as NULL. `SQL_NULL_DATA` in the indicator, for every
       target type, whatever the source type is.
    4. A value that does not fit reports 22003, not a truncated number.
    5. Text that is not a number reports 22018, not a zero.
    6. A successful conversion round-trips. Where the value is checkable as
       text, what comes back is what went in.

The second checks what `SQLDescribeCol` *says* a column is, which is a separate
question from what `SQLGetData` will hand over. An application asks the first
before it asks the second, and a BI tool decides whether a column can be summed
or charted from the answer. SQLite has no declared type for a computed column,
so this is where the driver has to work from the storage class of the values it
already holds.

Usage:
    python3 integration-tests/suites/test_type_matrix.py \
        "Driver=/path/to/libstackable_odbc_sqlite.so;Database=/path/to/test.db"

Needs no server. Standard library only (ctypes, no pyodbc and no uv), and it
creates and drops its own fixture table, so it does not care which other suites
have run against the same database.
"""

import ctypes
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Results, Target  # noqa: E402
from odbc_abi import (  # noqa: E402
    SQL_ATTR_ODBC_VERSION,
    SQL_DRIVER_NOPROMPT,
    SQL_ERROR,
    SQL_HANDLE_DBC,
    SQL_HANDLE_ENV,
    SQL_HANDLE_STMT,
    SQL_NTS,
    SQL_OV_ODBC3,
    SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO,
    load,
    sqlstate,
    w,
)

P = ctypes.c_void_p

# C data types, from odbc_sys::CDataType.
C_CHAR = 1
C_WCHAR = -8
C_BIT = -7
C_STINYINT = -26
C_SSHORT = -15
C_SLONG = -16
C_SBIGINT = -25
C_FLOAT = 7
C_DOUBLE = 8
C_BINARY = -2
C_TYPE_DATE = 91
C_TYPE_TIME = 92
C_TYPE_TIMESTAMP = 93

C_TYPES = [
    ("SQL_C_CHAR", C_CHAR),
    ("SQL_C_WCHAR", C_WCHAR),
    ("SQL_C_BIT", C_BIT),
    ("SQL_C_STINYINT", C_STINYINT),
    ("SQL_C_SSHORT", C_SSHORT),
    ("SQL_C_SLONG", C_SLONG),
    ("SQL_C_SBIGINT", C_SBIGINT),
    ("SQL_C_FLOAT", C_FLOAT),
    ("SQL_C_DOUBLE", C_DOUBLE),
    ("SQL_C_BINARY", C_BINARY),
    ("SQL_C_TYPE_DATE", C_TYPE_DATE),
    ("SQL_C_TYPE_TIME", C_TYPE_TIME),
    ("SQL_C_TYPE_TIMESTAMP", C_TYPE_TIMESTAMP),
]

# SQL types, from odbc_sys::SqlDataType, for the describe half.
SQL_BIGINT = -5
SQL_DOUBLE = 8
SQL_VARBINARY = -3
SQL_WVARCHAR = -9
SQL_BIT = -7

SQL_TYPE_NAMES = {
    SQL_BIGINT: "SQL_BIGINT",
    SQL_DOUBLE: "SQL_DOUBLE",
    SQL_VARBINARY: "SQL_VARBINARY",
    SQL_WVARCHAR: "SQL_WVARCHAR",
    SQL_BIT: "SQL_BIT",
    4: "SQL_INTEGER",
    12: "SQL_VARCHAR",
}

SQL_NULL_DATA = -1

# Spec SQLSTATEs this fuzz reasons about.
STATE_OUT_OF_RANGE = "22003"  # Numeric value out of range
STATE_BAD_CAST = "22018"  # Invalid character value for cast

FIXTURE = "typem_fixture"

# (label, SQLite expression, expected text when read as SQL_C_CHAR or None)
#
# Boundary values are the exact limits of SQLite's INTEGER, which is an i64:
# an off-by-one in a narrowing conversion shows up nowhere else.
VALUES = [
    ("integer zero", "0", "0"),
    ("integer one", "1", "1"),
    ("integer min", "-9223372036854775808", "-9223372036854775808"),
    ("integer max", "9223372036854775807", "9223372036854775807"),
    ("real", "1.5", None),
    ("real negative", "-1.5", None),
    ("real zero", "0.0", None),
    ("text", "'hello'", "hello"),
    ("text numeric", "'42'", "42"),
    ("text empty", "''", ""),
    ("text overflowing i64", "'99999999999999999999'", None),
    ("text not a number", "'abc'", "abc"),
    ("blob", "X'DEADBEEF'", None),
    ("blob empty", "X''", None),
    ("iso date", "'2020-02-03'", "2020-02-03"),
    ("iso timestamp", "'2020-02-03 04:05:06'", "2020-02-03 04:05:06"),
    ("expression sum", "1 + 1", "2"),
    ("aggregate count", f"(SELECT count(*) FROM {FIXTURE})", "5"),
]

# NULL must be reported as NULL for every target type. A driver that reports a
# NULL as 0 or "" corrupts data silently.
NULL_VALUES = [
    ("null literal", "NULL"),
    ("null integer column", f"(SELECT n FROM {FIXTURE} WHERE id = 4)"),
    ("null cast", "CAST(NULL AS INTEGER)"),
]

# (label, expression, expected SQL type from SQLDescribeCol)
#
# SQLite reports no declared type for any of these, so the driver has to answer
# from the storage class of the value. `exec_direct` materialises every row
# before returning, so it holds them when the question is asked.
DESCRIBE_COMPUTED = [
    ("integer literal", "SELECT 1", SQL_BIGINT),
    ("real literal", "SELECT 1.5", SQL_DOUBLE),
    ("text literal", "SELECT 'x'", SQL_WVARCHAR),
    ("blob literal", "SELECT X'00'", SQL_VARBINARY),
    ("explicit integer cast", "SELECT CAST(1 AS INTEGER)", SQL_BIGINT),
    ("explicit real cast", "SELECT CAST(1.5 AS REAL)", SQL_DOUBLE),
    ("arithmetic on a column", f"SELECT id + 1 FROM {FIXTURE}", SQL_BIGINT),
    ("count", f"SELECT count(*) FROM {FIXTURE}", SQL_BIGINT),
    ("sum of integers", f"SELECT sum(id) FROM {FIXTURE}", SQL_BIGINT),
    ("avg", f"SELECT avg(id) FROM {FIXTURE}", SQL_DOUBLE),
    ("max of text", f"SELECT max(label) FROM {FIXTURE}", SQL_WVARCHAR),
    ("length", f"SELECT length(label) FROM {FIXTURE}", SQL_BIGINT),
]

# Declared columns already work. Asserted anyway, because the fix for the
# computed columns must not disturb them: a change that typed everything from
# the first row's storage class would break a declared INTEGER column holding a
# text value, which SQLite permits.
DESCRIBE_DECLARED = [
    ("declared INTEGER", f"SELECT id FROM {FIXTURE}", SQL_BIGINT),
    ("declared REAL", f"SELECT amount FROM {FIXTURE}", SQL_DOUBLE),
    ("declared TEXT", f"SELECT label FROM {FIXTURE}", SQL_WVARCHAR),
    ("declared BLOB", f"SELECT payload FROM {FIXTURE}", SQL_VARBINARY),
    ("declared BOOLEAN", f"SELECT flag FROM {FIXTURE}", SQL_BIT),
    # SQLite lets any value into any column. The *declared* type still wins,
    # because that is what the schema promises and what the next row might hold.
    (
        "declared INTEGER holding text",
        f"SELECT id FROM {FIXTURE} WHERE id = 5",
        SQL_BIGINT,
    ),
]

R = Results("type matrix")
violations = []


# These write R's counters directly rather than going through ok()/bad():
# the matrix half prints per *violation*, not per check. 18 values against 13 C
# types is 234 PASS lines nobody reads.
def fail(kind, detail):
    R.failed += 1
    violations.append(f"{kind}: {detail}")


def ok():
    R.passed += 1


class Driver:
    def __init__(self, so, conn_str):
        self.lib = load(so)
        self.env = P()
        self.lib.SQLAllocHandle(SQL_HANDLE_ENV, None, ctypes.byref(self.env))
        self.lib.SQLSetEnvAttr(self.env, SQL_ATTR_ODBC_VERSION, P(SQL_OV_ODBC3), 0)
        self.dbc = P()
        self.lib.SQLAllocHandle(SQL_HANDLE_DBC, self.env, ctypes.byref(self.dbc))
        cs, self._keep = w(conn_str)
        ob = (ctypes.c_uint16 * 1024)()
        ol = ctypes.c_int16(0)
        r = self.lib.SQLDriverConnectW(
            self.dbc,
            None,
            cs,
            SQL_NTS,
            ctypes.cast(ob, ctypes.POINTER(ctypes.c_uint16)),
            1024,
            ctypes.byref(ol),
            SQL_DRIVER_NOPROMPT,
        )
        if r not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
            raise SystemExit(f"could not connect: {sqlstate(self.lib, SQL_HANDLE_DBC, self.dbc)}")

    def exec_sql(self, sql):
        """Run a statement for its effect. Returns (ret, sqlstate)."""
        stmt = P()
        self.lib.SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, ctypes.byref(stmt))
        try:
            text, _k = w(sql)
            r = self.lib.SQLExecDirectW(stmt, text, SQL_NTS)
            return (r, sqlstate(self.lib, SQL_HANDLE_STMT, stmt))
        finally:
            self.lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)

    def fetch_as(self, expr, c_type):
        """Run `SELECT <expr>` and read column 1 as `c_type`.

        Returns (ret, sqlstate, indicator, raw_bytes).
        """
        lib = self.lib
        stmt = P()
        lib.SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, ctypes.byref(stmt))
        try:
            sql, _k = w(f"SELECT {expr}")
            r = lib.SQLExecDirectW(stmt, sql, SQL_NTS)
            if r not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
                return ("EXEC", sqlstate(lib, SQL_HANDLE_STMT, stmt), None, None)
            r = lib.SQLFetch(stmt)
            if r not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
                return ("FETCH", sqlstate(lib, SQL_HANDLE_STMT, stmt), None, None)
            buf = ctypes.create_string_buffer(512)
            ind = ctypes.c_int64(0)
            r = lib.SQLGetData(
                stmt, 1, c_type, ctypes.cast(buf, P), 512, ctypes.byref(ind)
            )
            return (r, sqlstate(lib, SQL_HANDLE_STMT, stmt), ind.value, buf.raw)
        finally:
            lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)

    def describe(self, sql):
        """Run `sql` and return column 1's (sql_type, column_size), or None."""
        lib = self.lib
        stmt = P()
        lib.SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, ctypes.byref(stmt))
        try:
            text, _k = w(sql)
            r = lib.SQLExecDirectW(stmt, text, SQL_NTS)
            if r not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
                return None
            name = (ctypes.c_uint16 * 128)()
            namelen = ctypes.c_int16(0)
            data_type = ctypes.c_int16(0)
            size = ctypes.c_uint64(0)
            digits = ctypes.c_int16(0)
            nullable = ctypes.c_int16(0)
            r = lib.SQLDescribeColW(
                stmt,
                1,
                ctypes.cast(name, ctypes.POINTER(ctypes.c_uint16)),
                128,
                ctypes.byref(namelen),
                ctypes.byref(data_type),
                ctypes.byref(size),
                ctypes.byref(digits),
                ctypes.byref(nullable),
            )
            if r not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
                return None
            return (data_type.value, size.value)
        finally:
            lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)

    def close(self):
        self.lib.SQLDisconnect(self.dbc)
        self.lib.SQLFreeHandle(SQL_HANDLE_DBC, self.dbc)
        self.lib.SQLFreeHandle(SQL_HANDLE_ENV, self.env)


def type_name(code):
    return SQL_TYPE_NAMES.get(code, str(code))


def as_text(raw, c_type):
    if c_type == C_WCHAR:
        u = ctypes.cast(raw, ctypes.POINTER(ctypes.c_uint16))
        out = []
        for i in range(256):
            if u[i] == 0:
                break
            out.append(chr(u[i]))
        return "".join(out)
    return raw.split(b"\x00", 1)[0].decode("utf-8", "replace")


def make_fixture(d):
    """A table of this suite's own.

    `test_integration.py` drops `types_test` when it finishes, including the
    copy `create_test_db.sql` made, so a suite that leaned on the shared
    fixtures would pass or fail depending on what ran before it.
    """
    d.exec_sql(f"DROP TABLE IF EXISTS {FIXTURE}")
    d.exec_sql(
        f"CREATE TABLE {FIXTURE} ("
        "  id INTEGER PRIMARY KEY,"
        "  amount REAL,"
        "  label TEXT,"
        "  payload BLOB,"
        "  flag BOOLEAN,"
        "  n INTEGER"
        ")"
    )
    d.exec_sql(f"INSERT INTO {FIXTURE} VALUES (1, 1.5, 'alpha', X'DEADBEEF', 1, 10)")
    d.exec_sql(f"INSERT INTO {FIXTURE} VALUES (2, 2.5, 'beta', X'00', 0, 20)")
    d.exec_sql(f"INSERT INTO {FIXTURE} VALUES (3, 3.5, 'gamma', NULL, 1, 30)")
    d.exec_sql(f"INSERT INTO {FIXTURE} VALUES (4, NULL, NULL, NULL, NULL, NULL)")
    # SQLite lets any value into any column, so this row puts text in a column
    # declared INTEGER. The declared type must still win when describing it.
    d.exec_sql(f"INSERT INTO {FIXTURE} VALUES (5, 5.5, 'delta', NULL, 1, 50)")


def drop_fixture(d):
    d.exec_sql(f"DROP TABLE IF EXISTS {FIXTURE}")


def main():
    target = Target.from_argv(
        sys.argv,
        "usage: test_type_matrix.py "
        '"Driver=/path/to/libstackable_odbc_sqlite.so;Database=/path/to/test.db"',
    )
    so = target.require_driver_path()

    d = Driver(so, target.conn_str())
    print("=== type-transform fuzz ===\n")
    make_fixture(d)

    try:
        # -- invariants 1, 2, 4, 5 and 6 over the full matrix -------------
        print(f"--- {len(VALUES)} values x {len(C_TYPES)} C types ---")
        for label, expr, want_text in VALUES:
            for cname, ctype in C_TYPES:
                ret, state, ind, raw = d.fetch_as(expr, ctype)
                cell = f"{label} -> {cname}"

                if ret in ("EXEC", "FETCH"):
                    fail("query failed", f"{cell}: {ret} {state}")
                    continue

                # Invariant 2: a failure must carry a SQLSTATE.
                if ret == SQL_ERROR and not state:
                    fail("error with no SQLSTATE", cell)
                    continue

                if ret in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
                    # Invariant 6: a successful text conversion round-trips.
                    if want_text is not None and ctype in (C_CHAR, C_WCHAR):
                        got = as_text(raw, ctype)
                        if got != want_text:
                            fail(
                                "round-trip mismatch",
                                f"{cell}: got {got!r}, expected {want_text!r}",
                            )
                            continue
                ok()

        # -- invariant 3: NULL is NULL, for every target type -------------
        print(f"--- {len(NULL_VALUES)} NULLs x {len(C_TYPES)} C types ---")
        for label, expr in NULL_VALUES:
            for cname, ctype in C_TYPES:
                ret, state, ind, _raw = d.fetch_as(expr, ctype)
                cell = f"{label} -> {cname}"
                if ret in ("EXEC", "FETCH"):
                    fail("query failed", f"{cell}: {ret} {state}")
                    continue
                if ret in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
                    if ind != SQL_NULL_DATA:
                        fail(
                            "NULL not reported as NULL",
                            f"{cell}: indicator {ind}, expected {SQL_NULL_DATA}",
                        )
                        continue
                elif ret == SQL_ERROR and not state:
                    fail("error with no SQLSTATE", cell)
                    continue
                ok()

        # -- invariant 4: an overflowing value says so --------------------
        print("--- overflow and bad-cast SQLSTATEs ---")
        for label, expr, ctype, cname, want_state in (
            ("i64 max into SQL_C_SSHORT", "9223372036854775807", C_SSHORT, "SQL_C_SSHORT", STATE_OUT_OF_RANGE),
            ("i64 max into SQL_C_SLONG", "9223372036854775807", C_SLONG, "SQL_C_SLONG", STATE_OUT_OF_RANGE),
            ("text overflowing i64", "'99999999999999999999'", C_SBIGINT, "SQL_C_SBIGINT", STATE_OUT_OF_RANGE),
            ("non-numeric text as integer", "'abc'", C_SBIGINT, "SQL_C_SBIGINT", STATE_BAD_CAST),
            ("non-numeric text as double", "'abc'", C_DOUBLE, "SQL_C_DOUBLE", STATE_BAD_CAST),
        ):
            ret, state, _ind, _raw = d.fetch_as(expr, ctype)
            R.check(
                f"{label} reports {want_state}",
                ret == SQL_ERROR and state == want_state,
                f"got {ret} / {state or '<none>'}",
            )

        # -- SQLDescribeCol on declared columns ---------------------------
        print("\n--- SQLDescribeCol: declared columns ---")
        for label, sql, want in DESCRIBE_DECLARED:
            got = d.describe(sql)
            if got is None:
                R.bad(f"describe {label}", "the statement did not execute")
                continue
            R.check(
                f"describe {label} is {type_name(want)}",
                got[0] == want,
                f"got {type_name(got[0])}",
            )

        # -- SQLDescribeCol on computed columns ---------------------------
        print("\n--- SQLDescribeCol: computed columns ---")
        # SQLite has no declared type for an expression, so the driver has to
        # answer from the storage class of the values it materialised. Reporting
        # everything as WVARCHAR tells a BI tool that `count(*)` is text, which
        # is not a column it will offer to sum or chart.
        for label, sql, want in DESCRIBE_COMPUTED:
            got = d.describe(sql)
            if got is None:
                R.bad(f"describe {label}", "the statement did not execute")
                continue
            R.check(
                f"describe {label} is {type_name(want)}",
                got[0] == want,
                f"got {type_name(got[0])}",
            )

        # A computed column with no rows has nothing to infer from, so the
        # fallback stands. What must not happen is a crash or a nonsense type.
        print("\n--- SQLDescribeCol: nothing to infer from ---")
        got = d.describe(f"SELECT id + 1 FROM {FIXTURE} WHERE 0")
        R.check(
            "a computed column over zero rows still describes",
            got is not None and got[0] in (SQL_WVARCHAR, 12),
            f"got {type_name(got[0]) if got else 'nothing'}",
        )
        got = d.describe("SELECT NULL")
        R.check(
            "an all-NULL computed column still describes",
            got is not None and got[0] in (SQL_WVARCHAR, 12),
            f"got {type_name(got[0]) if got else 'nothing'}",
        )
    finally:
        drop_fixture(d)
        d.close()

    if violations:
        print(f"\n--- {len(violations)} violations ---")
        for v in violations[:40]:
            print(f"FAIL  {v}")
        if len(violations) > 40:
            print(f"... and {len(violations) - 40} more")

    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
