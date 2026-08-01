use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
};

use snafu::Snafu;
use stackable_odbc_core::{
    backend::Backend,
    errors::OdbcError,
    types::{
        ColumnDescriptor, ColumnRow, ColumnValue, ConnectParams, CursorBehavior, ExecuteOutcome,
        ForeignKeyRow, InfoValue, PrimaryKeyRow, SQL_CB_NULL, SQL_CN_ANY, SQL_GB_NO_RELATION,
        SQL_IC_MIXED, SQL_NC_LOW, SQL_NNC_NON_NULL, SQL_TC_DML, SQL_TXN_SERIALIZABLE,
        SpecialColumnRow, StatisticsRow, TableRow, TypeInfoRow,
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
    /// `sqlite3_interrupt`'s handle for this connection, captured in
    /// [`SqliteBackend::connect`] and handed to every statement as its cancel
    /// token. See [`SqliteBackend::CancelToken`].
    ///
    /// Held here rather than taken from `conn` on demand because
    /// [`Backend::cancel_token`] cannot fail and cannot block: reaching through
    /// the `Mutex` would mean either waiting on whatever thread is executing or
    /// inventing an answer for a poisoned lock. Capturing it once at connect
    /// time is also what core's `cancel_token` doc asks for: assemble the
    /// token with the connection in hand, never lazily inside `cancel`.
    pub(crate) interrupt: Arc<rusqlite::InterruptHandle>,
    /// True while the application has turned autocommit off. `end_tran` reads
    /// this to decide whether to open the next transaction after committing.
    pub(crate) manual_commit: std::sync::atomic::AtomicBool,
}

pub struct SqliteStatement {
    /// SQL text set by `prepare()`. Present until `execute()` has run.
    pub(crate) prepared_sql: Option<String>,
    columns: Vec<ColumnDescriptor>,
    rows: Vec<Vec<ColumnValue>>,
    cursor: i64, // -1 = before first row
    /// `Some(n)` for a searched INSERT / UPDATE / DELETE, which is the only
    /// case with an affected-row count. `None` everywhere else: a SELECT
    /// reports its materialised row count instead, and DDL, transaction
    /// control and the rest have no count at all. See
    /// `StatementBackend::row_count` in `execute.rs` for why the distinction
    /// between "counted zero" and "no count" is load-bearing.
    affected_rows: Option<usize>,
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

    /// Create a new SqliteStatement representing a completed statement that
    /// produced no result set: DML, DDL, transaction control or a PRAGMA.
    ///
    /// `affected_rows` is `Some` only for a searched INSERT / UPDATE / DELETE,
    /// carrying the count reported by rusqlite's `execute()`; everything else
    /// passes `None`. See `execute::is_searched_dml`.
    pub fn non_query(affected_rows: Option<usize>) -> Self {
        Self {
            prepared_sql: None,
            columns: vec![],
            rows: vec![],
            cursor: -1,
            affected_rows,
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
    /// An [`OdbcError`] core itself produced, carried unchanged.
    ///
    /// `Backend::Error` is bounded by `From<OdbcError>` so that a defaulted
    /// trait body can construct an error and still name `Self::Error`. This
    /// variant is how such an error travels back to core with its SQLSTATE,
    /// native error code and causal chain intact. Classifying it a second
    /// time would flatten all three.
    #[snafu(display("{source}"))]
    Odbc { source: OdbcError },

    #[snafu(display("SQLite error: {source}"))]
    Rusqlite { source: rusqlite::Error },
    #[snafu(display("Missing parameter: {name}"))]
    MissingParam { name: String },
    #[snafu(display("{feature} is not implemented"))]
    NotImplemented { feature: String },
    #[snafu(display("{message}"))]
    General { message: String },

    // --- Classified variants produced by `map_sqlite_error` ---
    //
    // Each carries the `rusqlite::Error` it was classified from, so the
    // conversion to `OdbcError` can report SQLite's own extended result code
    // through `SQLGetDiagRec`'s `NativeErrorPtr` and preserve the causal chain
    // rather than flattening it into the message. `None` is for the classes
    // rusqlite raises itself, which have no SQLite result code behind them.
    //
    // The field is named `cause`, not `source`, because `snafu` special-cases
    // a field called `source` and requires it to implement `std::error::Error`
    // directly, which `Option<rusqlite::Error>` does not.
    #[snafu(display("unable to open database: {message}"))]
    ConnectionFailed {
        message: String,
        cause: Option<rusqlite::Error>,
    },
    #[snafu(display("integrity constraint violation: {message}"))]
    ConstraintViolation {
        message: String,
        cause: Option<rusqlite::Error>,
    },
    #[snafu(display("syntax error or access violation: {message}"))]
    SyntaxError {
        message: String,
        cause: Option<rusqlite::Error>,
    },
    #[snafu(display("table or view not found: {message}"))]
    TableNotFound {
        message: String,
        cause: Option<rusqlite::Error>,
    },
    #[snafu(display("column not found: {message}"))]
    ColumnNotFound {
        message: String,
        cause: Option<rusqlite::Error>,
    },
    #[snafu(display("database is busy: {message}"))]
    DatabaseBusy {
        message: String,
        cause: Option<rusqlite::Error>,
    },
    #[snafu(display("data type mismatch: {message}"))]
    DataTypeMismatch {
        message: String,
        cause: Option<rusqlite::Error>,
    },
    #[snafu(display("numeric value out of range: {message}"))]
    NumericOutOfRange {
        message: String,
        cause: Option<rusqlite::Error>,
    },
    /// `SQLITE_INTERRUPT`: the statement was stopped by `sqlite3_interrupt`,
    /// which for this driver means `SQLCancel`. See
    /// [`SqliteBackend::cancel`].
    #[snafu(display("operation canceled: {message}"))]
    OperationCanceled {
        message: String,
        cause: Option<rusqlite::Error>,
    },
}

/// Operation canceled (`HY008`).
///
/// The SQLSTATE the spec lists for every function that can be stopped by
/// `SQLCancel` (`SQLExecDirect`, `SQLExecute`, `SQLFetch`, the catalog
/// functions). `stackable-odbc-core` has no named constructor for it, so it is
/// declared here rather than written as a bare literal at the use site.
pub(crate) const SQL_STATE_OPERATION_CANCELED: &str = "HY008";

/// SQLite's extended result code for `e`, or `0` when there is none.
///
/// The extended code is the useful one: it distinguishes
/// `SQLITE_CONSTRAINT_FOREIGNKEY` (787) from `SQLITE_CONSTRAINT_NOTNULL` (1299)
/// where the primary code says only `SQLITE_CONSTRAINT` (19). ODBC defines `0`
/// as "no data-source code", which is the right answer for the failures
/// rusqlite raises without ever reaching SQLite.
fn sqlite_extended_code(e: &rusqlite::Error) -> i32 {
    match e {
        rusqlite::Error::SqliteFailure(ffi_err, _) => ffi_err.extended_code,
        rusqlite::Error::SqlInputError { error, .. } => error.extended_code,
        _ => 0,
    }
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
                ErrorCode::ConstraintViolation => SqliteError::ConstraintViolation {
                    message,
                    cause: Some(e),
                },
                ErrorCode::CannotOpen | ErrorCode::NotADatabase | ErrorCode::PermissionDenied => {
                    SqliteError::ConnectionFailed {
                        message,
                        cause: Some(e),
                    }
                }
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => SqliteError::DatabaseBusy {
                    message,
                    cause: Some(e),
                },
                ErrorCode::TypeMismatch => SqliteError::DataTypeMismatch {
                    message,
                    cause: Some(e),
                },
                // SQLITE_INTERRUPT. `sqlite3_interrupt` is only ever called by
                // this driver's `SQLCancel` implementation, so this is a
                // cancelled statement rather than a failure of the data
                // source, and `HY008` is what the spec's diagnostics tables
                // list for exactly that.
                ErrorCode::OperationInterrupted => SqliteError::OperationCanceled {
                    message,
                    cause: Some(e),
                },
                // SQLITE_ERROR covers syntax errors and unresolved names alike.
                ErrorCode::Unknown => classify_sqlite_error_message(message, Some(e)),
                _ => SqliteError::Rusqlite { source: e },
            }
        }
        // Failures raised while compiling SQL. rusqlite reports these as a
        // distinct variant from `SqliteFailure`, but the classification is the
        // same: SQLITE_ERROR with a message that names the failure.
        rusqlite::Error::SqlInputError {
            ref error, ref msg, ..
        } => {
            let message = msg.clone();
            match error.code {
                ErrorCode::ConstraintViolation => SqliteError::ConstraintViolation {
                    message,
                    cause: Some(e),
                },
                ErrorCode::Unknown => classify_sqlite_error_message(message, Some(e)),
                _ => SqliteError::Rusqlite { source: e },
            }
        }
        // Errors rusqlite raises itself, without a SQLite result code, so
        // there is no extended code to carry, but the error itself is still
        // worth preserving as the cause.
        rusqlite::Error::InvalidColumnName(ref name) => {
            let message = format!("no such column: {name}");
            SqliteError::ColumnNotFound {
                message,
                cause: Some(e),
            }
        }
        rusqlite::Error::InvalidColumnType(..) | rusqlite::Error::FromSqlConversionFailure(..) => {
            SqliteError::DataTypeMismatch {
                message: e.to_string(),
                cause: Some(e),
            }
        }
        rusqlite::Error::IntegralValueOutOfRange(..) => SqliteError::NumericOutOfRange {
            message: e.to_string(),
            cause: Some(e),
        },
        other => SqliteError::Rusqlite { source: other },
    }
}

/// Split a `SQLITE_ERROR` message into the SQLSTATE classes the ODBC spec
/// distinguishes. SQLite's wording for these is stable across versions.
fn classify_sqlite_error_message(message: String, cause: Option<rusqlite::Error>) -> SqliteError {
    if message.starts_with("no such table") || message.starts_with("no such view") {
        SqliteError::TableNotFound { message, cause }
    } else if message.starts_with("no such column") {
        SqliteError::ColumnNotFound { message, cause }
    } else {
        SqliteError::SyntaxError { message, cause }
    }
}

/// Carries an [`OdbcError`] core produced without reclassifying it.
///
/// Required by `Backend::Error`'s `From<OdbcError>` bound. Paired with the
/// [`SqliteError::Odbc`] arm of the reverse conversion, the round trip is
/// lossless.
impl From<OdbcError> for SqliteError {
    fn from(source: OdbcError) -> Self {
        SqliteError::Odbc { source }
    }
}

impl From<SqliteError> for OdbcError {
    fn from(e: SqliteError) -> Self {
        use stackable_odbc_core::types::SqlState;

        // Taken before the match moves `e`.
        let message = e.to_string();

        let (sqlstate, cause) = match e {
            // Already an `OdbcError`; hand it straight back. See the variant.
            SqliteError::Odbc { source } => return source,
            SqliteError::NotImplemented { feature } => {
                return OdbcError::NotImplemented { feature };
            }
            SqliteError::ConnectionFailed { cause, .. } => {
                (SqlState::client_unable_to_establish_connection(), cause)
            }
            SqliteError::ConstraintViolation { cause, .. } => {
                (SqlState::integrity_constraint_violation(), cause)
            }
            SqliteError::SyntaxError { cause, .. } => {
                (SqlState::syntax_error_or_access_violation(), cause)
            }
            SqliteError::TableNotFound { cause, .. } => {
                (SqlState::base_table_or_view_not_found(), cause)
            }
            SqliteError::ColumnNotFound { cause, .. } => (SqlState::column_not_found(), cause),
            SqliteError::DatabaseBusy { cause, .. } => (SqlState::timeout_expired(), cause),
            SqliteError::DataTypeMismatch { cause, .. } => {
                (SqlState::restricted_data_type_attribute_violation(), cause)
            }
            SqliteError::NumericOutOfRange { cause, .. } => {
                (SqlState::numeric_value_out_of_range(), cause)
            }
            SqliteError::OperationCanceled { cause, .. } => {
                (SqlState::new(SQL_STATE_OPERATION_CANCELED), cause)
            }
            SqliteError::Rusqlite { source } => (SqlState::general_error(), Some(source)),
            SqliteError::MissingParam { .. } | SqliteError::General { .. } => {
                (SqlState::general_error(), None)
            }
        };

        // `SQLGetDiagRec` reports the native code through `NativeErrorPtr` and
        // walks the causal chain into the diagnostic message. Both were
        // dropped on the floor before: every SQLite error reached the
        // application as native code 0 with its inner links flattened away.
        let native_error = cause.as_ref().map_or(0, sqlite_extended_code);
        let err = OdbcError::general(message, sqlstate).with_native_error(native_error);
        match cause {
            Some(cause) => err.with_source(cause),
            None => err,
        }
    }
}

impl Backend for SqliteBackend {
    /// `sqlite3_interrupt`'s handle, the *aliasing* token shape
    /// [`Backend::CancelToken`] names SQLite as the example of: it refers to
    /// the same connection the statement is executing on, which is sound only
    /// because SQLite documents `sqlite3_interrupt` as safe to call from a
    /// thread other than the one running the query.
    ///
    /// The `Arc` is the requirement core states for an aliasing token: it has
    /// to survive a concurrent `SQLDisconnect`, because core clones the token
    /// out before doing anything else. `rusqlite`'s `InterruptHandle` already
    /// satisfies the underlying rule ("it is not safe to call this routine with
    /// a database connection that is closed or might close before
    /// `sqlite3_interrupt()` returns"): it holds an
    /// `Arc<Mutex<*mut sqlite3>>` shared with the connection, and
    /// `InnerConnection::close` nulls that pointer *while holding the same
    /// mutex*, so a racing `interrupt()` either runs against a live handle or
    /// sees null and does nothing. Wrapping it in this crate's own `Arc` is
    /// what makes the token cheap to clone per statement.
    type CancelToken = Arc<rusqlite::InterruptHandle>;
    type Connection = SqliteConnection;
    type Error = SqliteError;
    type Statement = SqliteStatement;

    /// Hand out the connection's interrupt handle. Infallible and lock-free:
    /// the handle was captured in [`SqliteBackend::connect`], so this only
    /// bumps a refcount (see `SqliteConnection::interrupt`). Not an intra-doc
    /// link: that field is `pub(crate)`, and rustdoc rejects a public item
    /// linking to a private one.
    fn cancel_token(conn: &SqliteConnection) -> Arc<rusqlite::InterruptHandle> {
        Arc::clone(&conn.interrupt)
    }

    /// Interrupt whatever is running on the token's connection.
    ///
    /// `sqlite3_interrupt` makes the in-flight `sqlite3_step` return
    /// `SQLITE_INTERRUPT`, which surfaces from
    /// [`stackable_odbc_core::backend::Backend::exec_direct`] and friends as
    /// `HY008` ("operation canceled") via `map_sqlite_error`, the SQLSTATE the
    /// spec defines for a statement stopped by `SQLCancel`.
    ///
    /// Safe on both of `SQLCancel`'s paths. It never blocks on this
    /// connection's own `Mutex`, so the idle path, where core holds the
    /// connection's group lock across this call, cannot deadlock; the only
    /// lock taken is `rusqlite`'s short-lived interrupt lock, which no ODBC
    /// entry point holds. It is also a no-op rather than an error when nothing
    /// is running, which is exactly what the spec asks of `SQLCancel` in that
    /// case.
    fn cancel(token: &Arc<rusqlite::InterruptHandle>) -> Result<(), SqliteError> {
        tracing::debug!("SQLCancel: interrupting the SQLite connection");
        token.interrupt();
        Ok(())
    }

    fn connect(params: &ConnectParams) -> Result<SqliteConnection, SqliteError> {
        let p = types::connect_params::SqliteConnectParams::try_from(params)?;
        let conn = rusqlite::Connection::open(p.database()).map_err(map_sqlite_error)?;

        // Enforce foreign keys explicitly, so that `SQL_INTEGRITY = "Y"` is
        // true by construction rather than by build configuration.
        //
        // SQLite defaults this off for backward compatibility. The bundled
        // library happens to be compiled with `SQLITE_DEFAULT_FOREIGN_KEYS`,
        // so it was already on, but that is a property of one dependency's
        // build, not of SQLite, and dropping `rusqlite`'s `bundled` feature
        // for a system library would silently turn referential integrity off
        // while the driver went on advertising it.
        //
        // The pragma is per-connection and a no-op inside a transaction; here
        // there is not one yet. `PRAGMA foreign_keys` is also a no-op rather
        // than an error on a build compiled with `SQLITE_OMIT_FOREIGN_KEY`,
        // which is why `Backend::connect` cannot treat success as proof.
        // `integrity_enhancement_facility_is_actually_enforced` reads the
        // value back through this function.
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .map_err(map_sqlite_error)?;

        // Captured before the connection moves into the `Mutex`: after that,
        // reaching it would mean taking a lock, and `cancel_token` can neither
        // block nor fail. See `SqliteConnection::interrupt`.
        let interrupt = Arc::new(conn.get_interrupt_handle());

        Ok(SqliteConnection {
            conn: Mutex::new(conn),
            interrupt,
            manual_commit: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn disconnect(_conn: &mut SqliteConnection) -> Result<(), SqliteError> {
        Ok(()) // rusqlite closes on drop
    }

    fn browse_connect_attrs() -> Cow<'static, [Cow<'static, str>]> {
        Cow::Borrowed(&[Cow::Borrowed("database")])
    }

    /// SQLite supports transactions and this driver reports `SQL_TC_DML` for
    /// `SQL_TXN_CAPABLE`, so manual-commit mode must actually be honoured.
    ///
    /// Manual-commit mode is entered by opening a transaction with `BEGIN`;
    /// `end_tran` then commits or rolls it back and, while still in
    /// manual-commit mode, opens the next one.
    fn set_autocommit(conn: &SqliteConnection, enabled: bool) -> Result<(), SqliteError> {
        let db = conn.conn.lock().map_err(|e| SqliteError::General {
            message: format!("Mutex poisoned: {e}"),
        })?;
        if enabled {
            // Returning to autocommit commits any open transaction, per the
            // ODBC spec: "Any open transactions on the connection are committed
            // when SQL_ATTR_AUTOCOMMIT is set to SQL_AUTOCOMMIT_ON".
            if !db.is_autocommit() {
                db.execute_batch("COMMIT").map_err(map_sqlite_error)?;
            }
        } else if db.is_autocommit() {
            db.execute_batch("BEGIN").map_err(map_sqlite_error)?;
        }
        conn.manual_commit
            .store(!enabled, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn end_tran(conn: &SqliteConnection, commit: bool) -> Result<(), SqliteError> {
        let db = conn.conn.lock().map_err(|e| SqliteError::General {
            message: format!("Mutex poisoned: {e}"),
        })?;
        // If SQLite is in autocommit mode there is no open transaction to commit/roll back.
        if db.is_autocommit() {
            return Ok(());
        }
        let sql = if commit { "COMMIT" } else { "ROLLBACK" };
        db.execute_batch(sql).map_err(map_sqlite_error)?;

        // Still in manual-commit mode: open the next transaction, otherwise
        // subsequent statements would silently autocommit.
        if conn
            .manual_commit
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            db.execute_batch("BEGIN").map_err(map_sqlite_error)?;
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
    /// If result sets ever become lazily streamed, revisit both hooks, and
    /// note that `SQL_CB_CLOSE` would then also require a real
    /// [`stackable_odbc_core::backend::StatementBackend::close_cursor`].
    ///
    /// Spec: <https://www.sqlite.org/lang_transaction.html>
    fn cursor_commit_behavior() -> CursorBehavior {
        CursorBehavior::Preserve
    }

    /// See [`SqliteBackend::cursor_commit_behavior`]: same reasoning, same
    /// value.
    fn cursor_rollback_behavior() -> CursorBehavior {
        CursorBehavior::Preserve
    }

    /// `SQL_IC_MIXED`: SQLite stores an unquoted identifier with the case it
    /// was written in, and matches it case-insensitively.
    ///
    /// `SQL_IC_MIXED` is the spec's value for exactly that pair ("stored in
    /// mixed case and case-insensitive"), as opposed to `SQL_IC_UPPER` /
    /// `SQL_IC_LOWER`, which fold the stored name, and `SQL_IC_SENSITIVE`,
    /// which would make `SELECT * FROM T` and `SELECT * FROM t` name different
    /// tables. They do not.
    ///
    /// Case-insensitive matching is ASCII-only in SQLite unless the build
    /// carries ICU; that does not change the answer, since ODBC has no value
    /// for "case-insensitive for some characters".
    ///
    /// Distinct from [`SqliteBackend::quoted_identifier_case`], which describes
    /// *quoted* identifiers, and which answers the same here, for the reason
    /// given there.
    ///
    /// <https://sqlite.org/lang_keywords.html>
    fn identifier_case(_conn: &SqliteConnection) -> u16 {
        SQL_IC_MIXED
    }

    /// `SQL_IC_MIXED`, the same as [`SqliteBackend::identifier_case`]: in
    /// SQLite quoting an identifier does **not** make it case-sensitive.
    ///
    /// This is the one place the two commonly diverge for other data sources,
    /// so it is worth stating what SQLite actually does. Double quotes are a
    /// *delimiter* here, not a case-sensitivity switch: they let a keyword or a
    /// name with punctuation be used as an identifier, and nothing more. A
    /// table created as `"MixedCase"` is still found by `"mixedcase"`, and the
    /// catalog stores the name with the case it was written in, which is
    /// precisely `SQL_IC_MIXED`.
    ///
    /// `quoted_identifiers_are_not_case_sensitive` probes this against the
    /// bundled library rather than taking it from the documentation. It
    /// corrects a claim of `SQL_IC_SENSITIVE`, which would have had an
    /// application quote-and-case-match identifiers that SQLite folds anyway.
    ///
    /// <https://sqlite.org/lang_keywords.html>
    fn quoted_identifier_case(_conn: &SqliteConnection) -> u16 {
        SQL_IC_MIXED
    }

    /// SQLite has no ODBC catalogs: `metadata::tables` reports `TABLE_CAT` as
    /// NULL for every row, and a `catalog = "%"` enumeration returns an empty
    /// result set.
    ///
    /// Core derives the whole catalog group from this (`SQL_CATALOG_NAME`,
    /// `SQL_CATALOG_TERM`, `SQL_CATALOG_NAME_SEPARATOR`,
    /// `SQL_CATALOG_LOCATION` and `SQL_CATALOG_USAGE`), so this driver answers
    /// none of them itself. Before the hook existed it answered three and let
    /// the other two inherit defaults that named a catalog, telling an
    /// application catalogs do not exist and giving their name in the same
    /// breath.
    fn supports_catalogs(_conn: &SqliteConnection) -> bool {
        false
    }

    /// SQLite has no ODBC schemas: a `schema = "%"` enumeration returns an
    /// empty result set and `TABLE_SCHEM` is always NULL.
    ///
    /// Drives `SQL_SCHEMA_TERM` and `SQL_SCHEMA_USAGE`; see
    /// [`SqliteBackend::supports_catalogs`].
    fn supports_schemas(_conn: &SqliteConnection) -> bool {
        false
    }

    /// The `ALTER TABLE` clauses SQLite accepts, of those the ODBC bitmap can
    /// express. See `info::SQLITE_ALTER_TABLE` for what is claimed, what is
    /// supported-but-unrepresentable, and how each bit was verified.
    fn alter_table_support(_conn: &SqliteConnection) -> u32 {
        info::SQLITE_ALTER_TABLE
    }

    /// Every outer-join form SQLite implements. See
    /// `info::SQLITE_OUTER_JOIN_CAPABILITIES`.
    fn outer_join_capabilities(_conn: &SqliteConnection) -> u32 {
        info::SQLITE_OUTER_JOIN_CAPABILITIES
    }

    /// "Transactions in SQLite are SERIALIZABLE."
    ///
    /// Core derives both `SQL_DEFAULT_TXN_ISOLATION` and the value
    /// `SQLGetConnectAttr(SQL_ATTR_TXN_ISOLATION)` reports on a fresh
    /// connection from this, so the two cannot disagree.
    ///
    /// Spec: <https://www.sqlite.org/isolation.html>
    fn default_txn_isolation(_conn: &SqliteConnection) -> u32 {
        SQL_TXN_SERIALIZABLE
    }

    /// The only level reachable from this driver.
    ///
    /// READ COMMITTED and REPEATABLE READ are not SQLite concepts. READ
    /// UNCOMMITTED needs shared-cache mode ("the only way that one database
    /// connection can see uncommitted changes on a different database
    /// connection"), and [`SqliteBackend::connect`] opens with a plain
    /// `rusqlite::Connection::open`, so it is unreachable.
    ///
    /// Returning a single level also means core's default
    /// [`Backend::set_txn_isolation`] is correct as-is: the one supported
    /// level is always already in effect, and anything else is rejected with
    /// `HY024` before it reaches the backend.
    fn txn_isolation_options(_conn: &SqliteConnection) -> u32 {
        SQL_TXN_SERIALIZABLE
    }

    /// `SQL_TC_DML`: SQLite runs DML inside a transaction, and a DDL statement
    /// inside one causes neither a commit nor an error. SQLite's DDL is
    /// transactional, so `CREATE TABLE` simply participates.
    ///
    /// `SQL_TC_ALL` would be the stronger claim and is tempting for that
    /// reason, but the spec defines it as "transactions can contain DDL
    /// statements **and** DML statements in any order", and this driver's
    /// manual-commit mode is built on `BEGIN`/`COMMIT` around whatever the
    /// application sends. `SQL_TC_DML` states what an application can rely on
    /// without also promising the DDL-ordering freedom the spec attaches to
    /// `SQL_TC_ALL`.
    ///
    /// Core pins this against [`SqliteBackend::txn_isolation_options`]:
    /// `SQL_TC_NONE` if and only if no isolation level is declared. Declaring a
    /// level and then reporting no transaction support is the
    /// self-contradiction that pairing exists to catch.
    ///
    /// `SQL_TC_DML` is a small fixed constant, so the narrowing `as u16`
    /// cannot lose information. (The `SQL_TC_*` constants are typed `u32` for
    /// bitmask use, while the info type is `SQLUSMALLINT`.)
    fn txn_capable(_conn: &SqliteConnection) -> u16 {
        SQL_TC_DML as u16
    }

    /// `true`: each connection this driver opens is its own
    /// `rusqlite::Connection` with its own SQLite handle, so two connections
    /// can each have a transaction open at the same time.
    ///
    /// The spec asks about the *driver*, not about one connection: "`"Y"` if
    /// the driver supports more than one active transaction at the same time".
    /// Nothing here serialises across connections: `SqliteBackend::connect`
    /// opens a fresh handle per call and shares no state between them. What
    /// SQLite does when those transactions contend for the same file is a
    /// locking question (`SQLITE_BUSY`), not a question of how many can be
    /// active.
    fn multiple_active_txn(_conn: &SqliteConnection) -> bool {
        true
    }

    /// `true`: SQLite implements the whole Integrity Enhancement Facility
    /// (`PRIMARY KEY`, `UNIQUE`, `NOT NULL`, `CHECK`, `DEFAULT` and `FOREIGN
    /// KEY` with referential actions), and this build enforces all of it.
    ///
    /// Referential integrity in particular is enforced by construction, not by
    /// chance: [`SqliteBackend::connect`] issues `PRAGMA foreign_keys = ON`,
    /// because plain SQLite defaults it off for backward compatibility and the
    /// bundled library only *happens* to compile with
    /// `SQLITE_DEFAULT_FOREIGN_KEYS`. Without the pragma this claim would
    /// depend on a dependency's build flags.
    /// `integrity_enhancement_facility_is_actually_enforced` asserts it through
    /// `connect`, and fails loudly if that ever stops holding.
    ///
    /// `SQLForeignKeys` is genuinely implemented (`metadata::foreign_keys`,
    /// over `PRAGMA foreign_key_list`), so an application that acts on this
    /// finds the metadata it then asks for.
    fn integrity(_conn: &SqliteConnection) -> bool {
        true
    }

    /// `SQL_GB_NO_RELATION`: SQLite relates the `GROUP BY` list and the select
    /// list not at all. It accepts a bare non-aggregated column absent from
    /// `GROUP BY` (returning an arbitrary row from each group), and accepts
    /// `GROUP BY` columns and expressions absent from the select list.
    ///
    /// Verified in `group_by_is_unrelated_to_the_select_list`.
    fn group_by(_conn: &SqliteConnection) -> u16 {
        SQL_GB_NO_RELATION
    }

    /// `SQL_NC_LOW`: SQLite sorts NULLs at the low end, first ascending, last
    /// descending.
    fn null_collation(_conn: &SqliteConnection) -> u16 {
        SQL_NC_LOW
    }

    /// `SQL_CN_ANY`: SQLite accepts a table alias with or without `AS`, and
    /// places no restriction on the name.
    fn correlation_name(_conn: &SqliteConnection) -> u16 {
        SQL_CN_ANY
    }

    /// `SQL_NNC_NON_NULL`: SQLite implements `NOT NULL` column constraints.
    fn non_nullable_columns(_conn: &SqliteConnection) -> u16 {
        SQL_NNC_NON_NULL
    }

    /// SQLite takes arbitrary expressions in `ORDER BY`, including over columns
    /// absent from the select list.
    fn expressions_in_order_by(_conn: &SqliteConnection) -> bool {
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
    /// first: SQLite's `GROUP BY` is deliberately unrelated to the select list
    /// (see [`SqliteBackend::group_by`]), which is a permissive extension, not
    /// entry-level behaviour.
    ///
    /// `0` is the honest answer: it claims no level rather than asserting one
    /// the driver demonstrably fails. Raising it later means auditing SQL-92
    /// entry level properly, not restoring the value core used to invent.
    fn sql_conformance(_conn: &SqliteConnection) -> u32 {
        0
    }

    /// `0`: `TIMESTAMPADD` is not supported. `SQLITE_TIMEDATE_FUNCTIONS`
    /// deliberately omits `SQL_FN_TD_TIMESTAMPADD`, so claiming interval units
    /// here would describe a function this driver does not offer.
    fn timedate_add_intervals(_conn: &SqliteConnection) -> u32 {
        0
    }

    /// `0`: `TIMESTAMPDIFF` is not supported, for the same reason as
    /// [`SqliteBackend::timedate_add_intervals`].
    fn timedate_diff_intervals(_conn: &SqliteConnection) -> u32 {
        0
    }

    /// See `info::SQLITE_SUBQUERIES`. Notably excludes `SQL_SQ_QUANTIFIED`,
    /// which core's default claimed while this driver's
    /// `SQL_SQL92_PREDICATES` denied it.
    fn subqueries(_conn: &SqliteConnection) -> u32 {
        info::SQLITE_SUBQUERIES
    }

    /// SQLite accepts `SELECT a AS x`, and `AS` is optional.
    fn column_alias(_conn: &SqliteConnection) -> bool {
        true
    }

    /// `SQL_CB_NULL`: concatenating a NULL yields NULL. `'a' || NULL` is
    /// NULL, not `'a'`.
    fn concat_null_behavior(_conn: &SqliteConnection) -> u16 {
        SQL_CB_NULL
    }

    /// See `info::SQLITE_UNION`: both `UNION` and `UNION ALL`.
    fn union_support(_conn: &SqliteConnection) -> u32 {
        info::SQLITE_UNION
    }

    /// See `info::SQLITE_CONVERT_FUNCTIONS`: `CAST` only.
    fn convert_functions(_conn: &SqliteConnection) -> u32 {
        info::SQLITE_CONVERT_FUNCTIONS
    }

    /// `false`: SQLite orders by expressions and by columns absent from the
    /// select list, so `ORDER BY` is not restricted to selected columns. Same
    /// permissiveness as [`SqliteBackend::group_by`].
    fn order_by_columns_in_select(_conn: &SqliteConnection) -> bool {
        false
    }

    /// `true`: SQLite has no per-table permissions. Every table `SQLTables`
    /// returns is one the connection can `SELECT` from, because opening the
    /// database file is the only access check there is.
    ///
    /// This is the one value in this group that is a claim about the connected
    /// principal rather than about SQL. It is safe here precisely because
    /// SQLite has no principal.
    fn accessible_tables(_conn: &SqliteConnection) -> bool {
        true
    }

    /// `false`: SQLite has no stored procedures, so there is no procedure the
    /// connected user can execute.
    ///
    /// The counterpart of [`SqliteBackend::accessible_tables`], and the
    /// opposite answer for a different reason. That one is `true` because every
    /// table SQLTables returns is reachable; this is `false` because
    /// `SQLProcedures` returns nothing to be reachable in the first place:
    /// this driver leaves `Backend::procedures` defaulted to no rows, and
    /// reports `SQL_PROCEDURES = "N"` through core.
    fn accessible_procedures(_conn: &SqliteConnection) -> bool {
        false
    }

    /// `false`: the driver opens the database read-write.
    ///
    /// This describes the driver's own behaviour, not the file. A database on
    /// read-only media, or one whose file permissions deny writes, still
    /// reports `false` here and fails the write itself, which is what the
    /// spec's "data source is set to READ ONLY mode" means.
    fn data_source_read_only(_conn: &SqliteConnection) -> bool {
        false
    }

    /// SQLite's reserved words, read out of the linked library rather than
    /// transcribed. See `info::sqlite_keywords`.
    ///
    /// This is the raw list; core subtracts `ODBC_RESERVED_KEYWORDS` and joins
    /// it into `SQL_KEYWORDS`, so the "excluding ODBC's own" rule is applied
    /// once for every driver instead of per backend.
    fn keywords(_conn: &SqliteConnection) -> Cow<'static, [Cow<'static, str>]> {
        Cow::Borrowed(info::sqlite_keywords())
    }

    /// `"$"`, the one character beyond `a`–`z`, `A`–`Z`, `0`–`9` and `_` that
    /// SQLite accepts in an undelimited identifier.
    ///
    /// SQLite's tokenizer treats `$` as an identifier character, so
    /// `CREATE TABLE a$b (...)` parses and the name round-trips through
    /// `sqlite_master` unchanged. An application reads this info type to decide
    /// when it must quote, so the previous `""` (core's old default, not a
    /// claim this driver ever made) told it to quote a name that needs no
    /// quoting.
    ///
    /// Every candidate is executed against the bundled library in
    /// `special_characters_are_each_live_probed`, which checks the characters
    /// *not* claimed as well: a list that only grows when someone notices can
    /// understate forever.
    ///
    /// Deliberately excluded even though SQLite's tokenizer accepts them:
    /// characters at or above `0x80`. The spec wants a character list, and
    /// "every non-ASCII code point" is not one that fits in a `SQLGetInfo`
    /// string.
    ///
    /// <https://sqlite.org/lang_keywords.html>
    fn special_characters(_conn: &SqliteConnection) -> Cow<'static, str> {
        Cow::Borrowed(info::SQLITE_SPECIAL_CHARACTERS)
    }

    /// Backslash: SQLite's `LIKE ... ESCAPE` takes any character, and this
    /// driver reports `SQL_LIKE_ESCAPE_CLAUSE = "Y"`. Backslash is the
    /// conventional choice and the one `SQLTables`-style pattern arguments are
    /// documented against.
    fn search_pattern_escape(_conn: &SqliteConnection) -> Cow<'static, str> {
        Cow::Borrowed("\\")
    }

    // --- Identity ---
    //
    // `driver_name` and `driver_version` take no connection: the Windows
    // Driver Manager asks for driver identity before `SQLDriverConnectW`, and
    // an answer that needed a connection could not be given then. The two DBMS
    // values are per-connection by signature but constant here, since a
    // `rusqlite` link always reaches the one bundled library.

    fn driver_name() -> Cow<'static, str> {
        Cow::Borrowed("stackable-odbc-sqlite")
    }

    /// This crate's version in the spec's `##.##.####` form.
    ///
    /// `driver_version!` reads it from `CARGO_PKG_VERSION` at compile time, so
    /// a release bump cannot leave the reported version behind;
    /// `driver_version_tracks_the_crate_version` pins that.
    fn driver_version() -> Cow<'static, str> {
        stackable_odbc_core::driver_version!().into()
    }

    fn dbms_name(_conn: &SqliteConnection) -> Cow<'static, str> {
        Cow::Borrowed("SQLite")
    }

    /// The bundled library's version, `##.##.####` followed by SQLite's own
    /// spelling in parentheses.
    ///
    /// The spec permits appending the data source's own version string after
    /// the fixed-width prefix, which keeps the familiar `3.53.2` visible to
    /// anyone reading the value by eye. Read from `rusqlite::version()`, the
    /// library actually linked, rather than written down, for the reason
    /// AGENTS.md gives about the system `sqlite3` binary being a different
    /// version.
    fn dbms_version(_conn: &SqliteConnection) -> Cow<'static, str> {
        use stackable_odbc_core::types::{format_odbc_version, parse_dotted_version};

        let raw = rusqlite::version();
        match parse_dotted_version(raw) {
            Some((major, minor, release)) => {
                format!("{} ({raw})", format_odbc_version(major, minor, release)).into()
            }
            None => {
                tracing::warn!(
                    raw,
                    "could not parse the SQLite version; reporting it verbatim"
                );
                raw.into()
            }
        }
    }

    // --- Delegations ---

    fn exec_direct(
        conn: &SqliteConnection,
        _cancel: &Arc<rusqlite::InterruptHandle>,
        sql: &str,
    ) -> Result<SqliteStatement, SqliteError> {
        execute::exec_direct(conn, sql)
    }

    fn prepare(
        conn: &SqliteConnection,
        _cancel: &Arc<rusqlite::InterruptHandle>,
        sql: &str,
    ) -> Result<SqliteStatement, SqliteError> {
        execute::prepare(conn, sql)
    }

    fn execute(
        conn: &SqliteConnection,
        _cancel: &Arc<rusqlite::InterruptHandle>,
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
    ) -> Result<InfoValue, SqliteError> {
        info::get_info_pre_connect(info_type)
    }

    fn get_info_raw(
        conn: &SqliteConnection,
        info_type: u16,
    ) -> Option<Result<InfoValue, SqliteError>> {
        info::get_info_raw(conn, info_type)
    }

    fn get_functions() -> Cow<'static, [stackable_odbc_core::function_id::FunctionId]> {
        Cow::Borrowed(info::get_functions())
    }

    fn get_type_info(_conn: &SqliteConnection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(info::get_type_info())
    }

    fn tables(
        conn: &SqliteConnection,
        _cancel: &Arc<rusqlite::InterruptHandle>,
        query: &stackable_odbc_core::types::TablesQuery<'_>,
    ) -> Result<Vec<TableRow>, SqliteError> {
        metadata::tables(conn, query)
    }

    /// `TABLE` and `VIEW`, the two values `metadata::tables` can put in
    /// `TABLE_TYPE`. See `metadata::table_types`.
    fn table_types(_conn: &SqliteConnection) -> Vec<Cow<'static, str>> {
        metadata::table_types()
    }

    fn columns(
        conn: &SqliteConnection,
        _cancel: &Arc<rusqlite::InterruptHandle>,
        query: &stackable_odbc_core::types::ColumnsQuery<'_>,
    ) -> Result<Vec<ColumnRow>, SqliteError> {
        metadata::columns(conn, query)
    }

    fn primary_keys(
        conn: &SqliteConnection,
        _cancel: &Arc<rusqlite::InterruptHandle>,
        query: &stackable_odbc_core::types::PrimaryKeysQuery<'_>,
    ) -> Result<Vec<PrimaryKeyRow>, SqliteError> {
        metadata::primary_keys(conn, query)
    }

    fn foreign_keys(
        conn: &SqliteConnection,
        _cancel: &Arc<rusqlite::InterruptHandle>,
        query: &stackable_odbc_core::types::ForeignKeysQuery<'_>,
    ) -> Result<Vec<ForeignKeyRow>, SqliteError> {
        metadata::foreign_keys(conn, query)
    }

    fn statistics(
        conn: &SqliteConnection,
        _cancel: &Arc<rusqlite::InterruptHandle>,
        query: &stackable_odbc_core::types::StatisticsQuery<'_>,
    ) -> Result<Vec<StatisticsRow>, SqliteError> {
        metadata::statistics(conn, query)
    }

    fn special_columns(
        conn: &SqliteConnection,
        _cancel: &Arc<rusqlite::InterruptHandle>,
        query: &stackable_odbc_core::types::SpecialColumnsQuery<'_>,
    ) -> Result<Vec<SpecialColumnRow>, SqliteError> {
        metadata::special_columns(conn, query)
    }

    /// SQLite's `{fn}`/`{d}`/`{t}`/`{ts}` escape-translation dialect. See
    /// `crate::escape_dialect` for the remap table and its justification
    /// against the `SQL_*_FUNCTIONS` bitmaps in `backend/info.rs`.
    fn escape_dialect(_conn: &SqliteConnection) -> stackable_odbc_core::escape::EscapeDialect {
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
        let mut stmt = SqliteBackend::prepare(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "SELECT id FROM t WHERE id = ?1",
        )
        .unwrap();

        // First execute: matching param -> exactly one row.
        SqliteBackend::execute(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            &mut stmt,
            &[ColumnValue::I64(2)],
        )
        .unwrap();
        assert!(matches!(stmt.fetch().unwrap(), FetchResult::Row));
        assert!(matches!(stmt.fetch().unwrap(), FetchResult::NoData));

        // Re-execute the SAME handle with a non-matching param -> no rows. This
        // fails if the cached compiled statement leaked the previous binding.
        SqliteBackend::execute(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            &mut stmt,
            &[ColumnValue::I64(999)],
        )
        .unwrap();
        assert!(matches!(stmt.fetch().unwrap(), FetchResult::NoData));

        // And once more with a matching param -> one row again.
        SqliteBackend::execute(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            &mut stmt,
            &[ColumnValue::I64(1)],
        )
        .unwrap();
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
        let Err(err) = SqliteBackend::exec_direct(&conn, &SqliteBackend::cancel_token(&conn), sql)
        else {
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
        SqliteBackend::exec_direct(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "INSERT INTO child VALUES (999)",
        )
        .unwrap();
        let Err(err) = SqliteBackend::end_tran(&conn, true) else {
            panic!("COMMIT should have failed the deferred foreign-key constraint");
        };
        // `end_tran` reports the backend's own error type now; the SQLSTATE an
        // application sees is the one the conversion to `OdbcError` assigns.
        assert_eq!(
            OdbcError::from(err).sqlstate().as_str(),
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
        let mut stmt = SqliteBackend::exec_direct(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "SELECT id, name FROM t",
        )
        .unwrap();
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
        let mut stmt = SqliteBackend::exec_direct(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "SELECT * FROM t",
        )
        .unwrap();
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
        let mut stmt = SqliteBackend::exec_direct(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "SELECT v FROM t",
        )
        .unwrap();
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
        let stmt = SqliteBackend::exec_direct(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "SELECT id, name FROM t",
        )
        .unwrap();
        let col1 = stmt.describe_col(1).unwrap();
        assert_eq!(col1.name(), "id");
        let col2 = stmt.describe_col(2).unwrap();
        assert_eq!(col2.name(), "name");
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
        let stmt = SqliteBackend::exec_direct(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "SELECT * FROM t",
        )
        .unwrap();
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
        let stmt = SqliteBackend::exec_direct(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "UPDATE t SET v = 99 WHERE v = 10",
        )
        .unwrap();
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
        let stmt = SqliteBackend::exec_direct(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "DELETE FROM t WHERE id > 1",
        )
        .unwrap();
        assert_eq!(stmt.column_count(), 0);
        assert_eq!(stmt.row_count(), Some(2));

        // Confirm only row 1 remains
        let mut sel = SqliteBackend::exec_direct(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "SELECT COUNT(*) FROM t",
        )
        .unwrap();
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
        let mut stmt = SqliteBackend::exec_direct(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "INSERT INTO t VALUES (1)",
        )
        .unwrap();
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
        let stmt = SqliteBackend::prepare(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "SELECT id FROM t WHERE id = ?",
        )
        .unwrap();
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
        let result = SqliteBackend::prepare(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "NOT VALID SQL %%%",
        );
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
        let mut stmt = SqliteBackend::prepare(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "SELECT name FROM t WHERE id = ?",
        )
        .unwrap();
        SqliteBackend::execute(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            &mut stmt,
            &[ColumnValue::I64(1)],
        )
        .unwrap();
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
        let mut stmt = SqliteBackend::prepare(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "SELECT name FROM t WHERE id = ?",
        )
        .unwrap();

        // First execution
        SqliteBackend::execute(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            &mut stmt,
            &[ColumnValue::I64(1)],
        )
        .unwrap();
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            stmt.get_data(1, CDataType::Default).unwrap().into_owned(),
            ColumnValue::String("alice".into())
        );

        // Re-execute with different param
        SqliteBackend::execute(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            &mut stmt,
            &[ColumnValue::I64(2)],
        )
        .unwrap();
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
        let mut stmt = SqliteBackend::prepare(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "INSERT INTO t VALUES (?, ?)",
        )
        .unwrap();
        SqliteBackend::execute(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            &mut stmt,
            &[ColumnValue::I64(42), ColumnValue::String("test".into())],
        )
        .unwrap();
        assert_eq!(stmt.row_count(), Some(1));

        // Verify with exec_direct
        let mut q = SqliteBackend::exec_direct(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "SELECT id, name FROM t",
        )
        .unwrap();
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
        let mut stmt = SqliteBackend::prepare(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            "SELECT name FROM t WHERE id = ?",
        )
        .unwrap();
        SqliteBackend::execute(
            &conn,
            &SqliteBackend::cancel_token(&conn),
            &mut stmt,
            &[ColumnValue::I64(1)],
        )
        .unwrap();
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            stmt.get_data(1, CDataType::Default).unwrap().into_owned(),
            ColumnValue::Null
        );
    }
}
