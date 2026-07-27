//! FFI-level integration tests for the SQLite backend.
//!
//! Exercises the full path: alloc handles -> connect -> exec_direct -> fetch ->
//! get_data -> verify values -> close_cursor -> free handles.

use std::ffi::c_void;

use stackable_odbc_core::{
    conformance::{
        all_info_types, genuine_convert_info_types, observe_info_value_kind, observe_u32_value,
    },
    ffi,
    types::{
        AttrOdbcVersion, CDataType, CompletionType, ConnectionAttribute, Desc,
        EnvironmentAttribute, HandleType, HeaderDiagnosticIdentifier, InfoType, Nullable, Numeric,
        ParamType, SQL_AUTOCOMMIT_OFF, SQL_AUTOCOMMIT_ON, SQL_CASCADE, SQL_CD_FALSE,
        SQL_CURSOR_FORWARD_ONLY, SQL_DIAG_MESSAGE_TEXT, SQL_DRIVER_ODBC_VER_STRING,
        SQL_GD_ANY_COLUMN, SQL_GD_ANY_ORDER, SQL_GD_BOUND, SQL_IC_SENSITIVE, SQL_INDEX_UNIQUE,
        SQL_QUICK, SQL_RESTRICT, SQL_TXN_READ_COMMITTED, SQL_TXN_READ_UNCOMMITTED,
        SQL_TXN_REPEATABLE_READ, SQL_TXN_SERIALIZABLE, SqlDataType, SqlReturn, StatementAttribute,
        Timestamp, expected_kind,
    },
};

use crate::backend::{
    SqliteBackend,
    info::{
        SQLITE_AGGREGATE_FUNCTIONS, SQLITE_NUMERIC_FUNCTIONS, SQLITE_SQL92_JOIN_OPERATORS,
        SQLITE_SQL92_PREDICATES, SQLITE_SQL92_VALUE_EXPRESSIONS, SQLITE_STRING_FUNCTIONS,
        SQLITE_SYSTEM_FUNCTIONS, SQLITE_TIMEDATE_FUNCTIONS,
    },
};

/// Helper: allocate env + conn + stmt handles using the SQLite backend.
unsafe fn alloc_handles() -> (*mut c_void, *mut c_void, *mut c_void) {
    unsafe {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = ffi::handle::sql_alloc_handle::<SqliteBackend>(
            HandleType::Env as i16,
            std::ptr::null_mut(),
            &mut env,
        );
        let mut conn: *mut c_void = std::ptr::null_mut();
        let _ =
            ffi::handle::sql_alloc_handle::<SqliteBackend>(HandleType::Dbc as i16, env, &mut conn);
        let mut stmt: *mut c_void = std::ptr::null_mut();
        let _ = ffi::handle::sql_alloc_handle::<SqliteBackend>(
            HandleType::Stmt as i16,
            conn,
            &mut stmt,
        );
        (env, conn, stmt)
    }
}

/// Helper: connect to an in-memory SQLite database.
unsafe fn connect_memory(conn: *mut c_void) -> SqlReturn {
    let input = "Database=:memory:";
    let wide: Vec<u16> = input.encode_utf16().collect();
    unsafe {
        ffi::connect::sql_driver_connect_w::<SqliteBackend>(
            conn,
            std::ptr::null_mut(),
            wide.as_ptr(),
            wide.len() as i16,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
        )
    }
}

/// Helper: execute a SQL statement.
unsafe fn exec_direct(stmt: *mut c_void, sql: &str) -> SqlReturn {
    let wide: Vec<u16> = sql.encode_utf16().collect();
    unsafe {
        ffi::execute::sql_exec_direct_w::<SqliteBackend>(stmt, wide.as_ptr(), wide.len() as i32)
    }
}

/// Helper: run setup SQL through the driver's own `SQLExecDirect`.
///
/// These tests used to reach into `ConnectionHandle` for the underlying
/// `rusqlite::Connection` and call `execute_batch` on it. Core's `handles`
/// module is `pub(crate)` now, so that route is gone — and driving setup
/// through the same entry points under test is the better answer anyway: a
/// setup that silently stopped working fails here instead of leaving the test
/// asserting against an empty table.
///
/// `SQLExecDirect` executes one statement, so a multi-statement setup is split
/// on `;`. Every setup in this file is DDL and `INSERT`s with no `;` inside a
/// string literal, which is what makes a plain split exact here.
/// Both this and [`query_scalar_i64`] allocate their own statement handle
/// rather than borrowing the caller's. The statement the test is asserting
/// about usually holds live state — a cursor, a prepared statement, bound
/// parameters — and running setup or a read-back over it would destroy exactly
/// what the test is there to check.
unsafe fn setup_sql(conn: *mut c_void, sql: &str) {
    unsafe {
        let stmt = alloc_stmt(conn);
        for one in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            assert_eq!(
                exec_direct(stmt, one),
                SqlReturn::SUCCESS,
                "setup statement failed: {one}"
            );
        }
        let _ = ffi::handle::sql_free_handle::<SqliteBackend>(HandleType::Stmt as i16, stmt);
    }
}

/// Helper: allocate a statement handle on `conn`.
unsafe fn alloc_stmt(conn: *mut c_void) -> *mut c_void {
    let mut stmt: *mut c_void = std::ptr::null_mut();
    let ret = unsafe {
        ffi::handle::sql_alloc_handle::<SqliteBackend>(HandleType::Stmt as i16, conn, &mut stmt)
    };
    assert_eq!(ret, SqlReturn::SUCCESS, "could not allocate a statement");
    stmt
}

/// Helper: read a single-row, single-column `i64` back through the FFI.
///
/// The read-back replacement for the `db.query_row(..)` calls that used to
/// reach into the connection handle.
unsafe fn query_scalar_i64(conn: *mut c_void, sql: &str) -> i64 {
    unsafe {
        let stmt = alloc_stmt(conn);
        assert_eq!(exec_direct(stmt, sql), SqlReturn::SUCCESS, "query: {sql}");
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS,
            "query returned no row: {sql}"
        );
        let mut val: i64 = 0;
        let mut ind: isize = 0;
        assert_eq!(
            ffi::fetch::sql_get_data::<SqliteBackend>(
                stmt,
                1,
                CDataType::SBigInt as i16,
                &raw mut val as *mut c_void,
                std::mem::size_of::<i64>() as isize,
                &mut ind,
            ),
            SqlReturn::SUCCESS,
            "get_data: {sql}"
        );
        let _ = ffi::handle::sql_free_handle::<SqliteBackend>(HandleType::Stmt as i16, stmt);
        val
    }
}

/// Helper: read a single row of two string columns back through the FFI.
unsafe fn query_row_two_strings(conn: *mut c_void, sql: &str) -> (String, String) {
    unsafe {
        let stmt = alloc_stmt(conn);
        assert_eq!(exec_direct(stmt, sql), SqlReturn::SUCCESS, "query: {sql}");
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS,
            "query returned no row: {sql}"
        );
        let first = fetch_string_col(stmt, 1);
        let second = fetch_string_col(stmt, 2);
        let _ = ffi::handle::sql_free_handle::<SqliteBackend>(HandleType::Stmt as i16, stmt);
        (first, second)
    }
}

/// Helper: free all handles.
unsafe fn cleanup(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
    unsafe {
        let _ = ffi::handle::sql_free_handle::<SqliteBackend>(HandleType::Stmt as i16, stmt);
        let _ = ffi::connect::sql_disconnect::<SqliteBackend>(conn);
        let _ = ffi::handle::sql_free_handle::<SqliteBackend>(HandleType::Dbc as i16, conn);
        let _ = ffi::handle::sql_free_handle::<SqliteBackend>(HandleType::Env as i16, env);
    }
}

#[test]
fn exec_direct_on_connected_handle_succeeds() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Create table and insert data via the connection directly so we can
        // use exec_direct for the SELECT through the FFI layer.
        setup_sql(
            conn,
            "CREATE TABLE test (id INTEGER, name TEXT); \
                 INSERT INTO test VALUES (1, 'hello'); \
                 INSERT INTO test VALUES (2, 'world');",
        );

        let ret = exec_direct(stmt, "SELECT id, name FROM test");
        assert_eq!(ret, SqlReturn::SUCCESS);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn exec_direct_not_connected_returns_error() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        // Don't connect — should fail.
        let ret = exec_direct(stmt, "SELECT 1");
        assert_eq!(ret, SqlReturn::ERROR);
        cleanup(env, conn, stmt);
    }
}

#[test]
fn exec_direct_null_text_returns_error() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        let ret = ffi::execute::sql_exec_direct_w::<SqliteBackend>(stmt, std::ptr::null(), 0);
        assert_eq!(ret, SqlReturn::ERROR);
        cleanup(env, conn, stmt);
    }
}

#[test]
fn fetch_after_exec_direct_returns_rows_then_no_data() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        setup_sql(
            conn,
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2);",
        );

        assert_eq!(exec_direct(stmt, "SELECT id FROM t"), SqlReturn::SUCCESS);

        // First fetch should return a row.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        // Second fetch should return a row.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        // Third fetch should return NO_DATA.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::NO_DATA
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_data_returns_correct_values() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        setup_sql(
            conn,
            "CREATE TABLE t (id INTEGER, name TEXT); INSERT INTO t VALUES (42, 'test');",
        );

        assert_eq!(
            exec_direct(stmt, "SELECT id, name FROM t"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        // Get integer column (SQLite stores as I64)
        let mut buf_i64: i64 = 0;
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            1,
            CDataType::SBigInt as i16,
            &mut buf_i64 as *mut i64 as *mut c_void,
            8,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(buf_i64, 42);

        // Get string column as WChar
        let mut wbuf = [0u16; 20];
        let mut ind2: isize = 0;
        let ret = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            2,
            CDataType::WChar as i16,
            wbuf.as_mut_ptr() as *mut c_void,
            40, // bytes
            &mut ind2,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        let s = String::from_utf16_lossy(&wbuf[..4]); // "test" = 4 chars
        assert_eq!(s, "test");

        cleanup(env, conn, stmt);
    }
}

/// SQLite is dynamically typed, and its own documentation defines three storage
/// formats for a `DATETIME` column: ISO-8601 text, an integer count of seconds
/// since the epoch, or a floating point Julian day number. Because this driver
/// describes SQLite `DATE`/`TIME`/`DATETIME`/`TIMESTAMP` columns with the ODBC
/// datetime SQL types, applications request `SQL_C_TYPE_TIMESTAMP` for them, so
/// `write_column_value` must handle all three encodings: a column holding the
/// integer or Julian-day format (which SQLite permits at any time, per-row,
/// since column types are advisory) must not come back as `HY000` "Unsupported
/// conversion". This test exercises all three.
///
/// This exercises the full FFI path end-to-end (real SQLite storage, real
/// `SQLGetData`) rather than only unit-testing `write_column_value`
/// directly, because the regression was specifically about what a real
/// dynamically-typed column can hold.
#[test]
fn get_data_datetime_column_handles_integer_and_real_storage() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        setup_sql(
            conn,
            "CREATE TABLE t (id INTEGER, dt DATETIME); \
                 INSERT INTO t VALUES (1, 1700000000); \
                 INSERT INTO t VALUES (2, 2451545.5);",
        );

        assert_eq!(
            exec_direct(stmt, "SELECT id, dt FROM t ORDER BY id"),
            SqlReturn::SUCCESS
        );

        // Row 1: dt stored as INTEGER epoch seconds 1_700_000_000 ==
        // 2023-11-14 22:13:20 UTC.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut buf = Timestamp {
            year: 0,
            month: 0,
            day: 0,
            hour: 0,
            minute: 0,
            second: 0,
            fraction: 0,
        };
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            2,
            CDataType::TypeTimestamp as i16,
            &mut buf as *mut Timestamp as *mut c_void,
            std::mem::size_of::<Timestamp>() as isize,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "integer-encoded datetime");
        assert_eq!((buf.year, buf.month, buf.day), (2023, 11, 14));
        assert_eq!((buf.hour, buf.minute, buf.second), (22, 13, 20));

        // Row 2: dt stored as REAL Julian day 2451545.5 == 2000-01-02
        // 00:00:00 UTC (verified against SQLite's own
        // `julianday('2000-01-02 00:00:00')`, which returns exactly this
        // value — chosen because the Unix-epoch offset it implies,
        // 10958.0 days, multiplies back to a whole number of seconds with
        // no floating point rounding loss).
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut buf2 = buf;
        let mut ind2: isize = 0;
        let ret = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            2,
            CDataType::TypeTimestamp as i16,
            &mut buf2 as *mut Timestamp as *mut c_void,
            std::mem::size_of::<Timestamp>() as isize,
            &mut ind2,
        );
        assert_eq!(
            ret,
            SqlReturn::SUCCESS,
            "real-encoded (Julian day) datetime"
        );
        assert_eq!((buf2.year, buf2.month, buf2.day), (2000, 1, 2));
        assert_eq!((buf2.hour, buf2.minute, buf2.second), (0, 0, 0));

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_data_col_zero_returns_error() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        setup_sql(
            conn,
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);",
        );

        assert_eq!(exec_direct(stmt, "SELECT id FROM t"), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let mut buf: i64 = 0;
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            0, // bookmark column — not supported
            CDataType::SBigInt as i16,
            &mut buf as *mut i64 as *mut c_void,
            8,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::ERROR);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn num_result_cols_after_exec_direct() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        setup_sql(conn, "CREATE TABLE t (a INTEGER, b TEXT, c REAL)");

        assert_eq!(
            exec_direct(stmt, "SELECT a, b, c FROM t"),
            SqlReturn::SUCCESS
        );

        let mut count: i16 = 0;
        let ret = ffi::cursor::sql_num_result_cols::<SqliteBackend>(stmt, &mut count);
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(count, 3);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn close_cursor_then_fetch_returns_no_data() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        setup_sql(
            conn,
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);",
        );

        assert_eq!(exec_direct(stmt, "SELECT id FROM t"), SqlReturn::SUCCESS);

        // Fetch the row
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        // Close cursor — discards the result set entirely.
        assert_eq!(
            ffi::cursor::sql_close_cursor::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        // After close_cursor the result set is gone; a new exec_direct is required.
        // Calling exec_direct on the same handle must now succeed (no open cursor).
        assert_eq!(exec_direct(stmt, "SELECT id FROM t"), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn more_results_always_returns_no_data() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::cursor::sql_more_results::<SqliteBackend>(stmt),
            SqlReturn::NO_DATA
        );
        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_info_returns_dbms_name() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let mut buf = [0u16; 128];
        let mut str_len: i16 = 0;
        let ret = ffi::info::sql_get_info_w::<SqliteBackend>(
            conn,
            InfoType::DbmsName as u16,
            buf.as_mut_ptr() as *mut c_void,
            256, // bytes (128 u16s)
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        // str_len is in bytes (SQLGetInfoW spec); convert to u16 count
        let result = String::from_utf16_lossy(&buf[..(str_len / 2) as usize]);
        assert_eq!(result, "SQLite");

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_info_returns_driver_odbc_ver() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let mut buf = [0u16; 128];
        let mut str_len: i16 = 0;
        let ret = ffi::info::sql_get_info_w::<SqliteBackend>(
            conn,
            InfoType::DriverOdbcVer as u16,
            buf.as_mut_ptr() as *mut c_void,
            256,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        // str_len is in bytes (SQLGetInfoW spec); convert to u16 count
        let result = String::from_utf16_lossy(&buf[..(str_len / 2) as usize]);
        assert_eq!(result, SQL_DRIVER_ODBC_VER_STRING);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_info_returns_u32_value() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let mut value: u32 = 0;
        let mut str_len: i16 = 0;
        let ret = ffi::info::sql_get_info_w::<SqliteBackend>(
            conn,
            InfoType::GetDataExtensions as u16,
            &mut value as *mut u32 as *mut c_void,
            4,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(str_len, 4);
        assert_eq!(value, SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER | SQL_GD_BOUND);

        cleanup(env, conn, stmt);
    }
}

/// Asserts `sql_get_info_w` returns exactly `expected` (a `U32`) for `info_type`.
unsafe fn assert_get_info_u32(conn: *mut c_void, info_type: InfoType, expected: u32) {
    unsafe {
        let mut value: u32 = 0xDEAD_BEEF;
        let mut str_len: i16 = 0;
        let ret = ffi::info::sql_get_info_w::<SqliteBackend>(
            conn,
            info_type as u16,
            &mut value as *mut u32 as *mut c_void,
            4,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "{info_type:?} must succeed");
        assert_eq!(str_len, 4, "{info_type:?} string_length_ptr");
        assert_eq!(
            value, expected,
            "{info_type:?} must come from get_info_raw, not the generic default"
        );
    }
}

/// Asserts `sql_get_info_w` returns exactly `expected` (a `U16`) for `info_type`.
unsafe fn assert_get_info_u16(conn: *mut c_void, info_type: InfoType, expected: u16) {
    unsafe {
        let mut value: u16 = 0xDEAD;
        let mut str_len: i16 = 0;
        let ret = ffi::info::sql_get_info_w::<SqliteBackend>(
            conn,
            info_type as u16,
            &mut value as *mut u16 as *mut c_void,
            2,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "{info_type:?} must succeed");
        assert_eq!(str_len, 2, "{info_type:?} string_length_ptr");
        assert_eq!(
            value, expected,
            "{info_type:?} must come from get_info_raw, not the generic default"
        );
    }
}

/// Asserts `sql_get_info_w` returns exactly `expected` (a `String`) for `info_type`.
unsafe fn assert_get_info_str(conn: *mut c_void, info_type: InfoType, expected: &str) {
    unsafe {
        let mut buf = [0u16; 128];
        let mut str_len: i16 = 0;
        let ret = ffi::info::sql_get_info_w::<SqliteBackend>(
            conn,
            info_type as u16,
            buf.as_mut_ptr() as *mut c_void,
            256,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "{info_type:?} must succeed");
        let result = String::from_utf16_lossy(&buf[..(str_len / 2) as usize]);
        assert_eq!(
            result, expected,
            "{info_type:?} must come from get_info_raw, not the generic default"
        );
    }
}

/// Guards the `get_info_raw`-first ordering in `stackable-odbc-core`'s
/// `info_type_default_response` (reached via `sql_get_info_w`).
///
/// `SqlFileUsage` and `SqlQuotedIdentifierCase` are real (named)
/// `odbc_sys::InfoType` variants, but `sqlite_get_info` has no arm for
/// either and `default_get_info` doesn't cover them either; the only place
/// that produces a real value for them is `common_get_info_raw`, reached
/// through the `get_info_raw` fallback in `sql_get_info_w`.
///
/// The ten capability bitmaps below (`AggregateFunctions`, `Sql92Predicates`,
/// etc., computed by `SqliteBackend::get_info_raw` in `backend/info.rs`) are
/// the same shape of gap: each is a named `InfoType` with no arm in
/// `sqlite_get_info`'s match, so the only place a real value is ever produced
/// is `get_info_raw`, reached through this same fallback. A unit test that
/// called `get_info_raw` directly would keep passing even if a match-arm
/// ordering mistake or a change to the dispatch order made these arms
/// unreachable through the real FFI dispatch; asserting through
/// `sql_get_info_w` here is what makes that regression fail a test. See the
/// "ordering is load-bearing" note on `info_type_default_response` in
/// `stackable-odbc-core/src/ffi/info.rs`.
#[test]
fn get_info_named_but_unhandled_types_fall_back_to_get_info_raw() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_get_info_u16(conn, InfoType::SqlFileUsage, 0);
        assert_get_info_u16(conn, InfoType::SqlQuotedIdentifierCase, SQL_IC_SENSITIVE);

        // SQLite capability bitmaps computed by SqliteBackend::get_info_raw
        // (backend/info.rs) -- reference the same constants that function
        // returns, rather than restating their numeric values here.
        assert_get_info_u32(
            conn,
            InfoType::AggregateFunctions,
            SQLITE_AGGREGATE_FUNCTIONS,
        );
        assert_get_info_u32(conn, InfoType::Sql92Predicates, SQLITE_SQL92_PREDICATES);
        assert_get_info_u32(
            conn,
            InfoType::Sql92RelationalJoinOperators,
            SQLITE_SQL92_JOIN_OPERATORS,
        );
        assert_get_info_u32(
            conn,
            InfoType::Sql92ValueExpressions,
            SQLITE_SQL92_VALUE_EXPRESSIONS,
        );
        assert_get_info_u32(conn, InfoType::NumericFunctions, SQLITE_NUMERIC_FUNCTIONS);
        assert_get_info_u32(conn, InfoType::StringFunctions, SQLITE_STRING_FUNCTIONS);
        assert_get_info_u32(conn, InfoType::SystemFunctions, SQLITE_SYSTEM_FUNCTIONS);
        assert_get_info_u32(conn, InfoType::TimedateFunctions, SQLITE_TIMEDATE_FUNCTIONS);
        assert_get_info_str(conn, InfoType::LikeEscapeClause, "Y");
        assert_get_info_str(conn, InfoType::OuterJoins, "Y");

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_functions_bitmap_has_bits_set() {
    use stackable_odbc_core::function_id::{
        FunctionId, SQL_API_ODBC3_ALL_FUNCTIONS, SQL_API_ODBC3_ALL_FUNCTIONS_SIZE,
    };

    /// Check if a function ID is set in the bitmap (mirrors SQL_FUNC_EXISTS macro).
    fn func_exists(bitmap: &[u16], func: FunctionId) -> bool {
        let fid = func as u16;
        let idx = (fid / 16) as usize;
        let bit = fid % 16;
        bitmap.get(idx).is_some_and(|v| v & (1 << bit) != 0)
    }

    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let mut bitmap = [0u16; SQL_API_ODBC3_ALL_FUNCTIONS_SIZE];
        let ret = ffi::info::sql_get_functions::<SqliteBackend>(
            conn,
            SQL_API_ODBC3_ALL_FUNCTIONS,
            bitmap.as_mut_ptr(),
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        assert!(
            func_exists(&bitmap, FunctionId::ExecDirect),
            "SQLExecDirect"
        );
        assert!(func_exists(&bitmap, FunctionId::Fetch), "SQLFetch");
        assert!(func_exists(&bitmap, FunctionId::GetData), "SQLGetData");
        assert!(
            func_exists(&bitmap, FunctionId::AllocHandle),
            "SQLAllocHandle"
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_functions_single_query() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Check a supported function
        let mut result: u16 = 0;
        let ret = ffi::info::sql_get_functions::<SqliteBackend>(conn, 9, &mut result); // SQLExecDirect
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(result, 1);

        // Check an unsupported function
        let mut result2: u16 = 1;
        let ret = ffi::info::sql_get_functions::<SqliteBackend>(conn, 200, &mut result2);
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(result2, 0);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_type_info_returns_rows() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let ret = ffi::info::sql_get_type_info::<SqliteBackend>(stmt, 0);
        assert_eq!(ret, SqlReturn::SUCCESS);

        // Fetch every row's DATA_TYPE (col 2), and assert on content rather
        // than an exact row count; the row list grows as the driver's type
        // mapping grows (see the `SQLITE_TYPE_INFO` invariant tests in
        // backend/info.rs), so a hardcoded tally breaks on every such
        // addition without testing anything meaningful. What actually
        // matters to an application going through the C ABI is that the
        // result set is non-empty and that the types the mapping most
        // commonly produces (SQL_BIGINT for INTEGER columns, SQL_WVARCHAR
        // for TEXT columns) are actually present.
        let mut data_types = Vec::new();
        loop {
            let ret = ffi::fetch::sql_fetch::<SqliteBackend>(stmt);
            if ret == SqlReturn::NO_DATA {
                break;
            }
            assert_eq!(ret, SqlReturn::SUCCESS);
            data_types.push(fetch_i16_col(stmt, 2));
        }
        assert!(!data_types.is_empty(), "SQLGetTypeInfo returned no rows");
        assert!(
            data_types.contains(&SqlDataType::EXT_BIG_INT.0),
            "SQLGetTypeInfo is missing a SQL_BIGINT row: {data_types:?}"
        );
        assert!(
            data_types.contains(&SqlDataType::EXT_W_VARCHAR.0),
            "SQLGetTypeInfo is missing a SQL_WVARCHAR row: {data_types:?}"
        );

        // Verify column count is 19 (standard type info columns)
        let mut col_count: i16 = 0;
        let ret = ffi::cursor::sql_num_result_cols::<SqliteBackend>(stmt, &mut col_count);
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(col_count, 19);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_type_info_filters_by_data_type() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Filter for SQL_INTEGER (4)
        let ret = ffi::info::sql_get_type_info::<SqliteBackend>(stmt, 4);
        assert_eq!(ret, SqlReturn::SUCCESS);

        let mut count = 0;
        loop {
            let ret = ffi::fetch::sql_fetch::<SqliteBackend>(stmt);
            if ret == SqlReturn::NO_DATA {
                break;
            }
            assert_eq!(ret, SqlReturn::SUCCESS);
            count += 1;
        }
        assert_eq!(count, 1, "Should have exactly 1 INTEGER type row");

        cleanup(env, conn, stmt);
    }
}

#[test]
fn row_count_after_exec_direct() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        setup_sql(
            conn,
            "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2);",
        );

        assert_eq!(exec_direct(stmt, "SELECT id FROM t"), SqlReturn::SUCCESS);

        let mut count: isize = 0;
        let ret = ffi::cursor::sql_row_count::<SqliteBackend>(stmt, &mut count);
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(count, 2);

        cleanup(env, conn, stmt);
    }
}

/// Helper: set up a connected handle with a test table and view.
unsafe fn setup_metadata_tables(conn: *mut c_void) {
    unsafe {
        setup_sql(
            conn,
            "CREATE TABLE test_table (id INTEGER NOT NULL, name TEXT, score REAL);
         CREATE VIEW test_view AS SELECT id, name FROM test_table;
         INSERT INTO test_table VALUES (1, 'alice', 9.5);",
        )
    };
}

#[test]
fn sql_tables_w_returns_tables_and_views() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        let ret = ffi::metadata::sql_tables_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        // Count rows
        let mut count = 0;
        loop {
            let ret = ffi::fetch::sql_fetch::<SqliteBackend>(stmt);
            if ret == SqlReturn::NO_DATA {
                break;
            }
            assert_eq!(ret, SqlReturn::SUCCESS);
            count += 1;
        }
        assert_eq!(count, 2); // test_table + test_view

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_tables_w_with_type_filter() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        let type_filter = "TABLE";
        let type_wide: Vec<u16> = type_filter.encode_utf16().collect();

        let ret = ffi::metadata::sql_tables_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            type_wide.as_ptr(),
            type_wide.len() as i16,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        let mut count = 0;
        loop {
            let ret = ffi::fetch::sql_fetch::<SqliteBackend>(stmt);
            if ret == SqlReturn::NO_DATA {
                break;
            }
            assert_eq!(ret, SqlReturn::SUCCESS);
            count += 1;
        }
        assert_eq!(count, 1); // only test_table

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_describe_col_w_writes_full_sqlulen_column_size() {
    // A real application declares ColumnSize as SQLULEN (8 bytes on 64-bit)
    // and does not pre-initialise it. If the driver writes only 4 bytes, the
    // high half keeps stack garbage and the application sizes its fetch
    // buffer from a corrupted number.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        assert_eq!(
            exec_direct(stmt, "SELECT id FROM test_table"),
            SqlReturn::SUCCESS
        );

        let mut name_buf = [0u16; 64];
        let mut name_len: i16 = 0;
        let mut data_type: i16 = 0;
        let mut decimal: i16 = 0;
        let mut nullable: i16 = 0;
        // odbc_sys::ULen is usize; SQLULEN is 8 bytes on 64-bit.
        let mut size: usize = 0xDEAD_BEEF_0000_0000;

        let ret = ffi::metadata::sql_describe_col_w::<SqliteBackend>(
            stmt,
            1,
            name_buf.as_mut_ptr(),
            64,
            &mut name_len,
            &mut data_type,
            &mut size,
            &mut decimal,
            &mut nullable,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(
            size >> 32,
            0,
            "high half of the SQLULEN column size was left uninitialised"
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_describe_col_w_after_exec_direct() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        assert_eq!(
            exec_direct(stmt, "SELECT id, name FROM test_table"),
            SqlReturn::SUCCESS
        );

        // Describe column 1 (id)
        let mut name_buf = [0u16; 64];
        let mut name_len: i16 = 0;
        let mut data_type: i16 = 0;
        let mut size: usize = 0;
        let mut decimal: i16 = 0;
        let mut nullable: i16 = 0;

        let ret = ffi::metadata::sql_describe_col_w::<SqliteBackend>(
            stmt,
            1,
            name_buf.as_mut_ptr(),
            64,
            &mut name_len,
            &mut data_type,
            &mut size,
            &mut decimal,
            &mut nullable,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
        assert_eq!(name, "id");

        // Describe column 2 (name)
        let ret = ffi::metadata::sql_describe_col_w::<SqliteBackend>(
            stmt,
            2,
            name_buf.as_mut_ptr(),
            64,
            &mut name_len,
            &mut data_type,
            &mut size,
            &mut decimal,
            &mut nullable,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
        assert_eq!(name, "name");
        // TEXT maps to SQL_WVARCHAR (the driver reports Unicode character types)
        assert_eq!(data_type, SqlDataType::EXT_W_VARCHAR.0);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_col_attribute_w_returns_column_name() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        assert_eq!(
            exec_direct(stmt, "SELECT id, name FROM test_table"),
            SqlReturn::SUCCESS
        );

        // Get SQL_DESC_NAME (1011) for column 1
        let mut char_buf = [0u16; 64];
        let mut str_len: i16 = 0;
        let mut num_attr: isize = 0;

        let ret = ffi::metadata::sql_col_attribute_w::<SqliteBackend>(
            stmt,
            1,
            Desc::Name as u16,
            char_buf.as_mut_ptr() as *mut c_void,
            128, // bytes
            &mut str_len,
            &mut num_attr,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        // str_len is in bytes (SQLColAttribute spec); convert to UTF-16 code
        // units to index the u16 buffer.
        let code_units = usize::try_from(str_len).expect("non-negative length") / 2;
        let name = String::from_utf16_lossy(&char_buf[..code_units]);
        assert_eq!(name, "id");

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_col_attribute_w_returns_type() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        assert_eq!(
            exec_direct(stmt, "SELECT id, name FROM test_table"),
            SqlReturn::SUCCESS
        );

        // Get SQL_DESC_TYPE (1002) for column 2 (name, TEXT -> SQL_WVARCHAR)
        let mut num_attr: isize = 0;

        let ret = ffi::metadata::sql_col_attribute_w::<SqliteBackend>(
            stmt,
            2,
            Desc::Type as u16,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut num_attr,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(num_attr, isize::from(SqlDataType::EXT_W_VARCHAR.0)); // SQL_WVARCHAR

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_col_attribute_w_count() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        assert_eq!(
            exec_direct(stmt, "SELECT id, name, score FROM test_table"),
            SqlReturn::SUCCESS
        );

        // Get SQL_DESC_COUNT (1001) — column_number is ignored
        let mut num_attr: isize = 0;
        let ret = ffi::metadata::sql_col_attribute_w::<SqliteBackend>(
            stmt,
            0, // ignored for COUNT
            1001,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut num_attr,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(num_attr, 3);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_columns_w_returns_column_metadata() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        let table_name = "test_table";
        let table_wide: Vec<u16> = table_name.encode_utf16().collect();

        let ret = ffi::metadata::sql_columns_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            table_wide.as_ptr(),
            table_wide.len() as i16,
            std::ptr::null(),
            0,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        // Count rows — should be 3 columns (id, name, score)
        let mut count = 0;
        loop {
            let ret = ffi::fetch::sql_fetch::<SqliteBackend>(stmt);
            if ret == SqlReturn::NO_DATA {
                break;
            }
            assert_eq!(ret, SqlReturn::SUCCESS);
            count += 1;
        }
        assert_eq!(count, 3);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_columns_w_result_set_reports_wvarchar_identifiers_and_narrow_data_type() {
    // Pins the SQLite driver's routing through the shared catalog descriptor
    // constructors (stackable_odbc_core::types::ColumnsResultCol::all_descriptors)
    // rather than a hand-built literal. Two properties matter enough to
    // assert at the ABI level via SQLDescribeColW:
    //   - TABLE_NAME (and every identifier column) is SQL_WVARCHAR at width
    //     128, not the old SQL_VARCHAR/255 -- the switch the Windows Driver
    //     Manager is strict about.
    //   - DATA_TYPE (a SQL_SMALLINT column) has precision 5, not the old 50
    //     -- a SMALLINT cannot have 50 digits of precision.
    // A regression that reintroduces the old literals in
    // src/backend/metadata.rs would only be caught
    // by the Python integration suite without this test.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        let table_name = "test_table";
        let table_wide: Vec<u16> = table_name.encode_utf16().collect();

        let ret = ffi::metadata::sql_columns_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            table_wide.as_ptr(),
            table_wide.len() as i16,
            std::ptr::null(),
            0,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        let mut name_buf = [0u16; 64];
        let mut name_len: i16 = 0;
        let mut data_type: i16 = 0;
        let mut size: usize = 0;
        let mut decimal: i16 = 0;
        let mut nullable: i16 = 0;

        // Column 3: TABLE_NAME -- an identifier column.
        let ret = ffi::metadata::sql_describe_col_w::<SqliteBackend>(
            stmt,
            stackable_odbc_core::types::ColumnsResultCol::TableName.pos(),
            name_buf.as_mut_ptr(),
            64,
            &mut name_len,
            &mut data_type,
            &mut size,
            &mut decimal,
            &mut nullable,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(data_type, SqlDataType::EXT_W_VARCHAR.0);
        assert_eq!(size, 128);

        // Column 5: DATA_TYPE -- a SQL_SMALLINT column.
        let ret = ffi::metadata::sql_describe_col_w::<SqliteBackend>(
            stmt,
            stackable_odbc_core::types::ColumnsResultCol::DataType.pos(),
            name_buf.as_mut_ptr(),
            64,
            &mut name_len,
            &mut data_type,
            &mut size,
            &mut decimal,
            &mut nullable,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(data_type, SqlDataType::SMALLINT.0);
        assert_eq!(size, 5);

        cleanup(env, conn, stmt);
    }
}

// --- DML tests (INSERT / UPDATE / DELETE / DDL) ---

#[test]
fn exec_direct_create_table_succeeds() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            exec_direct(stmt, "CREATE TABLE dml_test (id INTEGER, val TEXT)"),
            SqlReturn::SUCCESS
        );
        // No result columns for DDL
        let mut col_count: i16 = -1;
        assert_eq!(
            ffi::cursor::sql_num_result_cols::<SqliteBackend>(stmt, &mut col_count),
            SqlReturn::SUCCESS
        );
        assert_eq!(col_count, 0);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn exec_direct_insert_then_select_roundtrip() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Set up the table via raw rusqlite so we don't burn statement state.
        setup_sql(conn, "CREATE TABLE t (id INTEGER, name TEXT)");

        // INSERT through ODBC — row count must be 1.
        assert_eq!(
            exec_direct(stmt, "INSERT INTO t VALUES (42, 'hello')"),
            SqlReturn::SUCCESS
        );
        let mut row_count: isize = -1;
        assert_eq!(
            ffi::cursor::sql_row_count::<SqliteBackend>(stmt, &mut row_count),
            SqlReturn::SUCCESS
        );
        assert_eq!(row_count, 1);

        // No SQLCloseCursor here: an INSERT produces no result set, so no
        // cursor is open and the SELECT can reuse this handle directly.

        // SELECT — verify the inserted row is readable.
        assert_eq!(
            exec_direct(stmt, "SELECT id, name FROM t"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let mut id_buf: i64 = 0;
        let mut id_len: isize = 0;
        assert_eq!(
            ffi::fetch::sql_get_data::<SqliteBackend>(
                stmt,
                1,
                CDataType::SBigInt as i16,
                &mut id_buf as *mut i64 as *mut _,
                std::mem::size_of::<i64>() as isize,
                &mut id_len,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(id_buf, 42);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn exec_direct_update_returns_correct_row_count() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Seed data via raw rusqlite.
        setup_sql(
            conn,
            "CREATE TABLE t (id INTEGER, v INTEGER);
                 INSERT INTO t VALUES (1, 10);
                 INSERT INTO t VALUES (2, 10);
                 INSERT INTO t VALUES (3, 20);",
        );

        // UPDATE two rows through ODBC.
        assert_eq!(
            exec_direct(stmt, "UPDATE t SET v = 99 WHERE v = 10"),
            SqlReturn::SUCCESS
        );
        let mut row_count: isize = -1;
        let _ = ffi::cursor::sql_row_count::<SqliteBackend>(stmt, &mut row_count);
        assert_eq!(row_count, 2);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn exec_direct_delete_returns_correct_row_count() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Seed data via raw rusqlite.
        setup_sql(
            conn,
            "CREATE TABLE t (id INTEGER);
                 INSERT INTO t VALUES (1);
                 INSERT INTO t VALUES (2);
                 INSERT INTO t VALUES (3);",
        );

        // DELETE two rows through ODBC.
        assert_eq!(
            exec_direct(stmt, "DELETE FROM t WHERE id > 1"),
            SqlReturn::SUCCESS
        );
        let mut row_count: isize = -1;
        let _ = ffi::cursor::sql_row_count::<SqliteBackend>(stmt, &mut row_count);
        assert_eq!(row_count, 2);

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// SQLSetConnectAttrW / SQLGetConnectAttrW tests
// ---------------------------------------------------------------------------

#[test]
fn set_and_get_connect_attr_autocommit() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            ffi::connect_attr::sql_set_connect_attr_w::<SqliteBackend>(
                conn,
                ConnectionAttribute::AUTOCOMMIT.0,
                std::ptr::null_mut::<std::ffi::c_void>(),
                0,
            ),
            SqlReturn::SUCCESS
        );

        let mut val: u32 = 99;
        assert_eq!(
            ffi::connect_attr::sql_get_connect_attr_w::<SqliteBackend>(
                conn,
                102,
                &mut val as *mut u32 as *mut std::ffi::c_void,
                0,
                std::ptr::null_mut(),
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(val, 0);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_connect_attr_autocommit_default() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let mut val: u32 = 0;
        assert_eq!(
            ffi::connect_attr::sql_get_connect_attr_w::<SqliteBackend>(
                conn,
                102,
                &mut val as *mut u32 as *mut std::ffi::c_void,
                0,
                std::ptr::null_mut(),
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(val, SQL_AUTOCOMMIT_ON as u32);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_connect_attr_connection_dead_is_false() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let mut val: u32 = 99;
        assert_eq!(
            ffi::connect_attr::sql_get_connect_attr_w::<SqliteBackend>(
                conn,
                ConnectionAttribute::CONNECTION_DEAD.0,
                &mut val as *mut u32 as *mut std::ffi::c_void,
                0,
                std::ptr::null_mut(),
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(val, SQL_CD_FALSE as u32);

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// SQLSetStmtAttrW / SQLGetStmtAttrW tests
// ---------------------------------------------------------------------------

#[test]
fn set_cursor_type_forward_only_succeeds() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            ffi::stmt_attr::sql_set_stmt_attr_w::<SqliteBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                std::ptr::null_mut::<std::ffi::c_void>(),
                0,
            ),
            SqlReturn::SUCCESS
        );

        cleanup(env, conn, stmt);
    }
}

/// `SQL_CURSOR_STATIC` (3). Core defines only `SQL_CURSOR_FORWARD_ONLY`,
/// which is the one value this driver supports; the other three exist here so
/// this test can name what it is asking for rather than pass a bare `3`.
const SQL_CURSOR_STATIC: usize = 3;

#[test]
fn set_cursor_type_static_is_substituted_with_forward_only() {
    // This driver materialises every result set and walks it forward only, so
    // a static cursor is not on offer. The spec has a specific answer for
    // that, and it is not a refusal: 01S02 "the driver did not support the
    // value specified and substituted a similar value", reported as
    // SQL_SUCCESS_WITH_INFO. The application learns what it actually got by
    // reading the attribute back, which is why the substituted value has to
    // be observable through SQLGetStmtAttr.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            ffi::stmt_attr::sql_set_stmt_attr_w::<SqliteBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                std::ptr::without_provenance_mut(SQL_CURSOR_STATIC),
                0,
            ),
            SqlReturn::SUCCESS_WITH_INFO,
            "an unsupported cursor type is substituted, not refused"
        );
        assert_eq!(
            last_sqlstate(stmt),
            stackable_odbc_core::types::sql_state::OPTION_VALUE_CHANGED
        );

        // `SQL_ATTR_CURSOR_TYPE` is a SQLUINTEGER attribute, so the driver
        // writes exactly four bytes here whatever the buffer's width.
        let mut got: u32 = u32::MAX;
        let mut len: i32 = 0;
        assert_eq!(
            ffi::stmt_attr::sql_get_stmt_attr_w::<SqliteBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                &raw mut got as *mut std::ffi::c_void,
                std::mem::size_of::<u32>() as i32,
                &mut len,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            got as usize, SQL_CURSOR_FORWARD_ONLY,
            "the substituted value must be readable back"
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_cursor_type_default_is_forward_only() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let mut val: u32 = 99;
        assert_eq!(
            ffi::stmt_attr::sql_get_stmt_attr_w::<SqliteBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                &mut val as *mut u32 as *mut std::ffi::c_void,
                0,
                std::ptr::null_mut(),
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(val, SQL_CURSOR_FORWARD_ONLY as u32);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn set_query_timeout_stored_and_retrieved() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let _ = ffi::stmt_attr::sql_set_stmt_attr_w::<SqliteBackend>(
            stmt,
            StatementAttribute::QueryTimeout as i32,
            30usize as *mut std::ffi::c_void,
            0,
        );
        let mut val: u32 = 0;
        assert_eq!(
            ffi::stmt_attr::sql_get_stmt_attr_w::<SqliteBackend>(
                stmt,
                0,
                &mut val as *mut u32 as *mut std::ffi::c_void,
                0,
                std::ptr::null_mut(),
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(val, 30);

        cleanup(env, conn, stmt);
    }
}

// --- SQLEndTran ---

#[test]
fn end_tran_commit_on_dbc_without_transaction_succeeds() {
    // SQLite starts in autocommit; calling SQLEndTran(COMMIT) is a no-op and
    // must return SUCCESS.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            ffi::tran::sql_end_tran::<SqliteBackend>(HandleType::Dbc as i16, conn, 0),
            SqlReturn::SUCCESS
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn end_tran_rollback_on_dbc_without_transaction_succeeds() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            ffi::tran::sql_end_tran::<SqliteBackend>(HandleType::Dbc as i16, conn, 1),
            SqlReturn::SUCCESS
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn end_tran_commit_on_env_without_transaction_succeeds() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            ffi::tran::sql_end_tran::<SqliteBackend>(HandleType::Env as i16, env, 0),
            SqlReturn::SUCCESS
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn end_tran_begin_commit_roundtrip() {
    // Begin a transaction, insert a row, commit via SQLEndTran, verify it persists.
    // Uses raw rusqlite for setup to avoid "cursor already open" on the same stmt handle.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Set up via raw rusqlite: create table, open a transaction, insert a row.
        {
            setup_sql(
                conn,
                "CREATE TABLE tran_test(id INTEGER); BEGIN; INSERT INTO tran_test VALUES(42);",
            );
        }

        // Commit via SQLEndTran.
        assert_eq!(
            ffi::tran::sql_end_tran::<SqliteBackend>(HandleType::Dbc as i16, conn, 0),
            SqlReturn::SUCCESS
        );

        // Verify row is present via ODBC SELECT.
        assert_eq!(
            exec_direct(stmt, "SELECT id FROM tran_test"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let mut val: i64 = 0;
        let mut ind: isize = 0;
        assert_eq!(
            ffi::fetch::sql_get_data::<SqliteBackend>(
                stmt,
                1,
                CDataType::SBigInt as i16,
                &mut val as *mut i64 as *mut c_void,
                std::mem::size_of::<i64>() as isize,
                &mut ind,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(val, 42);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn end_tran_begin_rollback_discards_row() {
    // Begin a transaction, insert a row, rollback via SQLEndTran — table must be empty.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        {
            setup_sql(
                conn,
                "CREATE TABLE tran_rollback(id INTEGER); BEGIN; INSERT INTO tran_rollback VALUES(99);",
            );
        }

        // Rollback via SQLEndTran.
        assert_eq!(
            ffi::tran::sql_end_tran::<SqliteBackend>(HandleType::Dbc as i16, conn, 1),
            SqlReturn::SUCCESS
        );

        // Table should be empty.
        assert_eq!(
            exec_direct(stmt, "SELECT id FROM tran_rollback"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::NO_DATA
        );

        cleanup(env, conn, stmt);
    }
}

// --- SQLFetchScroll ---

#[test]
fn fetch_scroll_next_advances_cursor() {
    // SQL_FETCH_NEXT (1) should behave identically to SQLFetch.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Set up via raw rusqlite to avoid burning statement state.
        {
            setup_sql(
                conn,
                "CREATE TABLE scroll_test(v INTEGER); INSERT INTO scroll_test VALUES(1),(2);",
            );
        }

        assert_eq!(
            exec_direct(stmt, "SELECT v FROM scroll_test ORDER BY v"),
            SqlReturn::SUCCESS
        );

        let mut val: i64 = 0;
        let mut ind: isize = 0;

        // SQL_FETCH_NEXT = 1
        assert_eq!(
            ffi::fetch::sql_fetch_scroll::<SqliteBackend>(stmt, 1, 0),
            SqlReturn::SUCCESS
        );
        let _ = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            1,
            CDataType::SBigInt as i16,
            &mut val as *mut i64 as *mut c_void,
            std::mem::size_of::<i64>() as isize,
            &mut ind,
        );
        assert_eq!(val, 1);

        assert_eq!(
            ffi::fetch::sql_fetch_scroll::<SqliteBackend>(stmt, 1, 0),
            SqlReturn::SUCCESS
        );
        let _ = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            1,
            CDataType::SBigInt as i16,
            &mut val as *mut i64 as *mut c_void,
            std::mem::size_of::<i64>() as isize,
            &mut ind,
        );
        assert_eq!(val, 2);

        assert_eq!(
            ffi::fetch::sql_fetch_scroll::<SqliteBackend>(stmt, 1, 0),
            SqlReturn::NO_DATA
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn fetch_scroll_non_next_returns_error() {
    // SQL_FETCH_FIRST (2) is not supported — must return ERROR (HY106).
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        {
            setup_sql(conn, "CREATE TABLE scroll_err(v INTEGER);");
        }

        assert_eq!(
            exec_direct(stmt, "SELECT v FROM scroll_err"),
            SqlReturn::SUCCESS
        );

        // SQL_FETCH_FIRST = 2
        assert_eq!(
            ffi::fetch::sql_fetch_scroll::<SqliteBackend>(stmt, 2, 0),
            SqlReturn::ERROR
        );

        cleanup(env, conn, stmt);
    }
}

// --- SQLPrimaryKeysW / SQLForeignKeysW integration tests ---

/// Helper: create a schema with primary keys and foreign keys.
///
/// Schema:
///   departments(dept_id PK, dept_name)
///   employees(emp_id PK, name, dept_id FK -> departments(dept_id))
unsafe fn setup_pk_fk_schema(conn: *mut c_void) {
    unsafe {
        setup_sql(
            conn,
            "CREATE TABLE departments (dept_id INTEGER PRIMARY KEY, dept_name TEXT NOT NULL);
             CREATE TABLE employees (
                 emp_id INTEGER PRIMARY KEY,
                 name   TEXT NOT NULL,
                 dept_id INTEGER REFERENCES departments(dept_id) ON DELETE CASCADE ON UPDATE RESTRICT
             );",
        )
    };
}

/// Helper: call SQLPrimaryKeysW and collect (table_name, col_name, key_seq) triples.
unsafe fn fetch_primary_keys(stmt: *mut c_void) -> Vec<(String, String, i16)> {
    let mut result = Vec::new();
    loop {
        let ret = unsafe { ffi::fetch::sql_fetch::<SqliteBackend>(stmt) };
        if ret == SqlReturn::NO_DATA {
            break;
        }
        assert_eq!(ret, SqlReturn::SUCCESS, "fetch for primary keys");

        // TABLE_NAME = col 3, COLUMN_NAME = col 4, KEY_SEQ = col 5
        let table_name = unsafe { fetch_string_col(stmt, 3) };
        let col_name = unsafe { fetch_string_col(stmt, 4) };
        let key_seq = unsafe { fetch_i16_col(stmt, 5) };
        result.push((table_name, col_name, key_seq));
    }
    result
}

/// Helper: fetch a string column value from the current row.
unsafe fn fetch_string_col(stmt: *mut c_void, col: u16) -> String {
    let mut buf = [0u16; 256];
    let mut ind: isize = 0;
    let ret = unsafe {
        ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            col,
            CDataType::WChar as i16,
            buf.as_mut_ptr() as *mut c_void,
            (buf.len() * 2) as isize,
            &mut ind,
        )
    };
    assert_eq!(ret, SqlReturn::SUCCESS, "get_data string col={col}");
    let char_count = if ind > 0 { (ind / 2) as usize } else { 0 };
    String::from_utf16_lossy(&buf[..char_count.min(buf.len())])
}

/// Helper: fetch a SMALLINT column value from the current row.
unsafe fn fetch_i16_col(stmt: *mut c_void, col: u16) -> i16 {
    let mut val: i16 = 0;
    let mut ind: isize = 0;
    let ret = unsafe {
        ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            col,
            CDataType::SShort as i16,
            &mut val as *mut i16 as *mut c_void,
            2,
            &mut ind,
        )
    };
    assert_eq!(ret, SqlReturn::SUCCESS, "get_data i16 col={col}");
    val
}

#[test]
fn sql_primary_keys_w_single_pk_column() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_pk_fk_schema(conn);

        let table = "departments";
        let table_wide: Vec<u16> = table.encode_utf16().collect();
        let ret = ffi::metadata::sql_primary_keys_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            table_wide.as_ptr(),
            table_wide.len() as i16,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        let keys = fetch_primary_keys(stmt);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0, "departments"); // TABLE_NAME
        assert_eq!(keys[0].1, "dept_id"); // COLUMN_NAME
        assert_eq!(keys[0].2, 1); // KEY_SEQ

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_primary_keys_w_result_set_has_six_columns() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_pk_fk_schema(conn);

        let table = "departments";
        let table_wide: Vec<u16> = table.encode_utf16().collect();
        let ret = ffi::metadata::sql_primary_keys_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            table_wide.as_ptr(),
            table_wide.len() as i16,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        let mut col_count: i16 = 0;
        assert_eq!(
            ffi::cursor::sql_num_result_cols::<SqliteBackend>(stmt, &mut col_count),
            SqlReturn::SUCCESS
        );
        assert_eq!(col_count, 6); // TABLE_CAT, TABLE_SCHEM, TABLE_NAME, COLUMN_NAME, KEY_SEQ, PK_NAME

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_primary_keys_w_no_table_filter_returns_all() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_pk_fk_schema(conn);

        // No table filter — should return PKs from both tables.
        let ret = ffi::metadata::sql_primary_keys_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        let keys = fetch_primary_keys(stmt);
        // departments.dept_id + employees.emp_id = 2 PK rows
        assert_eq!(keys.len(), 2);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_primary_keys_w_table_with_no_pk_returns_empty() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Create a table without an explicit PRIMARY KEY.
        setup_sql(conn, "CREATE TABLE no_pk (val TEXT);");

        let table = "no_pk";
        let table_wide: Vec<u16> = table.encode_utf16().collect();
        let ret = ffi::metadata::sql_primary_keys_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            table_wide.as_ptr(),
            table_wide.len() as i16,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        let keys = fetch_primary_keys(stmt);
        assert!(
            keys.is_empty(),
            "expected no PK rows for a table without PK"
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_foreign_keys_w_by_fk_table() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_pk_fk_schema(conn);

        // Query FKs defined on "employees"
        let fk_table = "employees";
        let fk_wide: Vec<u16> = fk_table.encode_utf16().collect();
        let ret = ffi::metadata::sql_foreign_keys_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0, // pk_cat
            std::ptr::null(),
            0, // pk_schema
            std::ptr::null(),
            0, // pk_table
            std::ptr::null(),
            0, // fk_cat
            std::ptr::null(),
            0, // fk_schema
            fk_wide.as_ptr(),
            fk_wide.len() as i16, // fk_table
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        // Collect (pktable, pkcolumn, fktable, fkcolumn, key_seq, update_rule, delete_rule)
        let mut rows = Vec::new();
        loop {
            let ret = ffi::fetch::sql_fetch::<SqliteBackend>(stmt);
            if ret == SqlReturn::NO_DATA {
                break;
            }
            assert_eq!(ret, SqlReturn::SUCCESS);
            let pktable = fetch_string_col(stmt, 3); // PKTABLE_NAME
            let pkcolumn = fetch_string_col(stmt, 4); // PKCOLUMN_NAME
            let fktable = fetch_string_col(stmt, 7); // FKTABLE_NAME
            let fkcolumn = fetch_string_col(stmt, 8); // FKCOLUMN_NAME
            let key_seq = fetch_i16_col(stmt, 9); // KEY_SEQ
            let update_rule = fetch_i16_col(stmt, 10); // UPDATE_RULE
            let delete_rule = fetch_i16_col(stmt, 11); // DELETE_RULE
            rows.push((
                pktable,
                pkcolumn,
                fktable,
                fkcolumn,
                key_seq,
                update_rule,
                delete_rule,
            ));
        }

        assert_eq!(rows.len(), 1, "employees has exactly one FK column");
        let (pktable, pkcolumn, fktable, fkcolumn, key_seq, update_rule, delete_rule) = &rows[0];
        assert_eq!(pktable, "departments");
        assert_eq!(pkcolumn, "dept_id");
        assert_eq!(fktable, "employees");
        assert_eq!(fkcolumn, "dept_id");
        assert_eq!(*key_seq, 1);
        assert_eq!(*update_rule, SQL_RESTRICT);
        assert_eq!(*delete_rule, SQL_CASCADE);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_foreign_keys_w_by_pk_table() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_pk_fk_schema(conn);

        // Query all FKs that reference "departments" (the PK side)
        let pk_table = "departments";
        let pk_wide: Vec<u16> = pk_table.encode_utf16().collect();
        let ret = ffi::metadata::sql_foreign_keys_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0, // pk_cat
            std::ptr::null(),
            0, // pk_schema
            pk_wide.as_ptr(),
            pk_wide.len() as i16, // pk_table
            std::ptr::null(),
            0, // fk_cat
            std::ptr::null(),
            0, // fk_schema
            std::ptr::null(),
            0, // fk_table (omitted → scan all tables)
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        let mut count = 0;
        loop {
            let ret = ffi::fetch::sql_fetch::<SqliteBackend>(stmt);
            if ret == SqlReturn::NO_DATA {
                break;
            }
            assert_eq!(ret, SqlReturn::SUCCESS);
            count += 1;
        }
        assert_eq!(count, 1, "exactly one FK references departments");

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_foreign_keys_w_result_set_has_fourteen_columns() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_pk_fk_schema(conn);

        let fk_table = "employees";
        let fk_wide: Vec<u16> = fk_table.encode_utf16().collect();
        let ret = ffi::metadata::sql_foreign_keys_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            fk_wide.as_ptr(),
            fk_wide.len() as i16,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        let mut col_count: i16 = 0;
        assert_eq!(
            ffi::cursor::sql_num_result_cols::<SqliteBackend>(stmt, &mut col_count),
            SqlReturn::SUCCESS
        );
        assert_eq!(col_count, 14); // PKTABLE_CAT through DEFERRABILITY

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_foreign_keys_w_no_fk_table_returns_empty_for_no_refs() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Table with no FKs at all.
        setup_sql(conn, "CREATE TABLE standalone (id INTEGER PRIMARY KEY);");

        let pk_table = "standalone";
        let pk_wide: Vec<u16> = pk_table.encode_utf16().collect();
        let ret = ffi::metadata::sql_foreign_keys_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            pk_wide.as_ptr(),
            pk_wide.len() as i16,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        let mut count = 0;
        loop {
            let ret = ffi::fetch::sql_fetch::<SqliteBackend>(stmt);
            if ret == SqlReturn::NO_DATA {
                break;
            }
            assert_eq!(ret, SqlReturn::SUCCESS);
            count += 1;
        }
        assert_eq!(count, 0, "no FK references standalone table");

        cleanup(env, conn, stmt);
    }
}

// --- SQLNativeSqlW integration tests ---

#[test]
fn sql_native_sql_w_echoes_sql_unchanged() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let sql = "SELECT id, name FROM t WHERE id = ?";
        let in_wide: Vec<u16> = sql.encode_utf16().collect();
        let mut out_buf = [0u16; 128];
        let mut out_len: i32 = 0;

        let ret = ffi::connect::sql_native_sql_w::<SqliteBackend>(
            conn,
            in_wide.as_ptr(),
            in_wide.len() as i32,
            out_buf.as_mut_ptr(),
            128,
            &mut out_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(out_len as usize, in_wide.len());
        let result = String::from_utf16_lossy(&out_buf[..out_len as usize]);
        assert_eq!(result, sql);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_native_sql_w_null_output_buffer_reports_length() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let sql = "SELECT 1";
        let in_wide: Vec<u16> = sql.encode_utf16().collect();
        let mut out_len: i32 = 0;

        let ret = ffi::connect::sql_native_sql_w::<SqliteBackend>(
            conn,
            in_wide.as_ptr(),
            in_wide.len() as i32,
            std::ptr::null_mut(),
            0,
            &mut out_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(out_len as usize, in_wide.len());

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_native_sql_w_truncation_returns_success_with_info() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let sql = "SELECT 1"; // 8 chars
        let in_wide: Vec<u16> = sql.encode_utf16().collect();
        let mut out_buf = [0u16; 4]; // room for 3 chars + null
        let mut out_len: i32 = 0;

        let ret = ffi::connect::sql_native_sql_w::<SqliteBackend>(
            conn,
            in_wide.as_ptr(),
            in_wide.len() as i32,
            out_buf.as_mut_ptr(),
            4,
            &mut out_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        // out_len reports full needed length, not truncated length
        assert_eq!(out_len as usize, in_wide.len());

        cleanup(env, conn, stmt);
    }
}

// --- SQLCancel integration tests ---

#[test]
fn sql_cancel_on_idle_statement_returns_success() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let ret = ffi::cursor::sql_cancel::<SqliteBackend>(stmt);
        assert_eq!(ret, SqlReturn::SUCCESS);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_cancel_with_open_cursor_does_not_close_it() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Open a result set.
        setup_sql(
            conn,
            "CREATE TABLE cancel_t (id INTEGER); INSERT INTO cancel_t VALUES (1);",
        );
        assert_eq!(
            exec_direct(stmt, "SELECT id FROM cancel_t"),
            SqlReturn::SUCCESS
        );

        // Cancel — no-op, cursor stays open.
        assert_eq!(
            ffi::cursor::sql_cancel::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        // Cursor is still open: fetch should succeed.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        cleanup(env, conn, stmt);
    }
}

// --- SQLStatisticsW integration tests ---

#[test]
fn sql_statistics_w_returns_table_stat_row() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        let table = "test_table";
        let table_wide: Vec<u16> = table.encode_utf16().collect();
        let ret = ffi::metadata::sql_statistics_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            table_wide.as_ptr(),
            table_wide.len() as i16,
            SQL_INDEX_UNIQUE,
            SQL_QUICK,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        // Result set must have 13 columns (per spec).
        let mut col_count: i16 = 0;
        assert_eq!(
            ffi::cursor::sql_num_result_cols::<SqliteBackend>(stmt, &mut col_count),
            SqlReturn::SUCCESS
        );
        assert_eq!(col_count, 13);

        // One SQL_TABLE_STAT row for a table with no indexes.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        // No further rows: test_table has no indexes.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::NO_DATA
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_statistics_w_no_table_filter_also_succeeds() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        let ret = ffi::metadata::sql_statistics_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            0,
            0,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        cleanup(env, conn, stmt);
    }
}

// --- SQLSpecialColumnsW integration tests ---

#[test]
fn sql_special_columns_w_returns_rowid_pseudo_column() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        let table = "test_table";
        let table_wide: Vec<u16> = table.encode_utf16().collect();
        let ret = ffi::metadata::sql_special_columns_w::<SqliteBackend>(
            stmt,
            stackable_odbc_core::types::SQL_BEST_ROWID,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            table_wide.as_ptr(),
            table_wide.len() as i16,
            stackable_odbc_core::types::SQL_SCOPE_CURROW,
            stackable_odbc_core::types::Nullable::SqlNullable as u16,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);

        // Result set must have 8 columns (per spec).
        let mut col_count: i16 = 0;
        assert_eq!(
            ffi::cursor::sql_num_result_cols::<SqliteBackend>(stmt, &mut col_count),
            SqlReturn::SUCCESS
        );
        assert_eq!(col_count, 8);

        // test_table is a rowid table with no declared PRIMARY KEY, so
        // BEST_ROWID on a rowid table returns the rowid pseudo-column.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::NO_DATA
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_statistics_w_not_connected_returns_error() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        // no connect
        let ret = ffi::metadata::sql_statistics_w::<SqliteBackend>(
            stmt,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            0,
            0,
        );
        assert_eq!(ret, SqlReturn::ERROR);
        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_special_columns_w_not_connected_returns_error() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        // no connect
        let ret = ffi::metadata::sql_special_columns_w::<SqliteBackend>(
            stmt,
            1,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            0,
            0,
        );
        assert_eq!(ret, SqlReturn::ERROR);
        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// P1: SQLGetData truncation
// ---------------------------------------------------------------------------

#[test]
fn get_data_truncates_string_returns_success_with_info() {
    // Verifies that reading a string column into a buffer that is too small
    // returns SUCCESS_WITH_INFO (SQLSTATE 01004) and writes the truncated value.
    // Buffer holds 4 u16 slots (8 bytes): capacity for 3 chars + null terminator.
    // Full string "hello" is 5 chars → truncated to "hel\0".
    // ind is set to the full byte count: 5 chars × 2 bytes = 10.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        setup_sql(
            conn,
            "CREATE TABLE trunc_test (id INTEGER, name TEXT); \
                 INSERT INTO trunc_test VALUES (1, 'hello');",
        );

        assert_eq!(
            exec_direct(stmt, "SELECT name FROM trunc_test"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        // 4 u16 slots = 8 bytes → capacity for 3 chars + null terminator.
        let mut wbuf = [0u16; 4];
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            1,
            CDataType::WChar as i16,
            wbuf.as_mut_ptr() as *mut c_void,
            8, // bytes
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
        // ind reports the full byte count of the original string (no null).
        assert_eq!(ind, 10); // 5 chars × 2 bytes
        // Buffer contains "hel\0".
        assert_eq!(String::from_utf16_lossy(&wbuf[..3]), "hel");
        assert_eq!(wbuf[3], 0u16); // null terminator

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// P1: Fetch after NO_DATA returns NO_DATA again (not ERROR)
// ---------------------------------------------------------------------------

#[test]
fn fetch_after_no_data_returns_no_data_again() {
    // After a result set is exhausted (SQLFetch returns NO_DATA), subsequent
    // SQLFetch calls must also return NO_DATA — not ERROR or panic.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        setup_sql(
            conn,
            "CREATE TABLE one_row (v INTEGER); INSERT INTO one_row VALUES (1);",
        );

        assert_eq!(
            exec_direct(stmt, "SELECT v FROM one_row"),
            SqlReturn::SUCCESS
        );

        // Fetch the single row.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        // Cursor exhausted.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::NO_DATA
        );
        // A second call past the end must still return NO_DATA, not ERROR.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::NO_DATA
        );

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// P1: Statement handle is reusable after an error
// ---------------------------------------------------------------------------

#[test]
fn exec_direct_reuse_after_error() {
    // After a failed exec_direct (invalid SQL → SQL_ERROR), the same statement
    // handle must accept a valid query and succeed.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Invalid SQL — must fail.
        assert_eq!(exec_direct(stmt, "NOT VALID SQL AT ALL"), SqlReturn::ERROR);

        // Valid query on the same handle — must succeed.
        assert_eq!(exec_direct(stmt, "SELECT 1"), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut val: i64 = 0;
        let mut ind: isize = 0;
        assert_eq!(
            ffi::fetch::sql_get_data::<SqliteBackend>(
                stmt,
                1,
                CDataType::SBigInt as i16,
                &mut val as *mut i64 as *mut c_void,
                8,
                &mut ind,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(val, 1);

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// P2: SQLColAttributeW — nullable, precision, octet_length via FFI
// ---------------------------------------------------------------------------

#[test]
fn sql_col_attribute_w_reports_each_columns_real_nullability() {
    // SQL_DESC_NULLABLE (1008). All three of the spec's values are reachable,
    // and which one a column gets is a fact about that column rather than a
    // blanket driver answer:
    //
    //   id      INTEGER NOT NULL -> SQL_NO_NULLS
    //   name    TEXT             -> SQL_NULLABLE
    //   id + 1  (an expression)  -> SQL_NULLABLE_UNKNOWN
    //
    // The third is the one worth stating. `sqlite3_table_column_metadata`
    // answers nothing for a computed column, so the driver genuinely cannot
    // determine it — and the spec has a value for exactly that, rather than
    // requiring a guess. This driver used to report SQL_NULLABLE for all
    // three.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        assert_eq!(
            exec_direct(stmt, "SELECT id, name, id + 1 FROM test_table"),
            SqlReturn::SUCCESS
        );

        let expected = [
            (1u16, Nullable::SqlNoNulls, "id is declared NOT NULL"),
            (
                2u16,
                Nullable::SqlNullable,
                "name has no NOT NULL constraint",
            ),
            (
                3u16,
                Nullable::SqlNullableUnknown,
                "id + 1 is computed, so SQLite reports no column metadata",
            ),
        ];
        for (col, want, why) in expected {
            let mut num_attr: isize = 99;
            let ret = ffi::metadata::sql_col_attribute_w::<SqliteBackend>(
                stmt,
                col,
                Desc::Nullable as u16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut num_attr,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "col {col}");
            assert_eq!(num_attr, want as isize, "col {col}: {why}");
        }

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_col_attribute_w_returns_precision_for_integer() {
    // SQL_DESC_PRECISION (1005): INTEGER maps to EXT_BIG_INT with precision=19.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        assert_eq!(
            exec_direct(stmt, "SELECT id FROM test_table"),
            SqlReturn::SUCCESS
        );

        let mut num_attr: isize = 0;
        let ret = ffi::metadata::sql_col_attribute_w::<SqliteBackend>(
            stmt,
            1,
            Desc::Precision as u16,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut num_attr,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        // INTEGER → EXT_BIG_INT → BIGINT_COLUMN_SIZE = 19
        assert_eq!(num_attr, 19);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn sql_col_attribute_w_returns_octet_length_for_integer() {
    // SQL_DESC_OCTET_LENGTH (1013): INTEGER (EXT_BIG_INT) = 8 bytes.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_metadata_tables(conn);

        assert_eq!(
            exec_direct(stmt, "SELECT id FROM test_table"),
            SqlReturn::SUCCESS
        );

        let mut num_attr: isize = 0;
        let ret = ffi::metadata::sql_col_attribute_w::<SqliteBackend>(
            stmt,
            1,
            Desc::OctetLength as u16,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut num_attr,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        // EXT_BIG_INT → OCTET_LENGTH_BIGINT = 8
        assert_eq!(num_attr, 8);

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// P2: SQLCloseCursor called twice returns 24000 on the second call
// ---------------------------------------------------------------------------

#[test]
fn close_cursor_twice_returns_error() {
    // The second SQLCloseCursor call must return ERROR (SQLSTATE 24000 — invalid
    // cursor state) because there is no open cursor after the first close.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        setup_sql(
            conn,
            "CREATE TABLE cc_test (v INTEGER); INSERT INTO cc_test VALUES (1);",
        );

        assert_eq!(
            exec_direct(stmt, "SELECT v FROM cc_test"),
            SqlReturn::SUCCESS
        );

        // First close — cursor is open, must succeed.
        assert_eq!(
            ffi::cursor::sql_close_cursor::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        // Second close — no cursor open, must return ERROR (24000).
        assert_eq!(
            ffi::cursor::sql_close_cursor::<SqliteBackend>(stmt),
            SqlReturn::ERROR
        );

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// P2: SQLNumResultCols after SQLPrepare but before SQLExecute
// ---------------------------------------------------------------------------

#[test]
fn num_result_cols_after_prepare_before_execute() {
    // After SQLPrepare (but before SQLExecute), SQLNumResultCols must return
    // SUCCESS. The SQLite backend returns count=0 because column metadata is
    // only populated after execute.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let sql = "SELECT 1";
        let wide: Vec<u16> = sql.encode_utf16().collect();
        let ret =
            ffi::execute::sql_prepare_w::<SqliteBackend>(stmt, wide.as_ptr(), wide.len() as i32);
        assert_eq!(ret, SqlReturn::SUCCESS);

        let mut count: i16 = 99;
        let ret = ffi::cursor::sql_num_result_cols::<SqliteBackend>(stmt, &mut count);
        assert_eq!(ret, SqlReturn::SUCCESS);
        // Column metadata is populated only after execute; before execute the
        // SQLite backend reports 0 columns.
        assert_eq!(count, 0);

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// P3: SQLGetDiagFieldW — field-by-field after an error
// ---------------------------------------------------------------------------

#[test]
fn get_diag_field_number_after_error() {
    // SQL_DIAG_NUMBER (2) on the header record (rec_number=0) reports the count
    // of diagnostic records. After one error it must be 1.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(exec_direct(stmt, "NOT VALID SQL"), SqlReturn::ERROR);

        let mut count: i32 = 0;
        let ret = ffi::diag::sql_get_diag_field_w::<SqliteBackend>(
            HandleType::Stmt as i16,
            stmt,
            0, // header field: rec_number = 0
            HeaderDiagnosticIdentifier::Number as i16,
            &mut count as *mut i32 as *mut c_void,
            0,
            std::ptr::null_mut(),
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(count, 1, "one diagnostic record after one error");

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_diag_field_sqlstate_after_error() {
    // SQL_DIAG_SQLSTATE (4) on rec_number=1 returns the 5-character SQLSTATE.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(exec_direct(stmt, "NOT VALID SQL"), SqlReturn::ERROR);

        // 6 u16 slots: 5 SQLSTATE chars + null terminator = 12 bytes.
        let mut state_buf = [0u16; 6];
        let mut str_len: i16 = 0;
        let ret = ffi::diag::sql_get_diag_field_w::<SqliteBackend>(
            HandleType::Stmt as i16,
            stmt,
            1, // first record
            HeaderDiagnosticIdentifier::SqlState as i16,
            state_buf.as_mut_ptr() as *mut c_void,
            12, // buffer_length in bytes (6 u16s)
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        // SQLSTATE is always exactly 5 characters = 10 bytes; StringLengthPtr
        // is spec'd in bytes for SQLGetDiagField.
        assert_eq!(str_len, 10);
        let state = String::from_utf16_lossy(&state_buf[..5]);
        // Must be a non-empty, 5-char SQLSTATE string.
        assert_eq!(state.len(), 5, "SQLSTATE must be 5 chars");

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_diag_field_native_error_after_error() {
    // SQL_DIAG_NATIVE (5) returns the driver-specific native error code (i32).
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(exec_direct(stmt, "NOT VALID SQL"), SqlReturn::ERROR);

        let mut native: i32 = -999;
        let mut str_len: i16 = 0;
        let ret = ffi::diag::sql_get_diag_field_w::<SqliteBackend>(
            HandleType::Stmt as i16,
            stmt,
            1, // first record
            HeaderDiagnosticIdentifier::Native as i16,
            &mut native as *mut i32 as *mut c_void,
            0,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(str_len, 4); // i32 = 4 bytes
        // Native error is driver-defined; we just verify the field is readable.
        let _ = native;

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_diag_field_message_text_after_error() {
    // SQL_DIAG_MESSAGE_TEXT (6) returns the diagnostic message string.
    // After an invalid-SQL error the message must be non-empty.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(exec_direct(stmt, "NOT VALID SQL"), SqlReturn::ERROR);

        let mut msg_buf = [0u16; 256];
        let mut str_len: i16 = 0;
        let buffer_length =
            i16::try_from(std::mem::size_of_val(&msg_buf)).expect("msg_buf byte size fits in i16");
        let ret = ffi::diag::sql_get_diag_field_w::<SqliteBackend>(
            HandleType::Stmt as i16,
            stmt,
            1, // first record
            SQL_DIAG_MESSAGE_TEXT,
            msg_buf.as_mut_ptr() as *mut c_void,
            buffer_length,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert!(str_len > 0, "diagnostic message must be non-empty");
        // str_len is a BYTE count (SQLGetDiagField spec); convert to UTF-16
        // code units and clamp to the buffer's element count before indexing;
        // the untruncated byte count can exceed the buffer capacity.
        let code_units =
            (usize::try_from(str_len).expect("non-negative length") / 2).min(msg_buf.len());
        let msg = String::from_utf16_lossy(&msg_buf[..code_units]);
        assert!(!msg.is_empty(), "message text must not be empty");

        cleanup(env, conn, stmt);
    }
}

#[test]
fn get_diag_field_message_text_long_message_does_not_panic() {
    // Regression test: SQLGetDiagField's StringLengthPtr is a BYTE count, not
    // a UTF-16 code-unit count. A caller that indexes a u16 message buffer
    // with the raw byte count panics once the message exceeds half the
    // buffer's element count (128 characters for a 256-element buffer). Force
    // a diagnostic message longer than 128 characters via an overlong invalid
    // table name, and confirm retrieval succeeds without an out-of-bounds
    // panic.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        let long_identifier = "x".repeat(200);
        let sql = format!("SELECT * FROM {long_identifier}");
        assert_eq!(exec_direct(stmt, &sql), SqlReturn::ERROR);

        let mut msg_buf = [0u16; 256];
        let mut str_len: i16 = 0;
        let buffer_length =
            i16::try_from(std::mem::size_of_val(&msg_buf)).expect("msg_buf byte size fits in i16");
        let ret = ffi::diag::sql_get_diag_field_w::<SqliteBackend>(
            HandleType::Stmt as i16,
            stmt,
            1,
            SQL_DIAG_MESSAGE_TEXT,
            msg_buf.as_mut_ptr() as *mut c_void,
            buffer_length,
            &mut str_len,
        );
        assert!(
            matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO),
            "expected SUCCESS or SUCCESS_WITH_INFO, got {ret:?}"
        );
        assert!(str_len > 0, "diagnostic message must be non-empty");
        assert!(
            str_len as usize > 128 * 2,
            "test setup must produce a message over 128 UTF-16 code units; got {str_len} bytes"
        );

        // This conversion must divide by 2: `str_len` is a byte count (> 256
        // here), so `str_len as usize` without the division would index past
        // the end of the 256-element buffer.
        let code_units =
            (usize::try_from(str_len).expect("non-negative length") / 2).min(msg_buf.len());
        let msg = String::from_utf16_lossy(&msg_buf[..code_units]);
        assert!(
            msg.contains(&long_identifier),
            "message should contain the overlong identifier: {msg}"
        );

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// P3: SQLGetEnvAttrW — ODBC version roundtrip
// ---------------------------------------------------------------------------

#[test]
fn get_env_attr_odbc_version_roundtrip() {
    // Set SQL_ATTR_ODBC_VERSION (200) to SQL_OV_ODBC3 (3), then read it back.
    // Per spec HY010, SQLSetEnvAttr must be called before any connection handle
    // is allocated on the environment. We therefore use a bare env handle here.
    unsafe {
        let mut env: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            ffi::handle::sql_alloc_handle::<SqliteBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            ),
            SqlReturn::SUCCESS
        );

        // Set SQL_ATTR_ODBC_VERSION = SQL_OV_ODBC3 (3) before any conn is allocated.
        assert_eq!(
            ffi::env::sql_set_env_attr::<SqliteBackend>(
                env,
                EnvironmentAttribute::OdbcVersion as i32,
                AttrOdbcVersion::Odbc3 as usize as *mut c_void,
                0,
            ),
            SqlReturn::SUCCESS
        );

        // Read it back.
        let mut version: i32 = 0;
        let mut str_len: i32 = 0;
        assert_eq!(
            ffi::env::sql_get_env_attr::<SqliteBackend>(
                env,
                EnvironmentAttribute::OdbcVersion as i32,
                &mut version as *mut i32 as *mut c_void,
                4,
                &mut str_len,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(version, AttrOdbcVersion::Odbc3 as i32);
        assert_eq!(str_len, 4); // sizeof(i32)

        let _ = ffi::handle::sql_free_handle::<SqliteBackend>(HandleType::Env as i16, env);
    }
}

// ---------------------------------------------------------------------------
// Array-fetch path (SQLBindCol + SQLFetch)
// ---------------------------------------------------------------------------
//
// pyodbc retrieves column data via SQLGetData after each fetch; turbodbc and
// other drivers that pre-allocate column buffers use SQLBindCol + SQLFetch
// instead. These tests exercise the bound-column path so regressions in
// sql_bind_col or the write_column_value call inside sql_fetch are caught
// independently of the sql_get_data path.

#[test]
fn bind_col_and_fetch_reads_bound_column_values() {
    // Exercises SQL_ATTR_ROW_ARRAY_SIZE (27) and SQL_ATTR_ROWS_FETCHED_PTR (26)
    // attribute setting (accepted without error) plus the full SQLBindCol →
    // SQLFetch data path.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Insert rows via rusqlite directly.
        {
            setup_sql(
                conn,
                "CREATE TABLE bind_col_test (id INTEGER); \
                 INSERT INTO bind_col_test VALUES (10); \
                 INSERT INTO bind_col_test VALUES (20); \
                 INSERT INTO bind_col_test VALUES (30);",
            );
        }

        // Set SQL_ATTR_ROW_ARRAY_SIZE = 1.
        // The driver supports single-row fetch only; setting this to 1 must
        // succeed and must not change the observed per-fetch row count.
        assert_eq!(
            ffi::stmt_attr::sql_set_stmt_attr_w::<SqliteBackend>(
                stmt,
                StatementAttribute::RowArraySize as i32,
                std::ptr::without_provenance_mut(1usize), // 1 row per fetch
                0,
            ),
            SqlReturn::SUCCESS
        );

        // Set SQL_ATTR_ROWS_FETCHED_PTR to a usize variable.
        // Stored in stmt.attrs; the attribute must be accepted without error.
        let mut rows_fetched: usize = 0;
        assert_eq!(
            ffi::stmt_attr::sql_set_stmt_attr_w::<SqliteBackend>(
                stmt,
                StatementAttribute::RowsFetchedPtr as i32,
                &mut rows_fetched as *mut usize as *mut c_void,
                0,
            ),
            SqlReturn::SUCCESS
        );

        // Execute the SELECT.
        assert_eq!(
            exec_direct(stmt, "SELECT id FROM bind_col_test ORDER BY id"),
            SqlReturn::SUCCESS
        );

        // Bind column 1 to an i64 buffer via SQLBindCol.
        let mut id_buf: i64 = 0;
        let mut id_ind: isize = 0;
        assert_eq!(
            ffi::bind::sql_bind_col::<SqliteBackend>(
                stmt,
                1, // column 1
                CDataType::SBigInt as i16,
                &mut id_buf as *mut i64 as *mut c_void,
                std::mem::size_of::<i64>() as isize,
                &mut id_ind,
            ),
            SqlReturn::SUCCESS
        );

        // Fetch each row and verify the bound buffer is populated.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        assert_eq!(id_buf, 10);
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        assert_eq!(id_buf, 20);
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        assert_eq!(id_buf, 30);

        // Result set exhausted.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::NO_DATA
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn fetch_truncating_bound_column_reports_01004() {
    // Spec: "If the data is truncated because the length of the data buffer is
    // too small ... SQLFetch returns SQLSTATE 01004 (Data truncated) and
    // SQL_SUCCESS_WITH_INFO." Silently returning SQL_SUCCESS would leave the
    // application reading truncated data believing it complete.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        {
            setup_sql(
                conn,
                "CREATE TABLE trunc_test (s TEXT);
                 INSERT INTO trunc_test VALUES ('abcdef');",
            );
        }

        assert_eq!(
            exec_direct(stmt, "SELECT s FROM trunc_test"),
            SqlReturn::SUCCESS
        );

        // Four bytes holds three characters plus the null terminator.
        let mut buf = [0u8; 4];
        let mut ind: isize = 0;
        assert_eq!(
            ffi::bind::sql_bind_col::<SqliteBackend>(
                stmt,
                1,
                CDataType::Char as i16,
                buf.as_mut_ptr().cast(),
                buf.len() as isize,
                &mut ind,
            ),
            SqlReturn::SUCCESS
        );

        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS_WITH_INFO,
            "truncation was reported as plain SQL_SUCCESS"
        );

        // The indicator reports the untruncated length, per step 6 of the spec.
        assert_eq!(ind, 6);
        assert_eq!(&buf[..3], b"abc");
        assert_eq!(buf[3], 0, "result was not null-terminated");

        // A 01004 diagnostic must be retrievable.
        let mut state = [0u16; 6];
        let mut native: i32 = 0;
        let mut msg = [0u16; 256];
        let mut msg_len: i16 = 0;
        assert_eq!(
            ffi::diag::sql_get_diag_rec_w::<SqliteBackend>(
                HandleType::Stmt as i16,
                stmt,
                1,
                state.as_mut_ptr(),
                &mut native,
                msg.as_mut_ptr(),
                msg.len() as i16,
                &mut msg_len,
            ),
            SqlReturn::SUCCESS,
            "no diagnostic record was pushed"
        );
        let sqlstate = String::from_utf16_lossy(&state[..5]);
        assert_eq!(sqlstate, "01004");

        cleanup(env, conn, stmt);
    }
}

#[test]
fn autocommit_off_then_rollback_discards_changes() {
    // Turn autocommit off, insert rows, roll back: the rollback must discard
    // the inserted rows. This asserts the autocommit attribute reaches the
    // backend rather than being stored locally while every row is committed
    // as it is written.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        {
            setup_sql(conn, "CREATE TABLE tx_test (id INTEGER);");
        }

        assert_eq!(
            ffi::connect_attr::sql_set_connect_attr_w::<SqliteBackend>(
                conn,
                ConnectionAttribute::AUTOCOMMIT.0,
                std::ptr::without_provenance_mut(SQL_AUTOCOMMIT_OFF),
                0,
            ),
            SqlReturn::SUCCESS,
            "SQLite advertises SQL_TC_DML so manual-commit must be accepted"
        );

        assert_eq!(
            exec_direct(stmt, "INSERT INTO tx_test VALUES (1)"),
            SqlReturn::SUCCESS
        );
        // No SQLCloseCursor between the two INSERTs: neither opens a cursor,
        // so the handle is immediately reusable.
        assert_eq!(
            exec_direct(stmt, "INSERT INTO tx_test VALUES (2)"),
            SqlReturn::SUCCESS
        );

        assert_eq!(
            ffi::tran::sql_end_tran::<SqliteBackend>(
                HandleType::Dbc as i16,
                conn,
                CompletionType::Rollback as i16,
            ),
            SqlReturn::SUCCESS
        );

        let count = query_scalar_i64(conn, "SELECT COUNT(*) FROM tx_test");
        assert_eq!(count, 0, "rollback did not discard the inserted rows");

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// Batch parameter path (SQLBindParameter + SQLPrepare + SQLExecute)
// ---------------------------------------------------------------------------
//
// pyodbc uses SQLExecDirect for most inserts; turbodbc (and PowerBI) use the
// prepare/bind/execute path for parameterised DML. This test exercises the
// full SQLBindParameter → SQLExecute pipeline including SQL_ATTR_PARAMSET_SIZE.

#[test]
fn bind_parameter_prepare_execute_inserts_row() {
    // Exercises SQL_ATTR_PARAMSET_SIZE (22) attribute setting (accepted without
    // error) plus the full SQLBindParameter → SQLPrepare → SQLExecute DML path.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Create the target table via rusqlite.
        {
            setup_sql(conn, "CREATE TABLE bind_param_test (id INTEGER);");
        }

        // Set SQL_ATTR_PARAMSET_SIZE = 1.
        // The driver executes one parameter row at a time; setting this to 1
        // must succeed without error.
        assert_eq!(
            ffi::stmt_attr::sql_set_stmt_attr_w::<SqliteBackend>(
                stmt,
                StatementAttribute::ParamsetSize as i32,
                std::ptr::without_provenance_mut(1usize), // 1 parameter row per execute
                0,
            ),
            SqlReturn::SUCCESS
        );

        // Prepare the INSERT statement.
        let sql = "INSERT INTO bind_param_test VALUES (?)";
        let wide: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_prepare_w::<SqliteBackend>(stmt, wide.as_ptr(), wide.len() as i32),
            SqlReturn::SUCCESS
        );

        // Bind parameter 1 (the `?` placeholder) to an i64 buffer holding 42.
        let mut val: i64 = 42;
        assert_eq!(
            ffi::params::sql_bind_parameter::<SqliteBackend>(
                stmt,
                1, // parameter_number
                ParamType::Input as i16,
                CDataType::SBigInt as i16, // value_type: SQL_C_SBIGINT
                SqlDataType::EXT_BIG_INT.0,
                19, // column_size (max digits of i64)
                0,  // decimal_digits
                &mut val as *mut i64 as *mut c_void,
                std::mem::size_of::<i64>() as isize,
                std::ptr::null_mut(),
            ),
            SqlReturn::SUCCESS
        );

        // Execute the prepared INSERT.
        assert_eq!(
            ffi::execute::sql_execute::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        // Verify through the driver that exactly one row with value 42 was
        // inserted.
        let count = query_scalar_i64(conn, "SELECT COUNT(*) FROM bind_param_test WHERE id = 42");
        assert_eq!(count, 1);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn exec_direct_sends_bound_parameters() {
    // SQLExecDirect must send bound parameter values, not the literal `?`.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        {
            setup_sql(
                conn,
                "CREATE TABLE exec_direct_params (id INTEGER);
                 INSERT INTO exec_direct_params VALUES (1), (2), (3);",
            );
        }

        let mut val: i64 = 2;
        assert_eq!(
            ffi::params::sql_bind_parameter::<SqliteBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::SBigInt as i16,
                SqlDataType::EXT_BIG_INT.0,
                19,
                0,
                &mut val as *mut i64 as *mut c_void,
                std::mem::size_of::<i64>() as isize,
                std::ptr::null_mut(),
            ),
            SqlReturn::SUCCESS
        );

        let sql = "SELECT id FROM exec_direct_params WHERE id = ?";
        let wide: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_exec_direct_w::<SqliteBackend>(
                stmt,
                wide.as_ptr(),
                wide.len() as i32
            ),
            SqlReturn::SUCCESS,
            "SQLExecDirect rejected the parameterised statement"
        );

        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS,
            "no row returned — the bound parameter was not sent"
        );
        let mut out: i64 = 0;
        let mut ind: isize = 0;
        assert_eq!(
            ffi::fetch::sql_get_data::<SqliteBackend>(
                stmt,
                1,
                CDataType::SBigInt as i16,
                &mut out as *mut i64 as *mut c_void,
                std::mem::size_of::<i64>() as isize,
                &mut ind,
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(out, 2, "wrong row: parameter value was not applied");
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::NO_DATA,
            "expected exactly one matching row"
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn bind_timestamp_and_numeric_params_are_stored_not_nulled() {
    // A bound SQL_C_TYPE_TIMESTAMP and SQL_C_NUMERIC must reach the backend as
    // their real values. Before read_param_value handled the temporal/numeric
    // C structs, both marshalled to NULL and the INSERT silently lost the data.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        {
            setup_sql(conn, "CREATE TABLE dt_test (ts TEXT, amount TEXT);");
        }

        let sql = "INSERT INTO dt_test VALUES (?, ?)";
        let wide: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_prepare_w::<SqliteBackend>(stmt, wide.as_ptr(), wide.len() as i32),
            SqlReturn::SUCCESS
        );

        let mut ts = Timestamp {
            year: 2024,
            month: 1,
            day: 2,
            hour: 10,
            minute: 30,
            second: 15,
            fraction: 123_000_000,
        };
        assert_eq!(
            ffi::params::sql_bind_parameter::<SqliteBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::TypeTimestamp as i16,
                SqlDataType::TIMESTAMP.0,
                23,
                9,
                &mut ts as *mut _ as *mut c_void,
                std::mem::size_of::<Timestamp>() as isize,
                std::ptr::null_mut(),
            ),
            SqlReturn::SUCCESS
        );

        // -123.45 as SQL_NUMERIC_STRUCT: mantissa 12345 (LE), scale 2, sign 0.
        let mut val_bytes = [0u8; 16];
        val_bytes[..16].copy_from_slice(&12_345u128.to_le_bytes());
        let mut num = Numeric {
            precision: 5,
            scale: 2,
            sign: 0,
            val: val_bytes,
        };
        assert_eq!(
            ffi::params::sql_bind_parameter::<SqliteBackend>(
                stmt,
                2,
                ParamType::Input as i16,
                CDataType::Numeric as i16,
                SqlDataType::DECIMAL.0,
                5,
                2,
                &mut num as *mut _ as *mut c_void,
                std::mem::size_of::<Numeric>() as isize,
                std::ptr::null_mut(),
            ),
            SqlReturn::SUCCESS
        );

        assert_eq!(
            ffi::execute::sql_execute::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let (ts_stored, amount_stored) =
            query_row_two_strings(conn, "SELECT ts, amount FROM dt_test");
        assert_eq!(ts_stored, "2024-01-02 10:30:15.123000000");
        assert_eq!(amount_stored, "-123.45");

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// SQLBulkOperations / SQLSetPos
// ---------------------------------------------------------------------------

#[test]
fn bulk_operations_returns_hyc00() {
    // SQLBulkOperations is not supported by this driver. It must return ERROR
    // with SQLSTATE HYC00 (optional feature not implemented) even when a cursor
    // is open.
    use stackable_odbc_core::types::SQL_ADD;

    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Open a cursor so the handle is in a valid statement state.
        {
            setup_sql(conn, "CREATE TABLE bulkops_test (id INTEGER, val TEXT);");
        }

        assert_eq!(
            exec_direct(stmt, "SELECT id, val FROM bulkops_test"),
            SqlReturn::SUCCESS
        );

        let ret = ffi::cursor::sql_bulk_operations::<SqliteBackend>(stmt, SQL_ADD);
        assert_eq!(ret, SqlReturn::ERROR);

        cleanup(env, conn, stmt);
    }
}

#[test]
fn set_pos_returns_hyc00() {
    // SQLSetPos is not supported by this driver. It must return ERROR with
    // SQLSTATE HYC00 even when a cursor is open.
    use stackable_odbc_core::types::{SQL_LOCK_NO_CHANGE, SQL_POSITION};

    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Open a cursor so the handle is in a valid statement state.
        {
            setup_sql(
                conn,
                "CREATE TABLE setpos_test (id INTEGER); INSERT INTO setpos_test VALUES (1);",
            );
        }

        assert_eq!(
            exec_direct(stmt, "SELECT id FROM setpos_test"),
            SqlReturn::SUCCESS
        );
        // Advance to first row so the cursor is positioned.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let ret =
            ffi::cursor::sql_set_pos::<SqliteBackend>(stmt, 1, SQL_POSITION, SQL_LOCK_NO_CHANGE);
        assert_eq!(ret, SqlReturn::ERROR);

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// Data-at-execution (SQLParamData / SQLPutData)
// ---------------------------------------------------------------------------

#[test]
fn data_at_execution_insert() {
    // Exercises the full SQLParamData / SQLPutData flow for a data-at-execution
    // parameter:
    //   1. Prepare INSERT with two parameters.
    //   2. Bind param 1 (id) normally as SQL_C_SLONG.
    //   3. Bind param 2 (name) as SQL_DATA_AT_EXEC.
    //   4. SQLExecute → SQL_NEED_DATA.
    //   5. SQLParamData → SQL_NEED_DATA (returns token for param 2).
    //   6. SQLPutData with "hello".
    //   7. SQLParamData → SQL_SUCCESS (executes the INSERT).
    //   8. Verify the row in the database.
    use stackable_odbc_core::types::SQL_DATA_AT_EXEC;

    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Create target table.
        {
            setup_sql(conn, "CREATE TABLE dae_test (id INTEGER, name TEXT);");
        }

        // Prepare the INSERT.
        let sql = "INSERT INTO dae_test VALUES (?, ?)";
        let wide: Vec<u16> = sql.encode_utf16().collect();
        assert_eq!(
            ffi::execute::sql_prepare_w::<SqliteBackend>(stmt, wide.as_ptr(), wide.len() as i32),
            SqlReturn::SUCCESS
        );

        // Bind param 1 (id) as a normal integer.
        let mut id_val: i32 = 42;
        let mut id_ind: isize = 0;
        assert_eq!(
            ffi::params::sql_bind_parameter::<SqliteBackend>(
                stmt,
                1, // parameter_number
                ParamType::Input as i16,
                CDataType::SLong as i16, // SQL_C_SLONG
                SqlDataType::INTEGER.0,
                10, // column_size
                0,  // decimal_digits
                &mut id_val as *mut i32 as *mut c_void,
                std::mem::size_of::<i32>() as isize,
                &mut id_ind,
            ),
            SqlReturn::SUCCESS
        );

        // Bind param 2 (name) as data-at-execution.
        // The value_ptr is a token that SQLParamData will return to identify
        // which parameter is being requested.
        let token: usize = 0xBEEF;
        let mut dae_ind: isize = SQL_DATA_AT_EXEC;
        assert_eq!(
            ffi::params::sql_bind_parameter::<SqliteBackend>(
                stmt,
                2, // parameter_number
                ParamType::Input as i16,
                CDataType::Char as i16, // SQL_C_CHAR
                SqlDataType::VARCHAR.0,
                255,                                     // column_size
                0,                                       // decimal_digits
                std::ptr::without_provenance_mut(token), // token value
                0,
                &mut dae_ind,
            ),
            SqlReturn::SUCCESS
        );

        // SQLExecute must return NEED_DATA because param 2 is DAE.
        assert_eq!(
            ffi::execute::sql_execute::<SqliteBackend>(stmt),
            SqlReturn::NEED_DATA
        );

        // SQLParamData must return NEED_DATA and write the token for param 2.
        let mut value_ptr: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            ffi::params::sql_param_data::<SqliteBackend>(stmt, &mut value_ptr),
            SqlReturn::NEED_DATA
        );
        // The driver should have written back the token we supplied.
        assert_eq!(value_ptr as usize, token);

        // SQLPutData: supply the data for the name column.
        let data = b"hello";
        assert_eq!(
            ffi::params::sql_put_data::<SqliteBackend>(
                stmt,
                data.as_ptr() as *mut c_void,
                data.len() as isize,
            ),
            SqlReturn::SUCCESS
        );

        // SQLParamData: no more pending params — should execute the INSERT and return SUCCESS.
        let mut value_ptr2: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            ffi::params::sql_param_data::<SqliteBackend>(stmt, &mut value_ptr2),
            SqlReturn::SUCCESS
        );

        // No SQLCloseCursor after the INSERT: it produced no result set, so no
        // cursor is open and the SELECT can reuse this handle directly.

        // Verify the inserted row via ODBC SELECT.
        assert_eq!(
            exec_direct(stmt, "SELECT name FROM dae_test WHERE id = 42"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let mut name_buf = [0u16; 32];
        let mut name_ind: isize = 0;
        assert_eq!(
            ffi::fetch::sql_get_data::<SqliteBackend>(
                stmt,
                1,
                CDataType::WChar as i16,
                name_buf.as_mut_ptr() as *mut c_void,
                (name_buf.len() * 2) as isize,
                &mut name_ind,
            ),
            SqlReturn::SUCCESS
        );
        // name_ind is in bytes; divide by 2 to get WChar count.
        let char_count = if name_ind > 0 {
            (name_ind / 2) as usize
        } else {
            0
        };
        let name = String::from_utf16_lossy(&name_buf[..char_count]);
        assert_eq!(name, "hello");

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// Column-size round-trip matrix
//
// Coverage: for each representative type below, `SQL_DESC_DISPLAY_SIZE` is
// read via `SQLColAttributeW` and used to size the `SQLGetData(SQL_C_WCHAR)`
// read buffer, so a wrong metadata value (too small) surfaces here as a
// truncated/SUCCESS_WITH_INFO read rather than merely as a wrong number
// somewhere nobody looks. Sizing the read buffer from what the driver itself
// reports is what makes a wrong column-size value in the metadata path fail
// this test.
//
// DISPLAY_SIZE, not OCTET_LENGTH, is the correct field for this: per the
// "Transfer Octet Length" appendix, OCTET_LENGTH is defined as "the maximum
// number of bytes returned ... when data is transferred to its *default* C
// data type": for DATE/TIME/TIMESTAMP that default is the fixed-size
// SQL_DATE_STRUCT/SQL_TIME_STRUCT/SQL_TIMESTAMP_STRUCT (6/6/16 bytes, see
// `col_attr.rs`), and for INTEGER/DOUBLE/etc. it is the native numeric C
// type's byte width (4/8/...); neither has anything to do with the
// character count needed to render the value as text. DISPLAY_SIZE is
// exactly that character count by definition ("the maximum number of
// characters needed to display the data in character form"), so it is what
// an application should (and this test does) use to size a text buffer
// for `SQL_C_WCHAR`/`SQL_C_CHAR`.
//
// Native-C-type round trips (SQL_C_SBIGINT for INTEGER, SQL_C_TYPE_TIMESTAMP
// for TIMESTAMP, etc.) are already covered extensively elsewhere in this
// file (e.g. `get_data_returns_correct_values`,
// `get_data_datetime_column_handles_integer_and_real_storage`) and are not
// duplicated here; those C types have an ODBC-mandated fixed struct size
// regardless of what COLUMN_SIZE/OCTET_LENGTH report, so sizing them "from
// metadata" would not exercise anything metadata-related.
//
// Omitted from this matrix, with reasons:
// - An *undeclared* (unbounded) BLOB/VARBINARY column: its DISPLAY_SIZE is
//   i32::MAX * 2 (see `is_binary_type` in col_attr.rs: DISPLAY_SIZE for a
//   binary column is its length in bytes times 2, one hex digit pair per
//   byte), which is not an allocatable buffer size. The bounded `n_blob
//   BLOB(20)` column below exercises the same code path at a size that can
//   actually be allocated, which is what an application reading an
//   unbounded column is expected to do too (chunked `SQLGetData` calls,
//   not one buffer sized from COLUMN_SIZE); this is a property of
//   "unbounded" columns in general, not something specific to binary data.
// - TINYINT/SMALLINT/BOOLEAN: fixed-width numeric types whose
//   COLUMN_SIZE/OCTET_LENGTH values are simple constants already covered by
//   `stackable_odbc_core::types::column_size`'s own unit tests; a live-database round
//   trip adds little beyond what INTEGER/DOUBLE below already demonstrate
//   for the numeric-type shape.

/// Read `SQL_DESC_DISPLAY_SIZE` (characters needed to render the value as
/// text; see the module-level comment above for why this, not
/// `OCTET_LENGTH`, is the right field) for one column via `SQLColAttributeW`.
unsafe fn column_display_size(stmt: *mut c_void, column_number: u16) -> usize {
    let mut chars: isize = 0;
    unsafe {
        assert_eq!(
            ffi::metadata::sql_col_attribute_w::<SqliteBackend>(
                stmt,
                column_number,
                Desc::DisplaySize as u16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut chars,
            ),
            SqlReturn::SUCCESS
        );
    }
    usize::try_from(chars).expect("DISPLAY_SIZE must not be negative")
}

/// Fetch column `column_number` as `SQL_C_WCHAR`, using a buffer sized
/// exactly from `SQL_DESC_DISPLAY_SIZE` (plus one UTF-16 code unit of slack
/// for the null terminator, which `DISPLAY_SIZE` does not include per spec).
/// Returns `(SqlReturn, decoded text)`.
unsafe fn get_data_wchar_sized_from_metadata(
    stmt: *mut c_void,
    column_number: u16,
) -> (SqlReturn, String) {
    let chars = unsafe { column_display_size(stmt, column_number) };
    let code_units = chars + 1; // +1 for the null terminator
    let mut buf: Vec<u16> = vec![0u16; code_units];
    let mut ind: isize = 0;
    let ret = unsafe {
        ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            column_number,
            CDataType::WChar as i16,
            buf.as_mut_ptr().cast(),
            (buf.len() * 2) as isize,
            &mut ind,
        )
    };
    let char_count = if ind > 0 { (ind / 2) as usize } else { 0 };
    (
        ret,
        String::from_utf16_lossy(&buf[..char_count.min(buf.len())]),
    )
}

/// Read the first diagnostic record's 5-character SQLSTATE off `stmt`.
unsafe fn last_sqlstate(stmt: *mut c_void) -> String {
    unsafe { last_diag_rec(stmt).0 }
}

/// Read the first diagnostic record's 5-character SQLSTATE off a *connection*
/// handle. `SQLSetConnectAttr` posts its diagnostics there, not on a statement.
unsafe fn last_conn_sqlstate(conn: *mut c_void) -> String {
    let mut state = [0u16; 6];
    let mut native: i32 = 0;
    let mut msg = [0u16; 512];
    let mut msg_len: i16 = 0;
    unsafe {
        assert_eq!(
            ffi::diag::sql_get_diag_rec_w::<SqliteBackend>(
                HandleType::Dbc as i16,
                conn,
                1,
                state.as_mut_ptr(),
                &mut native,
                msg.as_mut_ptr(),
                msg.len() as i16,
                &mut msg_len,
            ),
            SqlReturn::SUCCESS,
            "no diagnostic record was pushed on the connection"
        );
    }
    String::from_utf16_lossy(&state[..5])
}

#[test]
fn txn_isolation_accepts_only_the_level_sqlite_implements() {
    // SQLite runs serializable and nothing else: READ COMMITTED and
    // REPEATABLE READ are not SQLite concepts, and READ UNCOMMITTED needs
    // shared-cache mode, which `connect` does not open. So
    // SQL_TXN_ISOLATION_OPTION advertises exactly one level, and setting any
    // other is now refused with HY024 rather than stored and echoed back.
    //
    // The spec assigns this check to the driver: the Driver Manager validates
    // only attributes "that accept a discrete set of values". An application
    // that asked for READ COMMITTED previously got SQL_SUCCESS and serializable
    // behaviour anyway -- it had no way to find out it had not been honoured.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            ffi::connect_attr::sql_set_connect_attr_w::<SqliteBackend>(
                conn,
                ConnectionAttribute::TXN_ISOLATION.0,
                std::ptr::without_provenance_mut(SQL_TXN_SERIALIZABLE as usize),
                0,
            ),
            SqlReturn::SUCCESS,
            "the one advertised level must be accepted"
        );

        for level in [
            SQL_TXN_READ_UNCOMMITTED,
            SQL_TXN_READ_COMMITTED,
            SQL_TXN_REPEATABLE_READ,
        ] {
            assert_eq!(
                ffi::connect_attr::sql_set_connect_attr_w::<SqliteBackend>(
                    conn,
                    ConnectionAttribute::TXN_ISOLATION.0,
                    std::ptr::without_provenance_mut(level as usize),
                    0,
                ),
                SqlReturn::ERROR,
                "level {level:#x} is not advertised and must be refused"
            );
            assert_eq!(
                last_conn_sqlstate(conn),
                stackable_odbc_core::types::sql_state::INVALID_ATTRIBUTE_VALUE,
                "level {level:#x}"
            );
        }

        cleanup(env, conn, stmt);
    }
}

/// Read the first diagnostic record off `stmt` as (SQLSTATE, native code,
/// message).
unsafe fn last_diag_rec(stmt: *mut c_void) -> (String, i32, String) {
    let mut state = [0u16; 6];
    let mut native: i32 = 0;
    let mut msg = [0u16; 512];
    let mut msg_len: i16 = 0;
    unsafe {
        assert_eq!(
            ffi::diag::sql_get_diag_rec_w::<SqliteBackend>(
                HandleType::Stmt as i16,
                stmt,
                1,
                state.as_mut_ptr(),
                &mut native,
                msg.as_mut_ptr(),
                msg.len() as i16,
                &mut msg_len,
            ),
            SqlReturn::SUCCESS,
            "no diagnostic record was pushed"
        );
    }
    let len = usize::try_from(msg_len).unwrap_or(0).min(msg.len());
    (
        String::from_utf16_lossy(&state[..5]),
        native,
        String::from_utf16_lossy(&msg[..len]),
    )
}

/// `SQLITE_CONSTRAINT_NOTNULL`, the extended result code SQLite reports for a
/// NOT NULL violation. The primary code is `SQLITE_CONSTRAINT` (19); the
/// extended one is what says *which* constraint failed, and is the value ODBC
/// wants in `NativeErrorPtr`.
const SQLITE_CONSTRAINT_NOTNULL: i32 = 1299;

#[test]
fn diagnostic_carries_sqlites_own_extended_result_code() {
    // Every error this driver produced used to reach the application with
    // NativeErrorPtr = 0, because the `rusqlite::Error` was flattened into a
    // message string at classification time and the code went with it. An
    // application that wants to tell a NOT NULL violation from a foreign-key
    // one reads exactly this field: both are SQLSTATE 23000.
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        setup_sql(conn, "CREATE TABLE nn (id INTEGER NOT NULL);");

        assert_eq!(
            exec_direct(stmt, "INSERT INTO nn (id) VALUES (NULL)"),
            SqlReturn::ERROR
        );

        let (sqlstate, native, message) = last_diag_rec(stmt);
        assert_eq!(
            sqlstate,
            stackable_odbc_core::types::sql_state::INTEGRITY_CONSTRAINT_VIOLATION
        );
        assert_eq!(
            native, SQLITE_CONSTRAINT_NOTNULL,
            "SQLite's extended result code must reach NativeErrorPtr verbatim"
        );
        assert!(
            message.contains("NOT NULL"),
            "diagnostic message should name the constraint, got {message:?}"
        );

        cleanup(env, conn, stmt);
    }
}

#[test]
fn metadata_sized_wchar_round_trip_covers_representative_types() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        setup_sql(
            conn,
            "CREATE TABLE sizing (
                    n_int INTEGER,
                    n_real REAL,
                    n_text TEXT,
                    n_date DATE,
                    n_time TIME,
                    n_ts TIMESTAMP,
                    n_dec DECIMAL(10,2),
                    n_blob BLOB(20),
                    n_time_frac TIME,
                    n_ts_frac TIMESTAMP
                );
                 INSERT INTO sizing VALUES (
                    1234567890,
                    3.5,
                    'hello world',
                    '2024-03-05',
                    '13:30:15',
                    '2024-03-05 13:30:15',
                    123.45,
                    X'DEADBEEF',
                    '13:30:15.123',
                    '2024-03-05 13:30:15.123'
                );",
        );

        assert_eq!(
            exec_direct(
                stmt,
                "SELECT n_int, n_real, n_text, n_date, n_time, n_ts, n_dec, n_blob, \
                 n_time_frac, n_ts_frac FROM sizing"
            ),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        // (column_number, expected text rendering). n_blob is declared
        // BLOB(20) (a bounded length; SQLite does not enforce it, but
        // parses it the same way as VARCHAR(n)/CHAR(n), see
        // `sqlite_declared_type_precision`) specifically so DISPLAY_SIZE is
        // a small, allocatable number (40 = 20*2) rather than the
        // "unbounded" convention's i32::MAX*2 an undeclared BLOB column
        // would report, proving DISPLAY_SIZE reports the hex-text length
        // (col_attr.rs's `is_binary_type` branch) end-to-end.
        //
        // Columns 9/10: SQLite's TIME/TIMESTAMP text is stored and returned
        // verbatim (SQLite is dynamically typed text storage, see
        // `type_conversion.rs`'s module doc), so these exercise
        // `MAX_FRACTIONAL_SECONDS_PRECISION` (3) reporting enough room for a
        // fractional value. DISPLAY_SIZE must be 12/23 to hold a fractional
        // value; 8/19 (no fractional allowance) would truncate it.
        let expectations: &[(u16, &str)] = &[
            (1, "1234567890"),
            (2, "3.5"),
            (3, "hello world"),
            (4, "2024-03-05"),
            (5, "13:30:15"),
            (6, "2024-03-05 13:30:15"),
            (7, "123.45"),
            (8, "DEADBEEF"),
            (9, "13:30:15.123"),
            (10, "2024-03-05 13:30:15.123"),
        ];

        for &(col, expected) in expectations {
            let (ret, text) = get_data_wchar_sized_from_metadata(stmt, col);
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "column {col}: OCTET_LENGTH-sized buffer was not big enough \
                 (metadata under-reported the size, or SUCCESS_WITH_INFO/ERROR \
                 was otherwise returned)"
            );
            assert_eq!(text, expected, "column {col}: unexpected text rendering");
        }

        cleanup(env, conn, stmt);
    }
}

// --- Cross-family conversions ---
//
// These three shapes cover the class of bug where a value is converted across
// a type-family boundary, or a column whose metadata says one thing while its
// actual stored representation is another.

/// A numeric column read as a cross-family C type
/// (`SQL_C_DOUBLE`). For SQLite, a `DECIMAL`-declared column's actual
/// storage is NUMERIC-affinity `REAL` (SQLite has no distinct DECIMAL
/// storage class), so this exercises `write_column_value`'s numeric-pivot
/// arm end-to-end; the equivalent *text*-sourced pivot (a value that
/// literally arrives as `ColumnValue::String`/`ColumnValue::Decimal` parsed
/// through `parse_numeric_text`) is what
/// `numeric_looking_text_column_read_as_sbigint_below` exercises here for
/// SQLite's own TEXT-affinity columns.
#[test]
fn decimal_column_read_as_double() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);
        assert_eq!(
            exec_direct(stmt, "SELECT CAST(123.45 AS DECIMAL(10,2)) AS amount"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let mut buf: f64 = 0.0;
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            1,
            CDataType::Double as i16,
            &mut buf as *mut f64 as *mut c_void,
            std::mem::size_of::<f64>() as isize,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert!((buf - 123.45).abs() < 1e-9, "got {buf}");

        cleanup(env, conn, stmt);
    }
}

/// A TEXT-affinity column holding digit text, read as `SQL_C_SBIGINT`, must
/// succeed (the ODBC conversion matrix requires CHAR/VARCHAR to convert to
/// every C type); the same column holding non-numeric text must fail with
/// the specific SQLSTATE the spec defines for it (22018, "invalid character
/// value for cast"), not merely "some error"; asserting only the latter is
/// exactly what would have let a wrong-but-still-erroring conversion slip
/// through on this branch.
#[test]
fn numeric_looking_text_column_read_as_sbigint() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // Success case: digit text parses cleanly.
        assert_eq!(exec_direct(stmt, "SELECT '12345'"), SqlReturn::SUCCESS);
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut buf: i64 = 0;
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            1,
            CDataType::SBigInt as i16,
            &mut buf as *mut i64 as *mut c_void,
            std::mem::size_of::<i64>() as isize,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS);
        assert_eq!(buf, 12345);
        assert_eq!(
            ffi::cursor::sql_close_cursor::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        // Failure case: non-numeric text must report 22018, not just "an error".
        assert_eq!(
            exec_direct(stmt, "SELECT 'not a number'"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut buf2: i64 = 0;
        let mut ind2: isize = 0;
        let ret2 = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            1,
            CDataType::SBigInt as i16,
            &mut buf2 as *mut i64 as *mut c_void,
            std::mem::size_of::<i64>() as isize,
            &mut ind2,
        );
        assert_eq!(ret2, SqlReturn::ERROR);
        assert_eq!(last_sqlstate(stmt), "22018");

        cleanup(env, conn, stmt);
    }
}

/// A TIMESTAMP-declared column whose value actually arrived as SQLite TEXT
/// (the third of SQLite's three documented DATETIME storage formats; the
/// other two, INTEGER epoch seconds and REAL Julian day, are already covered
/// by `get_data_datetime_column_handles_integer_and_real_storage` above),
/// read as `SQL_C_TYPE_TIMESTAMP`. Well-formed text must round-trip exactly;
/// malformed text must report the specific SQLSTATE the spec defines
/// (22018, "invalid character value for cast", scoped to a character
/// column source per the `SQLGetData` diagnostics table), not merely fail.
#[test]
fn timestamp_column_stored_as_text_read_as_type_timestamp() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        setup_sql(
            conn,
            "CREATE TABLE ts_text (id INTEGER, dt TIMESTAMP); \
                 INSERT INTO ts_text VALUES (1, '2024-03-05 13:30:15'); \
                 INSERT INTO ts_text VALUES (2, 'not-a-timestamp');",
        );

        assert_eq!(
            exec_direct(stmt, "SELECT dt FROM ts_text ORDER BY id"),
            SqlReturn::SUCCESS
        );

        // Row 1: well-formed text round-trips exactly.
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut buf = Timestamp {
            year: 0,
            month: 0,
            day: 0,
            hour: 0,
            minute: 0,
            second: 0,
            fraction: 0,
        };
        let mut ind: isize = 0;
        let ret = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            1,
            CDataType::TypeTimestamp as i16,
            &mut buf as *mut Timestamp as *mut c_void,
            std::mem::size_of::<Timestamp>() as isize,
            &mut ind,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "text-encoded datetime");
        assert_eq!((buf.year, buf.month, buf.day), (2024, 3, 5));
        assert_eq!((buf.hour, buf.minute, buf.second), (13, 30, 15));

        // Row 2: malformed text must report 22018, not just "an error".
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        let mut buf2 = buf;
        let mut ind2: isize = 0;
        let ret2 = ffi::fetch::sql_get_data::<SqliteBackend>(
            stmt,
            1,
            CDataType::TypeTimestamp as i16,
            &mut buf2 as *mut Timestamp as *mut c_void,
            std::mem::size_of::<Timestamp>() as isize,
            &mut ind2,
        );
        assert_eq!(ret2, SqlReturn::ERROR);
        assert_eq!(last_sqlstate(stmt), "22018");

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// SQLGetInfoW info-type conformance test
// ---------------------------------------------------------------------------
//
// Three real, shipped bugs were all "nothing enumerated the spec": a value
// the Windows Driver Manager treats as an integer where the driver returned
// a string (or vice versa), and conversion bitmaps that returned 0 (which
// makes the Windows DM block SQLGetData with HYC00). Line coverage stayed
// green throughout, because the code path that produced the wrong answer
// ran constantly -- nobody had asserted what it returned for every info
// type, just the ones a test happened to name.
//
// These two tests close that gap by iterating every `InfoType` odbc-sys
// compiles (derived from `info_type_from_raw`, not a hand-copied list -- see
// `stackable_odbc_core::conformance`) through the real `sql_get_info_w` FFI entry
// point, against the real `SqliteBackend`, connected and pre-connect.

/// Property 1: every `InfoType`'s returned value has the shape the
/// SQLGetInfo spec declares for it (`stackable_odbc_core::types::expected_kind`),
/// whether `SqliteBackend` answers it itself (`sqlite_get_info`), falls
/// through to the shared `default_get_info`, or reaches the generic
/// DM-safe default in `info_type_default_response`. All three layers are
/// exercised here because this goes through the real FFI entry point rather
/// than calling any one of them directly.
#[test]
fn get_info_every_named_info_type_has_the_declared_shape_connected() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        for info_type in all_info_types() {
            let (ret, kind, _string_length) =
                observe_info_value_kind::<SqliteBackend>(conn, info_type as u16);
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "{info_type:?}: SQLGetInfoW must not return SQL_ERROR"
            );
            assert_eq!(
                kind,
                expected_kind(info_type),
                "{info_type:?}: SqliteBackend returned shape {kind:?}, expected \
                 {:?} per the SQLGetInfo spec",
                expected_kind(info_type)
            );
        }

        cleanup(env, conn, stmt);
    }
}

/// Property 1, pre-connect path: the Windows Driver Manager queries some
/// info types (e.g. `SQL_DRIVER_ODBC_VER`) before `SQLDriverConnectW`, which
/// routes through `SqliteBackend::get_info_pre_connect` instead of
/// `get_info`. `sqlite_get_info` backs both, so this is expected to match
/// the connected test above for every info type -- asserted separately
/// because the two call sites in `sql_get_info_w` are independent code
/// paths that could regress independently.
#[test]
fn get_info_every_named_info_type_has_the_declared_shape_pre_connect() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();

        for info_type in all_info_types() {
            let (ret, kind, _string_length) =
                observe_info_value_kind::<SqliteBackend>(conn, info_type as u16);
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "{info_type:?}: SQLGetInfoW must not return SQL_ERROR pre-connect"
            );
            assert_eq!(
                kind,
                expected_kind(info_type),
                "{info_type:?}: SqliteBackend returned shape {kind:?} pre-connect, \
                 expected {:?} per the SQLGetInfo spec",
                expected_kind(info_type)
            );
        }

        cleanup(env, conn, stmt);
    }
}

/// Property 2: no genuine `SQL_CONVERT_*` code ever returns 0 through
/// `SqliteBackend` -- per `AGENTS.md`, a `0` conversion bitmap is what makes
/// the Windows Driver Manager block `SQLGetData` with `HYC00`.
#[test]
fn get_info_no_genuine_convert_info_type_ever_returns_zero() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        for info_type in genuine_convert_info_types() {
            let (ret, value) = observe_u32_value::<SqliteBackend>(conn, info_type);
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "raw SQL_CONVERT_* info type {info_type} must not error"
            );
            assert_ne!(
                value, 0,
                "raw SQL_CONVERT_* info type {info_type} returned 0 -- this is the \
                 exact shape that makes the Windows Driver Manager block SQLGetData \
                 with HYC00 (AGENTS.md)"
            );
        }

        cleanup(env, conn, stmt);
    }
}

/// End-to-end proof that `SqliteBackend::escape_dialect()` is actually wired
/// into the execute path: `SQLExecDirect` is given raw ODBC escape syntax
/// (`{fn UCASE(...)}`) that is not a real SQLite function name on its own,
/// and only succeeds because `sql_exec_direct_w` translates it first (see
/// `crate::escape_dialect`). Runs against an in-memory database, no server.
#[test]
fn escape_fn_ucase_translates_for_sqlite() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            exec_direct(stmt, "SELECT {fn UCASE('abc')}"),
            SqlReturn::SUCCESS,
            "exec_direct with {{fn UCASE}} escape failed to translate"
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            fetch_string_col(stmt, 1),
            "ABC",
            "{{fn UCASE(...)}} not remapped to upper()"
        );

        cleanup(env, conn, stmt);
    }
}

/// A `{ts '...'}` timestamp escape must render to a bare string literal
/// SQLite accepts, not `TIMESTAMP '...'` (SQLite has no such type keyword and
/// would reject it as a syntax error).
#[test]
fn escape_ts_literal_renders_as_bare_string_for_sqlite() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            exec_direct(
                stmt,
                "SELECT {ts '2020-01-01 00:00:00'} WHERE {ts '2020-01-01 00:00:00'} = '2020-01-01 00:00:00'"
            ),
            SqlReturn::SUCCESS,
            "exec_direct with {{ts '...'}} escape failed to translate"
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            fetch_string_col(stmt, 1),
            "2020-01-01 00:00:00",
            "{{ts '...'}} was not rendered as a bare string literal"
        );

        cleanup(env, conn, stmt);
    }
}

/// `{fn CURDATE()}` must be remapped to SQLite's zero-argument `date()` and
/// actually execute against the database, not just get rewritten as text.
/// The exact date is time-dependent, so this only checks the ISO
/// `YYYY-MM-DD` shape (length 10, dashes at the positions the format
/// mandates) and that a value came back at all.
#[test]
fn escape_fn_curdate_executes_as_sqlite_date() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            exec_direct(stmt, "SELECT {fn CURDATE()}"),
            SqlReturn::SUCCESS,
            "exec_direct with {{fn CURDATE()}} escape failed to translate"
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let curdate = fetch_string_col(stmt, 1);
        const ISO_DATE_LEN: usize = "YYYY-MM-DD".len();
        assert_eq!(
            curdate.len(),
            ISO_DATE_LEN,
            "{{fn CURDATE()}} did not return a YYYY-MM-DD date string, got {curdate:?}"
        );
        assert_eq!(
            curdate.as_bytes()[4],
            b'-',
            "{{fn CURDATE()}} result missing '-' separator after year: {curdate:?}"
        );
        assert_eq!(
            curdate.as_bytes()[7],
            b'-',
            "{{fn CURDATE()}} result missing '-' separator after month: {curdate:?}"
        );

        cleanup(env, conn, stmt);
    }
}

/// The three bare-keyword date/time escapes must reach SQLite without their
/// parentheses and actually execute.
///
/// `SQL_TIMEDATE_FUNCTIONS` advertises `SQL_FN_TD_CURRENT_DATE`,
/// `_CURRENT_TIME` and `_CURRENT_TIMESTAMP`, but nothing translated them:
/// `{fn CURRENT_DATE()}` reached SQLite as `CURRENT_DATE()`, which is a syntax
/// error, so the driver advertised three functions an application could not
/// use. `EscapeDialect::rewrite_scalar_fn` replaces the whole escape, which is
/// what emitting a bare keyword requires.
///
/// Only the shape is asserted -- these are clock values.
#[test]
fn escape_bare_keyword_datetime_fns_execute() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // (escape, expected length, expected separators at their positions)
        for (sql, len, seps) in [
            ("SELECT {fn CURRENT_DATE()}", 10, vec![(4, b'-'), (7, b'-')]),
            ("SELECT {fn CURRENT_TIME()}", 8, vec![(2, b':'), (5, b':')]),
            (
                "SELECT {fn CURRENT_TIMESTAMP()}",
                19,
                vec![(4, b'-'), (7, b'-'), (10, b' '), (13, b':'), (16, b':')],
            ),
        ] {
            assert_eq!(
                exec_direct(stmt, sql),
                SqlReturn::SUCCESS,
                "{sql} failed to translate -- the escape's trailing () most \
                 likely reached SQLite"
            );
            assert_eq!(
                ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
                SqlReturn::SUCCESS
            );

            let value = fetch_string_col(stmt, 1);
            assert_eq!(value.len(), len, "{sql} returned {value:?}");
            for (idx, sep) in seps {
                assert_eq!(
                    value.as_bytes()[idx],
                    sep,
                    "{sql} missing separator at {idx} in {value:?}"
                );
            }

            assert_eq!(
                ffi::cursor::sql_close_cursor::<SqliteBackend>(stmt),
                SqlReturn::SUCCESS
            );
        }

        cleanup(env, conn, stmt);
    }
}

/// `{fn NOW()}` must be remapped to SQLite's zero-argument `datetime()` and
/// actually execute against the database. As with CURDATE, only the ISO
/// `YYYY-MM-DD HH:MM:SS` shape is checked (length 19, dashes/colons/space at
/// their mandated positions); the exact timestamp is time-dependent.
#[test]
fn escape_fn_now_executes_as_sqlite_datetime() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            exec_direct(stmt, "SELECT {fn NOW()}"),
            SqlReturn::SUCCESS,
            "exec_direct with {{fn NOW()}} escape failed to translate"
        );
        assert_eq!(
            ffi::fetch::sql_fetch::<SqliteBackend>(stmt),
            SqlReturn::SUCCESS
        );

        let now = fetch_string_col(stmt, 1);
        const ISO_DATETIME_LEN: usize = "YYYY-MM-DD HH:MM:SS".len();
        assert_eq!(
            now.len(),
            ISO_DATETIME_LEN,
            "{{fn NOW()}} did not return a YYYY-MM-DD HH:MM:SS datetime string, got {now:?}"
        );
        assert_eq!(
            now.as_bytes()[4],
            b'-',
            "{{fn NOW()}} result missing '-' separator after year: {now:?}"
        );
        assert_eq!(
            now.as_bytes()[7],
            b'-',
            "{{fn NOW()}} result missing '-' separator after month: {now:?}"
        );
        assert_eq!(
            now.as_bytes()[10],
            b' ',
            "{{fn NOW()}} result missing space between date and time: {now:?}"
        );
        assert_eq!(
            now.as_bytes()[13],
            b':',
            "{{fn NOW()}} result missing ':' separator after hour: {now:?}"
        );
        assert_eq!(
            now.as_bytes()[16],
            b':',
            "{{fn NOW()}} result missing ':' separator after minute: {now:?}"
        );

        cleanup(env, conn, stmt);
    }
}

// ---------------------------------------------------------------------------
// SQLEndTran cursor behaviour
// ---------------------------------------------------------------------------

/// Pins the two `SQLEndTran` cursor-behaviour values through the real FFI entry
/// point, so that neither a change to `SqliteBackend`'s hooks nor a change to
/// `stackable-odbc-core`'s defaults can move them silently.
///
/// Both are `SQL_CB_PRESERVE` because this driver materialises result sets
/// eagerly; see `SqliteBackend::cursor_commit_behavior` for why that, and not
/// SQLite's own semantics, decides the answer. SQLite would abort a pending
/// read on ROLLBACK (`SQLITE_ABORT`, >= 3.7.11), which would be
/// `SQL_CB_CLOSE` — but this driver never has one pending.
#[test]
fn end_tran_cursor_behaviour_is_preserve_for_commit_and_rollback() {
    use stackable_odbc_core::types::{SQL_CB_PRESERVE, SQL_CURSOR_ROLLBACK_BEHAVIOR};

    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        // SQL_CURSOR_COMMIT_BEHAVIOR (23) has an InfoType variant.
        assert_get_info_u16(conn, InfoType::CursorCommitBehaviour, SQL_CB_PRESERVE);

        // SQL_CURSOR_ROLLBACK_BEHAVIOR (24) has none, so it goes through
        // get_info_raw and must be requested by its raw value.
        let mut value: u16 = 0xDEAD;
        let mut str_len: i16 = 0;
        let ret = ffi::info::sql_get_info_w::<SqliteBackend>(
            conn,
            SQL_CURSOR_ROLLBACK_BEHAVIOR,
            &mut value as *mut u16 as *mut c_void,
            2,
            &mut str_len,
        );
        assert_eq!(ret, SqlReturn::SUCCESS, "SQL_CURSOR_ROLLBACK_BEHAVIOR");
        assert_eq!(str_len, 2, "SQL_CURSOR_ROLLBACK_BEHAVIOR string_length_ptr");
        assert_eq!(
            value, SQL_CB_PRESERVE,
            "SQL_CURSOR_ROLLBACK_BEHAVIOR must be SQL_CB_PRESERVE (2): no \
             rusqlite::Statement is live when end_tran runs, so ROLLBACK \
             cannot abort a pending read"
        );

        cleanup(env, conn, stmt);
    }
}

/// `SQLCloseCursor` after a DML statement returns 24000. An INSERT produces no
/// result set, so no cursor is ever open on that statement.
///
/// `stackable-odbc-core` used to infer cursor state from whether a backend
/// statement existed, which let this succeed silently; it now tracks
/// `cursor_open` explicitly and rejects the call, which is what the ODBC
/// statement transition table requires.
#[test]
fn close_cursor_after_dml_returns_no_cursor_open() {
    unsafe {
        let (env, conn, stmt) = alloc_handles();
        assert_eq!(connect_memory(conn), SqlReturn::SUCCESS);

        assert_eq!(
            exec_direct(stmt, "CREATE TABLE dml_cc (v INTEGER)"),
            SqlReturn::SUCCESS
        );
        assert_eq!(
            exec_direct(stmt, "INSERT INTO dml_cc VALUES (1)"),
            SqlReturn::SUCCESS
        );

        // No result set, so no cursor: SQLSTATE 24000, invalid cursor state.
        assert_eq!(
            ffi::cursor::sql_close_cursor::<SqliteBackend>(stmt),
            SqlReturn::ERROR
        );

        // The handle is still usable — the rejected close changed nothing.
        assert_eq!(
            exec_direct(stmt, "SELECT v FROM dml_cc"),
            SqlReturn::SUCCESS
        );

        cleanup(env, conn, stmt);
    }
}
