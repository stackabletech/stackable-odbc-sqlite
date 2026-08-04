#!/usr/bin/env python3
"""
Integration tests for the SQLite ODBC driver.

Runs through the ODBC Driver Manager (unixODBC on Linux, odbc32.dll on Windows)
using pyodbc: DDL, DML, queries, aggregation, joins and parameterised
statements.

Three things here are covered nowhere else:

  - **`SQLRowCount`.** The affected-row count of an INSERT, UPDATE and DELETE,
    which is the observable half of the `row_count` rule in AGENTS.md: `None`
    and `Some(0)` mean different things, and answering the wrong one turns a
    `CREATE TABLE` into `SQL_NO_DATA`.
  - **Unicode round-tripping.** Japanese, emoji and accented Latin through
    core's UTF-16 marshalling and back.
  - **`SQLGetData` rather than `SQLBindCol`.** Registering a pyodbc output
    converter makes it fetch the column with `SQLGetData(SQL_C_BINARY)` instead
    of binding it, which is a different path through the driver.

The rest overlaps `test_sql_surface.py` on purpose: this is the one suite the
Windows VM ran before the others were ported, so its breadth is what Windows
coverage rested on.

Fixture tables are named for this suite (`it_*`) and dropped at the end. They
deliberately do not reuse `types_test` from `create_test_db.sql`: this suite
used to create that name with `IF NOT EXISTS` and then drop it, which removed a
table it did not own and left whatever ran next to find it missing.

Usage:
    python3 integration-tests/suites/test_integration.py \
        "Driver=/path/to/libstackable_odbc_sqlite.so;Database=/path/to/test.db"
    python3 integration-tests/suites/test_integration.py "DSN=test_sqlite"
"""

import os
import sys

import pyodbc

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results, Target  # noqa: E402

R = Results("integration")

EMPLOYEES = "it_employees"
TYPES = "it_types"


def make_fixture(cur):
    cur.execute(f"DROP TABLE IF EXISTS {EMPLOYEES}")
    cur.execute(f"""
        CREATE TABLE {EMPLOYEES} (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            salary REAL,
            active BOOLEAN
        )
    """)
    cur.execute(f"INSERT INTO {EMPLOYEES} VALUES (1, 'Alice', 75000.50, 1)")
    cur.execute(f"INSERT INTO {EMPLOYEES} VALUES (2, 'Bob', 62000.00, 0)")
    cur.execute(f"INSERT INTO {EMPLOYEES} VALUES (3, 'Charlie', 91000.25, 1)")

    cur.execute(f"DROP TABLE IF EXISTS {TYPES}")
    cur.execute(f"""
        CREATE TABLE {TYPES} (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            price REAL,
            quantity INTEGER,
            active BOOLEAN,
            data BLOB,
            created_at TEXT
        )
    """)
    cur.execute(
        f"INSERT INTO {TYPES} VALUES "
        "(1, 'Widget', 9.99, 100, 1, X'DEADBEEF', '2026-01-15T10:30:00')"
    )
    cur.execute(
        f"INSERT INTO {TYPES} VALUES "
        "(2, 'Gadget', 24.50, NULL, 0, NULL, '2026-02-20T14:00:00')"
    )
    cur.execute(
        f"INSERT INTO {TYPES} VALUES "
        "(3, 'Doohickey', 0.50, 9999, 1, X'00', '2026-03-01T00:00:00')"
    )


def drop_fixture(cur):
    for table in (EMPLOYEES, TYPES):
        try:
            cur.execute(f"DROP TABLE IF EXISTS {table}")
        except Exception:  # noqa: BLE001
            pass


def main():
    target = Target.from_argv(
        sys.argv,
        "usage: test_integration.py "
        '"Driver=/path/to/libstackable_odbc_sqlite.so;Database=/path/to/test.db"',
    )
    conn = pyodbc.connect(target.conn_str(), autocommit=True)
    cur = conn.cursor()

    make_fixture(cur)
    try:
        # --------------------------------------------------------------
        print("--- SELECT basics ---")

        def select_all():
            cur.execute(f"SELECT * FROM {EMPLOYEES} ORDER BY id")
            rows = cur.fetchall()
            assert len(rows) == 3, f"expected 3 rows, got {len(rows)}"
            assert [r[1] for r in rows] == ["Alice", "Bob", "Charlie"]

        R.run("SELECT all rows", select_all)

        def select_where():
            cur.execute(f"SELECT name, salary FROM {EMPLOYEES} WHERE id = 1")
            row = cur.fetchone()
            assert row is not None
            assert row[0] == "Alice"
            assert abs(row[1] - 75000.50) < 0.01

        R.run("SELECT with WHERE", select_where)

        def select_count():
            # Compared against an int, not coerced with `int()`. `count(*)` is a
            # computed column, and the driver types it from the storage class of
            # the value, so a string here would be a regression rather than
            # something to work around. `test_type_matrix.py` tests that
            # properly.
            cur.execute(f"SELECT COUNT(*) FROM {EMPLOYEES}")
            count = cur.fetchone()[0]
            assert count == 3, f"expected 3, got {count!r}"

        R.run("SELECT COUNT(*)", select_count)

        def select_empty():
            cur.execute(f"SELECT * FROM {EMPLOYEES} WHERE id = 999")
            assert cur.fetchone() is None

        R.run("SELECT with no matching rows", select_empty)

        # --------------------------------------------------------------
        print("\n--- DDL and DML ---")

        def create_insert_drop():
            cur.execute("DROP TABLE IF EXISTS it_temp")
            cur.execute("CREATE TABLE it_temp (id INTEGER PRIMARY KEY, val TEXT)")
            cur.execute("INSERT INTO it_temp VALUES (1, 'hello')")
            cur.execute("INSERT INTO it_temp VALUES (2, 'world')")
            cur.execute("SELECT COUNT(*) FROM it_temp")
            assert cur.fetchone()[0] == 2
            cur.execute("DROP TABLE it_temp")

        R.run("CREATE + INSERT + DROP", create_insert_drop)

        # The three row-count probes. `SQLRowCount` has to distinguish "no
        # applicable count" from "counted zero": core turns a zero-column
        # statement reporting 0 into SQL_NO_DATA, so a DDL statement answering
        # the same way would look to an application like a DELETE that matched
        # nothing.
        def insert_row_count():
            cur.execute("DROP TABLE IF EXISTS it_rowcount")
            cur.execute("CREATE TABLE it_rowcount (id INTEGER PRIMARY KEY, val TEXT)")
            count = cur.execute("INSERT INTO it_rowcount VALUES (1, 'a')").rowcount
            assert count == 1, f"expected rowcount 1, got {count}"
            cur.execute("DROP TABLE it_rowcount")

        R.run("INSERT rowcount", insert_row_count)

        def update_and_verify():
            cur.execute(f"UPDATE {EMPLOYEES} SET salary = 80000.00 WHERE name = 'Alice'")
            cur.execute(f"SELECT salary FROM {EMPLOYEES} WHERE name = 'Alice'")
            assert abs(cur.fetchone()[0] - 80000.00) < 0.01
            cur.execute(f"UPDATE {EMPLOYEES} SET salary = 75000.50 WHERE name = 'Alice'")

        R.run("UPDATE + verify", update_and_verify)

        def update_row_count():
            count = cur.execute(
                f"UPDATE {EMPLOYEES} SET salary = salary WHERE active = 1"
            ).rowcount
            assert count == 2, f"expected rowcount 2, got {count}"

        R.run("UPDATE rowcount", update_row_count)

        def delete_and_verify():
            cur.execute(f"INSERT INTO {EMPLOYEES} VALUES (99, 'Temp', 10000, 1)")
            count = cur.execute(f"DELETE FROM {EMPLOYEES} WHERE id = 99").rowcount
            assert count == 1, f"expected rowcount 1, got {count}"
            cur.execute(f"SELECT * FROM {EMPLOYEES} WHERE id = 99")
            assert cur.fetchone() is None

        R.run("DELETE + verify", delete_and_verify)

        # --------------------------------------------------------------
        print("\n--- aggregation ---")

        def group_by():
            cur.execute("DROP TABLE IF EXISTS it_orders")
            cur.execute(
                "CREATE TABLE it_orders (id INTEGER PRIMARY KEY, customer TEXT, amount REAL)"
            )
            for row in [
                (1, "Alice", 29.99),
                (2, "Bob", 49.99),
                (3, "Alice", 49.99),
                (4, "Bob", 29.99),
                (5, "Alice", 99.99),
            ]:
                cur.execute("INSERT INTO it_orders VALUES (?,?,?)", row)
            cur.execute(
                "SELECT customer, COUNT(*), SUM(amount) FROM it_orders "
                "GROUP BY customer ORDER BY customer"
            )
            rows = cur.fetchall()
            assert len(rows) == 2
            assert rows[0][0] == "Alice"
            assert rows[0][1] == 3, f"expected 3, got {rows[0][1]!r}"
            assert abs(rows[0][2] - 179.97) < 0.01
            assert rows[1][0] == "Bob"
            assert rows[1][1] == 2, f"expected 2, got {rows[1][1]!r}"
            cur.execute("DROP TABLE it_orders")

        R.run("GROUP BY + COUNT + SUM", group_by)

        def having():
            cur.execute("DROP TABLE IF EXISTS it_orders2")
            cur.execute(
                "CREATE TABLE it_orders2 (id INTEGER PRIMARY KEY, customer TEXT, amount REAL)"
            )
            for row in [(1, "Alice", 10), (2, "Alice", 20), (3, "Bob", 30)]:
                cur.execute("INSERT INTO it_orders2 VALUES (?,?,?)", row)
            cur.execute(
                "SELECT customer, COUNT(*) AS cnt FROM it_orders2 "
                "GROUP BY customer HAVING cnt > 1"
            )
            rows = cur.fetchall()
            assert len(rows) == 1
            assert rows[0][0] == "Alice"
            cur.execute("DROP TABLE it_orders2")

        R.run("GROUP BY + HAVING", having)

        def order_by():
            cur.execute(f"SELECT name FROM {EMPLOYEES} ORDER BY salary DESC")
            names = [r[0] for r in cur.fetchall()]
            assert names == ["Charlie", "Alice", "Bob"], f"got {names}"

        R.run("ORDER BY DESC", order_by)

        def join():
            cur.execute("DROP TABLE IF EXISTS it_departments")
            cur.execute("CREATE TABLE it_departments (id INTEGER PRIMARY KEY, dept TEXT)")
            cur.execute("INSERT INTO it_departments VALUES (1, 'Engineering')")
            cur.execute("INSERT INTO it_departments VALUES (2, 'Marketing')")
            cur.execute(f"""
                SELECT e.name, d.dept
                FROM {EMPLOYEES} e JOIN it_departments d ON e.id = d.id
                ORDER BY e.id
            """)
            rows = cur.fetchall()
            assert len(rows) == 2
            assert rows[0][0] == "Alice" and rows[0][1] == "Engineering"
            assert rows[1][0] == "Bob" and rows[1][1] == "Marketing"
            cur.execute("DROP TABLE it_departments")

        R.run("JOIN", join)

        # --------------------------------------------------------------
        print("\n--- parameterised statements ---")

        def param_select_int():
            cur.execute(f"SELECT name, price FROM {TYPES} WHERE id = ?", (1,))
            row = cur.fetchone()
            assert row is not None
            assert row[0] == "Widget"
            assert abs(row[1] - 9.99) < 1e-9

        R.run("Param: SELECT by integer", param_select_int)

        def param_select_string():
            cur.execute(f"SELECT id, price FROM {TYPES} WHERE name = ?", ("Gadget",))
            row = cur.fetchone()
            assert row is not None
            assert row[0] == 2

        R.run("Param: SELECT by string", param_select_string)

        def param_no_rows():
            cur.execute(f"SELECT id FROM {TYPES} WHERE id = ?", (999,))
            assert cur.fetchone() is None

        R.run("Param: no matching rows", param_no_rows)

        def param_null_column():
            cur.execute(f"SELECT quantity FROM {TYPES} WHERE id = ?", (2,))
            row = cur.fetchone()
            assert row is not None
            assert row[0] is None, f"expected NULL, got {row[0]!r}"

        R.run("Param: NULL column", param_null_column)

        def param_multiple_rows():
            cur.execute(f"SELECT id FROM {TYPES} WHERE active = ? ORDER BY id", (1,))
            ids = [r[0] for r in cur.fetchall()]
            assert ids == [1, 3], f"expected [1, 3], got {ids}"

        R.run("Param: multiple rows", param_multiple_rows)

        def param_insert():
            cur.execute(
                f"INSERT INTO {TYPES} (id, name, price, quantity, active) "
                "VALUES (?, ?, ?, ?, ?)",
                (100, "TestItem", 1.23, 42, 1),
            )
            cur.execute(f"SELECT name, price, quantity FROM {TYPES} WHERE id = ?", (100,))
            row = cur.fetchone()
            assert row is not None
            assert row[0] == "TestItem"
            assert abs(row[1] - 1.23) < 1e-9
            assert row[2] == 42
            cur.execute(f"DELETE FROM {TYPES} WHERE id = 100")

        R.run("Param: INSERT + verify", param_insert)

        def param_reexecute():
            expected = {1: "Widget", 2: "Gadget", 3: "Doohickey"}
            for id_, name in expected.items():
                cur.execute(f"SELECT name FROM {TYPES} WHERE id = ?", (id_,))
                row = cur.fetchone()
                assert row is not None, f"no row for id={id_}"
                assert row[0] == name, f"id={id_}: expected {name!r}, got {row[0]!r}"

        R.run("Param: re-execute with different values", param_reexecute)

        def param_null_binding():
            cur.execute(
                f"INSERT INTO {TYPES} (id, name, price) VALUES (?, ?, ?)",
                (101, "NullPrice", None),
            )
            cur.execute(f"SELECT price FROM {TYPES} WHERE id = ?", (101,))
            row = cur.fetchone()
            assert row is not None
            assert row[0] is None, f"expected NULL, got {row[0]!r}"
            cur.execute(f"DELETE FROM {TYPES} WHERE id = 101")

        R.run("Param: NULL binding", param_null_binding)

        # --------------------------------------------------------------
        print("\n--- marshalling ---")

        def unicode_roundtrip():
            """Text through core's UTF-16 marshalling and back.

            SQLWCHAR is 16-bit, so an emoji is a surrogate pair and a mistake in
            the conversion shows up here and nowhere else in the suite.
            """
            cur.execute("DROP TABLE IF EXISTS it_unicode")
            cur.execute("CREATE TABLE it_unicode (id INTEGER PRIMARY KEY, val TEXT)")
            values = [
                (1, "日本語"),
                (2, "🎉🦀"),
                (3, "café résumé"),
                (4, "Ünïcödé"),
            ]
            for row in values:
                cur.execute("INSERT INTO it_unicode VALUES (?, ?)", row)
            cur.execute("SELECT id, val FROM it_unicode ORDER BY id")
            rows = cur.fetchall()
            assert len(rows) == len(values), f"expected {len(values)} rows, got {len(rows)}"
            for (row_id, row_val), (exp_id, exp_val) in zip(rows, values):
                assert row_id == exp_id
                assert row_val == exp_val, (
                    f"id={exp_id}: expected {exp_val!r}, got {row_val!r}"
                )
            cur.execute("DROP TABLE it_unicode")

        R.run("Unicode roundtrip (Japanese, emoji, accents)", unicode_roundtrip)

        def getdata_instead_of_bindcol():
            """The `SQLGetData` path rather than `SQLBindCol`.

            Registering an output converter makes pyodbc stop binding the column
            and call `SQLGetData(SQL_C_BINARY)` for it instead, which is a
            different route through the driver. The converter decodes the
            8-byte little-endian value, so it also pins that an INTEGER column
            is delivered as `SQL_BIGINT`.
            """
            import struct

            SQL_BIGINT = -5
            received = []

            def decode_bigint(b):
                val = struct.unpack("<q", b)[0]
                received.append(val)
                return val

            conn.add_output_converter(SQL_BIGINT, decode_bigint)
            try:
                cur.execute(f"SELECT id FROM {EMPLOYEES} ORDER BY id")
                ids = [r[0] for r in cur.fetchall()]
                assert received == [1, 2, 3], (
                    f"SQLGetData not called, or wrong raw values: {received!r}"
                )
                assert ids == [1, 2, 3], f"expected [1, 2, 3], got {ids!r}"
            finally:
                conn.clear_output_converters()

        R.run("SQLGetData rather than SQLBindCol", getdata_instead_of_bindcol)
    finally:
        drop_fixture(cur)
        conn.close()

    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
