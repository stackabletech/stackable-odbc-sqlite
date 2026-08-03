#!/usr/bin/env python3
"""ODBC manual-commit transactions, through unixODBC.

Needs no server: the database is a file, and `setup.sh` made it. Every scenario
works in tables of its own and drops them, so it can run against the shared
test database without disturbing the other suites.

Four measured SQLite behaviours shape what is asserted here, and three of them
are the *opposite* of the Trino driver's, so a scenario copied across without
thinking would assert the wrong thing:

  - **A failed statement does not abort the transaction.** Trino discards the
    whole thing and then refuses the commit. SQLite leaves the transaction open
    and the earlier writes intact, so the commit must succeed and publish them.
  - **A commit does not close an open cursor.** The driver reports
    `SQL_CURSOR_COMMIT_BEHAVIOR = SQL_CB_PRESERVE`, which is only true because
    `exec_direct` materialises every row before returning. Fetching must go on
    working after the commit.
  - **DDL participates in the transaction.** `SQL_TXN_CAPABLE` is `SQL_TC_ALL`,
    so a `CREATE TABLE` inside a transaction is undone by a rollback along with
    the rows around it.
  - **Serializable is the only isolation level**, so it is the one that must be
    accepted and every other one must be refused. Trino is the mirror image,
    offering only `READ UNCOMMITTED`.

Usage:
    python3 integration-tests/suites/test_transactions.py \
        "Driver=/path/to/libstackable_odbc_sqlite.so;Database=/path/to/test.db"
"""

import os
import sys
import time
import uuid

import pyodbc

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Results, Target  # noqa: E402

# pyodbc enables ODBC connection pooling by default, and a pooled connection is
# handed back to the application without the driver being reconnected, so it
# arrives still carrying whatever commit mode the previous borrower left on it.
# A "fresh" connection would then run inside the previous borrower's
# manual-commit mode, and a write would be discarded while reporting success.
#
# Turned off so this suite measures the driver rather than the Driver Manager's
# pooling.
pyodbc.pooling = False

# pyodbc exposes neither of these, so they are spelled out rather than taken
# from it.
SQL_ATTR_TXN_ISOLATION = 108
SQL_TXN_READ_UNCOMMITTED = 1
SQL_TXN_READ_COMMITTED = 2
SQL_TXN_REPEATABLE_READ = 4
SQL_TXN_SERIALIZABLE = 8


def scenario(results, label, fn):
    """Run one scenario, recording an exception as a single failure.

    `Results.run` is not used because each scenario does its own `check`
    accounting, and a wrapper PASS printed beside an inner FAIL reads as though
    something passed."""
    start = time.monotonic()
    try:
        fn()
    except Exception as e:  # noqa: BLE001
        results.bad(label, f"raised after {time.monotonic() - start:.1f}s: {e}")
    else:
        print(f"      {label}: {time.monotonic() - start:.1f}s")


def unique_table(prefix):
    """A table name of this run's own, so a suite left half-finished by an
    earlier failure cannot make the next run pass or fail for the wrong
    reason."""
    return f"tx_{prefix}_{uuid.uuid4().hex[:8]}"


def make_table(conn, table):
    conn.cursor().execute(f"CREATE TABLE {table} (id INTEGER)")


def drop_table(conn, table):
    try:
        conn.cursor().execute(f"DROP TABLE IF EXISTS {table}")
    except Exception:  # noqa: BLE001
        # Cleanup only. A failure here must not mask the scenario's own result.
        pass


def as_int(value):
    """An aggregate read back as a number, whatever the driver typed it as.

    `count(*)` arrives as a *string*: `sqlite3_column_decltype` is NULL for any
    computed column, and `describe_column` falls back to `TEXT`, so every
    expression is described as VARCHAR regardless of the storage class of the
    value in it. That is a real finding, but it belongs to the type suite, not
    to this one. Coercing here keeps a transaction failure from being reported
    as a typing failure and the other way round."""
    return int(value)


def count_rows(target, table):
    """Count from a *fresh* connection, which is what makes a commit or a
    rollback observable rather than merely reported.

    Only ever called once the writing transaction has ended. SQLite takes a
    write lock for the duration of one, and a second connection reading through
    it would be answered `SQLITE_BUSY` rather than with a row count."""
    with target.connect() as conn:
        return as_int(
            conn.cursor().execute(f"SELECT count(*) FROM {table}").fetchone()[0]
        )


def table_exists(target, table):
    with target.connect() as conn:
        return (
            as_int(
                conn.cursor()
                .execute("SELECT count(*) FROM sqlite_master WHERE name = ?", table)
                .fetchone()[0]
            )
            > 0
        )


def a_rollback_discards_a_write(target, results):
    table = unique_table("rollback")
    with target.connect() as setup:
        make_table(setup, table)
    try:
        conn = target.connect()
        conn.autocommit = False
        conn.cursor().execute(f"INSERT INTO {table} VALUES (1)")
        conn.rollback()
        conn.close()

        count = count_rows(target, table)
        results.check("a rolled-back insert is not there", count == 0, f"count is {count}")
    finally:
        with target.connect() as cleanup:
            drop_table(cleanup, table)


def a_commit_publishes_a_write(target, results):
    table = unique_table("commit")
    with target.connect() as setup:
        make_table(setup, table)
    try:
        conn = target.connect()
        conn.autocommit = False
        conn.cursor().execute(f"INSERT INTO {table} VALUES (1)")
        conn.commit()
        conn.close()

        count = count_rows(target, table)
        results.check(
            "a committed insert is visible to another connection",
            count == 1,
            f"count is {count}",
        )
    finally:
        with target.connect() as cleanup:
            drop_table(cleanup, table)


def a_commit_spanning_two_tables_is_atomic(target, results):
    first, second = unique_table("atomic_a"), unique_table("atomic_b")
    with target.connect() as setup:
        make_table(setup, first)
        make_table(setup, second)
    try:
        conn = target.connect()
        conn.autocommit = False
        conn.cursor().execute(f"INSERT INTO {first} VALUES (1)")
        conn.cursor().execute(f"INSERT INTO {second} VALUES (2)")
        conn.commit()
        conn.close()

        a, b = count_rows(target, first), count_rows(target, second)
        results.check("both tables carry the commit", a == 1 and b == 1, f"{a} and {b}")
    finally:
        with target.connect() as cleanup:
            drop_table(cleanup, first)
            drop_table(cleanup, second)


def a_failed_statement_leaves_the_transaction_usable(target, results):
    """The inverse of the Trino driver's equivalent, and the reason this suite
    could not be copied across.

    Trino aborts the whole transaction on any statement error and then refuses
    the commit, so its driver rolls back and reports `25S03`. SQLite does no
    such thing: a statement that fails to prepare or resolve leaves the
    transaction open and the earlier writes intact. The commit must therefore
    succeed and publish them.

    A driver that rolled back here to look consistent with the Trino one would
    silently discard writes the application was told nothing about."""
    table = unique_table("survives")
    with target.connect() as setup:
        make_table(setup, table)
    try:
        conn = target.connect()
        conn.autocommit = False
        conn.cursor().execute(f"INSERT INTO {table} VALUES (1)")

        try:
            conn.cursor().execute("SELECT * FROM a_table_that_does_not_exist").fetchall()
            results.bad("the bad statement fails", "it succeeded")
            conn.close()
            return
        except Exception:  # noqa: BLE001
            pass

        try:
            conn.commit()
        except Exception as e:  # noqa: BLE001
            results.bad(
                "committing after a failed statement succeeds",
                f"it raised {type(e).__name__}: {e}",
            )
            conn.close()
            return

        count = count_rows(target, table)
        results.check(
            "the write made before the failed statement survives the commit",
            count == 1,
            f"count is {count}",
        )

        conn.autocommit = True
        value = as_int(conn.cursor().execute("SELECT 1").fetchone()[0])
        results.check("the connection still works", value == 1, f"got {value}")
        conn.close()
    finally:
        with target.connect() as cleanup:
            drop_table(cleanup, table)


def ddl_inside_a_transaction_is_rolled_back(target, results):
    """`SQL_TXN_CAPABLE` is `SQL_TC_ALL`: "Transactions support both DML and DDL
    statements in any order".

    The C ABI suite proves this without a Driver Manager. Here it goes through
    unixODBC as an application would, because the claim is what a tool reads
    before deciding whether it may run DDL inside a transaction at all."""
    outer, created = unique_table("ddl_outer"), unique_table("ddl_inner")
    with target.connect() as setup:
        make_table(setup, outer)
    try:
        conn = target.connect()
        conn.autocommit = False
        conn.cursor().execute(f"INSERT INTO {outer} VALUES (1)")
        conn.cursor().execute(f"CREATE TABLE {created} (x TEXT)")
        conn.cursor().execute(f"INSERT INTO {outer} VALUES (2)")
        conn.rollback()
        conn.close()

        count = count_rows(target, outer)
        results.check(
            "the rollback undid the rows around the DDL", count == 0, f"count is {count}"
        )
        results.check(
            "the rollback undid the DDL too",
            not table_exists(target, created),
            "the table created inside the transaction outlived it",
        )
    finally:
        with target.connect() as cleanup:
            drop_table(cleanup, outer)
            drop_table(cleanup, created)


def a_commit_preserves_an_open_cursor(target, results):
    """`SQL_CURSOR_COMMIT_BEHAVIOR` is `SQL_CB_PRESERVE`, the opposite of the
    Trino driver's `SQL_CB_CLOSE`.

    It is only true because `exec_direct` materialises every row before
    returning, so no `rusqlite::Statement` is live when the commit runs. That is
    why AGENTS.md calls eager materialisation load-bearing: making fetching lazy
    would make this claim false without touching the code that states it."""
    table = unique_table("cursor")
    with target.connect() as setup:
        make_table(setup, table)
        for i in range(3):
            setup.cursor().execute(f"INSERT INTO {table} VALUES ({i})")
    try:
        conn = target.connect()
        conn.autocommit = False
        cursor = conn.cursor()
        cursor.execute(f"SELECT id FROM {table} ORDER BY id")
        first = cursor.fetchone()
        results.check("the cursor produced a row before the commit", first is not None)

        conn.commit()

        try:
            rest = cursor.fetchall()
        except Exception as e:  # noqa: BLE001
            results.bad(
                "the cursor survives the commit",
                f"fetching after the commit raised {type(e).__name__}: {e}",
            )
        else:
            results.check(
                "the cursor still yields its remaining rows after the commit",
                len(rest) == 2,
                f"got {len(rest)} rows, expected 2",
            )
        conn.autocommit = True
        conn.close()
    finally:
        with target.connect() as cleanup:
            drop_table(cleanup, table)


def autocommit_is_the_default(target, results):
    """No explicit transaction, and the write is visible elsewhere with no
    commit, which is what ODBC's default commit mode means."""
    table = unique_table("autocommit")
    with target.connect() as setup:
        make_table(setup, table)
    try:
        with target.connect() as conn:
            conn.cursor().execute(f"INSERT INTO {table} VALUES (1)")
        count = count_rows(target, table)
        results.check("an autocommit write needs no commit", count == 1, f"count is {count}")
    finally:
        with target.connect() as cleanup:
            drop_table(cleanup, table)


def only_serializable_is_accepted(target, results):
    """`SQL_TXN_ISOLATION_OPTION` advertises `SQL_TXN_SERIALIZABLE` alone, so
    core accepts that one and rejects the rest with `HY024` before anything
    reaches SQLite.

    Both halves matter. A driver that refused every level would pass the
    rejection check while offering no isolation at all, and one that accepted
    every level would store a value nothing applies: `SQL_ATTR_TXN_ISOLATION` is
    kept on the connection and read back, never pushed to SQLite, so an
    application asking for REPEATABLE READ would be told it had it while running
    serializable."""
    conn = target.connect()
    try:
        try:
            conn.set_attr(SQL_ATTR_TXN_ISOLATION, SQL_TXN_SERIALIZABLE)
        except Exception as e:  # noqa: BLE001
            results.bad(
                "SQL_TXN_SERIALIZABLE is accepted",
                f"the level SQLite implements was refused: {e}",
            )
        else:
            results.ok("SQL_TXN_SERIALIZABLE is accepted")

        for label, level in (
            ("READ UNCOMMITTED", SQL_TXN_READ_UNCOMMITTED),
            ("READ COMMITTED", SQL_TXN_READ_COMMITTED),
            ("REPEATABLE READ", SQL_TXN_REPEATABLE_READ),
        ):
            try:
                conn.set_attr(SQL_ATTR_TXN_ISOLATION, level)
            except Exception as e:  # noqa: BLE001
                state = getattr(e, "args", ["", ""])[0]
                results.check(
                    f"{label} is refused with HY024", state == "HY024", f"SQLSTATE {state}"
                )
            else:
                results.bad(
                    f"{label} is refused",
                    "it was accepted, but nothing applies a level SQLite does not have",
                )
    finally:
        conn.close()


def main():
    target = Target.from_argv(
        sys.argv,
        "usage: test_transactions.py "
        '"Driver=/path/to/libstackable_odbc_sqlite.so;Database=/path/to/test.db"',
    )
    results = Results("transactions")

    for label, fn in (
        ("a rollback discards a write", a_rollback_discards_a_write),
        ("a commit publishes a write", a_commit_publishes_a_write),
        ("a commit spanning two tables is atomic", a_commit_spanning_two_tables_is_atomic),
        (
            "a failed statement leaves the transaction usable",
            a_failed_statement_leaves_the_transaction_usable,
        ),
        ("DDL inside a transaction is rolled back", ddl_inside_a_transaction_is_rolled_back),
        ("a commit preserves an open cursor", a_commit_preserves_an_open_cursor),
        ("autocommit is the default", autocommit_is_the_default),
        ("only serializable is accepted", only_serializable_is_accepted),
    ):
        scenario(results, label, lambda fn=fn: fn(target, results))

    sys.exit(results.summary())


if __name__ == "__main__":
    main()
