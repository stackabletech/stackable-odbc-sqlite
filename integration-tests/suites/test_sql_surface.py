#!/usr/bin/env python3
"""
SQL surface pen test for the SQLite ODBC driver.

Walks the SQL a BI tool emits and checks the driver carries it through intact:
joins of every shape, aggregates, window functions, subqueries, CTEs, set
operations, parameters in every clause that accepts one, the ODBC escape
sequences, and the ODBC catalog functions.

Where a query has one right answer it is asserted. Where it does not (a plan
listing), the assertion is that it returns a result of the expected shape, which
is still enough to catch a translation or fetch failure.

Two things this covers that the Trino driver's equivalent cannot:

  - **The ODBC escape sequences.** `{fn ...}`, `{d ...}`, `{ts ...}` and
    `{oj ...}` are translated by `escape_dialect.rs` into what SQLite spells
    them as, and three of them (`CURRENT_DATE`, `CURRENT_TIME`,
    `CURRENT_TIMESTAMP`) are bare keywords that a name swap alone cannot
    produce. Nothing else in the suite exercises that module.
  - **Keys and indexes that are really there.** Trino publishes no primary key,
    foreign key or index metadata, so its suite can only assert that those
    calls return an empty set without erroring. SQLite has all three, so the
    fixture here carries them and the assertions are on real rows.

Usage:
    python3 integration-tests/suites/test_sql_surface.py \
        "Driver=/path/to/libstackable_odbc_sqlite.so;Database=/path/to/test.db"
    python3 integration-tests/suites/test_sql_surface.py "DSN=test_sqlite"

Needs no server. Requires `pyodbc`, normally through `uv run --with pyodbc`.
Creates and drops its own fixture tables, so it does not care what else has run
against the same database.
"""

import os
import sys

import pyodbc

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results, Target  # noqa: E402

R = Results("sql surface")

# A query that hangs is worse than one that errors: it takes the whole suite
# with it and gives no diagnosis. Nothing here should come close.
QUERY_TIMEOUT_SECONDS = 60

PARENT = "sqlsurf_parent"
CHILD = "sqlsurf_child"


def make_fixture(cur):
    """Two related tables, so the key and index calls have something to find.

    Dropped in reverse order: `sqlsurf_child` holds the foreign key, and the
    driver turns foreign-key enforcement on for every connection, so dropping
    the parent first would be refused.
    """
    cur.execute(f"DROP TABLE IF EXISTS {CHILD}")
    cur.execute(f"DROP TABLE IF EXISTS {PARENT}")
    cur.execute(f"CREATE TABLE {PARENT} (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
    cur.execute(
        f"CREATE TABLE {CHILD} ("
        "  id INTEGER PRIMARY KEY,"
        f" parent_id INTEGER REFERENCES {PARENT}(id),"
        "  label TEXT"
        ")"
    )
    cur.execute(f"CREATE INDEX {CHILD}_label_idx ON {CHILD}(label)")
    cur.execute(f"INSERT INTO {PARENT} VALUES (1, 'alpha'), (2, 'beta')")
    cur.execute(f"INSERT INTO {CHILD} VALUES (1, 1, 'x'), (2, 1, 'y'), (3, 2, 'z')")


def drop_fixture(cur):
    try:
        cur.execute(f"DROP TABLE IF EXISTS {CHILD}")
        cur.execute(f"DROP TABLE IF EXISTS {PARENT}")
    except Exception:  # noqa: BLE001
        pass


def main():
    target = Target.from_argv(
        sys.argv,
        "usage: test_sql_surface.py "
        '"Driver=/path/to/libstackable_odbc_sqlite.so;Database=/path/to/test.db"',
    )
    conn = pyodbc.connect(target.conn_str(), autocommit=True)
    conn.timeout = QUERY_TIMEOUT_SECONDS
    cur = conn.cursor()

    def scalar(sql, want, params=None):
        got = (
            cur.execute(sql, params).fetchone()[0]
            if params
            else cur.execute(sql).fetchone()[0]
        )
        assert got == want, f"expected {want!r}, got {got!r}"

    def rows(sql, want_count=None, min_count=None, params=None):
        got = (cur.execute(sql, params) if params else cur.execute(sql)).fetchall()
        if want_count is not None:
            assert len(got) == want_count, f"expected {want_count} rows, got {len(got)}"
        if min_count is not None:
            assert len(got) >= min_count, f"expected >= {min_count} rows, got {len(got)}"
        return got

    def shape(sql, min_cols=1, min_rows=1):
        """Executes and returns rows; asserts only the result's shape."""
        cur.execute(sql)
        assert cur.description is not None, "no result set"
        assert len(cur.description) >= min_cols, f"expected >= {min_cols} columns"
        got = cur.fetchall()
        assert len(got) >= min_rows, f"expected >= {min_rows} rows, got {len(got)}"
        return got

    def refused(sql, near):
        """SQLite must reject `sql`, and the driver must say so rather than
        succeeding with something invented.

        `near` is the token SQLite should be complaining about. Without it this
        would pass for a statement that failed for some unrelated reason, such
        as a typo in the surrounding CTE, and a probe that cannot fail is worth
        nothing.
        """
        try:
            cur.execute(sql).fetchall()
        except pyodbc.Error as e:
            message = str(e)
            assert near in message, (
                f"refused, but not for the expected reason: wanted a complaint "
                f"about {near!r}, got {message!r}"
            )
            return
        raise AssertionError("the statement was accepted, but SQLite has no such syntax")

    make_fixture(cur)
    try:
        # --------------------------------------------------------------
        # SQLite has no `(VALUES ...) AS t(x)` aliasing, so the inline
        # relations below are CTEs, which do take a column list.
        print("--- joins ---")
        R.run("inner join", lambda: scalar(
            "WITH a(x) AS (VALUES (1),(2),(3)), b(y) AS (VALUES (2),(3),(4)) "
            "SELECT count(*) FROM a JOIN b ON a.x = b.y", 2))
        R.run("left outer join", lambda: scalar(
            "WITH a(x) AS (VALUES (1),(2),(3)), b(y) AS (VALUES (2)) "
            "SELECT count(*) FROM a LEFT JOIN b ON a.x = b.y", 3))
        # RIGHT and FULL arrived in SQLite 3.39.0. The bundled library is
        # newer, and SQL_OUTER_JOIN_CAPABILITIES claims both.
        R.run("right outer join", lambda: scalar(
            "WITH a(x) AS (VALUES (1)), b(y) AS (VALUES (1),(2),(3)) "
            "SELECT count(*) FROM a RIGHT JOIN b ON a.x = b.y", 3))
        R.run("full outer join", lambda: scalar(
            "WITH a(x) AS (VALUES (1),(2)), b(y) AS (VALUES (2),(3)) "
            "SELECT count(*) FROM a FULL JOIN b ON a.x = b.y", 3))
        R.run("cross join", lambda: scalar(
            "WITH a(x) AS (VALUES (1),(2),(3)), b(y) AS (VALUES (1),(2)) "
            "SELECT count(*) FROM a CROSS JOIN b", 6))
        R.run("non-equi join", lambda: scalar(
            "WITH a(x) AS (VALUES (1),(2),(3)), b(y) AS (VALUES (1),(2),(3)) "
            "SELECT count(*) FROM a JOIN b ON a.x < b.y", 3))
        R.run("three-way join", lambda: scalar(
            "WITH a(x) AS (VALUES (1),(2)), b(y) AS (VALUES (1),(2)), "
            "c(z) AS (VALUES (1),(2)) "
            "SELECT count(*) FROM a JOIN b ON a.x=b.y JOIN c ON b.y=c.z", 2))
        R.run("join across the fixture tables", lambda: scalar(
            f"SELECT count(*) FROM {PARENT} p JOIN {CHILD} c ON c.parent_id = p.id", 3))

        # --------------------------------------------------------------
        print("\n--- aggregates and GROUP BY ---")
        R.run("count/sum/min/max", lambda: scalar(
            "WITH t(x) AS (VALUES (1),(2),(3)) "
            "SELECT count(*) + sum(x) + min(x) + max(x) FROM t", 3 + 6 + 1 + 3))
        R.run("count(DISTINCT)", lambda: scalar(
            "WITH t(x) AS (VALUES (1),(1),(2)) SELECT count(DISTINCT x) FROM t", 2))
        R.run("avg is a real", lambda: scalar(
            "WITH t(x) AS (VALUES (1),(2)) SELECT avg(x) FROM t", 1.5))
        R.run("GROUP BY", lambda: rows(
            "WITH t(x) AS (VALUES (1),(1),(2)) SELECT x, count(*) FROM t GROUP BY x",
            want_count=2))
        R.run("HAVING", lambda: rows(
            "WITH t(x) AS (VALUES (1),(1),(2)) "
            "SELECT x FROM t GROUP BY x HAVING count(*) > 1", want_count=1))
        R.run("group_concat", lambda: scalar(
            "WITH t(x) AS (VALUES ('a'),('b')) SELECT group_concat(x, '-') FROM t", "a-b"))
        # SQLite has no GROUPING SETS, ROLLUP or CUBE, and the driver claims
        # none: SQL_GROUP_BY reports SQL_GB_NO_RELATION, not the extensions.
        R.run("GROUPING SETS is refused", lambda: refused(
            "WITH t(x,y) AS (VALUES (1,1)) SELECT x FROM t GROUP BY GROUPING SETS ((x),(y))",
            near="SETS"))

        # --------------------------------------------------------------
        print("\n--- window functions ---")
        R.run("row_number", lambda: scalar(
            "WITH t(x) AS (VALUES (1),(2),(3)) "
            "SELECT max(rn) FROM (SELECT row_number() OVER (ORDER BY x) rn FROM t)", 3))
        R.run("rank with PARTITION BY", lambda: rows(
            "WITH t(x,y) AS (VALUES (1,1),(1,2)) "
            "SELECT rank() OVER (PARTITION BY x ORDER BY y) FROM t", want_count=2))
        R.run("lag/lead", lambda: rows(
            "WITH t(x) AS (VALUES (1),(2),(3)) "
            "SELECT lag(x) OVER (ORDER BY x), lead(x) OVER (ORDER BY x) FROM t",
            want_count=3))
        R.run("running sum frame", lambda: scalar(
            "WITH t(x) AS (VALUES (1),(2),(3)) SELECT max(s) FROM ("
            "SELECT sum(x) OVER (ORDER BY x ROWS BETWEEN UNBOUNDED PRECEDING "
            "AND CURRENT ROW) s FROM t)", 6))

        # --------------------------------------------------------------
        print("\n--- subqueries and CTEs ---")
        R.run("scalar subquery", lambda: scalar(
            "SELECT (SELECT max(x) FROM (WITH t(x) AS (VALUES (1),(2),(3)) SELECT x FROM t))",
            3))
        R.run("IN subquery", lambda: scalar(
            "WITH a(x) AS (VALUES (1),(2),(3)), b(y) AS (VALUES (1),(2)) "
            "SELECT count(*) FROM a WHERE a.x IN (SELECT y FROM b)", 2))
        R.run("EXISTS subquery", lambda: scalar(
            "WITH a(x) AS (VALUES (1),(2)), b(y) AS (VALUES (1)) "
            "SELECT count(*) FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.y = a.x)", 1))
        R.run("correlated subquery", lambda: scalar(
            "WITH a(x) AS (VALUES (1),(2)), b(y) AS (VALUES (1)) "
            "SELECT count(*) FROM a WHERE a.x = (SELECT max(y) FROM b)", 1))
        R.run("multiple CTEs", lambda: scalar(
            "WITH a(x) AS (VALUES (1)), b(y) AS (VALUES (2)) SELECT a.x + b.y FROM a, b", 3))
        R.run("recursive CTE", lambda: scalar(
            "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i < 5) "
            "SELECT count(*) FROM c", 5))
        R.run("derived table", lambda: scalar(
            "SELECT count(*) FROM (WITH t(x) AS (VALUES (1),(2),(3)) SELECT x FROM t)", 3))
        # SQL_SUBQUERIES deliberately omits SQL_SQ_QUANTIFIED, and
        # SQL_SQL92_PREDICATES omits SQL_SP_QUANTIFIED_COMPARISON, because
        # SQLite parses neither `< ALL` nor `< ANY`. This is that claim measured
        # rather than asserted: a driver advertising it would have BI tools push
        # down SQL that cannot run.
        R.run("quantified comparison is refused", lambda: refused(
            "WITH a(x) AS (VALUES (1),(2)), b(y) AS (VALUES (1)) "
            "SELECT count(*) FROM a WHERE a.x > ALL (SELECT y FROM b)",
            near="ALL"))

        # --------------------------------------------------------------
        print("\n--- set operations ---")
        R.run("UNION", lambda: scalar(
            "SELECT count(*) FROM (SELECT 1 UNION SELECT 1 UNION SELECT 2)", 2))
        R.run("UNION ALL", lambda: scalar(
            "SELECT count(*) FROM (SELECT 1 UNION ALL SELECT 1)", 2))
        R.run("INTERSECT", lambda: scalar(
            "SELECT count(*) FROM (SELECT 1 INTERSECT SELECT 1)", 1))
        R.run("EXCEPT", lambda: scalar(
            "SELECT count(*) FROM (SELECT 1 EXCEPT SELECT 2)", 1))

        # --------------------------------------------------------------
        print("\n--- parameters in every clause that takes one ---")
        R.run("parameter in SELECT", lambda: scalar("SELECT CAST(? AS INTEGER)", 7, params=[7]))
        R.run("parameter in WHERE", lambda: scalar(
            "WITH t(x) AS (VALUES (1),(2),(3)) SELECT count(*) FROM t WHERE x > ?",
            2, params=[1]))
        R.run("parameter in HAVING", lambda: rows(
            "WITH t(x) AS (VALUES (1),(1),(2)) "
            "SELECT x FROM t GROUP BY x HAVING count(*) > ?", want_count=1, params=[1]))
        R.run("parameter in IN list", lambda: scalar(
            "WITH t(x) AS (VALUES (1),(2),(3)) SELECT count(*) FROM t WHERE x IN (?, ?)",
            2, params=[1, 2]))
        R.run("two parameters, order preserved", lambda: scalar(
            "SELECT CAST(? AS TEXT) || CAST(? AS TEXT)", "ab", params=["a", "b"]))
        R.run("parameter in a join condition", lambda: scalar(
            "WITH a(x) AS (VALUES (1),(2)), b(y) AS (VALUES (1),(2)) "
            "SELECT count(*) FROM a JOIN b ON a.x = b.y AND a.x > ?", 1, params=[1]))
        R.run("NULL parameter", lambda: scalar(
            "SELECT CAST(? AS INTEGER) IS NULL", 1, params=[None]))
        R.run("parameter in LIMIT", lambda: rows(
            "WITH t(x) AS (VALUES (1),(2),(3)) SELECT x FROM t LIMIT ?",
            want_count=2, params=[2]))
        R.run("parameter reused by re-execution", lambda: (
            scalar("SELECT CAST(? AS INTEGER)", 1, params=[1]),
            scalar("SELECT CAST(? AS INTEGER)", 2, params=[2]),
        ))

        # --------------------------------------------------------------
        print("\n--- ODBC escape sequences ---")
        # `escape_dialect.rs` translates these into SQLite's spelling. Nothing
        # else in the suite reaches that module.
        R.run("{fn UCASE}", lambda: scalar("SELECT {fn UCASE('abc')}", "ABC"))
        R.run("{fn LCASE}", lambda: scalar("SELECT {fn LCASE('ABC')}", "abc"))
        R.run("{fn SUBSTRING}", lambda: scalar(
            "SELECT {fn SUBSTRING('hello', 2, 3)}", "ell"))
        R.run("{fn ASCII}", lambda: scalar("SELECT {fn ASCII('A')}", 65))
        R.run("{fn LENGTH}", lambda: scalar("SELECT {fn LENGTH('abcd')}", 4))
        R.run("{fn ABS}", lambda: scalar("SELECT {fn ABS(-3)}", 3))
        R.run("{fn IFNULL}", lambda: scalar("SELECT {fn IFNULL(NULL, 5)}", 5))
        R.run("{fn CONCAT}", lambda: scalar("SELECT {fn CONCAT('a', 'b')}", "ab"))
        # CURDATE/CURTIME/NOW are real callable functions in SQLite once
        # renamed, so a bare name swap works.
        R.run("{fn CURDATE}", lambda: shape("SELECT {fn CURDATE()}"))
        R.run("{fn CURTIME}", lambda: shape("SELECT {fn CURTIME()}"))
        R.run("{fn NOW}", lambda: shape("SELECT {fn NOW()}"))
        # These three are bare keywords: `SELECT CURRENT_DATE();` is a syntax
        # error, so a name swap cannot express them and `rewrite_scalar_fn`
        # replaces the whole escape instead.
        R.run("{fn CURRENT_DATE} (bare keyword)", lambda: shape("SELECT {fn CURRENT_DATE()}"))
        R.run("{fn CURRENT_TIME} (bare keyword)", lambda: shape("SELECT {fn CURRENT_TIME()}"))
        R.run("{fn CURRENT_TIMESTAMP} (bare keyword)",
              lambda: shape("SELECT {fn CURRENT_TIMESTAMP()}"))
        # SQLite has no date/time storage classes, so a date literal is a
        # quoted string and the escape renders to exactly that.
        R.run("{d} date literal", lambda: scalar("SELECT {d '2020-02-03'}", "2020-02-03"))
        R.run("{t} time literal", lambda: scalar("SELECT {t '04:05:06'}", "04:05:06"))
        R.run("{ts} timestamp literal", lambda: scalar(
            "SELECT {ts '2020-02-03 04:05:06'}", "2020-02-03 04:05:06"))
        R.run("{oj} outer join escape", lambda: scalar(
            "WITH a(x) AS (VALUES (1),(2),(3)), b(y) AS (VALUES (2)) "
            "SELECT count(*) FROM {oj a LEFT OUTER JOIN b ON a.x = b.y}", 3))

        # --------------------------------------------------------------
        print("\n--- ODBC catalog functions ---")
        R.run("SQLTables", lambda: (
            cur.tables(table=PARENT).fetchall()
            or (_ for _ in ()).throw(AssertionError("no tables"))))
        R.run("SQLTables table-type enumeration", lambda: (
            cur.tables(catalog="", schema="", table="", tableType="%").fetchall()
            or (_ for _ in ()).throw(AssertionError("no table types"))))
        R.run("SQLColumns", lambda: (
            cur.columns(table=CHILD).fetchall()
            or (_ for _ in ()).throw(AssertionError("no columns"))))
        R.run("SQLGetTypeInfo", lambda: (
            cur.getTypeInfo().fetchall()
            or (_ for _ in ()).throw(AssertionError("no type info"))))
        # SQLite has no catalogs and no schemas, and the driver says so, so the
        # two enumerations must come back empty rather than inventing one.
        R.run("SQLTables catalog enumeration is empty", lambda: (
            cur.tables(catalog="%", schema="", table="").fetchall() == []
            or (_ for _ in ()).throw(AssertionError("a catalog was named"))))
        R.run("SQLTables schema enumeration is empty", lambda: (
            cur.tables(catalog="", schema="%", table="").fetchall() == []
            or (_ for _ in ()).throw(AssertionError("a schema was named"))))

        # Unlike Trino, SQLite publishes all three of these, so the assertion
        # is on real rows rather than on an empty set not erroring.
        def primary_keys_name_the_column():
            found = cur.primaryKeys(table=PARENT).fetchall()
            assert found, "no primary key reported"
            # COLUMN_NAME is column 4 of the SQLPrimaryKeys result set.
            assert found[0][3] == "id", f"expected id, got {found[0][3]!r}"

        R.run("SQLPrimaryKeys names the column", primary_keys_name_the_column)

        def foreign_keys_link_the_tables():
            found = cur.foreignKeys(foreignTable=CHILD).fetchall()
            assert found, "no foreign key reported"
            # PKTABLE_NAME is 3, PKCOLUMN_NAME 4, FKTABLE_NAME 7, FKCOLUMN_NAME 8.
            row = found[0]
            assert row[2] == PARENT, f"expected parent {PARENT}, got {row[2]!r}"
            # `REFERENCES parent(id)` names the column, and the driver must
            # carry it through: PKCOLUMN_NAME is a spec "not NULL" column.
            assert row[3] == "id", f"expected id, got {row[3]!r}"
            assert row[7] == "parent_id", f"expected parent_id, got {row[7]!r}"

        R.run("SQLForeignKeys links the tables", foreign_keys_link_the_tables)

        def statistics_report_the_index():
            found = cur.statistics(table=CHILD).fetchall()
            assert found, "no statistics reported"
            # INDEX_NAME is column 6. The table-stat row carries NULL there.
            names = {r[5] for r in found if r[5] is not None}
            assert f"{CHILD}_label_idx" in names, f"index missing, saw {names}"

        R.run("SQLStatistics reports the index", statistics_report_the_index)

        def special_columns_name_a_row_identifier():
            found = cur.rowIdColumns(table=PARENT).fetchall()
            assert found, "no row identifier reported"

        R.run("SQLSpecialColumns names a row identifier",
              special_columns_name_a_row_identifier)

        # SQLite has no stored procedures, so an empty result set is the
        # correct answer and the assertion is that the call succeeds.
        R.run("SQLProcedures (empty is correct)", lambda: cur.procedures().fetchall())
        R.run("SQLProcedureColumns (empty is correct)",
              lambda: cur.procedureColumns().fetchall())

        # --------------------------------------------------------------
        print("\n--- ordering, distinct, and null handling ---")
        R.run("ORDER BY on an unselected column", lambda: rows(
            "WITH t(x,y) AS (VALUES (1,'b'),(2,'a')) SELECT y FROM t ORDER BY x",
            want_count=2))
        R.run("ORDER BY an expression", lambda: rows(
            "WITH t(x) AS (VALUES (1),(2)) SELECT x FROM t ORDER BY -x", want_count=2))
        # SQLite sorts NULLs first, which is what SQL_NULL_COLLATION reports as
        # SQL_NC_LOW. Core takes NULL placement for the catalog result sets from
        # that same hook, so a change here would move those rows too.
        R.run("NULLs sort first, as SQL_NC_LOW says", lambda: scalar(
            "WITH t(x) AS (VALUES (1),(NULL)) SELECT count(x) FROM "
            "(SELECT x FROM t ORDER BY x LIMIT 1)", 0))
        R.run("DISTINCT", lambda: scalar(
            "WITH t(x) AS (VALUES (1),(1),(2)) "
            "SELECT count(*) FROM (SELECT DISTINCT x FROM t)", 2))
        R.run("CASE expression", lambda: scalar(
            "SELECT CASE WHEN 1 = 1 THEN 'y' ELSE 'n' END", "y"))
        R.run("COALESCE over NULL", lambda: scalar(
            "SELECT coalesce(CAST(NULL AS INTEGER), 5)", 5))

        # --------------------------------------------------------------
        print("\n--- statement forms with undeclared column lengths ---")
        # The driver has to describe a column whose size it cannot know, and an
        # application sizes its buffers from what it says.
        R.run("EXPLAIN", lambda: shape("EXPLAIN SELECT 1", min_rows=1))
        R.run("EXPLAIN QUERY PLAN", lambda: shape("EXPLAIN QUERY PLAN SELECT 1", min_rows=1))
        R.run("PRAGMA table_info", lambda: shape(f"PRAGMA table_info({PARENT})", min_rows=1))
        R.run("sqlite_master", lambda: shape(
            "SELECT name, type FROM sqlite_master ORDER BY name", min_cols=2))
    finally:
        drop_fixture(cur)
        cur.close()
        conn.close()

    return R.summary()


if __name__ == "__main__":
    sys.exit(main())
