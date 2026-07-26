use std::sync::Mutex;

use snafu::Snafu;
use stackable_odbc_core::{
    backend::Backend,
    errors::OdbcError,
    types::{
        ColumnDescriptor, ColumnValue, ConnectParams, CursorBehavior, ExecuteOutcome, InfoValue,
        SQL_CN_ANY, SQL_GB_NO_RELATION, SQL_NC_LOW, SQL_NNC_NON_NULL, SQL_TXN_SERIALIZABLE,
        TypeInfoRow,
    },
};

mod execute;
// `pub(crate)` only under `cfg(test)`: the FFI integration tests
// (`ffi_integration_tests.rs`, a sibling of this module under `lib.rs`, not
// a descendant of it) need to reach the `SQLITE_*` capability bitmap
// constants declared in `info`. Non-test callers of this module (`execute`,
// `metadata`) are themselves descendants of `backend` and can already see a
// plain private `mod info` without any visibility widening.
#[cfg(test)]
pub(crate) mod info;
#[cfg(not(test))]
mod info;
mod metadata;
mod params;
mod types;

/// The SQLite [`Backend`] implementation.
///
/// A zero-sized type: it carries no state, serving only as the type parameter
/// that [`stackable_odbc_core::forward_ffi!`] instantiates the generic ODBC C ABI entry
/// points with. All per-connection state lives in `SqliteConnection`.
pub struct SqliteBackend;

pub struct SqliteConnection {
    pub conn: Mutex<rusqlite::Connection>,
    /// True while the application has turned autocommit off. `end_tran` reads
    /// this to decide whether to open the next transaction after committing.
    pub(crate) manual_commit: std::sync::atomic::AtomicBool,
}

pub struct SqliteStatement {
    /// SQL text set by `prepare()`. Present until `execute()` has run.
    pub(crate) prepared_sql: Option<String>,
    columns: Vec<ColumnDescriptor>,
    rows: Vec<Vec<ColumnValue>>,
    cursor: i64,                  // -1 = before first row
    affected_rows: Option<usize>, // Some(n) for DML; None for SELECT (use rows.len())
}

impl SqliteStatement {
    /// Create a new SqliteStatement for a SELECT result set.
    pub fn new(columns: Vec<ColumnDescriptor>, rows: Vec<Vec<ColumnValue>>) -> Self {
        Self {
            prepared_sql: None,
            columns,
            rows,
            cursor: -1,
            affected_rows: None,
        }
    }

    /// Create a new SqliteStatement representing a completed DML statement
    /// (INSERT / UPDATE / DELETE / DDL). `affected_rows` is the count reported
    /// by rusqlite's `execute()`.
    pub fn dml(affected_rows: usize) -> Self {
        Self {
            prepared_sql: None,
            columns: vec![],
            rows: vec![],
            cursor: -1,
            affected_rows: Some(affected_rows),
        }
    }

    /// Create a new SqliteStatement that holds prepared SQL but has not yet
    /// been executed. Call `execute()` with parameter values to run it.
    pub fn prepared(sql: String) -> Self {
        Self {
            prepared_sql: Some(sql),
            columns: vec![],
            rows: vec![],
            cursor: -1,
            affected_rows: None,
        }
    }
}

#[derive(Debug, Snafu)]
pub enum SqliteError {
    #[snafu(display("SQLite error: {source}"))]
    Rusqlite { source: rusqlite::Error },
    #[snafu(display("Missing parameter: {name}"))]
    MissingParam { name: String },
    #[snafu(display("{feature} is not implemented"))]
    NotImplemented { feature: String },
    #[snafu(display("{message}"))]
    General { message: String },

    // --- Classified variants produced by `map_sqlite_error` ---
    #[snafu(display("unable to open database: {message}"))]
    ConnectionFailed { message: String },
    #[snafu(display("integrity constraint violation: {message}"))]
    ConstraintViolation { message: String },
    #[snafu(display("syntax error or access violation: {message}"))]
    SyntaxError { message: String },
    #[snafu(display("table or view not found: {message}"))]
    TableNotFound { message: String },
    #[snafu(display("column not found: {message}"))]
    ColumnNotFound { message: String },
    #[snafu(display("database is busy: {message}"))]
    DatabaseBusy { message: String },
    #[snafu(display("data type mismatch: {message}"))]
    DataTypeMismatch { message: String },
    #[snafu(display("numeric value out of range: {message}"))]
    NumericOutOfRange { message: String },
}

/// Central mapping from `rusqlite` errors to [`SqliteError`].
///
/// Every error originating from rusqlite must be routed through this function
/// so that SQLSTATE selection happens in exactly one place. Hand-building an
/// error at the call site silently degrades a specific SQLSTATE to `HY000`.
///
/// SQLite reports syntax errors, missing tables and missing columns with the
/// same result code (`SQLITE_ERROR`), so those three are distinguished by the
/// message text, which SQLite generates from a fixed set of formats.
pub(crate) fn map_sqlite_error(e: rusqlite::Error) -> SqliteError {
    use rusqlite::ErrorCode;

    match e {
        rusqlite::Error::SqliteFailure(ref ffi_err, ref msg) => {
            let message = msg.clone().unwrap_or_else(|| e.to_string());
            match ffi_err.code {
                ErrorCode::ConstraintViolation => SqliteError::ConstraintViolation { message },
                ErrorCode::CannotOpen | ErrorCode::NotADatabase | ErrorCode::PermissionDenied => {
                    SqliteError::ConnectionFailed { message }
                }
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => {
                    SqliteError::DatabaseBusy { message }
                }
                ErrorCode::TypeMismatch => SqliteError::DataTypeMismatch { message },
                // SQLITE_ERROR covers syntax errors and unresolved names alike.
                ErrorCode::Unknown => classify_sqlite_error_message(message),
                _ => SqliteError::Rusqlite { source: e },
            }
        }
        // Failures raised while compiling SQL. rusqlite reports these as a
        // distinct variant from `SqliteFailure`, but the classification is the
        // same: SQLITE_ERROR with a message that names the failure.
        rusqlite::Error::SqlInputError {
            ref error, ref msg, ..
        } => match error.code {
            ErrorCode::ConstraintViolation => SqliteError::ConstraintViolation {
                message: msg.clone(),
            },
            ErrorCode::Unknown => classify_sqlite_error_message(msg.clone()),
            _ => SqliteError::Rusqlite { source: e },
        },
        // Errors rusqlite raises itself, without a SQLite result code.
        rusqlite::Error::InvalidColumnName(ref name) => SqliteError::ColumnNotFound {
            message: format!("no such column: {name}"),
        },
        rusqlite::Error::InvalidColumnType(..) | rusqlite::Error::FromSqlConversionFailure(..) => {
            SqliteError::DataTypeMismatch {
                message: e.to_string(),
            }
        }
        rusqlite::Error::IntegralValueOutOfRange(..) => SqliteError::NumericOutOfRange {
            message: e.to_string(),
        },
        other => SqliteError::Rusqlite { source: other },
    }
}

/// Split a `SQLITE_ERROR` message into the SQLSTATE classes the ODBC spec
/// distinguishes. SQLite's wording for these is stable across versions.
fn classify_sqlite_error_message(message: String) -> SqliteError {
    if message.starts_with("no such table") || message.starts_with("no such view") {
        SqliteError::TableNotFound { message }
    } else if message.starts_with("no such column") {
        SqliteError::ColumnNotFound { message }
    } else {
        SqliteError::SyntaxError { message }
    }
}

impl From<SqliteError> for OdbcError {
    fn from(e: SqliteError) -> Self {
        use stackable_odbc_core::types::SqlState;

        let sqlstate = match &e {
            SqliteError::NotImplemented { feature } => {
                return OdbcError::NotImplemented {
                    feature: feature.clone(),
                };
            }
            SqliteError::ConnectionFailed { .. } => {
                SqlState::client_unable_to_establish_connection()
            }
            SqliteError::ConstraintViolation { .. } => SqlState::integrity_constraint_violation(),
            SqliteError::SyntaxError { .. } => SqlState::syntax_error_or_access_violation(),
            SqliteError::TableNotFound { .. } => SqlState::base_table_or_view_not_found(),
            SqliteError::ColumnNotFound { .. } => SqlState::column_not_found(),
            SqliteError::DatabaseBusy { .. } => SqlState::timeout_expired(),
            SqliteError::DataTypeMismatch { .. } => {
                SqlState::restricted_data_type_attribute_violation()
            }
            SqliteError::NumericOutOfRange { .. } => SqlState::numeric_value_out_of_range(),
            SqliteError::Rusqlite { .. }
            | SqliteError::MissingParam { .. }
            | SqliteError::General { .. } => SqlState::general_error(),
        };
        OdbcError::General {
            message: e.to_string(),
            sqlstate,
        }
    }
}

impl Backend for SqliteBackend {
    type Connection = SqliteConnection;
    type Error = SqliteError;
    type Statement = SqliteStatement;

    fn connect(params: &ConnectParams) -> Result<SqliteConnection, SqliteError> {
        let p = types::connect_params::SqliteConnectParams::try_from(params)?;
        let conn = rusqlite::Connection::open(p.database()).map_err(map_sqlite_error)?;

        // Enforce foreign keys explicitly, so that `SQL_INTEGRITY = "Y"` is
        // true by construction rather than by build configuration.
        //
        // SQLite defaults this off for backward compatibility. The bundled
        // library happens to be compiled with `SQLITE_DEFAULT_FOREIGN_KEYS`,
        // so it was already on — but that is a property of one dependency's
        // build, not of SQLite, and dropping `rusqlite`'s `bundled` feature
        // for a system library would silently turn referential integrity off
        // while the driver went on advertising it.
        //
        // The pragma is per-connection and a no-op inside a transaction; here
        // there is not one yet. `PRAGMA foreign_keys` is also a no-op rather
        // than an error on a build compiled with `SQLITE_OMIT_FOREIGN_KEY`,
        // which is why `Backend::connect` cannot treat success as proof —
        // `integrity_enhancement_facility_is_actually_enforced` reads the
        // value back through this function.
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .map_err(map_sqlite_error)?;

        Ok(SqliteConnection {
            conn: Mutex::new(conn),
            manual_commit: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn disconnect(_conn: &mut SqliteConnection) -> Result<(), SqliteError> {
        Ok(()) // rusqlite closes on drop
    }

    fn browse_connect_attrs() -> &'static [&'static str] {
        &["database"]
    }

    /// SQLite supports transactions and this driver reports `SQL_TC_DML` for
    /// `SQL_TXN_CAPABLE`, so manual-commit mode must actually be honoured.
    ///
    /// Manual-commit mode is entered by opening a transaction with `BEGIN`;
    /// `end_tran` then commits or rolls it back and, while still in
    /// manual-commit mode, opens the next one.
    fn set_autocommit(conn: &SqliteConnection, enabled: bool) -> Result<(), OdbcError> {
        let db = conn.conn.lock().map_err(|e| {
            OdbcError::general(
                format!("Mutex poisoned: {e}"),
                stackable_odbc_core::types::SqlState::general_error(),
            )
        })?;
        if enabled {
            // Returning to autocommit commits any open transaction, per the
            // ODBC spec: "Any open transactions on the connection are committed
            // when SQL_ATTR_AUTOCOMMIT is set to SQL_AUTOCOMMIT_ON".
            if !db.is_autocommit() {
                db.execute_batch("COMMIT")
                    .map_err(|e| OdbcError::from(map_sqlite_error(e)))?;
            }
        } else if db.is_autocommit() {
            db.execute_batch("BEGIN")
                .map_err(|e| OdbcError::from(map_sqlite_error(e)))?;
        }
        conn.manual_commit
            .store(!enabled, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn end_tran(conn: &SqliteConnection, commit: bool) -> Result<(), OdbcError> {
        let db = conn.conn.lock().map_err(|e| {
            OdbcError::general(
                format!("Mutex poisoned: {e}"),
                stackable_odbc_core::types::SqlState::general_error(),
            )
        })?;
        // If SQLite is in autocommit mode there is no open transaction to commit/roll back.
        if db.is_autocommit() {
            return Ok(());
        }
        let sql = if commit { "COMMIT" } else { "ROLLBACK" };
        db.execute_batch(sql)
            .map_err(|e| OdbcError::from(map_sqlite_error(e)))?;

        // Still in manual-commit mode: open the next transaction, otherwise
        // subsequent statements would silently autocommit.
        if conn
            .manual_commit
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            db.execute_batch("BEGIN")
                .map_err(|e| OdbcError::from(map_sqlite_error(e)))?;
        }
        Ok(())
    }

    /// `Preserve` for both commit and rollback.
    ///
    /// This driver materialises every result set eagerly
    /// (`execute::exec_direct`), so no `rusqlite::Statement` is live when
    /// `end_tran` runs and neither SQLite failure mode is reachable: COMMIT
    /// cannot hit `SQLITE_BUSY` on a pending write, and ROLLBACK cannot abort
    /// a pending read. The materialised rows and the cursor index survive both
    /// untouched.
    ///
    /// Raw SQLite is stricter than that. From 3.7.11 a ROLLBACK aborts pending
    /// statements with `SQLITE_ABORT`, which would make rollback
    /// `SQL_CB_CLOSE`. The value below is a property of this driver's
    /// architecture, not of SQLite.
    ///
    /// If result sets ever become lazily streamed, revisit both hooks — and
    /// note that `SQL_CB_CLOSE` would then also require a real
    /// [`stackable_odbc_core::backend::StatementBackend::close_cursor`].
    ///
    /// Spec: <https://www.sqlite.org/lang_transaction.html>
    fn cursor_commit_behavior() -> CursorBehavior {
        CursorBehavior::Preserve
    }

    /// See [`SqliteBackend::cursor_commit_behavior`] — same reasoning, same
    /// value.
    fn cursor_rollback_behavior() -> CursorBehavior {
        CursorBehavior::Preserve
    }

    /// SQLite has no ODBC catalogs: `metadata::tables` reports `TABLE_CAT` as
    /// NULL for every row, and a `catalog = "%"` enumeration returns an empty
    /// result set.
    ///
    /// Core derives the whole catalog group from this — `SQL_CATALOG_NAME`,
    /// `SQL_CATALOG_TERM`, `SQL_CATALOG_NAME_SEPARATOR`,
    /// `SQL_CATALOG_LOCATION` and `SQL_CATALOG_USAGE` — so this driver answers
    /// none of them itself. Before the hook existed it answered three and let
    /// the other two inherit defaults that named a catalog, telling an
    /// application catalogs do not exist and giving their name in the same
    /// breath.
    fn supports_catalogs() -> bool {
        false
    }

    /// SQLite has no ODBC schemas: a `schema = "%"` enumeration returns an
    /// empty result set and `TABLE_SCHEM` is always NULL.
    ///
    /// Drives `SQL_SCHEMA_TERM` and `SQL_SCHEMA_USAGE`; see
    /// [`SqliteBackend::supports_catalogs`].
    fn supports_schemas() -> bool {
        false
    }

    /// The `ALTER TABLE` clauses SQLite accepts, of those the ODBC bitmap can
    /// express. See `info::SQLITE_ALTER_TABLE` for what is claimed, what is
    /// supported-but-unrepresentable, and how each bit was verified.
    fn alter_table_support() -> u32 {
        info::SQLITE_ALTER_TABLE
    }

    /// Every outer-join form SQLite implements. See
    /// `info::SQLITE_OUTER_JOIN_CAPABILITIES`.
    fn outer_join_capabilities() -> u32 {
        info::SQLITE_OUTER_JOIN_CAPABILITIES
    }

    /// "Transactions in SQLite are SERIALIZABLE."
    ///
    /// Core derives both `SQL_DEFAULT_TXN_ISOLATION` and the value
    /// `SQLGetConnectAttr(SQL_ATTR_TXN_ISOLATION)` reports on a fresh
    /// connection from this, so the two cannot disagree.
    ///
    /// Spec: <https://www.sqlite.org/isolation.html>
    fn default_txn_isolation() -> u32 {
        SQL_TXN_SERIALIZABLE
    }

    /// The only level reachable from this driver.
    ///
    /// READ COMMITTED and REPEATABLE READ are not SQLite concepts. READ
    /// UNCOMMITTED needs shared-cache mode — "the only way that one database
    /// connection can see uncommitted changes on a different database
    /// connection" — and [`SqliteBackend::connect`] opens with a plain
    /// `rusqlite::Connection::open`, so it is unreachable.
    ///
    /// Returning a single level also means core's default
    /// [`Backend::set_txn_isolation`] is correct as-is: the one supported
    /// level is always already in effect, and anything else is rejected with
    /// `HY024` before it reaches the backend.
    fn txn_isolation_options() -> u32 {
        SQL_TXN_SERIALIZABLE
    }

    /// `SQL_GB_NO_RELATION`: SQLite relates the `GROUP BY` list and the select
    /// list not at all. It accepts a bare non-aggregated column absent from
    /// `GROUP BY` (returning an arbitrary row from each group), and accepts
    /// `GROUP BY` columns and expressions absent from the select list.
    ///
    /// Verified in `group_by_is_unrelated_to_the_select_list`.
    fn group_by() -> u16 {
        SQL_GB_NO_RELATION
    }

    /// `SQL_NC_LOW`: SQLite sorts NULLs at the low end — first ascending, last
    /// descending.
    fn null_collation() -> u16 {
        SQL_NC_LOW
    }

    /// `SQL_CN_ANY`: SQLite accepts a table alias with or without `AS`, and
    /// places no restriction on the name.
    fn correlation_name() -> u16 {
        SQL_CN_ANY
    }

    /// `SQL_NNC_NON_NULL`: SQLite implements `NOT NULL` column constraints.
    fn non_nullable_columns() -> u16 {
        SQL_NNC_NON_NULL
    }

    /// SQLite takes arbitrary expressions in `ORDER BY`, including over columns
    /// absent from the select list.
    fn expressions_in_order_by() -> bool {
        true
    }

    /// No SQL-92 conformance level is claimed.
    ///
    /// The previous `SQL_SC_SQL92_ENTRY` came from a core default, not from any
    /// assessment of SQLite, and it contradicted this driver's own answers. The
    /// spec ties entry level to three values: "a SQL-92 Entry level-conformant
    /// driver will always return the SQL_GB_GROUP_BY_EQUALS_SELECT option as
    /// supported", "will always return SQL_CN_ANY", and "will return
    /// SQL_NNC_NON_NULL". This driver matches the last two and cannot match the
    /// first — SQLite's `GROUP BY` is deliberately unrelated to the select list
    /// (see [`SqliteBackend::group_by`]), which is a permissive extension, not
    /// entry-level behaviour.
    ///
    /// `0` is the honest answer: it claims no level rather than asserting one
    /// the driver demonstrably fails. Raising it later means auditing SQL-92
    /// entry level properly, not restoring the value core used to invent.
    fn sql_conformance() -> u32 {
        0
    }

    /// `0`: `TIMESTAMPADD` is not supported. `SQLITE_TIMEDATE_FUNCTIONS`
    /// deliberately omits `SQL_FN_TD_TIMESTAMPADD`, so claiming interval units
    /// here would describe a function this driver does not offer.
    fn timedate_add_intervals() -> u32 {
        0
    }

    /// `0`: `TIMESTAMPDIFF` is not supported, for the same reason as
    /// [`SqliteBackend::timedate_add_intervals`].
    fn timedate_diff_intervals() -> u32 {
        0
    }

    // --- Delegations ---

    fn exec_direct(conn: &SqliteConnection, sql: &str) -> Result<SqliteStatement, SqliteError> {
        execute::exec_direct(conn, sql)
    }

    fn prepare(conn: &SqliteConnection, sql: &str) -> Result<SqliteStatement, SqliteError> {
        execute::prepare(conn, sql)
    }

    fn execute(
        conn: &SqliteConnection,
        stmt: &mut SqliteStatement,
        params: &[ColumnValue],
    ) -> Result<ExecuteOutcome, SqliteError> {
        execute::execute(conn, stmt, params)
    }

    fn get_info(
        conn: &SqliteConnection,
        info_type: stackable_odbc_core::types::InfoType,
    ) -> Result<InfoValue, SqliteError> {
        info::get_info(conn, info_type)
    }

    fn get_info_pre_connect(
        info_type: stackable_odbc_core::types::InfoType,
    ) -> Result<InfoValue, OdbcError> {
        info::get_info_pre_connect(info_type)
    }

    fn get_info_raw(
        conn: &SqliteConnection,
        info_type: u16,
    ) -> Option<Result<InfoValue, SqliteError>> {
        info::get_info_raw(conn, info_type)
    }

    fn get_functions() -> &'static [stackable_odbc_core::function_id::FunctionId] {
        info::get_functions()
    }

    fn get_type_info() -> &'static [TypeInfoRow] {
        info::get_type_info()
    }

    fn tables(
        conn: &SqliteConnection,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
        table_type: Option<&str>,
    ) -> Result<SqliteStatement, SqliteError> {
        metadata::tables(conn, catalog, schema, table, table_type)
    }

    fn columns(
        conn: &SqliteConnection,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
        column: Option<&str>,
    ) -> Result<SqliteStatement, SqliteError> {
        metadata::columns(conn, catalog, schema, table, column)
    }

    fn primary_keys(
        conn: &SqliteConnection,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<SqliteStatement, OdbcError> {
        metadata::primary_keys(conn, catalog, schema, table)
    }

    fn foreign_keys(
        conn: &SqliteConnection,
        pk_catalog: Option<&str>,
        pk_schema: Option<&str>,
        pk_table: Option<&str>,
        fk_catalog: Option<&str>,
        fk_schema: Option<&str>,
        fk_table: Option<&str>,
    ) -> Result<SqliteStatement, OdbcError> {
        metadata::foreign_keys(
            conn, pk_catalog, pk_schema, pk_table, fk_catalog, fk_schema, fk_table,
        )
    }

    fn statistics(
        conn: &SqliteConnection,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
        unique_only: bool,
    ) -> Result<SqliteStatement, OdbcError> {
        metadata::statistics(conn, catalog, schema, table, unique_only)
    }

    fn special_columns(
        conn: &SqliteConnection,
        identifier_type: stackable_odbc_core::types::IdentifierType,
        catalog: Option<&str>,
        schema: Option<&str>,
        table: Option<&str>,
        scope: stackable_odbc_core::types::Scope,
        nullable: stackable_odbc_core::types::Nullable,
    ) -> Result<SqliteStatement, OdbcError> {
        metadata::special_columns(
            conn,
            identifier_type,
            catalog,
            schema,
            table,
            scope,
            nullable,
        )
    }

    /// SQLite's `{fn}`/`{d}`/`{t}`/`{ts}` escape-translation dialect. See
    /// `crate::escape_dialect` for the remap table and its justification
    /// against the `SQL_*_FUNCTIONS` bitmaps in `backend/info.rs`.
    fn escape_dialect() -> stackable_odbc_core::escape::EscapeDialect {
        crate::escape_dialect::dialect()
    }
}

#[cfg(test)]
mod tests {
    use stackable_odbc_core::{
        backend::StatementBackend,
        types::{CDataType, FetchResult, SQL_DRIVER_ODBC_VER_STRING, sql_state},
    };

    use super::*;

    #[test]
    fn connect_to_in_memory_database() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let mut conn = SqliteBackend::connect(&params).unwrap();
        SqliteBackend::disconnect(&mut conn).unwrap();
    }

    #[test]
    fn connect_to_nonexistent_directory_fails() {
        // /nonexistent/path/ doesn't exist, so SQLite can't create the file
        let params = ConnectParams::parse("Database=/nonexistent/path/db.sqlite").unwrap();
        let result = SqliteBackend::connect(&params);
        assert!(result.is_err());
    }

    #[test]
    fn prepared_statement_reexecutes_with_fresh_params() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1),(2),(3);")
                .unwrap();
        }

        // Prepare a parameterized SELECT once.
        let mut stmt = SqliteBackend::prepare(&conn, "SELECT id FROM t WHERE id = ?1").unwrap();

        // First execute: matching param -> exactly one row.
        SqliteBackend::execute(&conn, &mut stmt, &[ColumnValue::I64(2)]).unwrap();
        assert!(matches!(stmt.fetch().unwrap(), FetchResult::Row));
        assert!(matches!(stmt.fetch().unwrap(), FetchResult::NoData));

        // Re-execute the SAME handle with a non-matching param -> no rows. This
        // fails if the cached compiled statement leaked the previous binding.
        SqliteBackend::execute(&conn, &mut stmt, &[ColumnValue::I64(999)]).unwrap();
        assert!(matches!(stmt.fetch().unwrap(), FetchResult::NoData));

        // And once more with a matching param -> one row again.
        SqliteBackend::execute(&conn, &mut stmt, &[ColumnValue::I64(1)]).unwrap();
        assert!(matches!(stmt.fetch().unwrap(), FetchResult::Row));
        assert!(matches!(stmt.fetch().unwrap(), FetchResult::NoData));
    }

    // --- SQLSTATE classification (map_sqlite_error) ---

    /// Run `sql` against a fresh in-memory database and return the SQLSTATE of
    /// the resulting error.
    fn sqlstate_of(setup: &str, sql: &str) -> String {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        if !setup.is_empty() {
            let db = conn.conn.lock().unwrap();
            db.execute_batch(setup).unwrap();
        }
        let Err(err) = SqliteBackend::exec_direct(&conn, sql) else {
            panic!("statement should have failed: {sql}");
        };
        OdbcError::from(err).sqlstate().as_str().to_string()
    }

    #[test]
    fn constraint_violation_produces_23000() {
        let state = sqlstate_of(
            "CREATE TABLE t (id INTEGER PRIMARY KEY); INSERT INTO t VALUES (1);",
            "INSERT INTO t VALUES (1)",
        );
        assert_eq!(state, sql_state::INTEGRITY_CONSTRAINT_VIOLATION);
    }

    #[test]
    fn not_null_violation_produces_23000() {
        let state = sqlstate_of(
            "CREATE TABLE t (id INTEGER NOT NULL);",
            "INSERT INTO t VALUES (NULL)",
        );
        assert_eq!(state, sql_state::INTEGRITY_CONSTRAINT_VIOLATION);
    }

    #[test]
    fn syntax_error_produces_42000() {
        let state = sqlstate_of("", "SELCT 1");
        assert_eq!(state, sql_state::SYNTAX_ERROR_OR_ACCESS_VIOLATION);
    }

    #[test]
    fn failed_commit_deferred_constraint_produces_23000() {
        // A deferred foreign-key violation surfaces only at COMMIT. end_tran
        // must route that rusqlite error through map_sqlite_error, reporting
        // 23000 rather than degrading it to a generic HY000.
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE child (
                     pid INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED
                 );",
            )
            .unwrap();
        }
        // Manual-commit mode; the FK violation is deferred until COMMIT.
        SqliteBackend::set_autocommit(&conn, false).unwrap();
        SqliteBackend::exec_direct(&conn, "INSERT INTO child VALUES (999)").unwrap();
        let Err(err) = SqliteBackend::end_tran(&conn, true) else {
            panic!("COMMIT should have failed the deferred foreign-key constraint");
        };
        assert_eq!(
            err.sqlstate().as_str(),
            sql_state::INTEGRITY_CONSTRAINT_VIOLATION
        );
    }

    #[test]
    fn missing_table_produces_42s02() {
        let state = sqlstate_of("", "SELECT * FROM no_such_table_here");
        assert_eq!(state, sql_state::BASE_TABLE_OR_VIEW_NOT_FOUND);
    }

    #[test]
    fn missing_column_produces_42s22() {
        let state = sqlstate_of("CREATE TABLE t (id INTEGER);", "SELECT nope FROM t");
        assert_eq!(state, sql_state::COLUMN_NOT_FOUND);
    }

    #[test]
    fn unopenable_database_produces_08001() {
        let params = ConnectParams::parse("Database=/nonexistent/path/db.sqlite").unwrap();
        let Err(err) = SqliteBackend::connect(&params) else {
            panic!("open should have failed");
        };
        assert_eq!(
            OdbcError::from(err).sqlstate().as_str(),
            sql_state::CLIENT_UNABLE_TO_ESTABLISH_CONNECTION
        );
    }

    #[test]
    fn unclassified_errors_still_produce_hy000() {
        let err = SqliteError::General {
            message: "internal invariant".into(),
        };
        assert_eq!(
            OdbcError::from(err).sqlstate().as_str(),
            sql_state::GENERAL_ERROR
        );
    }

    #[test]
    fn connect_missing_database_param_fails() {
        let params = ConnectParams::parse("Driver=SQLite").unwrap();
        let result = SqliteBackend::connect(&params);
        assert!(result.is_err());
    }

    #[test]
    fn exec_direct_returns_rows() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch(
                "CREATE TABLE t (id INTEGER, name TEXT); INSERT INTO t VALUES (1, 'hello');",
            )
            .unwrap();
        }
        let mut stmt = SqliteBackend::exec_direct(&conn, "SELECT id, name FROM t").unwrap();
        assert_eq!(stmt.column_count(), 2);
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            stmt.get_data(1, CDataType::Default).unwrap().into_owned(),
            ColumnValue::I64(1)
        );
        assert_eq!(
            stmt.get_data(2, CDataType::Default).unwrap().into_owned(),
            ColumnValue::String("hello".into())
        );
        assert_eq!(stmt.fetch().unwrap(), FetchResult::NoData);
    }

    #[test]
    fn exec_direct_empty_result() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch("CREATE TABLE t (id INTEGER)").unwrap();
        }
        let mut stmt = SqliteBackend::exec_direct(&conn, "SELECT * FROM t").unwrap();
        assert_eq!(stmt.fetch().unwrap(), FetchResult::NoData);
    }

    #[test]
    fn exec_direct_with_nulls() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch("CREATE TABLE t (v TEXT); INSERT INTO t VALUES (NULL);")
                .unwrap();
        }
        let mut stmt = SqliteBackend::exec_direct(&conn, "SELECT v FROM t").unwrap();
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            stmt.get_data(1, CDataType::Default).unwrap().into_owned(),
            ColumnValue::Null
        );
    }

    #[test]
    fn describe_col_returns_metadata() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch("CREATE TABLE t (id INTEGER, name TEXT)")
                .unwrap();
        }
        let stmt = SqliteBackend::exec_direct(&conn, "SELECT id, name FROM t").unwrap();
        let col1 = stmt.describe_col(1).unwrap();
        assert_eq!(col1.name, "id");
        let col2 = stmt.describe_col(2).unwrap();
        assert_eq!(col2.name, "name");
    }

    #[test]
    fn row_count_returns_correct_count() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch(
                "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2);",
            )
            .unwrap();
        }
        let stmt = SqliteBackend::exec_direct(&conn, "SELECT * FROM t").unwrap();
        assert_eq!(stmt.row_count(), Some(2));
    }

    #[test]
    fn get_info_dbms_name() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        let info =
            SqliteBackend::get_info(&conn, stackable_odbc_core::types::InfoType::DbmsName).unwrap();
        match info {
            InfoValue::String(s) => assert_eq!(s, "SQLite"),
            _ => panic!("Expected String InfoValue"),
        }
    }

    #[test]
    fn get_info_driver_odbc_ver() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        let info =
            SqliteBackend::get_info(&conn, stackable_odbc_core::types::InfoType::DriverOdbcVer)
                .unwrap();
        match info {
            InfoValue::String(s) => assert_eq!(s, SQL_DRIVER_ODBC_VER_STRING),
            _ => panic!("Expected String InfoValue"),
        }
    }

    #[test]
    fn exec_direct_update_returns_affected_row_count() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch(
                "CREATE TABLE t (id INTEGER, v INTEGER);
                 INSERT INTO t VALUES (1, 10);
                 INSERT INTO t VALUES (2, 20);
                 INSERT INTO t VALUES (3, 10);",
            )
            .unwrap();
        }
        let stmt = SqliteBackend::exec_direct(&conn, "UPDATE t SET v = 99 WHERE v = 10").unwrap();
        assert_eq!(stmt.column_count(), 0);
        assert_eq!(stmt.row_count(), Some(2)); // rows 1 and 3 were updated
    }

    #[test]
    fn exec_direct_delete_returns_affected_row_count() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch(
                "CREATE TABLE t (id INTEGER);
                 INSERT INTO t VALUES (1);
                 INSERT INTO t VALUES (2);
                 INSERT INTO t VALUES (3);",
            )
            .unwrap();
        }
        let stmt = SqliteBackend::exec_direct(&conn, "DELETE FROM t WHERE id > 1").unwrap();
        assert_eq!(stmt.column_count(), 0);
        assert_eq!(stmt.row_count(), Some(2));

        // Confirm only row 1 remains
        let mut sel = SqliteBackend::exec_direct(&conn, "SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(sel.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            sel.get_data(1, CDataType::Default).unwrap().into_owned(),
            ColumnValue::I64(1)
        );
    }

    #[test]
    fn exec_direct_fetch_on_dml_returns_no_data() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch("CREATE TABLE t (id INTEGER)").unwrap();
        }
        let mut stmt = SqliteBackend::exec_direct(&conn, "INSERT INTO t VALUES (1)").unwrap();
        // DML results have no rows; fetch must return NoData immediately
        assert_eq!(stmt.fetch().unwrap(), FetchResult::NoData);
    }

    // ---------------------------------------------------------------------------
    // prepare + execute tests
    // ---------------------------------------------------------------------------

    #[test]
    fn prepare_valid_sql_succeeds() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch("CREATE TABLE t (id INTEGER, name TEXT)")
                .unwrap();
        }
        let stmt = SqliteBackend::prepare(&conn, "SELECT id FROM t WHERE id = ?").unwrap();
        assert_eq!(
            stmt.prepared_sql.as_deref(),
            Some("SELECT id FROM t WHERE id = ?")
        );
        assert_eq!(stmt.column_count(), 0); // not yet executed
    }

    #[test]
    fn prepare_invalid_sql_returns_error() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        let result = SqliteBackend::prepare(&conn, "NOT VALID SQL %%%");
        assert!(result.is_err());
    }

    #[test]
    fn execute_select_with_param() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch(
                "CREATE TABLE t (id INTEGER, name TEXT);\
                 INSERT INTO t VALUES (1, 'alice');\
                 INSERT INTO t VALUES (2, 'bob');",
            )
            .unwrap();
        }
        let mut stmt = SqliteBackend::prepare(&conn, "SELECT name FROM t WHERE id = ?").unwrap();
        SqliteBackend::execute(&conn, &mut stmt, &[ColumnValue::I64(1)]).unwrap();
        assert_eq!(stmt.column_count(), 1);
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            stmt.get_data(1, CDataType::Default).unwrap().into_owned(),
            ColumnValue::String("alice".into())
        );
        assert_eq!(stmt.fetch().unwrap(), FetchResult::NoData);
    }

    #[test]
    fn execute_can_be_called_multiple_times() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch(
                "CREATE TABLE t (id INTEGER, name TEXT);\
                 INSERT INTO t VALUES (1, 'alice');\
                 INSERT INTO t VALUES (2, 'bob');",
            )
            .unwrap();
        }
        let mut stmt = SqliteBackend::prepare(&conn, "SELECT name FROM t WHERE id = ?").unwrap();

        // First execution
        SqliteBackend::execute(&conn, &mut stmt, &[ColumnValue::I64(1)]).unwrap();
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            stmt.get_data(1, CDataType::Default).unwrap().into_owned(),
            ColumnValue::String("alice".into())
        );

        // Re-execute with different param
        SqliteBackend::execute(&conn, &mut stmt, &[ColumnValue::I64(2)]).unwrap();
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            stmt.get_data(1, CDataType::Default).unwrap().into_owned(),
            ColumnValue::String("bob".into())
        );
    }

    #[test]
    fn execute_dml_with_param() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch("CREATE TABLE t (id INTEGER, name TEXT)")
                .unwrap();
        }
        let mut stmt = SqliteBackend::prepare(&conn, "INSERT INTO t VALUES (?, ?)").unwrap();
        SqliteBackend::execute(
            &conn,
            &mut stmt,
            &[ColumnValue::I64(42), ColumnValue::String("test".into())],
        )
        .unwrap();
        assert_eq!(stmt.row_count(), Some(1));

        // Verify with exec_direct
        let mut q = SqliteBackend::exec_direct(&conn, "SELECT id, name FROM t").unwrap();
        assert_eq!(q.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            q.get_data(1, CDataType::Default).unwrap().into_owned(),
            ColumnValue::I64(42)
        );
        assert_eq!(
            q.get_data(2, CDataType::Default).unwrap().into_owned(),
            ColumnValue::String("test".into())
        );
    }

    #[test]
    fn execute_select_with_null_param() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).unwrap();
        {
            let db = conn.conn.lock().unwrap();
            db.execute_batch(
                "CREATE TABLE t (id INTEGER, name TEXT);\
                 INSERT INTO t VALUES (1, NULL);",
            )
            .unwrap();
        }
        let mut stmt = SqliteBackend::prepare(&conn, "SELECT name FROM t WHERE id = ?").unwrap();
        SqliteBackend::execute(&conn, &mut stmt, &[ColumnValue::I64(1)]).unwrap();
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            stmt.get_data(1, CDataType::Default).unwrap().into_owned(),
            ColumnValue::Null
        );
    }
}
