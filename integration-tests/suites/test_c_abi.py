#!/usr/bin/env python3
"""
Raw C ABI pen test for the SQLite ODBC driver.

Loads the driver's shared object with ctypes and calls its exported entry
points directly, with **no Driver Manager in the loop**. unixODBC intercepts a
large part of the ODBC state machine and answers it itself, so a driver's own
handling of an out-of-order or malformed call is invisible to any test that
goes through pyodbc or isql. Everything asserted here is the driver's own
behaviour.

That also means the spec's **(DM)** diagnostics must not be expected. Where the
spec attributes a SQLSTATE to the Driver Manager, nothing produces it here, and
a probe that demanded it would be asserting the absence of a component rather
than the presence of a behaviour. Those probes assert what the driver does,
with a comment naming the (DM) diagnostic they do not demand.

Covers: handle lifecycle and parentage, invalid and stale handles, double free,
use after free, connection state, cursor state, prepare / execute / re-execute,
SQLFreeStmt options, statement and connection attribute round-trips, the
enforced query timeout, transactions including DDL, and the catalog functions
SQLite answers with no rows.

Usage:
    python3 integration-tests/suites/test_c_abi.py \
        "Driver=/path/to/libstackable_odbc_sqlite.so;Database=/path/to/test.db"

Needs no server and no setup beyond a built driver: if the database file does
not exist, SQLite creates it. Only the Python standard library is used (ctypes,
not pyodbc).
"""

import ctypes
import os
import sys

# --- ODBC constants -------------------------------------------------------
# Named rather than inlined, per the project's own rule about spec values.

# The handle types, return codes and DriverCompletion values live in odbc_abi,
# which is imported below.

# SQLFreeStmt options
SQL_CLOSE = 0
SQL_DROP = 1
SQL_UNBIND = 2
SQL_RESET_PARAMS = 3

# Statement attributes
SQL_ATTR_QUERY_TIMEOUT = 0
SQL_ATTR_MAX_ROWS = 1
SQL_ATTR_NOSCAN = 2
SQL_ATTR_MAX_LENGTH = 3
SQL_ATTR_ASYNC_ENABLE = 4
SQL_ATTR_CURSOR_TYPE = 6
SQL_ATTR_CONCURRENCY = 7
SQL_ATTR_KEYSET_SIZE = 8
SQL_ATTR_SIMULATE_CURSOR = 10
SQL_ATTR_RETRIEVE_DATA = 11
SQL_ATTR_USE_BOOKMARKS = 12
SQL_ATTR_PARAM_STATUS_PTR = 20
SQL_ATTR_PARAMS_PROCESSED_PTR = 21
SQL_ATTR_PARAMSET_SIZE = 22
SQL_ATTR_ROW_ARRAY_SIZE = 27
SQL_ATTR_CURSOR_SCROLLABLE = -1
SQL_ATTR_CURSOR_SENSITIVITY = -2
SQL_ATTR_METADATA_ID = 10014
SQL_CURSOR_FORWARD_ONLY = 0
SQL_CURSOR_STATIC = 3

# Values the driver substitutes to.
SQL_CONCUR_READ_ONLY = 1
SQL_SC_NON_UNIQUE = 0
SQL_NONSCROLLABLE = 0
SQL_ASYNC_ENABLE_OFF = 0

# Connection attributes
SQL_ATTR_AUTOCOMMIT = 102
SQL_ATTR_TXN_ISOLATION = 108
SQL_ATTR_CURRENT_CATALOG = 109
SQL_AUTOCOMMIT_OFF = 0
SQL_AUTOCOMMIT_ON = 1

# Isolation levels: SQLite implements exactly one, and refuses the rest.
SQL_TXN_READ_UNCOMMITTED = 1
SQL_TXN_READ_COMMITTED = 2
SQL_TXN_REPEATABLE_READ = 4
SQL_TXN_SERIALIZABLE = 8

# SQLEndTran completion types
SQL_COMMIT = 0
SQL_ROLLBACK = 1

# Info types read back beside the attributes that mirror them.
SQL_DATABASE_NAME = 16
SQL_DBMS_NAME = 17
SQL_TXN_CAPABLE = 46
SQL_TC_ALL = 2

SQL_C_CHAR = 1
SQL_C_SBIGINT = -25
SQL_BIGINT = -5

# SQLBindParameter arguments used by the bound-parameter probes.
SQL_PARAM_INPUT = 1

# SQLSpecialColumns / SQLStatistics arguments.
SQL_BEST_ROWID = 1
SQL_SCOPE_CURROW = 0
SQL_NULLABLE = 1
SQL_INDEX_ALL = 1
SQL_QUICK = 0

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results, Target  # noqa: E402
from odbc_abi import (  # noqa: E402
    SQL_ATTR_ODBC_VERSION,
    SQL_DRIVER_NOPROMPT,
    SQL_ERROR,
    SQL_HANDLE_DBC,
    SQL_HANDLE_ENV,
    SQL_HANDLE_STMT,
    SQL_INVALID_HANDLE,
    SQL_NO_DATA,
    SQL_NTS,
    SQL_OV_ODBC3,
    SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO,
    load,
    read_wide_info,
    sqlstate,
    w,
)

R = Results("raw C ABI")


RET_NAMES = {
    SQL_SUCCESS: "SUCCESS",
    SQL_SUCCESS_WITH_INFO: "SUCCESS_WITH_INFO",
    SQL_NO_DATA: "NO_DATA",
    SQL_ERROR: "ERROR",
    SQL_INVALID_HANDLE: "INVALID_HANDLE",
}


def rname(r):
    return RET_NAMES.get(r, str(r))


def check(label, got, want, state=None, got_state=None):
    """Assert a return code, and optionally the SQLSTATE that came with it.

    Kept here rather than in the harness: it speaks in ODBC return codes and
    SQLSTATEs, which is this suite's vocabulary, not generic machinery.
    """
    want_list = want if isinstance(want, (list, tuple)) else [want]
    ok = got in want_list
    detail = ""
    if ok and state is not None:
        ok = got_state == state
        detail = f" (SQLSTATE {got_state or '<none>'}, expected {state})"
    elif got_state:
        detail = f" (SQLSTATE {got_state})"
    if ok:
        R.ok(f"{label}: {rname(got)}{detail}")
    else:
        expect = "/".join(rname(x) for x in want_list)
        R.bad(f"{label}: got {rname(got)}{detail}, expected {expect}")


def note(label, text):
    """An observation the driver is entitled to make either way."""
    R.note(label, text)


def column_count(lib, stmt):
    """`SQLNumResultCols` as a plain int, or -1 if the call failed."""
    cols = ctypes.c_int16(-1)
    if lib.SQLNumResultCols(stmt, ctypes.byref(cols)) != SQL_SUCCESS:
        return -1
    return cols.value


def row_count(lib, stmt):
    """How many rows a result set yields, consuming it."""
    n = 0
    while lib.SQLFetch(stmt) == SQL_SUCCESS:
        n += 1
    return n


def scalar_i64(lib, stmt, sql):
    """Run `sql` and read column 1 of the first row as an integer."""
    text, _keep = w(sql)
    if lib.SQLExecDirectW(stmt, text, SQL_NTS) not in (
        SQL_SUCCESS,
        SQL_SUCCESS_WITH_INFO,
    ):
        return None
    value = ctypes.c_int64(0)
    ind = ctypes.c_int64(0)
    got = None
    if lib.SQLFetch(stmt) == SQL_SUCCESS:
        if (
            lib.SQLGetData(
                stmt,
                1,
                SQL_C_SBIGINT,
                ctypes.cast(ctypes.byref(value), ctypes.c_void_p),
                8,
                ctypes.byref(ind),
            )
            == SQL_SUCCESS
        ):
            got = value.value
    lib.SQLCloseCursor(stmt)
    return got


def main():
    target = Target.from_argv(
        sys.argv,
        "usage: test_c_abi.py "
        '"Driver=/path/to/libstackable_odbc_sqlite.so;Database=/path/to/test.db"',
    )
    so = target.require_driver_path()
    conn_str = target.conn_str()

    lib = load(so)
    P = ctypes.c_void_p

    print(f"=== raw C ABI pen test (no Driver Manager) ===\ndriver: {so}\n")

    # ---------------------------------------------------------------
    print("--- handle lifecycle ---")
    env = P()
    r = lib.SQLAllocHandle(SQL_HANDLE_ENV, None, ctypes.byref(env))
    check("alloc env", r, SQL_SUCCESS)

    r = lib.SQLSetEnvAttr(env, SQL_ATTR_ODBC_VERSION, P(SQL_OV_ODBC3), 0)
    check("set ODBC version 3", r, SQL_SUCCESS)

    dbc = P()
    r = lib.SQLAllocHandle(SQL_HANDLE_DBC, env, ctypes.byref(dbc))
    check("alloc connection", r, SQL_SUCCESS)

    # The env still owns a connection, so it must refuse to be freed.
    r = lib.SQLFreeHandle(SQL_HANDLE_ENV, env)
    check(
        "free env with a live connection",
        r,
        SQL_ERROR,
        state="HY010",
        got_state=sqlstate(lib, SQL_HANDLE_ENV, env),
    )

    # ---------------------------------------------------------------
    print("\n--- invalid and mismatched handles ---")
    bogus = P(0xDEADBEEF)
    out = P()
    r = lib.SQLAllocHandle(SQL_HANDLE_DBC, bogus, ctypes.byref(out))
    check("alloc connection on a non-handle parent", r, SQL_INVALID_HANDLE)

    r = lib.SQLFreeHandle(SQL_HANDLE_ENV, None)
    check("free a null handle", r, SQL_INVALID_HANDLE)

    r = lib.SQLFreeHandle(SQL_HANDLE_STMT, env)
    check("free an env under the wrong handle type", r, SQL_INVALID_HANDLE)

    r = lib.SQLAllocHandle(SQL_HANDLE_STMT, env, ctypes.byref(out))
    check("alloc statement parented on an env", r, SQL_INVALID_HANDLE)

    # ---------------------------------------------------------------
    print("\n--- statement on an unconnected connection ---")
    stmt0 = P()
    r = lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(stmt0))
    # The spec's 08003 for this is (DM)-owned, so the driver is entitled to
    # allocate: a statement on a not-yet-open connection is legal here.
    note("alloc statement before connecting", f"{rname(r)} (08003 here is DM-owned)")
    if r == SQL_SUCCESS:
        sql, _keep = w("SELECT 1")
        r = lib.SQLExecDirectW(stmt0, sql, SQL_NTS)
        # HY010, not 08003: SQLExecDirect's 08003 is (DM)-annotated, so with no
        # Driver Manager loaded nothing produces it, and the driver reports the
        # sequence error instead.
        check(
            "execute on an unconnected connection",
            r,
            SQL_ERROR,
            state="HY010",
            got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt0),
        )
        lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt0)

    r = lib.SQLDisconnect(dbc)
    check(
        "disconnect while not connected",
        r,
        SQL_ERROR,
        state="08003",
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )

    # ---------------------------------------------------------------
    print("\n--- connect ---")
    cs, _keep_cs = w(conn_str)
    outbuf = (ctypes.c_uint16 * 1024)()
    outlen = ctypes.c_int16(0)
    r = lib.SQLDriverConnectW(
        dbc,
        None,
        cs,
        SQL_NTS,
        ctypes.cast(outbuf, ctypes.POINTER(ctypes.c_uint16)),
        1024,
        ctypes.byref(outlen),
        SQL_DRIVER_NOPROMPT,
    )
    check(
        "SQLDriverConnectW",
        r,
        [SQL_SUCCESS, SQL_SUCCESS_WITH_INFO],
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )
    if r not in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
        print("\ncannot continue without a connection")
        return 1

    cs2, _keep_cs2 = w(conn_str)
    r = lib.SQLDriverConnectW(
        dbc,
        None,
        cs2,
        SQL_NTS,
        ctypes.cast(outbuf, ctypes.POINTER(ctypes.c_uint16)),
        1024,
        ctypes.byref(outlen),
        SQL_DRIVER_NOPROMPT,
    )
    check(
        "connect on an already-connected handle",
        r,
        SQL_ERROR,
        state="08002",
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )

    stmt = P()
    r = lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(stmt))
    check("alloc statement", r, SQL_SUCCESS)

    # Dirty the connection's diagnostic queue immediately before the free, with
    # no call in between. SQLite implements serializable and nothing else, so
    # asking for READ COMMITTED is refused with HY024 and leaves it as record 1.
    #
    # It has to be immediately before. The 08002 from the failed second connect
    # above is already gone by here, because SQLAllocHandle clears at entry too
    # and the statement allocation sits between the two.
    r = lib.SQLSetConnectAttrW(dbc, SQL_ATTR_TXN_ISOLATION, P(SQL_TXN_READ_COMMITTED), 0)
    check(
        "set an isolation level SQLite does not implement",
        r,
        SQL_ERROR,
        state="HY024",
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )

    # A function clears the handle's diagnostics at entry, so the HY010 this
    # posts must be record 1 rather than sitting behind the HY024 above. An
    # application reading the first record after a failed free would otherwise
    # act on the previous call's SQLSTATE.
    r = lib.SQLFreeHandle(SQL_HANDLE_DBC, dbc)
    check(
        "free connection while still connected, over a dirty queue",
        r,
        SQL_ERROR,
        state="HY010",
        got_state=sqlstate(lib, SQL_HANDLE_DBC, dbc),
    )

    # The level SQLite does implement is accepted, which is the other half of
    # the HY024 above: a driver that refused everything would pass that probe
    # while offering no isolation at all.
    r = lib.SQLSetConnectAttrW(dbc, SQL_ATTR_TXN_ISOLATION, P(SQL_TXN_SERIALIZABLE), 0)
    check("set SQL_TXN_SERIALIZABLE, the level SQLite implements", r, SQL_SUCCESS)

    # ---------------------------------------------------------------
    print("\n--- cursor state with no cursor ---")
    # HY010, not 24000. 24000 is for a statement that *was* executed but has no
    # result set, HY010 for one never put in an executed state. This statement
    # is the latter.
    r = lib.SQLFetch(stmt)
    check(
        "fetch on a never-executed statement",
        r,
        SQL_ERROR,
        state="HY010",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    ind = ctypes.c_int64(0)
    buf = ctypes.create_string_buffer(64)
    r = lib.SQLGetData(stmt, 1, SQL_C_CHAR, ctypes.cast(buf, P), 64, ctypes.byref(ind))
    check(
        "get_data with no cursor",
        r,
        SQL_ERROR,
        state="24000",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    r = lib.SQLCloseCursor(stmt)
    check(
        "close_cursor with no cursor",
        r,
        SQL_ERROR,
        state="24000",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    r = lib.SQLExecute(stmt)
    check(
        "execute with nothing prepared",
        r,
        SQL_ERROR,
        state="HY010",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    cols = ctypes.c_int16(-1)
    r = lib.SQLNumResultCols(stmt, ctypes.byref(cols))
    check(
        "num_result_cols before execute",
        r,
        SQL_ERROR,
        state="HY010",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    # ---------------------------------------------------------------
    print("\n--- prepare / execute / re-execute ---")
    sql, _k1 = w("SELECT 1 AS n")
    r = lib.SQLPrepareW(stmt, sql, SQL_NTS)
    check("prepare", r, SQL_SUCCESS)

    # SQL_ATTR_CURSOR_TYPE may not be set once a statement is prepared.
    r = lib.SQLSetStmtAttrW(stmt, SQL_ATTR_CURSOR_TYPE, P(SQL_CURSOR_STATIC), 0)
    check(
        "set cursor type after prepare",
        r,
        SQL_ERROR,
        state="HY011",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    for attempt in (1, 2):
        r = lib.SQLExecute(stmt)
        check(f"execute (attempt {attempt})", r, SQL_SUCCESS)
        r = lib.SQLNumResultCols(stmt, ctypes.byref(cols))
        check(f"num_result_cols after execute ({attempt})", r, SQL_SUCCESS)
        if cols.value != 1:
            note(f"num_result_cols after execute ({attempt})", f"got {cols.value}")
        r = lib.SQLFetch(stmt)
        check(f"fetch row ({attempt})", r, SQL_SUCCESS)
        r = lib.SQLFetch(stmt)
        check(f"fetch past the last row ({attempt})", r, SQL_NO_DATA)
        r = lib.SQLCloseCursor(stmt)
        check(f"close cursor ({attempt})", r, SQL_SUCCESS)

    # ---------------------------------------------------------------
    print("\n--- SQLFreeStmt options ---")
    sql, _k2 = w("SELECT 1 AS n")
    lib.SQLExecDirectW(stmt, sql, SQL_NTS)
    r = lib.SQLFreeStmt(stmt, SQL_CLOSE)
    check("free_stmt SQL_CLOSE with an open cursor", r, SQL_SUCCESS)
    r = lib.SQLFreeStmt(stmt, SQL_CLOSE)
    check("free_stmt SQL_CLOSE with no cursor", r, SQL_SUCCESS)
    r = lib.SQLFreeStmt(stmt, SQL_UNBIND)
    check("free_stmt SQL_UNBIND", r, SQL_SUCCESS)
    r = lib.SQLFreeStmt(stmt, SQL_RESET_PARAMS)
    check("free_stmt SQL_RESET_PARAMS", r, SQL_SUCCESS)
    # The SQLSTATE matters as much as the return code: an SQL_ERROR carrying no
    # diagnostic record leaves an application with an error it cannot interpret.
    r = lib.SQLFreeStmt(stmt, 99)
    check(
        "free_stmt with an undefined option",
        r,
        SQL_ERROR,
        state="HY092",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, stmt),
    )

    # ---------------------------------------------------------------
    print("\n--- statement attributes: substituted values (01S02) ---")
    # The spec's 01S02 row closes the set of statement attributes a driver may
    # substitute for. For each, the driver must store the value it will use,
    # which is what makes the row's parenthesis true: "(SQLGetStmtAttr can be
    # called to determine the temporarily substituted value.)". The read-back is
    # asserted, not merely observed. A driver that kept the requested value
    # would be claiming a block cursor it does not implement, and an application
    # reading its own number back has no way to tell.
    #
    # SQL_ATTR_QUERY_TIMEOUT is absent from this list because the driver
    # enforces it; it is checked on its own below.
    #
    # SQLULEN is 64-bit here, so the read-back buffer is too, and it is zeroed
    # before each read.
    #
    # A *fresh* statement, not the shared one: SQL_ATTR_CONCURRENCY,
    # SQL_ATTR_CURSOR_TYPE, SQL_ATTR_SIMULATE_CURSOR and SQL_ATTR_USE_BOOKMARKS
    # "must be set before the statement is executed", and the shared handle has
    # been prepared and executed by now.
    val = ctypes.c_uint64(0)
    outlen32 = ctypes.c_int32(0)
    attr_stmt = ctypes.c_void_p()
    r = lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(attr_stmt))
    check("allocate a fresh statement for the attribute probes", r, SQL_SUCCESS)
    for label, attr, asked, substituted in (
        ("SQL_ATTR_CONCURRENCY", SQL_ATTR_CONCURRENCY, 2, SQL_CONCUR_READ_ONLY),
        (
            "SQL_ATTR_CURSOR_TYPE",
            SQL_ATTR_CURSOR_TYPE,
            SQL_CURSOR_STATIC,
            SQL_CURSOR_FORWARD_ONLY,
        ),
        ("SQL_ATTR_KEYSET_SIZE", SQL_ATTR_KEYSET_SIZE, 50, 0),
        ("SQL_ATTR_MAX_LENGTH", SQL_ATTR_MAX_LENGTH, 4096, 0),
        ("SQL_ATTR_MAX_ROWS", SQL_ATTR_MAX_ROWS, 100, 0),
        # No block cursors: core pins the rowset at 1, which is why the driver
        # reports rows one at a time.
        ("SQL_ATTR_ROW_ARRAY_SIZE", SQL_ATTR_ROW_ARRAY_SIZE, 10, 1),
        ("SQL_ATTR_SIMULATE_CURSOR", SQL_ATTR_SIMULATE_CURSOR, 2, SQL_SC_NON_UNIQUE),
        # Two deviations from the spec's closed list, documented in core.
        # Substituting keeps SQL_ATTR_CURSOR_SCROLLABLE consistent with
        # SQL_ATTR_CURSOR_TYPE. For SQL_ATTR_PARAMSET_SIZE, refusing would fail
        # a call every parameter-array-capable tool makes, while accepting it
        # verbatim would silently drop every set past the first.
        ("SQL_ATTR_CURSOR_SCROLLABLE", SQL_ATTR_CURSOR_SCROLLABLE, 1, SQL_NONSCROLLABLE),
        ("SQL_ATTR_PARAMSET_SIZE", SQL_ATTR_PARAMSET_SIZE, 500, 1),
    ):
        r = lib.SQLSetStmtAttrW(attr_stmt, attr, P(asked), 0)
        check(
            f"set {label}={asked} (unsupported)",
            r,
            SQL_SUCCESS_WITH_INFO,
            state="01S02",
            got_state=sqlstate(lib, SQL_HANDLE_STMT, attr_stmt),
        )
        val.value = 0
        r = lib.SQLGetStmtAttrW(
            attr_stmt, attr, ctypes.byref(val), 8, ctypes.byref(outlen32)
        )
        check(f"get {label}", r, SQL_SUCCESS)
        if r == SQL_SUCCESS:
            check(
                f"{label} reads back the substituted value",
                SQL_SUCCESS if val.value == substituted else SQL_ERROR,
                SQL_SUCCESS,
                got_state=f"read {val.value}, expected {substituted}",
            )

    # ---------------------------------------------------------------
    print("\n--- SQL_ATTR_QUERY_TIMEOUT is enforced, not substituted ---")
    # The one attribute on the 01S02 list this driver honours. Core arms its own
    # timer and calls Backend::cancel, which reaches sqlite3_interrupt, so
    # SQLGetStmtAttr reporting 42 rather than 0 is how an application learns the
    # deadline is really in force.
    #
    # Core arms it from a timer thread inside the .so, so no Driver Manager
    # threading policy can serialise it: this suite exercises the same path a
    # unixODBC client gets.
    r = lib.SQLSetStmtAttrW(attr_stmt, SQL_ATTR_QUERY_TIMEOUT, P(42), 0)
    check(
        "set SQL_ATTR_QUERY_TIMEOUT=42 (enforced, not substituted)",
        r,
        SQL_SUCCESS,
        got_state=sqlstate(lib, SQL_HANDLE_STMT, attr_stmt),
    )
    val.value = 0
    r = lib.SQLGetStmtAttrW(
        attr_stmt, SQL_ATTR_QUERY_TIMEOUT, ctypes.byref(val), 8, ctypes.byref(outlen32)
    )
    check("get SQL_ATTR_QUERY_TIMEOUT", r, SQL_SUCCESS)
    check(
        "SQL_ATTR_QUERY_TIMEOUT reads back what was asked for",
        SQL_SUCCESS if val.value == 42 else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"read {val.value}, expected 42",
    )

    # And it actually fires. A recursive CTE counts far enough to outlast a
    # one-second deadline without allocating anything, so the work sits in
    # SQLite's step loop, which is exactly where sqlite3_interrupt lands.
    #
    # HYT00, not HY008: core marks its own cancel state timed out before
    # cancelling and relabels the failure ahead of the HY008 reclassification,
    # so the more specific timeout wins over the cancel.
    timeout_stmt = P()
    r = lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(timeout_stmt))
    check("allocate a statement for the query-timeout probe", r, SQL_SUCCESS)
    r = lib.SQLSetStmtAttrW(timeout_stmt, SQL_ATTR_QUERY_TIMEOUT, P(1), 0)
    check("set a 1-second deadline", r, SQL_SUCCESS)
    slow, _k_slow = w(
        "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c "
        "WHERE i < 900000000) SELECT count(*) FROM c"
    )
    r = lib.SQLExecDirectW(timeout_stmt, slow, SQL_NTS)
    check(
        "a query that outruns its deadline is stopped",
        r,
        SQL_ERROR,
        state="HYT00",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, timeout_stmt),
    )
    # The spec requires a cancelled statement to stay usable: "After the
    # statement has been canceled, the application can call SQLExecute or
    # SQLExecDirect again." A driver whose cancel flag stuck would fail here.
    r = lib.SQLSetStmtAttrW(timeout_stmt, SQL_ATTR_QUERY_TIMEOUT, P(0), 0)
    check("clear the deadline", r, SQL_SUCCESS)
    check(
        "the timed-out statement runs again",
        SQL_SUCCESS if scalar_i64(lib, timeout_stmt, "SELECT 7") == 7 else SQL_ERROR,
        SQL_SUCCESS,
    )
    r = lib.SQLFreeHandle(SQL_HANDLE_STMT, timeout_stmt)
    check("free the timed-out statement", r, SQL_SUCCESS)

    # ---------------------------------------------------------------
    print("\n--- connection attributes ---")
    r = lib.SQLGetConnectAttrW(
        dbc, SQL_ATTR_AUTOCOMMIT, ctypes.byref(val), 8, ctypes.byref(outlen32)
    )
    check("get SQL_ATTR_AUTOCOMMIT", r, SQL_SUCCESS)
    check(
        "autocommit defaults to on",
        SQL_SUCCESS if val.value == SQL_AUTOCOMMIT_ON else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"read {val.value}",
    )

    # SQLite has no catalogs, so there is no current one to report. Either
    # answer is defensible: an empty string, or a refusal. What would be wrong
    # is naming a catalog the driver also says does not exist.
    catalog_buf = (ctypes.c_uint16 * 256)()
    r = lib.SQLGetConnectAttrW(
        dbc,
        SQL_ATTR_CURRENT_CATALOG,
        ctypes.cast(catalog_buf, P),
        512,
        ctypes.byref(outlen32),
    )
    if r in (SQL_SUCCESS, SQL_SUCCESS_WITH_INFO):
        got = "".join(chr(c) for c in catalog_buf[: max(outlen32.value, 0) // 2])
        check(
            "SQL_ATTR_CURRENT_CATALOG names no catalog",
            SQL_SUCCESS if got == "" else SQL_ERROR,
            SQL_SUCCESS,
            got_state=f"read {got!r}",
        )
    else:
        note(
            "get SQL_ATTR_CURRENT_CATALOG",
            f"{rname(r)} ({sqlstate(lib, SQL_HANDLE_DBC, dbc)}); "
            "SQLite has no catalogs, so refusing is a fair answer",
        )

    # ---------------------------------------------------------------
    print("\n--- transactions, including DDL ---")
    # SQL_TXN_CAPABLE is SQL_TC_ALL, which the spec defines as "Transactions
    # support both DML and DDL statements in any order". This is that claim
    # exercised through the C ABI rather than only asserted in a unit test: a
    # CREATE TABLE between two inserts, then a rollback that undoes all three.
    info = ctypes.c_uint16(0)
    length = ctypes.c_int16(0)
    r = lib.SQLGetInfoW(
        dbc, SQL_TXN_CAPABLE, ctypes.byref(info), 2, ctypes.byref(length)
    )
    check("get SQL_TXN_CAPABLE", r, SQL_SUCCESS)
    check(
        "SQL_TXN_CAPABLE is SQL_TC_ALL",
        SQL_SUCCESS if info.value == SQL_TC_ALL else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"read {info.value}, expected {SQL_TC_ALL}",
    )

    txn = P()
    r = lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(txn))
    check("allocate a statement for the transaction probe", r, SQL_SUCCESS)

    setup_sql, _k_setup = w("CREATE TABLE IF NOT EXISTS abi_txn (id INTEGER)")
    lib.SQLExecDirectW(txn, setup_sql, SQL_NTS)
    clear_sql, _k_clear = w("DELETE FROM abi_txn")
    lib.SQLExecDirectW(txn, clear_sql, SQL_NTS)

    r = lib.SQLSetConnectAttrW(dbc, SQL_ATTR_AUTOCOMMIT, P(SQL_AUTOCOMMIT_OFF), 0)
    check("turn autocommit off", r, SQL_SUCCESS)

    for sql_text in (
        "INSERT INTO abi_txn VALUES (1)",
        "CREATE TABLE abi_mid (x TEXT)",
        "INSERT INTO abi_txn VALUES (2)",
    ):
        text, _keep = w(sql_text)
        r = lib.SQLExecDirectW(txn, text, SQL_NTS)
        check(
            f"in a transaction: {sql_text.split(' ')[0]} {sql_text.split(' ')[1]}",
            r,
            [SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SQL_NO_DATA],
            got_state=sqlstate(lib, SQL_HANDLE_STMT, txn),
        )

    r = lib.SQLEndTran(SQL_HANDLE_DBC, dbc, SQL_ROLLBACK)
    check("roll the transaction back", r, SQL_SUCCESS)

    check(
        "the rollback undid the rows around the DDL",
        SQL_SUCCESS if scalar_i64(lib, txn, "SELECT count(*) FROM abi_txn") == 0 else SQL_ERROR,
        SQL_SUCCESS,
    )
    survived = scalar_i64(
        lib, txn, "SELECT count(*) FROM sqlite_master WHERE name = 'abi_mid'"
    )
    check(
        "the rollback undid the DDL too",
        SQL_SUCCESS if survived == 0 else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"abi_mid rows in sqlite_master: {survived}",
    )

    r = lib.SQLSetConnectAttrW(dbc, SQL_ATTR_AUTOCOMMIT, P(SQL_AUTOCOMMIT_ON), 0)
    check("turn autocommit back on", r, SQL_SUCCESS)
    drop_sql, _k_drop = w("DROP TABLE IF EXISTS abi_txn")
    lib.SQLExecDirectW(txn, drop_sql, SQL_NTS)
    lib.SQLFreeHandle(SQL_HANDLE_STMT, txn)

    # ---------------------------------------------------------------
    print("\n--- catalog functions SQLite answers with no rows ---")
    # Each of these describes a concept SQLite does not have. The spec requires
    # the result set's *shape* regardless, so the column count is the real
    # assertion: a driver returning zero columns has failed the call rather than
    # answered it, and an application cannot tell the difference from the row
    # count alone.
    # `SQLColumnPrivileges` is the one that needs a table name: the spec says
    # its TableName "cannot be a null pointer", unlike the pattern arguments of
    # the other three. It is named here so the probe tests the empty result set
    # rather than the argument check, which is asserted separately below.
    priv_table, _k_priv = w("users")
    for label, call, columns in (
        (
            "table privileges",
            lambda s: lib.SQLTablePrivilegesW(s, None, 0, None, 0, None, 0),
            7,
        ),
        (
            "column privileges",
            lambda s: lib.SQLColumnPrivilegesW(
                s, None, 0, None, 0, priv_table, SQL_NTS, None, 0
            ),
            8,
        ),
        ("procedures", lambda s: lib.SQLProceduresW(s, None, 0, None, 0, None, 0), 8),
        (
            "procedure columns",
            lambda s: lib.SQLProcedureColumnsW(s, None, 0, None, 0, None, 0, None, 0),
            19,
        ),
    ):
        cat = P()
        lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(cat))
        r = call(cat)
        check(f"{label} succeeds", r, [SQL_SUCCESS, SQL_SUCCESS_WITH_INFO])
        got_cols = column_count(lib, cat)
        check(
            f"{label} describes {columns} columns",
            SQL_SUCCESS if got_cols == columns else SQL_ERROR,
            SQL_SUCCESS,
            got_state=f"got {got_cols}",
        )
        rows = row_count(lib, cat)
        check(
            f"{label} is empty",
            SQL_SUCCESS if rows == 0 else SQL_ERROR,
            SQL_SUCCESS,
            got_state=f"got {rows} rows",
        )
        lib.SQLFreeHandle(SQL_HANDLE_STMT, cat)

    # The required-argument check, which the loop above deliberately satisfies.
    # HY009 is the driver's own: the spec attributes this one to the driver, not
    # to the Driver Manager, so it is fair to demand it here.
    null_arg = P()
    lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(null_arg))
    r = lib.SQLColumnPrivilegesW(null_arg, None, 0, None, 0, None, 0, None, 0)
    check(
        "column privileges rejects a null table name",
        r,
        SQL_ERROR,
        state="HY009",
        got_state=sqlstate(lib, SQL_HANDLE_STMT, null_arg),
    )
    lib.SQLFreeHandle(SQL_HANDLE_STMT, null_arg)

    # ---------------------------------------------------------------
    print("\n--- bound parameters ---")
    param_stmt = P()
    lib.SQLAllocHandle(SQL_HANDLE_STMT, dbc, ctypes.byref(param_stmt))
    psql, _k_p = w("SELECT ? + 1")
    r = lib.SQLPrepareW(param_stmt, psql, SQL_NTS)
    check("prepare a statement with one parameter", r, SQL_SUCCESS)

    nparams = ctypes.c_int16(-1)
    r = lib.SQLNumParams(param_stmt, ctypes.byref(nparams))
    check("num_params", r, SQL_SUCCESS)
    check(
        "num_params counts the marker",
        SQL_SUCCESS if nparams.value == 1 else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"got {nparams.value}",
    )

    pval = ctypes.c_int64(41)
    plen = ctypes.c_int64(8)
    r = lib.SQLBindParameter(
        param_stmt,
        1,
        SQL_PARAM_INPUT,
        SQL_C_SBIGINT,
        SQL_BIGINT,
        0,
        0,
        ctypes.cast(ctypes.byref(pval), P),
        8,
        ctypes.byref(plen),
    )
    check("bind the parameter", r, SQL_SUCCESS)

    r = lib.SQLExecute(param_stmt)
    check("execute with one parameter set", r, SQL_SUCCESS)
    got = None
    if lib.SQLFetch(param_stmt) == SQL_SUCCESS:
        result = ctypes.c_int64(0)
        rind = ctypes.c_int64(0)
        if (
            lib.SQLGetData(
                param_stmt,
                1,
                SQL_C_SBIGINT,
                ctypes.cast(ctypes.byref(result), P),
                8,
                ctypes.byref(rind),
            )
            == SQL_SUCCESS
        ):
            got = result.value
    check(
        "the bound value reached SQLite",
        SQL_SUCCESS if got == 42 else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"got {got}, expected 42",
    )
    lib.SQLCloseCursor(param_stmt)
    lib.SQLFreeHandle(SQL_HANDLE_STMT, param_stmt)

    # ---------------------------------------------------------------
    print("\n--- cancel and teardown ---")
    # "A call to SQLCancel when no processing is being done on the statement
    # ... has no effect at all", so an idle cancel succeeds.
    r = lib.SQLCancel(stmt)
    check("cancel an idle statement", r, SQL_SUCCESS)

    r = lib.SQLFreeHandle(SQL_HANDLE_STMT, attr_stmt)
    check("free the attribute statement", r, SQL_SUCCESS)

    r = lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)
    check("free statement", r, SQL_SUCCESS)

    # The handle is gone, so its tag no longer validates. Nothing here may
    # dereference freed memory: the whole point of the tag is that a stale
    # handle is detected rather than followed.
    r = lib.SQLFreeHandle(SQL_HANDLE_STMT, stmt)
    check("free the same statement twice", r, SQL_INVALID_HANDLE)

    r = lib.SQLFetch(stmt)
    check("fetch on a freed statement", r, SQL_INVALID_HANDLE)

    r = lib.SQLExecute(stmt)
    check("execute on a freed statement", r, SQL_INVALID_HANDLE)

    dbms = read_wide_info(lib, dbc, SQL_DBMS_NAME)
    check(
        "SQL_DBMS_NAME says SQLite",
        SQL_SUCCESS if dbms == "SQLite" else SQL_ERROR,
        SQL_SUCCESS,
        got_state=f"read {dbms!r}",
    )

    r = lib.SQLDisconnect(dbc)
    check("disconnect", r, SQL_SUCCESS)

    r = lib.SQLFreeHandle(SQL_HANDLE_DBC, dbc)
    check("free connection", r, SQL_SUCCESS)

    r = lib.SQLFreeHandle(SQL_HANDLE_DBC, dbc)
    check("free the same connection twice", r, SQL_INVALID_HANDLE)

    r = lib.SQLFreeHandle(SQL_HANDLE_ENV, env)
    check("free env once its children are gone", r, SQL_SUCCESS)

    r = lib.SQLFreeHandle(SQL_HANDLE_ENV, env)
    check("free the same env twice", r, SQL_INVALID_HANDLE)

    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
