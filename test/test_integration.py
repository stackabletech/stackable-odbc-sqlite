#!/usr/bin/env python3
"""
Integration tests for the SQLite ODBC driver.

Runs through the ODBC Driver Manager (unixODBC on Linux, odbc32.dll on Windows)
using pyodbc. Tests DDL, DML, queries, aggregation, joins, and parameterised
statements.

Usage:
    python3 test/test_integration.py "Driver=/path/to/driver.so;Database=/path/to/test.db"
    python3 test/test_integration.py "Driver=C:\\path\\to\\driver.dll;Database=C:\\test.db"

Requires: pip install pyodbc
"""

import sys
import pyodbc

passed = 0
failed = 0


def run(label, fn):
    """Run a test function, print PASS/FAIL, track counts."""
    global passed, failed
    try:
        fn()
        print(f"PASS  {label}")
        passed += 1
    except Exception as e:
        print(f"FAIL  {label}: {e}")
        failed += 1


def main():
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <connection-string>")
        sys.exit(2)

    conn_str = sys.argv[1]
    conn = pyodbc.connect(conn_str, autocommit=True)
    cur = conn.cursor()

    # === Setup: create test tables ===
    cur.execute("""
        CREATE TABLE IF NOT EXISTS employees (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            salary REAL,
            active BOOLEAN
        )
    """)
    cur.execute("DELETE FROM employees")
    cur.execute("INSERT INTO employees VALUES (1, 'Alice', 75000.50, 1)")
    cur.execute("INSERT INTO employees VALUES (2, 'Bob', 62000.00, 0)")
    cur.execute("INSERT INTO employees VALUES (3, 'Charlie', 91000.25, 1)")

    cur.execute("""
        CREATE TABLE IF NOT EXISTS types_test (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            price REAL,
            quantity INTEGER,
            active BOOLEAN,
            data BLOB,
            created_at TEXT
        )
    """)
    cur.execute("DELETE FROM types_test")
    cur.execute("INSERT INTO types_test VALUES (1, 'Widget', 9.99, 100, 1, X'DEADBEEF', '2026-01-15T10:30:00')")
    cur.execute("INSERT INTO types_test VALUES (2, 'Gadget', 24.50, NULL, 0, NULL, '2026-02-20T14:00:00')")
    cur.execute("INSERT INTO types_test VALUES (3, 'Doohickey', 0.50, 9999, 1, X'00', '2026-03-01T00:00:00')")

    # ------------------------------------------------------------------
    # SELECT basics
    # ------------------------------------------------------------------
    def test_select_all():
        cur.execute("SELECT * FROM employees ORDER BY id")
        rows = cur.fetchall()
        assert len(rows) == 3, f"expected 3 rows, got {len(rows)}"
        assert rows[0][1] == "Alice"
        assert rows[1][1] == "Bob"
        assert rows[2][1] == "Charlie"

    run("SELECT all rows", test_select_all)

    def test_select_where():
        cur.execute("SELECT name, salary FROM employees WHERE id = 1")
        row = cur.fetchone()
        assert row is not None
        assert row[0] == "Alice"
        assert abs(row[1] - 75000.50) < 0.01

    run("SELECT with WHERE", test_select_where)

    def test_select_count():
        cur.execute("SELECT COUNT(*) FROM employees")
        count = cur.fetchone()[0]
        # SQLite COUNT(*) may come back as str depending on column type metadata
        assert int(count) == 3, f"expected 3, got {count!r}"

    run("SELECT COUNT(*)", test_select_count)

    def test_select_empty():
        cur.execute("SELECT * FROM employees WHERE id = 999")
        assert cur.fetchone() is None

    run("SELECT with no matching rows", test_select_empty)

    # ------------------------------------------------------------------
    # DDL + DML
    # ------------------------------------------------------------------
    def test_create_insert_drop():
        cur.execute("DROP TABLE IF EXISTS temp_test")
        cur.execute("CREATE TABLE temp_test (id INTEGER PRIMARY KEY, val TEXT)")
        cur.execute("INSERT INTO temp_test VALUES (1, 'hello')")
        cur.execute("INSERT INTO temp_test VALUES (2, 'world')")
        cur.execute("SELECT COUNT(*) FROM temp_test")
        assert int(cur.fetchone()[0]) == 2
        cur.execute("DROP TABLE temp_test")

    run("CREATE + INSERT + DROP", test_create_insert_drop)

    def test_insert_row_count():
        cur.execute("DROP TABLE IF EXISTS rc_test")
        cur.execute("CREATE TABLE rc_test (id INTEGER PRIMARY KEY, val TEXT)")
        count = cur.execute("INSERT INTO rc_test VALUES (1, 'a')").rowcount
        assert count == 1, f"expected rowcount 1, got {count}"
        cur.execute("DROP TABLE rc_test")

    run("INSERT rowcount", test_insert_row_count)

    def test_update():
        cur.execute("UPDATE employees SET salary = 80000.00 WHERE name = 'Alice'")
        cur.execute("SELECT salary FROM employees WHERE name = 'Alice'")
        assert abs(cur.fetchone()[0] - 80000.00) < 0.01
        # restore
        cur.execute("UPDATE employees SET salary = 75000.50 WHERE name = 'Alice'")

    run("UPDATE + verify", test_update)

    def test_update_row_count():
        count = cur.execute("UPDATE employees SET salary = salary WHERE active = 1").rowcount
        assert count == 2, f"expected rowcount 2, got {count}"

    run("UPDATE rowcount", test_update_row_count)

    def test_delete():
        cur.execute("INSERT INTO employees VALUES (99, 'Temp', 10000, 1)")
        count = cur.execute("DELETE FROM employees WHERE id = 99").rowcount
        assert count == 1, f"expected rowcount 1, got {count}"
        cur.execute("SELECT * FROM employees WHERE id = 99")
        assert cur.fetchone() is None

    run("DELETE + verify", test_delete)

    # ------------------------------------------------------------------
    # Aggregation
    # ------------------------------------------------------------------
    def test_group_by():
        cur.execute("DROP TABLE IF EXISTS orders")
        cur.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, customer TEXT, amount REAL)")
        for row in [(1,'Alice',29.99),(2,'Bob',49.99),(3,'Alice',49.99),(4,'Bob',29.99),(5,'Alice',99.99)]:
            cur.execute("INSERT INTO orders VALUES (?,?,?)", row)
        cur.execute("SELECT customer, COUNT(*), SUM(amount) FROM orders GROUP BY customer ORDER BY customer")
        rows = cur.fetchall()
        assert len(rows) == 2
        assert rows[0][0] == "Alice"
        assert int(rows[0][1]) == 3
        assert abs(float(rows[0][2]) - 179.97) < 0.01
        assert rows[1][0] == "Bob"
        assert int(rows[1][1]) == 2
        cur.execute("DROP TABLE orders")

    run("GROUP BY + COUNT + SUM", test_group_by)

    def test_having():
        cur.execute("DROP TABLE IF EXISTS orders2")
        cur.execute("CREATE TABLE orders2 (id INTEGER PRIMARY KEY, customer TEXT, amount REAL)")
        for row in [(1,'Alice',10),(2,'Alice',20),(3,'Bob',30)]:
            cur.execute("INSERT INTO orders2 VALUES (?,?,?)", row)
        cur.execute("SELECT customer, COUNT(*) AS cnt FROM orders2 GROUP BY customer HAVING cnt > 1")
        rows = cur.fetchall()
        assert len(rows) == 1
        assert rows[0][0] == "Alice"
        cur.execute("DROP TABLE orders2")

    run("GROUP BY + HAVING", test_having)

    def test_order_by():
        cur.execute("SELECT name FROM employees ORDER BY salary DESC")
        names = [r[0] for r in cur.fetchall()]
        assert names == ["Charlie", "Alice", "Bob"], f"got {names}"

    run("ORDER BY DESC", test_order_by)

    # ------------------------------------------------------------------
    # JOIN
    # ------------------------------------------------------------------
    def test_join():
        cur.execute("DROP TABLE IF EXISTS departments")
        cur.execute("CREATE TABLE departments (id INTEGER PRIMARY KEY, dept TEXT)")
        cur.execute("INSERT INTO departments VALUES (1, 'Engineering')")
        cur.execute("INSERT INTO departments VALUES (2, 'Marketing')")
        cur.execute("""
            SELECT e.name, d.dept
            FROM employees e JOIN departments d ON e.id = d.id
            ORDER BY e.id
        """)
        rows = cur.fetchall()
        assert len(rows) == 2
        assert rows[0][0] == "Alice" and rows[0][1] == "Engineering"
        assert rows[1][0] == "Bob" and rows[1][1] == "Marketing"
        cur.execute("DROP TABLE departments")

    run("JOIN", test_join)

    # ------------------------------------------------------------------
    # Parameterised queries (folded from test_params.py)
    # ------------------------------------------------------------------
    def test_param_select_int():
        cur.execute("SELECT name, price FROM types_test WHERE id = ?", (1,))
        row = cur.fetchone()
        assert row is not None
        assert row[0] == "Widget"
        assert abs(row[1] - 9.99) < 1e-9

    run("Param: SELECT by integer", test_param_select_int)

    def test_param_select_string():
        cur.execute("SELECT id, price FROM types_test WHERE name = ?", ("Gadget",))
        row = cur.fetchone()
        assert row is not None
        assert row[0] == 2

    run("Param: SELECT by string", test_param_select_string)

    def test_param_no_rows():
        cur.execute("SELECT id FROM types_test WHERE id = ?", (999,))
        assert cur.fetchone() is None

    run("Param: no matching rows", test_param_no_rows)

    def test_param_null_column():
        cur.execute("SELECT quantity FROM types_test WHERE id = ?", (2,))
        row = cur.fetchone()
        assert row is not None
        assert row[0] is None, f"expected NULL, got {row[0]!r}"

    run("Param: NULL column", test_param_null_column)

    def test_param_multiple_rows():
        cur.execute("SELECT id FROM types_test WHERE active = ? ORDER BY id", (1,))
        ids = [r[0] for r in cur.fetchall()]
        assert ids == [1, 3], f"expected [1, 3], got {ids}"

    run("Param: multiple rows", test_param_multiple_rows)

    def test_param_insert():
        cur.execute(
            "INSERT INTO types_test (id, name, price, quantity, active) VALUES (?, ?, ?, ?, ?)",
            (100, "TestItem", 1.23, 42, 1),
        )
        cur.execute("SELECT name, price, quantity FROM types_test WHERE id = ?", (100,))
        row = cur.fetchone()
        assert row is not None
        assert row[0] == "TestItem"
        assert abs(row[1] - 1.23) < 1e-9
        assert row[2] == 42
        cur.execute("DELETE FROM types_test WHERE id = 100")

    run("Param: INSERT + verify", test_param_insert)

    def test_param_reexecute():
        expected = {1: "Widget", 2: "Gadget", 3: "Doohickey"}
        for id_, name in expected.items():
            cur.execute("SELECT name FROM types_test WHERE id = ?", (id_,))
            row = cur.fetchone()
            assert row is not None, f"no row for id={id_}"
            assert row[0] == name, f"id={id_}: expected {name!r}, got {row[0]!r}"

    run("Param: re-execute with different values", test_param_reexecute)

    def test_param_null():
        cur.execute(
            "INSERT INTO types_test (id, name, price) VALUES (?, ?, ?)",
            (101, "NullPrice", None),
        )
        cur.execute("SELECT price FROM types_test WHERE id = ?", (101,))
        row = cur.fetchone()
        assert row is not None
        assert row[0] is None, f"expected NULL, got {row[0]!r}"
        cur.execute("DELETE FROM types_test WHERE id = 101")

    run("Param: NULL binding", test_param_null)

    # ------------------------------------------------------------------
    # Unicode roundtrip
    # ------------------------------------------------------------------
    def test_unicode_roundtrip():
        cur.execute("DROP TABLE IF EXISTS unicode_test")
        cur.execute("CREATE TABLE unicode_test (id INTEGER PRIMARY KEY, val TEXT)")
        values = [
            (1, "日本語"),
            (2, "🎉🦀"),
            (3, "café résumé"),
            (4, "Ünïcödé"),
        ]
        for row in values:
            cur.execute("INSERT INTO unicode_test VALUES (?, ?)", row)
        cur.execute("SELECT id, val FROM unicode_test ORDER BY id")
        rows = cur.fetchall()
        assert len(rows) == len(values), f"expected {len(values)} rows, got {len(rows)}"
        for (row_id, row_val), (exp_id, exp_val) in zip(rows, values):
            assert row_id == exp_id
            assert row_val == exp_val, f"id={exp_id}: expected {exp_val!r}, got {row_val!r}"
        cur.execute("DROP TABLE unicode_test")

    run("Unicode roundtrip (Japanese, emoji, accents)", test_unicode_roundtrip)

    # ------------------------------------------------------------------
    # SQLGetData type coercion
    # ------------------------------------------------------------------
    def test_getdata_integer_as_char():
        import struct
        # Our driver maps SQLite INTEGER columns to SQL_BIGINT (-5).
        # Registering an output converter causes pyodbc to skip SQLBindCol and
        # instead call SQLGetData(SQL_C_BINARY) for that column, exercising the
        # integer→binary coercion path. The converter decodes the 8-byte LE value.
        SQL_BIGINT = -5
        received = []
        def decode_bigint(b):
            val = struct.unpack("<q", b)[0]
            received.append(val)
            return val
        conn.add_output_converter(SQL_BIGINT, decode_bigint)
        try:
            cur.execute("SELECT id FROM employees ORDER BY id")
            ids = [r[0] for r in cur.fetchall()]
            assert received == [1, 2, 3], f"SQLGetData not called or wrong raw values: {received!r}"
            assert ids == [1, 2, 3], f"expected [1, 2, 3], got {ids!r}"
        finally:
            conn.clear_output_converters()

    run("SQLGetData type coercion: INTEGER via add_output_converter", test_getdata_integer_as_char)

    # === Cleanup ===
    cur.execute("DROP TABLE IF EXISTS employees")
    cur.execute("DROP TABLE IF EXISTS types_test")
    conn.close()

    # === Summary ===
    print(f"\n{passed} passed, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
