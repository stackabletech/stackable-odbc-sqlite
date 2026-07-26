//! Statement execution for the SQLite backend: `exec_direct`, `prepare` and
//! `execute` (including inline parameter binding), plus the
//! [`StatementBackend`] implementation. SELECT results are fetched eagerly and
//! streamed back through the shared `stackable-odbc-core` fetch path.

use stackable_odbc_core::backend::StatementBackend;
use stackable_odbc_core::errors::OdbcError;
use stackable_odbc_core::types::{
    CDataType, ColumnDescriptor, ColumnValue, ExecuteOutcome, FetchResult,
};

use super::info::sqlite_bare_type_name;
use super::{SqliteConnection, SqliteError, SqliteStatement, map_sqlite_error};
use crate::type_conversion::{
    column_value_to_rusqlite, sqlite_declared_type_precision, sqlite_declared_type_scale,
    sqlite_type_to_sql_data_type, sqlite_value_to_column_value,
};

pub(super) fn exec_direct(
    conn: &SqliteConnection,
    sql: &str,
) -> Result<SqliteStatement, SqliteError> {
    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;

    // Prepare the statement to inspect column count.
    let mut stmt = db.prepare(sql).map_err(map_sqlite_error)?;

    // Statements with no result columns are DML (INSERT/UPDATE/DELETE) or DDL.
    // Use execute() to run them and capture the affected-row count.
    if stmt.column_count() == 0 {
        let n = db.execute(sql, []).map_err(map_sqlite_error)?;
        return Ok(SqliteStatement::dml(n));
    }

    // SELECT path: collect column metadata, then eagerly fetch all rows.
    // Fully-qualified call to avoid name collision with Backend::columns.
    let sqlite_columns = rusqlite::Statement::columns(&stmt);
    let columns: Vec<ColumnDescriptor> = sqlite_columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let name = stmt
                .column_name(i)
                .map(|n| n.to_string())
                .unwrap_or_else(|_| "?".to_string());
            let decl = col.decl_type().unwrap_or("TEXT").to_string();
            let sql_type = sqlite_type_to_sql_data_type(&decl);
            ColumnDescriptor {
                name,
                sql_type,
                precision: sqlite_declared_type_precision(&decl),
                scale: sqlite_declared_type_scale(&decl),
                // Spec (SQL_DESC_TYPE_NAME / SQLColumns.TYPE_NAME): both list
                // bare examples ("CHAR", "VARCHAR", ...), not declarations, so
                // `decl` ("VARCHAR(50)") matches no `SQLGetTypeInfo` row.
                // `sqlite_bare_type_name` returns the bare name that does
                // (see its doc comment in `backend/info.rs`); the declared
                // length is not lost, only moved out of the name; it is still
                // carried above via `sqlite_declared_type_precision`.
                type_name: sqlite_bare_type_name(sql_type).to_string(),
                // Result-set columns are reported as nullable: a prepared
                // SELECT exposes no per-column NOT NULL metadata, so the driver
                // does not attempt to distinguish non-nullable columns here.
                nullable: true,
            }
        })
        .collect();

    // Eagerly fetch all rows
    let col_count = stmt.column_count();
    let mut rows = Vec::new();
    let mut raw_rows = stmt.query([]).map_err(map_sqlite_error)?;
    while let Some(row) = raw_rows.next().map_err(map_sqlite_error)? {
        let mut row_values = Vec::with_capacity(col_count);
        for (i, col) in columns.iter().enumerate() {
            let value: rusqlite::types::Value = row.get(i).map_err(map_sqlite_error)?;
            row_values.push(sqlite_value_to_column_value(value, col.sql_type));
        }
        rows.push(row_values);
    }

    Ok(SqliteStatement::new(columns, rows))
}

/// Validate and store a SQL statement for later execution via [`Backend::execute`].
///
/// The SQL is parsed by rusqlite to detect syntax errors early (at prepare time,
/// matching ODBC semantics). The validated SQL is stored in the returned
/// [`SqliteStatement`]; actual execution is deferred until `execute()` is called.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprepare-function>
pub(super) fn prepare(conn: &SqliteConnection, sql: &str) -> Result<SqliteStatement, SqliteError> {
    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;
    // Parse-validate the SQL. `prepare_cached` compiles it into the
    // connection's statement cache so `execute()` reuses this compilation
    // instead of recompiling. The cached statement is returned to the cache
    // when the returned handle drops at the end of this function.
    db.prepare_cached(sql).map_err(map_sqlite_error)?;
    Ok(SqliteStatement::prepared(sql.to_string()))
}

/// Execute a previously prepared statement with the given parameter values.
///
/// `params` corresponds to the values collected from `SQLBindParameter` calls.
/// The statement may be executed multiple times with different parameters.
/// Results (columns + rows or affected-row count) are stored back into `stmt`.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlexecute-function>
pub(super) fn execute(
    conn: &SqliteConnection,
    stmt: &mut SqliteStatement,
    params: &[ColumnValue],
) -> Result<ExecuteOutcome, SqliteError> {
    let sql = stmt
        .prepared_sql
        .clone()
        .ok_or_else(|| SqliteError::General {
            message: "execute() called on a statement with no prepared SQL".into(),
        })?;

    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;

    let rusqlite_params: Vec<rusqlite::types::Value> =
        params.iter().map(column_value_to_rusqlite).collect();

    let mut prepared = db.prepare_cached(&sql).map_err(map_sqlite_error)?;

    if prepared.column_count() == 0 {
        // DML / DDL path
        let n = prepared
            .execute(rusqlite::params_from_iter(rusqlite_params))
            .map_err(map_sqlite_error)?;
        stmt.columns = vec![];
        stmt.rows = vec![];
        stmt.affected_rows = Some(n);
        stmt.cursor = -1;
        // SQLite has no stored-procedure output parameters.
        return Ok(ExecuteOutcome::default());
    }

    // SELECT path
    let sqlite_columns = rusqlite::Statement::columns(&prepared);
    let columns: Vec<ColumnDescriptor> = sqlite_columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let name = prepared
                .column_name(i)
                .map(|n| n.to_string())
                .unwrap_or_else(|_| "?".to_string());
            let decl = col.decl_type().unwrap_or("TEXT").to_string();
            let sql_type = sqlite_type_to_sql_data_type(&decl);
            ColumnDescriptor {
                name,
                sql_type,
                precision: sqlite_declared_type_precision(&decl),
                scale: sqlite_declared_type_scale(&decl),
                // See the `exec_direct` block above for why this is not `decl`.
                type_name: sqlite_bare_type_name(sql_type).to_string(),
                nullable: true,
            }
        })
        .collect();

    let col_count = prepared.column_count();
    let mut rows = Vec::new();
    let mut raw_rows = prepared
        .query(rusqlite::params_from_iter(rusqlite_params))
        .map_err(map_sqlite_error)?;
    while let Some(row) = raw_rows.next().map_err(map_sqlite_error)? {
        let mut row_values = Vec::with_capacity(col_count);
        for (i, col) in columns.iter().enumerate() {
            let value: rusqlite::types::Value = row.get(i).map_err(map_sqlite_error)?;
            row_values.push(sqlite_value_to_column_value(value, col.sql_type));
        }
        rows.push(row_values);
    }

    stmt.columns = columns;
    stmt.rows = rows;
    stmt.affected_rows = None;
    stmt.cursor = -1;
    // SQLite has no stored-procedure output parameters.
    Ok(ExecuteOutcome::default())
}

impl StatementBackend for SqliteStatement {
    fn fetch(&mut self) -> Result<FetchResult, OdbcError> {
        self.cursor += 1;
        if (self.cursor as usize) < self.rows.len() {
            Ok(FetchResult::Row)
        } else {
            Ok(FetchResult::NoData)
        }
    }

    fn get_data(
        &mut self,
        col: u16,
        _target_type: CDataType,
    ) -> Result<std::borrow::Cow<'_, ColumnValue>, OdbcError> {
        use stackable_odbc_core::types::SqlState;
        if self.cursor < 0 || self.cursor as usize >= self.rows.len() {
            return Err(OdbcError::NoResultSet);
        }
        let col_idx = (col as usize).checked_sub(1).ok_or_else(|| {
            OdbcError::general("Column index must be >= 1", SqlState::general_error())
        })?;
        let row = &self.rows[self.cursor as usize];
        row.get(col_idx)
            .map(std::borrow::Cow::Borrowed)
            .ok_or_else(|| {
                OdbcError::general(
                    format!(
                        "Column index {} out of range (have {} columns)",
                        col,
                        row.len()
                    ),
                    SqlState::general_error(),
                )
            })
    }

    fn column_count(&self) -> u16 {
        self.columns.len() as u16
    }

    fn describe_col(&self, col: u16) -> Result<ColumnDescriptor, OdbcError> {
        use stackable_odbc_core::types::SqlState;
        let idx = (col as usize).checked_sub(1).ok_or_else(|| {
            OdbcError::general("Column index must be >= 1", SqlState::general_error())
        })?;
        self.columns.get(idx).cloned().ok_or_else(|| {
            OdbcError::general(
                format!("Column {} out of range", col),
                SqlState::general_error(),
            )
        })
    }

    fn row_count(&self) -> Option<usize> {
        Some(self.affected_rows.unwrap_or(self.rows.len()))
    }

    fn close_cursor(&mut self) {
        self.cursor = -1;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    use stackable_odbc_core::backend::StatementBackend;
    use stackable_odbc_core::errors::OdbcError;
    use stackable_odbc_core::types::{CDataType, ColumnValue, FetchResult};

    use super::*;
    use crate::backend::{SqliteConnection, SqliteStatement};

    fn conn_with(schema: &str) -> SqliteConnection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(schema).unwrap();
        SqliteConnection {
            conn: Mutex::new(c),
            manual_commit: AtomicBool::new(false),
        }
    }

    #[test]
    fn exec_direct_select_returns_columns_and_rows() {
        let conn = conn_with(
            "CREATE TABLE t (id INTEGER, name TEXT);
             INSERT INTO t VALUES (1, 'a'), (2, 'b');",
        );
        let mut stmt = exec_direct(&conn, "SELECT id, name FROM t ORDER BY id").unwrap();

        assert_eq!(stmt.column_count(), 2);
        assert_eq!(stmt.describe_col(1).unwrap().name, "id");
        assert_eq!(stmt.describe_col(2).unwrap().name, "name");
        assert_eq!(stmt.row_count(), Some(2));

        assert!(matches!(stmt.fetch().unwrap(), FetchResult::Row));
        assert!(matches!(
            stmt.get_data(1, CDataType::SLong).unwrap().as_ref(),
            ColumnValue::I64(1)
        ));
        assert!(matches!(
            stmt.get_data(2, CDataType::SLong).unwrap().as_ref(),
            ColumnValue::String(s) if s.as_str() == "a"
        ));
        assert!(matches!(stmt.fetch().unwrap(), FetchResult::Row));
        assert!(matches!(stmt.fetch().unwrap(), FetchResult::NoData));
    }

    #[test]
    fn exec_direct_dml_reports_affected_rows() {
        let conn = conn_with("CREATE TABLE t (id INTEGER);");
        let stmt = exec_direct(&conn, "INSERT INTO t VALUES (1), (2), (3)").unwrap();
        assert_eq!(stmt.column_count(), 0);
        assert_eq!(stmt.row_count(), Some(3));
    }

    #[test]
    fn exec_direct_ddl_has_no_columns() {
        let conn = conn_with("CREATE TABLE base (id INTEGER);");
        let stmt = exec_direct(&conn, "CREATE TABLE more (x TEXT)").unwrap();
        assert_eq!(stmt.column_count(), 0);
    }

    #[test]
    fn exec_direct_syntax_error_maps_to_42000() {
        let conn = conn_with("CREATE TABLE t (id INTEGER);");
        let err = match exec_direct(&conn, "SELEC bogus FROM t") {
            Ok(_) => panic!("expected a syntax error"),
            Err(e) => e,
        };
        let odbc: OdbcError = err.into();
        assert_eq!(odbc.sqlstate().as_str(), "42000");
    }

    #[test]
    fn prepare_rejects_invalid_sql_early() {
        let conn = conn_with("CREATE TABLE t (id INTEGER);");
        assert!(prepare(&conn, "INStERT bogus").is_err());
    }

    #[test]
    fn prepare_then_execute_inserts_with_params() {
        let conn = conn_with("CREATE TABLE t (id INTEGER, name TEXT);");
        let mut stmt = prepare(&conn, "INSERT INTO t (id, name) VALUES (?, ?)").unwrap();

        execute(
            &conn,
            &mut stmt,
            &[ColumnValue::I64(1), ColumnValue::String("a".into())],
        )
        .unwrap();
        assert_eq!(stmt.row_count(), Some(1));

        // The same prepared statement re-executes with fresh parameters.
        execute(
            &conn,
            &mut stmt,
            &[ColumnValue::I64(2), ColumnValue::String("b".into())],
        )
        .unwrap();

        let mut check = exec_direct(&conn, "SELECT COUNT(*) FROM t").unwrap();
        assert!(matches!(check.fetch().unwrap(), FetchResult::Row));
        assert!(matches!(
            check.get_data(1, CDataType::SLong).unwrap().as_ref(),
            ColumnValue::I64(2)
        ));
    }

    #[test]
    fn execute_without_prepared_sql_errors() {
        let conn = conn_with("CREATE TABLE t (id INTEGER);");
        let mut stmt = SqliteStatement::new(vec![], vec![]);
        assert!(execute(&conn, &mut stmt, &[]).is_err());
    }

    #[test]
    fn get_data_before_fetch_is_no_result_set() {
        let conn = conn_with("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);");
        let mut stmt = exec_direct(&conn, "SELECT id FROM t").unwrap();
        // No fetch() yet: the cursor is before the first row.
        assert!(matches!(
            stmt.get_data(1, CDataType::SLong),
            Err(OdbcError::NoResultSet)
        ));
    }

    #[test]
    fn describe_col_out_of_range_errors() {
        let conn = conn_with("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);");
        let stmt = exec_direct(&conn, "SELECT id FROM t").unwrap();
        assert!(stmt.describe_col(0).is_err());
        assert!(stmt.describe_col(2).is_err());
    }
}
