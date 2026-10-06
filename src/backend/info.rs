//! `SQLGetInfo`, `SQLGetTypeInfo` and `SQLGetFunctions` support for the SQLite
//! backend: the `get_info` / `get_info_pre_connect` / `get_info_raw`
//! handlers, the exported-function bitmap, the static type-info rows mapping
//! SQLite's storage classes onto ODBC types, and the SQLite capability
//! bitmaps (`SQLITE_*`).

use stackable_odbc_core::backend::{Backend, common_get_info_raw, default_get_info};
use stackable_odbc_core::function_id::{CORE_EXPORTED_FUNCTIONS, FunctionId};
use stackable_odbc_core::types::{
    InfoType, InfoValue, MaxPrecision, MaxScale, SQL_AF_ALL, SQL_AF_AVG, SQL_AF_COUNT,
    SQL_AF_DISTINCT, SQL_AF_MAX, SQL_AF_MIN, SQL_AF_SUM, SQL_AGGREGATE_FUNCTIONS,
    SQL_AT_ADD_COLUMN_COLLATION, SQL_AT_ADD_COLUMN_DEFAULT, SQL_AT_ADD_COLUMN_SINGLE,
    SQL_AT_ADD_CONSTRAINT, SQL_AT_ADD_TABLE_CONSTRAINT, SQL_AT_CONSTRAINT_NAME_DEFINITION,
    SQL_CODE_DATE, SQL_CODE_TIME, SQL_CODE_TIMESTAMP, SQL_FN_CVT_CAST, SQL_FN_NUM_ABS,
    SQL_FN_NUM_ROUND, SQL_FN_NUM_SIGN, SQL_FN_STR_ASCII, SQL_FN_STR_CHAR, SQL_FN_STR_CONCAT,
    SQL_FN_STR_LCASE, SQL_FN_STR_LENGTH, SQL_FN_STR_LTRIM, SQL_FN_STR_OCTET_LENGTH,
    SQL_FN_STR_REPLACE, SQL_FN_STR_RTRIM, SQL_FN_STR_SOUNDEX, SQL_FN_STR_SUBSTRING,
    SQL_FN_STR_UCASE, SQL_FN_SYS_IFNULL, SQL_FN_TD_CURDATE, SQL_FN_TD_CURRENT_DATE,
    SQL_FN_TD_CURRENT_TIME, SQL_FN_TD_CURRENT_TIMESTAMP, SQL_FN_TD_CURTIME, SQL_FN_TD_NOW,
    SQL_LIKE_ESCAPE_CLAUSE, SQL_NUMERIC_FUNCTIONS, SQL_OJ_ALL_COMPARISON_OPS, SQL_OJ_FULL,
    SQL_OJ_INNER, SQL_OJ_LEFT, SQL_OJ_NESTED, SQL_OJ_NOT_ORDERED, SQL_OJ_RIGHT, SQL_OUTER_JOINS,
    SQL_SP_BETWEEN, SQL_SP_COMPARISON, SQL_SP_EXISTS, SQL_SP_IN, SQL_SP_ISNOTNULL, SQL_SP_ISNULL,
    SQL_SP_LIKE, SQL_SQ_COMPARISON, SQL_SQ_CORRELATED_SUBQUERIES, SQL_SQ_EXISTS, SQL_SQ_IN,
    SQL_SQL92_PREDICATES, SQL_SQL92_RELATIONAL_JOIN_OPERATORS, SQL_SQL92_VALUE_EXPRESSIONS,
    SQL_SRJO_CROSS_JOIN, SQL_SRJO_EXCEPT_JOIN, SQL_SRJO_FULL_OUTER_JOIN, SQL_SRJO_INNER_JOIN,
    SQL_SRJO_INTERSECT_JOIN, SQL_SRJO_LEFT_OUTER_JOIN, SQL_SRJO_NATURAL_JOIN,
    SQL_SRJO_RIGHT_OUTER_JOIN, SQL_STRING_FUNCTIONS, SQL_SVE_CASE, SQL_SVE_CAST, SQL_SVE_COALESCE,
    SQL_SVE_NULLIF, SQL_SYSTEM_FUNCTIONS, SQL_TIMEDATE_FUNCTIONS, SQL_TXN_SERIALIZABLE,
    SQL_U_UNION, SQL_U_UNION_ALL, SqlDataType, TypeInfoRow, catalog_column_size,
};

use super::SqliteBackend;
use super::SqliteConnection;
use super::SqliteError;
use super::map_sqlite_error;
use crate::type_conversion::{
    BLOB_DEFAULT_COLUMN_SIZE, DECIMAL_DEFAULT_COLUMN_SIZE, MAX_FRACTIONAL_SECONDS_PRECISION,
    VARCHAR_DEFAULT_COLUMN_SIZE,
};

/// ODBC function IDs for functions this driver implements.
/// Used by `SQLGetFunctions` to report supported capabilities.
/// Reference: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetfunctions-function>
///
/// Superseded by [`CORE_EXPORTED_FUNCTIONS`], which [`get_functions`] returns
/// instead; kept only so `supported_functions_are_all_exported_by_core` can
/// assert the two agree. See that test for why the hand-written list went.
#[cfg(test)]
static SUPPORTED_FUNCTIONS: &[FunctionId] = &[
    FunctionId::BindCol,
    FunctionId::ColAttribute,
    FunctionId::Connect,
    FunctionId::DescribeCol,
    FunctionId::Disconnect,
    FunctionId::ExecDirect,
    FunctionId::Execute,
    FunctionId::Fetch,
    FunctionId::FreeStmt,
    FunctionId::NumResultCols,
    FunctionId::Prepare,
    FunctionId::RowCount,
    FunctionId::Columns,
    FunctionId::DriverConnect,
    FunctionId::GetData,
    FunctionId::GetFunctions,
    FunctionId::GetInfo,
    FunctionId::GetTypeInfo,
    FunctionId::Tables,
    FunctionId::MoreResults,
    FunctionId::AllocHandle,
    FunctionId::CloseCursor,
    FunctionId::FreeHandle,
    FunctionId::GetDiagRec,
    FunctionId::BindParameter,
    FunctionId::NumParams,
    FunctionId::EndTran,
    FunctionId::PrimaryKeys,
    FunctionId::ForeignKeys,
    FunctionId::NativeSql,
    FunctionId::Cancel,
    FunctionId::Statistics,
    FunctionId::SpecialColumns,
    FunctionId::SetStmtAttr,
    FunctionId::GetStmtAttr,
    FunctionId::SetConnectAttr,
    FunctionId::GetConnectAttr,
    FunctionId::GetDiagField,
    FunctionId::SetEnvAttr,
    FunctionId::GetEnvAttr,
    FunctionId::FetchScroll,
    FunctionId::Procedures,
    FunctionId::ProcedureColumns,
    FunctionId::GetCursorName,
    FunctionId::SetCursorName,
    FunctionId::ColumnPrivileges,
    FunctionId::TablePrivileges,
    FunctionId::DescribeParam,
    // Data-at-execution (fully implemented in stackable-odbc-core) and the remaining
    // exported entry points that delegate to a real implementation. Listed so
    // the Windows DM 3.x dispatch bitmap has no gaps.
    FunctionId::ParamData,
    FunctionId::PutData,
    FunctionId::BrowseConnect,
    FunctionId::BulkOperations,
    FunctionId::SetPos,
];

/// Static type information for SQLite's type system.
/// SQLite has 5 storage classes; this table maps them onto the fuller set of
/// ODBC SQL types applications expect, including the ANSI/Unicode character
/// variants and the temporal types SQLite stores as text.
//
// Every `column_size` value below is computed via `catalog_column_size` (the
// ODBC "Column Size" appendix formula, evaluated at this data source's
// maximum supported precision/scale) rather than hand-written (see
// `stackable_odbc_core::types::column_size` module docs).
//
// Core orders the result set itself (DATA_TYPE, then the row marked
// `with_preferred`, then TYPE_NAME; see
// `stackable_odbc_core::ffi::info::sql_get_type_info`), so rows are grouped
// here for reading, not for the spec. Every DATA_TYPE shared by several rows
// marks its closest match, as `every_shared_data_type_has_one_preferred_row`
// asserts.
//
// A `LazyLock` rather than a plain `static`: `TypeInfoRow`'s string fields are
// `Cow<'static, str>` so a backend can compute them, and converting a `&'static
// str` literal through `Into` is not a const operation, so `TypeInfoRow::new`
// and the three string builders are not `const fn`. The table is fixed at
// compile time, so it is built once and borrowed for the life of the process.
static SQLITE_TYPE_INFO: std::sync::LazyLock<Vec<TypeInfoRow>> = std::sync::LazyLock::new(|| {
    vec![
        // WVARCHAR: sqlite_type_to_sql_data_type maps VARCHAR/CHAR/CHARACTER/
        // NCHAR/NVARCHAR/VARYING CHARACTER/NATIVE CHARACTER/TEXT/CLOB here, and
        // it is the CHAR/CLOB/TEXT-affinity fallback too. This is the row that
        // actually satisfies the invariant for every text-affinity declared
        // type; the SQL_VARCHAR/SQL_CHAR rows further down this list
        // exist only for Windows DM/pyodbc ANSI compatibility.
        TypeInfoRow::new("WVARCHAR", SqlDataType::EXT_W_VARCHAR)
            .with_column_size(catalog_column_size(
                SqlDataType::EXT_W_VARCHAR,
                MaxPrecision(VARCHAR_DEFAULT_COLUMN_SIZE),
                MaxScale(0),
            ))
            .with_literal_affixes(Some("'"), Some("'"))
            .with_create_params(Some("max length"))
            .with_case_sensitive(true),
        // WCHAR: Unicode counterpart to the CHAR row further down this list,
        // included for symmetry per the Windows DM checklist even though
        // sqlite_type_to_sql_data_type itself never produces EXT_W_CHAR (declared
        // CHAR(n) collapses into the WVARCHAR affinity above, matching real
        // SQLite semantics where CHAR(n) is not length-limited).
        TypeInfoRow::new("WCHAR", SqlDataType::EXT_W_CHAR)
            .with_column_size(catalog_column_size(
                SqlDataType::EXT_W_CHAR,
                MaxPrecision(WCHAR_COLUMN_SIZE_ROW),
                MaxScale(0),
            ))
            .with_literal_affixes(Some("'"), Some("'"))
            .with_create_params(Some("length"))
            .with_case_sensitive(true),
        // BIT: sqlite_type_to_sql_data_type maps BOOLEAN/BOOL here.
        TypeInfoRow::new("BIT", SqlDataType::EXT_BIT).with_column_size(catalog_column_size(
            SqlDataType::EXT_BIT,
            MaxPrecision(0),
            MaxScale(0),
        )),
        // TINYINT: sqlite_type_to_sql_data_type maps TINYINT here.
        TypeInfoRow::new("TINYINT", SqlDataType::EXT_TINY_INT)
            .with_column_size(catalog_column_size(
                SqlDataType::EXT_TINY_INT,
                MaxPrecision(0),
                MaxScale(0),
            ))
            .with_unsigned(Some(false))
            .with_auto_unique_value(Some(false))
            .with_scale_range(Some(0), Some(0))
            .with_num_prec_radix(Some(10)),
        // BIGINT: sqlite_type_to_sql_data_type maps INTEGER/INT/BIGINT/INT8 here
        // (and the "INT"-substring affinity fallback), since SQLite integers are
        // always 64-bit storage. This is the row an INTEGER column's reported
        // type (SQL_BIGINT) actually resolves to.
        TypeInfoRow::new("BIGINT", SqlDataType::EXT_BIG_INT)
            .with_column_size(catalog_column_size(
                SqlDataType::EXT_BIG_INT,
                MaxPrecision(0),
                MaxScale(0),
            ))
            .with_unsigned(Some(false))
            .with_auto_unique_value(Some(false))
            .with_scale_range(Some(0), Some(0))
            .with_num_prec_radix(Some(10)),
        TypeInfoRow::new("BLOB", SqlDataType::EXT_VAR_BINARY)
            .with_column_size(catalog_column_size(
                SqlDataType::EXT_VAR_BINARY,
                MaxPrecision(BLOB_DEFAULT_COLUMN_SIZE),
                MaxScale(0),
            ))
            .with_literal_affixes(Some("X'"), Some("'"))
            .with_create_params(Some("max length")),
        // SQL_CHAR (1): ANSI alias. See the SQL_VARCHAR comment further down
        // this list; same rationale for why this is a distinct row from the
        // WCHAR row above.
        TypeInfoRow::new("CHAR", SqlDataType::CHAR)
            .with_column_size(catalog_column_size(
                SqlDataType::CHAR,
                MaxPrecision(CHAR_COLUMN_SIZE_ROW),
                MaxScale(0),
            ))
            .with_literal_affixes(Some("'"), Some("'"))
            .with_create_params(Some("length"))
            .with_case_sensitive(true),
        // DECIMAL: sqlite_type_to_sql_data_type maps DECIMAL/NUMERIC here, and
        // it is also the NUMERIC-affinity fallback for any declared type that
        // SQLite's own affinity rules do not otherwise classify.
        TypeInfoRow::new("DECIMAL", SqlDataType::DECIMAL)
            .with_column_size(catalog_column_size(
                SqlDataType::DECIMAL,
                MaxPrecision(DECIMAL_DEFAULT_COLUMN_SIZE),
                MaxScale(DECIMAL_MAX_SCALE),
            ))
            .with_create_params(Some("precision,scale"))
            .with_unsigned(Some(false))
            .with_auto_unique_value(Some(false))
            .with_scale_range(Some(0), Some(DECIMAL_MAX_SCALE))
            .with_num_prec_radix(Some(10)),
        TypeInfoRow::new("INTEGER", SqlDataType::INTEGER)
            .with_column_size(catalog_column_size(
                SqlDataType::INTEGER,
                MaxPrecision(0),
                MaxScale(0),
            ))
            .with_unsigned(Some(false))
            .with_auto_unique_value(Some(false))
            .with_scale_range(Some(0), Some(0))
            .with_num_prec_radix(Some(10)),
        // SMALLINT: sqlite_type_to_sql_data_type maps SMALLINT/INT2 here.
        TypeInfoRow::new("SMALLINT", SqlDataType::SMALLINT)
            .with_column_size(catalog_column_size(
                SqlDataType::SMALLINT,
                MaxPrecision(0),
                MaxScale(0),
            ))
            .with_unsigned(Some(false))
            .with_auto_unique_value(Some(false))
            .with_scale_range(Some(0), Some(0))
            .with_num_prec_radix(Some(10)),
        TypeInfoRow::new("REAL", SqlDataType::DOUBLE)
            .with_column_size(catalog_column_size(
                SqlDataType::DOUBLE,
                MaxPrecision(0),
                MaxScale(0),
            ))
            .with_unsigned(Some(false))
            .with_num_prec_radix(Some(2)),
        // TEXT: column_size matches VARCHAR_DEFAULT_COLUMN_SIZE (255), the
        // same default `default_precision_for_type` reports for both VARCHAR and
        // EXT_W_VARCHAR (see type_conversion.rs). This row and the VARCHAR row
        // immediately below both describe SQLite's single, unbounded TEXT
        // storage class under the shared ANSI DATA_TYPE=12, so they must report
        // the same size. 255 is the value the rest of the driver treats as
        // authoritative for this DATA_TYPE (`default_precision_for_type`, and the
        // WVARCHAR row below), so both rows use it.
        //
        // Preferred over the VARCHAR row below: TEXT is SQLite's own storage
        // class, and SQLite ignores a VARCHAR(n) length, so it is the closer
        // match for SQL_VARCHAR. Core orders it first among the two.
        TypeInfoRow::new("TEXT", SqlDataType::VARCHAR)
            .with_column_size(catalog_column_size(
                SqlDataType::VARCHAR,
                MaxPrecision(VARCHAR_DEFAULT_COLUMN_SIZE),
                MaxScale(0),
            ))
            .with_literal_affixes(Some("'"), Some("'"))
            .with_create_params(Some("max length"))
            .with_case_sensitive(true)
            .with_preferred(true),
        // SQL_VARCHAR (12): ANSI alias needed for Windows DM / pyodbc type
        // conversion (AGENTS.md "Windows Driver Manager compatibility
        // checklist"). sqlite_type_to_sql_data_type never actually returns this
        // ANSI code (only EXT_W_VARCHAR, see the WVARCHAR row above); this row
        // exists purely so SQLGetTypeInfo(SQL_VARCHAR) finds a match. TYPE_NAME
        // differs from the TEXT row immediately above (same DATA_TYPE) because
        // SQLite itself treats VARCHAR as a recognised alias of TEXT, and the
        // spec explicitly allows multiple rows sharing a DATA_TYPE; column_size
        // matches the TEXT row above for the same reason (see that row's
        // comment).
        TypeInfoRow::new("VARCHAR", SqlDataType::VARCHAR)
            .with_column_size(catalog_column_size(
                SqlDataType::VARCHAR,
                MaxPrecision(VARCHAR_DEFAULT_COLUMN_SIZE),
                MaxScale(0),
            ))
            .with_literal_affixes(Some("'"), Some("'"))
            .with_create_params(Some("max length"))
            .with_case_sensitive(true),
        // DATE: sqlite_type_to_sql_data_type maps DATE here. SQLite has no DATE
        // literal syntax; a date value is just a quoted ISO-8601 string, hence
        // the plain quote prefix/suffix (matching the TEXT row's convention)
        // rather than a typed `DATE '...'` literal.
        // DATA_TYPE=91 (SQL_TYPE_DATE), SQL_DATA_TYPE=9 (SQL_DATETIME), SQL_DATETIME_SUB=1 (SQL_CODE_DATE)
        TypeInfoRow::new("DATE", SqlDataType::DATE)
            .with_column_size(catalog_column_size(
                SqlDataType::DATE,
                MaxPrecision(0),
                MaxScale(0),
            ))
            // 'YYYY-MM-DD'
            .with_literal_affixes(Some("'"), Some("'"))
            .with_verbose_type(SqlDataType::DATETIME.0, Some(SQL_CODE_DATE)),
        // TIME: sqlite_type_to_sql_data_type maps TIME here. SQLite stores
        // time values as text, and MAX_FRACTIONAL_SECONDS_PRECISION (3) is the
        // fraction its own date/time functions render (see
        // column_value_to_rusqlite), so that is the maximum scale reported.
        // DATA_TYPE=92 (SQL_TYPE_TIME), SQL_DATA_TYPE=9 (SQL_DATETIME), SQL_DATETIME_SUB=2 (SQL_CODE_TIME)
        TypeInfoRow::new("TIME", SqlDataType::TIME)
            // 'HH:MM:SS.fff': the spec's TIME formula at
            // MAX_FRACTIONAL_SECONDS_PRECISION, which budgets the separator
            // and the three fractional digits alongside the eight fixed ones.
            .with_column_size(catalog_column_size(
                SqlDataType::TIME,
                MaxPrecision(0),
                MaxScale(MAX_FRACTIONAL_SECONDS_PRECISION),
            ))
            .with_literal_affixes(Some("'"), Some("'"))
            .with_scale_range(Some(0), Some(MAX_FRACTIONAL_SECONDS_PRECISION))
            .with_verbose_type(SqlDataType::DATETIME.0, Some(SQL_CODE_TIME)),
        // TIMESTAMP: sqlite_type_to_sql_data_type maps DATETIME/TIMESTAMP
        // here. column_size is computed via catalog_column_size at
        // MAX_FRACTIONAL_SECONDS_PRECISION, the same constant
        // sqlite_declared_type_precision uses as the fallback for an
        // undeclared TIMESTAMP column (see the consistency test below), so the
        // reported maximum scale and the budgeted column size cannot
        // disagree.
        // DATA_TYPE=93 (SQL_TYPE_TIMESTAMP), SQL_DATA_TYPE=9 (SQL_DATETIME), SQL_DATETIME_SUB=3 (SQL_CODE_TIMESTAMP)
        TypeInfoRow::new("TIMESTAMP", SqlDataType::TIMESTAMP)
            // 'YYYY-MM-DD HH:MM:SS.fff': same rationale as the TIME row
            // above.
            .with_column_size(catalog_column_size(
                SqlDataType::TIMESTAMP,
                MaxPrecision(0),
                MaxScale(MAX_FRACTIONAL_SECONDS_PRECISION),
            ))
            .with_literal_affixes(Some("'"), Some("'"))
            .with_scale_range(Some(0), Some(MAX_FRACTIONAL_SECONDS_PRECISION))
            .with_verbose_type(SqlDataType::DATETIME.0, Some(SQL_CODE_TIMESTAMP)),
    ]
});

// `CHAR`/`WCHAR`'s "unbounded" sentinel. `VARCHAR`/`DECIMAL`/`BLOB`'s default
// column sizes and `TIME`/`TIMESTAMP`'s maximum fractional-seconds precision
// come directly from `type_conversion.rs`'s `pub(crate)` constants (used
// below via `catalog_column_size`); there is now exactly one copy of each,
// not two kept in sync by a test.
const CHAR_COLUMN_SIZE_ROW: i32 = u16::MAX as i32;
const WCHAR_COLUMN_SIZE_ROW: i32 = u16::MAX as i32;
/// Conventional maximum scale for undeclared DECIMAL/NUMERIC, matching
/// `DECIMAL_DEFAULT_COLUMN_SIZE` (both 38); SQLite imposes no real limit,
/// so precision and scale share the same conventional ceiling.
const DECIMAL_MAX_SCALE: i16 = 38;

/// The arms of this match are connection-independent (driver-level constants).
/// Extracted so that both the connected and pre-connect paths can use it
/// without duplicating the match.
///
/// `conn` is `None` on the pre-connect path, and is carried only to be handed
/// on: core's capability hooks and `default_get_info` take
/// `Option<&Self::Connection>` since `SQLGetInfo` is a per-connection call.
/// Pre-connect they answer only what is knowable without a data source.
fn sqlite_get_info(
    conn: Option<&SqliteConnection>,
    info_type: InfoType,
) -> Result<InfoValue, SqliteError> {
    // Driver-specific overrides.
    //
    // The identity group (`SQL_DRIVER_NAME`, `SQL_DRIVER_VER`,
    // `SQL_DBMS_NAME`, `SQL_DBMS_VER`) is deliberately absent: each is a
    // `Backend` hook now, so core answers all four, and stating them here as
    // well would be the "declare it once" violation AGENTS.md describes.
    // `SQL_INTEGRITY` and `SQL_TXN_CAPABLE` moved for the same reason.
    // `get_info_snapshot` still pins every value an application sees.
    match info_type {
        // 0, not an identifier length: this driver reports no catalogs and no
        // schemas, so there is no name whose maximum length these could
        // describe. Core defaults them to its generic identifier length, which
        // states a bound on something it has just said does not exist. The
        // spec defines 0 as "no maximum length or the length is unknown",
        // which is the closest available reading of "not applicable".
        //
        // `supports_catalogs`/`supports_schemas` are per-connection hooks, so
        // these arms only apply once a connection exists. Pre-connect the
        // question falls through to core, which answers its generic identifier
        // length, the same shape it reports for every other `SQL_MAX_*_LEN`
        // before a data source is open.
        InfoType::MaxCatalogNameLen
            if conn.is_some_and(|c| !SqliteBackend::supports_catalogs(c)) =>
        {
            return Ok(InfoValue::U16(0));
        }
        InfoType::MaxSchemaNameLen if conn.is_some_and(|c| !SqliteBackend::supports_schemas(c)) => {
            return Ok(InfoValue::U16(0));
        }
        // Only SERIALIZABLE. "Transactions in SQLite are SERIALIZABLE", and
        // READ COMMITTED and REPEATABLE READ do not exist in SQLite at all.
        //
        // READ UNCOMMITTED is deliberately not claimed either. It requires
        // shared-cache mode as well as `PRAGMA read_uncommitted`: "The
        // combined use of shared cache mode and the read_uncommitted pragma is
        // the only way that one database connection can see uncommitted
        // changes on a different database connection." This driver opens with
        // a plain `rusqlite::Connection::open`, so shared cache is off and the
        // level is unreachable.
        //
        // Advertising all four would be a promise nothing keeps. Nothing
        // applies the value an application sets (`SQL_ATTR_TXN_ISOLATION` is
        // stored on the connection and read back, never pushed to SQLite), so
        // an application asking for REPEATABLE READ would be told it had it
        // while running serializable.
        //
        // Spec: <https://www.sqlite.org/isolation.html>
        InfoType::TransactionIsolationProtocol => {
            return Ok(InfoValue::U32(SQL_TXN_SERIALIZABLE));
        }
        // SQL_GETDATA_EXTENSIONS is deliberately not answered here. It states
        // what core's own fetch path supports: `sql_get_data` checks neither
        // column order nor binding state, and `sql_set_stmt_attr_w` substitutes
        // 1 back for any SQL_ATTR_ROW_ARRAY_SIZE, so no block cursor can exist
        // for SQL_GD_BLOCK to describe. None of that is a fact about SQLite,
        // and this driver cannot keep it true if core's fetch path changes.
        // Core answers it, and `get_info_snapshot` below still pins the value
        // an application actually sees.
        _ => {}
    }

    // Fall through to shared defaults. Core reads the catalog result column
    // widths off the backend type parameter itself, so they cannot disagree
    // with what this driver reports everywhere else.
    default_get_info::<SqliteBackend>(conn, info_type).ok_or_else(|| SqliteError::NotImplemented {
        feature: format!("get_info({info_type:?})"),
    })
}

pub(super) fn get_info(
    conn: &SqliteConnection,
    info_type: InfoType,
) -> Result<InfoValue, SqliteError> {
    if let Some(value) = connection_limit(conn, info_type)? {
        return Ok(value);
    }
    sqlite_get_info(Some(conn), info_type)
}

/// The `SQL_MAX_*` values SQLite can be asked for directly, via
/// `sqlite3_limit` (`rusqlite::Connection::limit`, a safe wrapper).
///
/// The spec allows `0` for "no specified limit or the limit is unknown", and
/// core answers `0` for exactly that reason, having no way to know. This
/// driver does: these are real, enforced limits, and an application reads them
/// to decide whether to chunk a wide `SELECT` or a long `IN` list. `0` tells it
/// there is nothing to chunk around.
///
/// They are read per connection rather than hardcoded because they are
/// per-connection settable: `sqlite3_limit` both reads and writes, so a
/// compile-time constant would be wrong for any connection that changed one.
///
/// Returns `None` for every other info type, leaving `sqlite_get_info` to
/// answer. `get_info_pre_connect` has no connection and so keeps reporting
/// `0`: with no connection the limit genuinely is unknown, which is what `0`
/// means.
fn connection_limit(
    conn: &SqliteConnection,
    info_type: InfoType,
) -> Result<Option<InfoValue>, SqliteError> {
    use rusqlite::limits::Limit;

    let limit = match info_type {
        // All five are bounded by the per-connection column limit: SQLite
        // applies SQLITE_LIMIT_COLUMN to a table definition, a result set, and
        // the terms of a GROUP BY / ORDER BY / index alike.
        InfoType::MaxColumnsInSelect
        | InfoType::MaxColumnsInTable
        | InfoType::MaxColumnsInGroupBy
        | InfoType::MaxColumnsInOrderBy
        | InfoType::MaxColumnsInIndex => Limit::SQLITE_LIMIT_COLUMN,
        InfoType::MaxStatementLen => Limit::SQLITE_LIMIT_SQL_LENGTH,
        // SQL_MAX_ROW_SIZE is the largest row the data source will accept.
        // SQLITE_LIMIT_LENGTH bounds any single string or blob, which is the
        // binding constraint on a row: SQLite imposes no separate row width.
        InfoType::MaxRowSize => Limit::SQLITE_LIMIT_LENGTH,
        // Deliberately absent: SQL_MAX_TABLES_IN_SELECT. SQLite caps a join at
        // 64 tables, but that is a compile-time constant with no sqlite3_limit
        // to read it from, and hardcoding 64 here would be the kind of
        // transcribed-from-documentation value that has gone stale twice in
        // this crate. It keeps core's 0, "unknown".
        _ => return Ok(None),
    };

    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;
    let raw = db.limit(limit).map_err(map_sqlite_error)?;

    // sqlite3_limit returns the current value, always non-negative in practice;
    // a negative would mean "query, do not set" leaked through, so treat it as
    // unknown rather than wrapping it into a huge unsigned number.
    if raw < 0 {
        return Ok(None);
    }

    Ok(Some(match info_type {
        InfoType::MaxStatementLen | InfoType::MaxRowSize => InfoValue::U32(raw as u32),
        // The column limits are SQLUSMALLINT. SQLite caps SQLITE_LIMIT_COLUMN
        // at 32767 so this cannot truncate, but clamp rather than cast so a
        // future cap increase understates instead of wrapping to a small number.
        _ => InfoValue::U16(u16::try_from(raw).unwrap_or(u16::MAX)),
    }))
}

pub(super) fn get_info_pre_connect(info_type: InfoType) -> Result<InfoValue, SqliteError> {
    sqlite_get_info(None, info_type)
}

/// `SQL_AGGREGATE_FUNCTIONS`: SQLite has every ODBC aggregate, and accepts
/// both `DISTINCT` and `ALL` as set quantifiers.
/// <https://sqlite.org/lang_aggfunc.html>
pub(crate) const SQLITE_AGGREGATE_FUNCTIONS: u32 =
    SQL_AF_AVG | SQL_AF_COUNT | SQL_AF_MAX | SQL_AF_MIN | SQL_AF_SUM | SQL_AF_DISTINCT | SQL_AF_ALL;

/// `SQL_SUBQUERIES` (95): the subquery forms SQLite accepts.
///
/// `SQL_SQ_QUANTIFIED` is deliberately absent. It covers `< ALL` / `< ANY` /
/// `< SOME`, which SQLite does not parse. That is the same finding
/// `sql92_predicates_excludes_quantified_comparison_and_match` records for
/// `SQL_SP_QUANTIFIED_COMPARISON`. Claiming it here would deny quantified
/// comparison in one info type while asserting it in another. Each remaining
/// bit is exercised by `subqueries_are_each_live_probed`.
pub(crate) const SQLITE_SUBQUERIES: u32 =
    SQL_SQ_COMPARISON | SQL_SQ_EXISTS | SQL_SQ_IN | SQL_SQ_CORRELATED_SUBQUERIES;

/// `SQL_UNION` (96): SQLite has both `UNION` and `UNION ALL`.
pub(crate) const SQLITE_UNION: u32 = SQL_U_UNION | SQL_U_UNION_ALL;

/// `SQL_SPECIAL_CHARACTERS` (94): the characters beyond `a`–`z`, `A`–`Z`,
/// `0`–`9` and `_` that may appear in an undelimited SQLite identifier.
///
/// Just `$`. SQLite's tokenizer classifies it as an identifier character, so a
/// name containing it parses unquoted and round-trips through `sqlite_master`
/// unchanged. Every character in `SPECIAL_CHARACTER_CANDIDATES` is executed
/// against the bundled library by
/// `special_characters_are_each_live_probed`, which checks the rejected ones
/// too.
pub(crate) const SQLITE_SPECIAL_CHARACTERS: &str = "$";

/// The punctuation `special_characters_are_each_live_probed` tries in an
/// undelimited identifier: everything on a US keyboard that is not
/// alphanumeric or `_`.
///
/// The probe asserts membership in [`SQLITE_SPECIAL_CHARACTERS`] both ways, so
/// this list is what stops that bitmap-equivalent from understating. A
/// character SQLite starts accepting shows up as a failure here rather than
/// going unnoticed.
#[cfg(test)]
pub(crate) const SPECIAL_CHARACTER_CANDIDATES: &str = "$#@!%^&*-+=./:?~`|\\'\"<>(){}[],;";

/// `SQL_CONVERT_FUNCTIONS` (48): SQLite's `CAST(x AS type)`. It has no
/// ODBC `CONVERT` scalar function, so only the `CAST` bit is claimed.
pub(crate) const SQLITE_CONVERT_FUNCTIONS: u32 = SQL_FN_CVT_CAST;

/// `SQL_OUTER_JOIN_CAPABILITIES` (115): every outer-join form SQLite
/// implements, and every relaxation of the `ON` clause the bitmap asks about.
///
/// Each bit is proved by executing the join it describes against the bundled
/// library in `outer_join_capabilities_are_each_live_probed`; `RIGHT` and
/// `FULL` arrived in SQLite 3.39.0.
pub(crate) const SQLITE_OUTER_JOIN_CAPABILITIES: u32 = SQL_OJ_LEFT
    | SQL_OJ_RIGHT
    | SQL_OJ_FULL
    | SQL_OJ_NESTED
    | SQL_OJ_NOT_ORDERED
    | SQL_OJ_INNER
    | SQL_OJ_ALL_COMPARISON_OPS;

/// `SQL_ALTER_TABLE` (86): the `ALTER TABLE` clauses SQLite accepts, of those
/// the ODBC bitmap can express.
///
/// Every bit here was established by executing the clause against the bundled
/// library (3.53.2), not read off the documentation.
/// `alter_table_capabilities_are_each_live_probed` is that probe, and it
/// checks the unclaimed bits too. That matters: `ADD CONSTRAINT` and
/// `DROP CONSTRAINT` are recent additions, rejected by 3.51.3 and accepted by
/// 3.53.2, so a bitmap written from an older recollection of SQLite's grammar
/// understates it.
///
/// Claimed:
///
/// - `ADD COLUMN`, with `DEFAULT` and `COLLATE`.
/// - `ADD CONSTRAINT <name> CHECK (...)`, which rewrites the stored schema to
///   carry a genuine table constraint. Note the ODBC bit is all-or-nothing
///   while SQLite accepts only `CHECK` here; `UNIQUE`, `PRIMARY KEY` and
///   `FOREIGN KEY` are still syntax errors.
/// - `SQL_AT_CONSTRAINT_NAME_DEFINITION`, since that `CONSTRAINT <name>` clause
///   is exactly what the bit describes.
/// - `SQL_AT_ADD_CONSTRAINT`, which despite its name means "`ADD COLUMN` is
///   supported *with column constraints*", not table constraints. SQLite takes
///   `NOT NULL` (given a non-null default), `CHECK`, `REFERENCES` and a named
///   `CONSTRAINT` on an added column. Only `UNIQUE` and `PRIMARY KEY` are
///   refused, with "Cannot add a UNIQUE column".
///
/// Supported by SQLite but *unrepresentable*, so absent by necessity rather
/// than because SQLite lacks them: unqualified `DROP COLUMN` (3.35.0+) and
/// unqualified `DROP CONSTRAINT`, for which the ODBC 3.x bitmap offers only
/// `CASCADE` and `RESTRICT` variants, and SQLite rejects both keywords, so
/// claiming either would advertise a syntax an application would send and have
/// refused. `sql.h` does carry ODBC 2.0-era `SQL_AT_ADD_COLUMN` and
/// `SQL_AT_DROP_COLUMN` bits for the unqualified forms, but the ODBC 3.x
/// `SQL_ALTER_TABLE` table does not define them, and this driver reports
/// `SQL_OIC_CORE` against ODBC 3.x. `RENAME TO` and `RENAME COLUMN` have no
/// bit at all.
///
/// Deliberately **not** claimed: the four `SQL_AT_CONSTRAINT_*` deferrability
/// bits. SQLite implements deferred constraints only inside a foreign-key
/// clause, and its parser additionally accepts `DEFERRABLE` after a `CHECK` or
/// `NOT NULL` constraint, where SQL-92 does not allow it and where it has no
/// effect. Accepting a token is not implementing the attribute, and deriving a
/// general capability from an FK-only feature plus a permissive parser is
/// exactly the overstatement these bitmaps invite.
///
/// Genuinely absent: `ALTER COLUMN ... SET DEFAULT` and
/// `ALTER COLUMN ... DROP DEFAULT` are not SQLite grammar.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetinfo-function>
/// SQLite: <https://www.sqlite.org/lang_altertable.html>
pub(crate) const SQLITE_ALTER_TABLE: u32 = SQL_AT_ADD_COLUMN_SINGLE
    | SQL_AT_ADD_COLUMN_DEFAULT
    | SQL_AT_ADD_COLUMN_COLLATION
    | SQL_AT_ADD_CONSTRAINT
    | SQL_AT_ADD_TABLE_CONSTRAINT
    | SQL_AT_CONSTRAINT_NAME_DEFINITION;

/// `SQL_SQL92_PREDICATES`.
///
/// Deliberately absent: quantified comparison (`< ALL` / `< ANY` / `< SOME`
/// all fail to prepare, SQLite's `ALL`/`ANY` being set quantifiers on
/// compound selects, not comparison quantifiers); the four `MATCH` variants
/// (SQLite's `MATCH` is an FTS extension hook, not the SQL-92 row-matching
/// predicate); `OVERLAPS`; and `UNIQUE`.
/// <https://sqlite.org/lang_expr.html>
pub(crate) const SQLITE_SQL92_PREDICATES: u32 = SQL_SP_EXISTS
    | SQL_SP_ISNOTNULL
    | SQL_SP_ISNULL
    | SQL_SP_LIKE
    | SQL_SP_IN
    | SQL_SP_BETWEEN
    | SQL_SP_COMPARISON;

/// `SQL_SQL92_RELATIONAL_JOIN_OPERATORS`.
///
/// `RIGHT OUTER JOIN` and `FULL OUTER JOIN` arrived in SQLite 3.39.0; this
/// build is 3.53.2 and both were confirmed by live query. Absent:
/// `CORRESPONDING` (fails to prepare) and SQL-92 `UNION JOIN`, which SQLite
/// has never had.
/// <https://sqlite.org/lang_select.html>
pub(crate) const SQLITE_SQL92_JOIN_OPERATORS: u32 = SQL_SRJO_CROSS_JOIN
    | SQL_SRJO_EXCEPT_JOIN
    | SQL_SRJO_FULL_OUTER_JOIN
    | SQL_SRJO_INNER_JOIN
    | SQL_SRJO_INTERSECT_JOIN
    | SQL_SRJO_LEFT_OUTER_JOIN
    | SQL_SRJO_NATURAL_JOIN
    | SQL_SRJO_RIGHT_OUTER_JOIN;

/// `SQL_SQL92_VALUE_EXPRESSIONS`: all four present.
/// <https://sqlite.org/lang_expr.html>
pub(crate) const SQLITE_SQL92_VALUE_EXPRESSIONS: u32 =
    SQL_SVE_CASE | SQL_SVE_CAST | SQL_SVE_COALESCE | SQL_SVE_NULLIF;

/// `SQL_NUMERIC_FUNCTIONS`: only three.
///
/// `rusqlite`'s `bundled` feature does **not** define
/// `SQLITE_ENABLE_MATH_FUNCTIONS`, so the entire trig/log/power/sqrt set is
/// compiled out and was confirmed absent by probing each one.
///
/// `SIGN` is the exception worth knowing about: it survives because
/// `sign()` is documented on the *core* functions page, not the math
/// functions page, so it is not gated by that flag. Do not remove it on the
/// assumption that "math functions are off" implies no `sign()`.
///
/// Deliberately absent despite near-misses: `MOD` (`%` is an operator, not a
/// function, and is integer-only, so `7.5 % 2` yields `1`) and `RAND`
/// (`random()` returns a signed 64-bit integer, not ODBC's float in `[0,1)`,
/// and takes no seed).
/// <https://sqlite.org/lang_corefunc.html>
pub(crate) const SQLITE_NUMERIC_FUNCTIONS: u32 =
    SQL_FN_NUM_ABS | SQL_FN_NUM_SIGN | SQL_FN_NUM_ROUND;

/// `SQL_STRING_FUNCTIONS`: SQLite equivalents, several under other names:
/// `LCASE` is `lower()`, `UCASE` is `upper()`, `SUBSTRING` is `substr()`,
/// `ASCII` is `unicode()`, `CHAR` is `char()`.
///
/// `SOUNDEX` is claimed because this build enables `SQLITE_SOUNDEX`, which is
/// **not** the SQLite default, verified by probe (`soundex('Robert')` gives
/// `R163`). A future `rusqlite` bump could silently drop it, which is why
/// `tests::live_sqlite_supports_sign_soundex_and_octet_length` opens a real
/// in-memory connection and calls it (along with `sign()` and
/// `octet_length()`, the other two counter-intuitive entries in this
/// bitmap) rather than only asserting the constant against its own
/// definition.
///
/// Deliberately absent: `LOCATE` and `LOCATE_2`, because `instr(haystack,
/// needle)` reverses ODBC's `LOCATE(needle, haystack)`; claiming it would
/// produce silently wrong answers rather than a clean failure. Also absent:
/// `LEFT`/`RIGHT`/`SPACE`/`INSERT`/`REPEAT`/`DIFFERENCE` (no such function)
/// and the `CHAR_LENGTH`/`CHARACTER_LENGTH`/`BIT_LENGTH`/`POSITION` family,
/// none of which SQLite defines.
/// <https://sqlite.org/lang_corefunc.html>
pub(crate) const SQLITE_STRING_FUNCTIONS: u32 = SQL_FN_STR_CONCAT
    | SQL_FN_STR_LTRIM
    | SQL_FN_STR_LENGTH
    | SQL_FN_STR_LCASE
    | SQL_FN_STR_REPLACE
    | SQL_FN_STR_RTRIM
    | SQL_FN_STR_SUBSTRING
    | SQL_FN_STR_UCASE
    | SQL_FN_STR_ASCII
    | SQL_FN_STR_CHAR
    | SQL_FN_STR_SOUNDEX
    | SQL_FN_STR_OCTET_LENGTH;

/// `SQL_SYSTEM_FUNCTIONS`: only `IFNULL`, which SQLite spells the same way.
///
/// SQLite has no user concept, so no `USERNAME`; and no scalar
/// database-name function, only the `pragma_database_list` table-valued
/// function, which is not an equivalent.
/// <https://sqlite.org/lang_corefunc.html>
pub(crate) const SQLITE_SYSTEM_FUNCTIONS: u32 = SQL_FN_SYS_IFNULL;

/// `SQL_TIMEDATE_FUNCTIONS`: only the current-date/time family.
///
/// `date()`, `time()` and `datetime()` take no arguments and return the
/// current value, so they are genuine equivalents of `CURDATE`, `CURTIME` and
/// `NOW`, and the three `CURRENT_*` keywords work directly.
///
/// Everything else is deliberately absent. SQLite has no `year()`,
/// `month()`, `day()`, `quarter()` or `extract()`, only `strftime()` with a
/// format string, which requires the application to write the format itself
/// and returns a zero-padded *string* rather than an integer. `timediff()`
/// exists but returns a formatted delta string, not a count in a caller-chosen
/// unit, so it is not `TIMESTAMPDIFF`. Date modifiers
/// (`date('2020-01-02', '+1 day')`) are a string argument, not a
/// unit-parameterised function, so they are not `TIMESTAMPADD`.
/// <https://sqlite.org/lang_datefunc.html>
pub(crate) const SQLITE_TIMEDATE_FUNCTIONS: u32 = SQL_FN_TD_NOW
    | SQL_FN_TD_CURDATE
    | SQL_FN_TD_CURTIME
    | SQL_FN_TD_CURRENT_DATE
    | SQL_FN_TD_CURRENT_TIME
    | SQL_FN_TD_CURRENT_TIMESTAMP;

/// SQLite's reserved words, read out of the linked library.
///
/// `Backend::keywords` returns the **raw** list: core subtracts
/// `ODBC_RESERVED_KEYWORDS`, sorts and joins it into `SQL_KEYWORDS` (89), so
/// the spec's "excluding ODBC's own" rule lives in one place across drivers
/// rather than being reimplemented per backend.
///
/// The names come from `sqlite3_keyword_count` / `sqlite3_keyword_name` rather
/// than from <https://www.sqlite.org/lang_keywords.html>. A transcribed list
/// would describe whichever SQLite the author was reading about; this describes
/// the one the driver is linked against, and needs no maintenance when that
/// changes. Same reason the `ALTER TABLE` and outer-join bitmaps are probed.
///
/// Cached behind a `OnceLock` because core recomputes `SQL_KEYWORDS` on every
/// call (it cannot cache a value that is generic over the backend), and
/// walking SQLite's keyword table each time would be wasteful. The table is
/// fixed at link time, so one walk is enough.
pub(crate) fn sqlite_keywords() -> &'static [std::borrow::Cow<'static, str>] {
    static KEYWORDS: std::sync::OnceLock<Vec<std::borrow::Cow<'static, str>>> =
        std::sync::OnceLock::new();
    KEYWORDS
        .get_or_init(|| {
            let count = unsafe { rusqlite::ffi::sqlite3_keyword_count() };
            let mut names: Vec<std::borrow::Cow<'static, str>> =
                Vec::with_capacity(count.max(0) as usize);

            for i in 0..count {
                let mut ptr: *const std::ffi::c_char = std::ptr::null();
                let mut len: std::ffi::c_int = 0;
                // SAFETY: `i` is in `0..sqlite3_keyword_count()`, the range the
                // API defines. On success it writes a pointer into SQLite's own
                // static keyword table, valid for the life of the process, and
                // its length; neither is owned by the caller, so the `'static`
                // borrow is sound.
                let rc = unsafe { rusqlite::ffi::sqlite3_keyword_name(i, &mut ptr, &mut len) };
                if rc != rusqlite::ffi::SQLITE_OK || ptr.is_null() || len <= 0 {
                    continue;
                }
                // SAFETY: as above. `ptr`/`len` describe a live, static, ASCII
                // keyword that SQLite never mutates or frees.
                let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
                if let Ok(name) = std::str::from_utf8(bytes) {
                    names.push(std::borrow::Cow::Borrowed(name));
                }
            }

            names
        })
        .as_slice()
}

pub(super) fn get_info_raw(
    conn: &SqliteConnection,
    info_type: u16,
) -> Option<Result<InfoValue, SqliteError>> {
    // Capability info types. Each one is a genuine `odbc_sys::InfoType`
    // variant (odbc-sys 0.31), but this driver still matches on the raw
    // `u16` here rather than the typed `InfoType` in `sqlite_get_info`,
    // because `get_info_raw` is the dispatch stage that runs before the
    // Driver-Manager-safe default and unconditionally wins for these types
    // (see `info_type_default_response` in `stackable-odbc-core/src/ffi/info.rs`).
    // Before these arms existed they fell through to stackable-odbc-core's generic
    // default of 0, so an application asking what SQLite could fold was
    // told "nothing".
    //
    // These describe SQLite *equivalents*, not literal ODBC escape-sequence
    // support in the naive sense: `SQLExecDirectW`
    // / `SQLPrepareW` do translate `{fn NAME(...)}` escapes
    // (`stackable_odbc_core::escape::translate_escapes`, driven by
    // `SqliteBackend::escape_dialect()`; see `crate::escape_dialect`), so
    // `{fn ABS(x)}` becomes `ABS(x)` and succeeds, and the "under other
    // names" entries documented above are remapped (`UCASE`->`upper`,
    // `LCASE`->`lower`, `SUBSTRING`->`substr`, `ASCII`->`unicode`, plus
    // `NOW`->`datetime`, `CURDATE`->`date`, `CURTIME`->`time` from the
    // `SQL_TIMEDATE_FUNCTIONS` bitmap below). Names SQLite spells identically
    // to ODBC (`ABS`, `ROUND`, `CONCAT`, `LENGTH`, `IFNULL`, `CHAR`, ...) pass
    // through unchanged and already worked. Still deliberately untranslated:
    // `CURRENT_DATE`/`CURRENT_TIME`/`CURRENT_TIMESTAMP`. SQLite treats these
    // as bare keywords (`SELECT CURRENT_DATE();` is a syntax error), and a
    // name-only remap cannot drop the trailing `()` the `{fn ...()}` escape
    // always includes; see the `crate::escape_dialect` module doc comment.
    // None of this is version-gated: SQLite's version is fixed at compile
    // time by the `bundled` feature.
    match info_type {
        SQL_AGGREGATE_FUNCTIONS => Some(Ok(InfoValue::U32(SQLITE_AGGREGATE_FUNCTIONS))),
        SQL_SQL92_PREDICATES => Some(Ok(InfoValue::U32(SQLITE_SQL92_PREDICATES))),
        SQL_SQL92_RELATIONAL_JOIN_OPERATORS => {
            Some(Ok(InfoValue::U32(SQLITE_SQL92_JOIN_OPERATORS)))
        }
        SQL_SQL92_VALUE_EXPRESSIONS => Some(Ok(InfoValue::U32(SQLITE_SQL92_VALUE_EXPRESSIONS))),
        SQL_NUMERIC_FUNCTIONS => Some(Ok(InfoValue::U32(SQLITE_NUMERIC_FUNCTIONS))),
        SQL_STRING_FUNCTIONS => Some(Ok(InfoValue::U32(SQLITE_STRING_FUNCTIONS))),
        SQL_SYSTEM_FUNCTIONS => Some(Ok(InfoValue::U32(SQLITE_SYSTEM_FUNCTIONS))),
        SQL_TIMEDATE_FUNCTIONS => Some(Ok(InfoValue::U32(SQLITE_TIMEDATE_FUNCTIONS))),
        // SQLite supports LIKE ... ESCAPE, and outer joins (RIGHT and FULL
        // since 3.39.0; this build is 3.53.2).
        SQL_LIKE_ESCAPE_CLAUSE => Some(Ok(InfoValue::String("Y".into()))),
        SQL_OUTER_JOINS => Some(Ok(InfoValue::String("Y".into()))),
        _ => common_get_info_raw::<SqliteBackend>(Some(conn), info_type).map(Ok),
    }
}

/// Every ODBC function this driver supports, which is exactly the set
/// `forward_ffi!` generates a C entry point for.
///
/// Derived from core rather than hand-listed. `SQLGetFunctions` is what the
/// Windows Driver Manager builds its dispatch table from, so a name in here
/// that core does not export hands the DM a null pointer to call; core pins
/// the list against its own macro arms, which no list maintained here could
/// do. The previous hand-written list over-claimed nothing but had drifted to
/// 53 of the 69 exported entry points, under-reporting sixteen the driver does
/// in fact export.
pub(super) fn get_functions() -> &'static [FunctionId] {
    CORE_EXPORTED_FUNCTIONS
}

pub(super) fn get_type_info() -> &'static [TypeInfoRow] {
    &SQLITE_TYPE_INFO
}

/// Bare, uppercase data-source-dependent type name for a column of
/// `sql_type`, shared by `SQL_DESC_TYPE_NAME` (`SQLColAttributeW`, via
/// `execute.rs`) and `SQLColumns.TYPE_NAME` (`metadata.rs`), so the two
/// never disagree, and so that name always matches a row in
/// [`SQLITE_TYPE_INFO`] (the same table `SQLGetTypeInfo` returns via
/// [`get_type_info`]) by construction, not merely by a matching test.
///
/// Spec (`SQL_DESC_TYPE_NAME`): "Data source-dependent data type name; for
/// example, "CHAR", "VARCHAR", "MONEY", "LONG VARBINARY", or "CHAR ( ) FOR
/// BIT DATA"." (`SQLColumns.TYPE_NAME` is worded identically, modulo a typo
/// in the truncated "LONG VARBINAR" example.) Every example in both spec
/// pages is a bare name, not a parameterised declaration. SQLite's declared
/// type string (e.g. `"VARCHAR(50)"`) is returned verbatim by neither of
/// these callers any more; the declared length is still carried, just via
/// `SQL_DESC_PRECISION`/`SQL_DESC_LENGTH`/`COLUMN_SIZE`
/// (`sqlite_declared_type_precision`), not the name.
///
/// A raw uppercase of the declared type's own base spelling is not used
/// here, because several declared aliases this driver recognises
/// (`INT8`/`INT2`/`DOUBLE PRECISION`/`FLOAT`/`CHARACTER`/`NCHAR`/…) do not
/// themselves appear as a `SQLITE_TYPE_INFO` `TYPE_NAME`; only their
/// canonical spelling does (`BIGINT`/`SMALLINT`/`REAL`/`WVARCHAR`/…, see the
/// row comments above). Looking the canonical name up directly in
/// `SQLITE_TYPE_INFO` by `sql_type` (rather than transcribing that mapping a
/// second time here) means the name returned can never drift from a real row.
///
/// Returns the empty string (matching `SQL_DESC_TYPE_NAME`'s documented
/// behaviour for an unknown type) if `sql_type` is not one this driver's own
/// `SQLGetTypeInfo` table has a row for; this should not happen for any
/// output of `sqlite_type_to_sql_data_type`, and is guarded by
/// `every_reportable_type_has_a_type_info_row` below, but a fallback avoids a
/// panic if that invariant is ever violated.
///
/// `SQLITE_TYPE_INFO` has two rows sharing `DATA_TYPE=SqlDataType::VARCHAR`
/// (`"TEXT"` and `"VARCHAR"`, both kept purely for Windows DM/pyodbc ANSI
/// compatibility; see the `WVARCHAR`/`TEXT`/`VARCHAR` row comments above),
/// with no single correct bare name for that DATA_TYPE. That case is
/// rejected explicitly below rather than left to `.find` picking whichever
/// row happens to come first: `sqlite_type_to_sql_data_type` never actually
/// returns the ANSI code (`SqlDataType::VARCHAR`) for any declared type
/// today, so the ambiguity is currently unreachable, but this function no
/// longer depends on that fact staying true to give a correct answer; it
/// would rather report "unknown" than silently guess.
pub(super) fn sqlite_bare_type_name(sql_type: SqlDataType) -> &'static str {
    if sql_type == SqlDataType::VARCHAR {
        tracing::warn!(
            ?sql_type,
            "sqlite_bare_type_name: DATA_TYPE=SQL_VARCHAR has more than one \
             SQLGetTypeInfo row (TEXT/VARCHAR) with no single canonical name; \
             reporting SQL_DESC_TYPE_NAME/TYPE_NAME as empty string"
        );
        return "";
    }
    SQLITE_TYPE_INFO
        .iter()
        .find(|row| row.data_type() == sql_type)
        .map(|row| row.type_name())
        .unwrap_or_else(|| {
            tracing::warn!(
                ?sql_type,
                "sqlite_bare_type_name: no SQLGetTypeInfo row for this SqlDataType; \
                 reporting SQL_DESC_TYPE_NAME/TYPE_NAME as empty string"
            );
            ""
        })
}

#[cfg(test)]
mod tests {

    /// Fixed-size types: the "Column Size" appendix formula for these takes
    /// no backend-specific parameter, so the row's value must equal the
    /// formula applied to *the row's own* `data_type`. Deriving the expected
    /// value from `row.data_type()` rather than repeating the table's own
    /// arguments is what makes this catch a row built with the wrong
    /// `SqlDataType`, the one way two drivers could disagree on a value the
    /// spec defines as backend-independent.
    ///
    /// This replaces a cross-driver test crate that compared the two drivers'
    /// tables directly. That crate had to link both drivers into one binary,
    /// which duplicates every `extern "system"` ODBC export (see the note in
    /// this crate's Cargo.toml), and it pinned expected sizes as literals:
    /// the very pattern deriving from the formula exists to remove.
    #[test]
    fn fixed_size_type_info_rows_use_the_backend_independent_formula() {
        // Arguments are ignored by the formula for every type listed here;
        // any value proves the point, so use deliberately absurd ones.
        const IGNORED_PRECISION: MaxPrecision = MaxPrecision(-1);
        const IGNORED_SCALE: MaxScale = MaxScale(-1);

        const BACKEND_INDEPENDENT: &[SqlDataType] = &[
            SqlDataType::EXT_BIT,
            SqlDataType::EXT_TINY_INT,
            SqlDataType::SMALLINT,
            SqlDataType::INTEGER,
            SqlDataType::EXT_BIG_INT,
            SqlDataType::REAL,
            SqlDataType::DOUBLE,
            SqlDataType::DATE,
        ];

        for row in SQLITE_TYPE_INFO.iter() {
            if !BACKEND_INDEPENDENT.contains(&row.data_type()) {
                continue;
            }
            let expected = catalog_column_size(row.data_type(), IGNORED_PRECISION, IGNORED_SCALE);
            assert_eq!(
                row.column_size(),
                expected,
                "{} (DATA_TYPE {:?}): COLUMN_SIZE is {} but the \
                 backend-independent appendix formula for that DATA_TYPE \
                 gives {}; the row is built from a different SqlDataType \
                 than it reports",
                row.type_name(),
                row.data_type(),
                row.column_size(),
                expected
            );
        }
    }
    use super::*;
    use stackable_odbc_core::types::{
        ConnectParams, DEFAULT_IDENTIFIER_LEN, InfoType, InfoValue, SQL_AM_NONE,
        SQL_AT_DROP_COLUMN_CASCADE, SQL_AT_DROP_COLUMN_DEFAULT, SQL_AT_DROP_COLUMN_RESTRICT,
        SQL_AT_DROP_TABLE_CONSTRAINT_CASCADE, SQL_AT_DROP_TABLE_CONSTRAINT_RESTRICT,
        SQL_AT_SET_COLUMN_DEFAULT, SQL_CA1_NEXT, SQL_CA2_READ_ONLY_CONCURRENCY, SQL_CB_PRESERVE,
        SQL_CN_ANY, SQL_DRIVER_ODBC_VER_STRING, SQL_FN_NUM_CEILING, SQL_FN_NUM_COS,
        SQL_FN_NUM_FLOOR, SQL_FN_NUM_LOG, SQL_FN_NUM_MOD, SQL_FN_NUM_POWER, SQL_FN_NUM_RAND,
        SQL_FN_NUM_SQRT, SQL_FN_NUM_TRUNCATE, SQL_FN_STR_BIT_LENGTH, SQL_FN_STR_CHAR_LENGTH,
        SQL_FN_STR_CHARACTER_LENGTH, SQL_FN_STR_DIFFERENCE, SQL_FN_STR_INSERT, SQL_FN_STR_LEFT,
        SQL_FN_STR_LOCATE, SQL_FN_STR_LOCATE_2, SQL_FN_STR_POSITION, SQL_FN_STR_REPEAT,
        SQL_FN_STR_RIGHT, SQL_FN_STR_SPACE, SQL_FN_TD_DAYNAME, SQL_FN_TD_DAYOFMONTH,
        SQL_FN_TD_EXTRACT, SQL_FN_TD_MONTH, SQL_FN_TD_MONTHNAME, SQL_FN_TD_QUARTER,
        SQL_FN_TD_TIMESTAMPADD, SQL_FN_TD_TIMESTAMPDIFF, SQL_FN_TD_YEAR, SQL_GB_NO_RELATION,
        SQL_GD_ANY_COLUMN, SQL_GD_ANY_ORDER, SQL_GD_BOUND, SQL_IC_MIXED, SQL_KEYWORDS,
        SQL_MAX_CURSOR_NAME_LEN, SQL_NC_LOW, SQL_NNC_NON_NULL, SQL_OIC_CORE, SQL_SO_FORWARD_ONLY,
        SQL_SP_MATCH_FULL, SQL_SP_MATCH_PARTIAL, SQL_SP_MATCH_UNIQUE_FULL,
        SQL_SP_MATCH_UNIQUE_PARTIAL, SQL_SP_OVERLAPS, SQL_SP_QUANTIFIED_COMPARISON, SQL_SP_UNIQUE,
        SQL_SQ_COMPARISON, SQL_SQ_CORRELATED_SUBQUERIES, SQL_SQ_EXISTS, SQL_SQ_IN,
        SQL_SQ_QUANTIFIED, SQL_SRJO_CORRESPONDING_CLAUSE, SQL_SRJO_UNION_JOIN, SQL_TC_ALL,
        SQL_TXN_READ_COMMITTED, SQL_TXN_READ_UNCOMMITTED, SQL_TXN_REPEATABLE_READ,
        SQL_TXN_SERIALIZABLE, SQL_UNSPECIFIED,
    };

    enum Expected {
        Str(&'static str),
        U16(u16),
        U32(u32),
    }

    /// An open connection for the tests that reach a per-connection capability
    /// hook.
    ///
    /// The hooks take a connection because `SQLGetInfo` is a per-connection
    /// call and a data source's capabilities can differ by server. Every one
    /// this driver declares is a property of the linked SQLite library rather
    /// than of the file opened, so any connection answers the same. The
    /// answers must still be read through one, which is what an application
    /// has.
    fn test_connection() -> SqliteConnection {
        let params = ConnectParams::parse("Database=:memory:").expect("parse");
        SqliteBackend::connect(&params).expect("connect")
    }

    #[rustfmt::skip]
    const EXPECTED: &[(InfoType, Expected)] = &[
        // --- String values ---
        (InfoType::DriverName,                    Expected::Str("stackable-odbc-sqlite")),
        (InfoType::DbmsName,                      Expected::Str("SQLite")),
        (InfoType::DriverOdbcVer,                 Expected::Str(SQL_DRIVER_ODBC_VER_STRING)),
        (InfoType::SearchPatternEscape,            Expected::Str("\\")),
        (InfoType::IdentifierQuoteChar,            Expected::Str("\"")),
        // Empty, not "catalog": core derives this from
        // Backend::supports_catalogs, which this driver answers false. The spec
        // requires an empty string when catalogs are unsupported, which
        // SQL_CATALOG_NAME = "N" declares.
        (InfoType::CatalogTerm,                   Expected::Str("")),
        // Empty, not "schema": derived from Backend::supports_schemas.
        (InfoType::SchemaTerm,                    Expected::Str("")),
        // Empty, not ".": same hook, same spec rule.
        (InfoType::CatalogNameSeparator,           Expected::Str("")),
        (InfoType::ColumnAlias,                   Expected::Str("Y")),
        (InfoType::OrderByColumnsInSelect,         Expected::Str("N")),
        (InfoType::CatalogName,                   Expected::Str("N")),
        (InfoType::DataSourceName,                Expected::Str("")),
        (InfoType::ServerName,                    Expected::Str("")),
        (InfoType::UserName,                      Expected::Str("")),
        (InfoType::DataSourceReadOnly,             Expected::Str("N")),
        (InfoType::AccessibleTables,              Expected::Str("Y")),
        (InfoType::AccessibleProcedures,          Expected::Str("N")),
        // "Y", not "N": SQLite implements and enforces the Integrity
        // Enhancement Facility. See Backend::integrity.
        (InfoType::Integrity,                     Expected::Str("Y")),
        // "$", not "": SQLite parses it inside an undelimited identifier. See
        // special_characters_are_each_live_probed.
        (InfoType::SpecialCharacters,             Expected::Str(SQLITE_SPECIAL_CHARACTERS)),
        (InfoType::XopenCliYear,                  Expected::Str("1995")),
        (InfoType::CollationSeq,                  Expected::Str("")),
        (InfoType::DescribeParameter,             Expected::Str("Y")),
        // --- U16 values ---
        (InfoType::GroupBy,                       Expected::U16(SQL_GB_NO_RELATION)),
        (InfoType::MaxDriverConnections,          Expected::U16(0)),
        (InfoType::MaxConcurrentActivities,       Expected::U16(0)),
        (InfoType::ConcatNullBehavior,            Expected::U16(0)),
        // SQL_CB_PRESERVE (2), derived from Backend::cursor_commit_behavior.
        // Not SQL_CB_DELETE: this driver materialises result sets eagerly, so
        // SQLEndTran cannot disturb an open cursor. See the hook in backend.rs.
        (InfoType::CursorCommitBehaviour,         Expected::U16(SQL_CB_PRESERVE)),
        (InfoType::IdentifierCase,                Expected::U16(SQL_IC_MIXED)),
        (InfoType::MaxColumnNameLen,              Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::MaxCursorNameLen,              Expected::U16(SQL_MAX_CURSOR_NAME_LEN)),
        (InfoType::MaxSchemaNameLen,              Expected::U16(0)),
        (InfoType::MaxCatalogNameLen,             Expected::U16(0)),
        (InfoType::MaxTableNameLen,               Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::NullCollation,                 Expected::U16(SQL_NC_LOW)),
        // These three were never in this snapshot: core invented them until it
        // made them required Backend methods, so nothing here asserted them.
        (InfoType::CorrelationName,               Expected::U16(SQL_CN_ANY)),
        (InfoType::NonNullableColumns,            Expected::U16(SQL_NNC_NON_NULL)),
        (InfoType::MaxColumnsInGroupBy,           Expected::U16(0)),
        (InfoType::MaxColumnsInIndex,             Expected::U16(0)),
        (InfoType::MaxColumnsInOrderBy,           Expected::U16(0)),
        (InfoType::MaxColumnsInSelect,            Expected::U16(0)),
        (InfoType::MaxColumnsInTable,             Expected::U16(0)),
        (InfoType::MaxTablesInSelect,             Expected::U16(0)),
        (InfoType::MaxUserNameLen,                Expected::U16(0)),
        (InfoType::ActiveEnvironments,            Expected::U16(0)),
        (InfoType::MaxIdentifierLen,              Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::CatalogLocation,               Expected::U16(0)),
        // TransactionCapable is SQLUSMALLINT per spec, not SQLUINTEGER. See
        // the matching comment on its arm in sqlite_get_info.
        (InfoType::TransactionCapable,            Expected::U16(SQL_TC_ALL as u16)),
        // --- U32 values ---
        // CursorSensitivity is SQLUINTEGER per spec, not SQLUSMALLINT. See
        // the matching comment in stackable-odbc-core's default_get_info.
        //
        // SQL_UNSPECIFIED, not SQL_INSENSITIVE. This describes core's fetch
        // path rather than SQLite, and core answers it: insensitivity is a
        // promise that no other cursor's changes become visible, which core
        // does not make about rows it has not read yet. Pinned here anyway,
        // because the snapshot's job is the value an application sees
        // regardless of which layer produced it.
        (InfoType::CursorSensitivity,             Expected::U32(SQL_UNSPECIFIED as u32)),
        // SQL_SQ_QUANTIFIED is absent: `< ALL` / `< ANY` / `< SOME` do not
        // parse, which SQL_SQL92_PREDICATES already records. Claiming it here
        // would make the two info types disagree.
        (InfoType::Subqueries,                    Expected::U32(SQLITE_SUBQUERIES)),
        (InfoType::UnionStatement,                Expected::U32(SQLITE_UNION)),
        (InfoType::DefaultTxnIsolation,           Expected::U32(SQL_TXN_SERIALIZABLE)),
        (InfoType::ScrollOptions,                 Expected::U32(SQL_SO_FORWARD_ONLY)),
        (InfoType::ConvertFunctions,              Expected::U32(SQLITE_CONVERT_FUNCTIONS)),
        // SERIALIZABLE only: READ COMMITTED and REPEATABLE READ do not exist
        // in SQLite, and READ UNCOMMITTED needs shared-cache mode, which this
        // driver never enables.
        (InfoType::TransactionIsolationProtocol,  Expected::U32(SQL_TXN_SERIALIZABLE)),
        (InfoType::AlterTable,                    Expected::U32(SQLITE_ALTER_TABLE)),
        (InfoType::MaxIndexSize,                  Expected::U32(0)),
        (InfoType::MaxRowSize,                    Expected::U32(0)),
        (InfoType::MaxStatementLen,               Expected::U32(0)),
        // Not 0: SQLite implements every outer-join form the spec asks about,
        // and 0 would contradict SQL_OUTER_JOINS = "Y".
        (InfoType::OuterJoinCapabilities,         Expected::U32(
            SQL_OJ_LEFT | SQL_OJ_RIGHT | SQL_OJ_FULL | SQL_OJ_NESTED
                | SQL_OJ_NOT_ORDERED | SQL_OJ_INNER | SQL_OJ_ALL_COMPARISON_OPS)),
        // 0, not SQL_SC_SQL92_ENTRY: entry level requires
        // SQL_GB_GROUP_BY_EQUALS_SELECT, and SQLite accepts a bare
        // non-aggregated column absent from GROUP BY. See
        // SqliteBackend::sql_conformance.
        (InfoType::SqlConformance,                Expected::U32(0)),
        (InfoType::OdbcInterfaceConformance,      Expected::U32(SQL_OIC_CORE)),
        (InfoType::AsyncMode,                     Expected::U32(SQL_AM_NONE)),
        (InfoType::AsyncDbcFunctions,             Expected::U32(0)),
        (InfoType::SchemaUsage,                   Expected::U32(0)),
        (InfoType::CatalogUsage,                  Expected::U32(0)),
        (InfoType::GetDataExtensions,             Expected::U32(SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER | SQL_GD_BOUND)),
        (InfoType::DynamicCursorAttributes1,      Expected::U32(0)),
        (InfoType::DynamicCursorAttributes2,      Expected::U32(0)),
        (InfoType::ForwardOnlyCursorAttributes1,  Expected::U32(SQL_CA1_NEXT)),
        // SQL_CA2_READ_ONLY_CONCURRENCY, not 0: core answers this, and reports
        // the concurrency its one cursor actually offers. `SQLSetStmtAttr`
        // accepts SQL_CONCUR_READ_ONLY unchanged and substitutes every other
        // value back to it with 01S02, so 0 would deny a concurrency the
        // driver had just accepted.
        (InfoType::ForwardOnlyCursorAttributes2,  Expected::U32(SQL_CA2_READ_ONLY_CONCURRENCY)),
        (InfoType::KeysetCursorAttributes1,       Expected::U32(0)),
        (InfoType::KeysetCursorAttributes2,       Expected::U32(0)),
        (InfoType::StaticCursorAttributes1,       Expected::U32(0)),
        (InfoType::StaticCursorAttributes2,       Expected::U32(0)),
    ];

    #[test]
    fn get_info_snapshot() {
        let conn = test_connection();
        for (info_type, expected) in EXPECTED {
            let actual = sqlite_get_info(Some(&conn), *info_type)
                .unwrap_or_else(|e| panic!("get_info returned error for {info_type:?}: {e:?}"));
            match (expected, &actual) {
                (Expected::Str(s), InfoValue::String(v)) => {
                    assert_eq!(v.as_str(), *s, "wrong value for {info_type:?}")
                }
                (Expected::U16(n), InfoValue::U16(v)) => {
                    assert_eq!(v, n, "wrong value for {info_type:?}")
                }
                (Expected::U32(n), InfoValue::U32(v)) => {
                    assert_eq!(v, n, "wrong value for {info_type:?}")
                }
                _ => panic!("type mismatch for {info_type:?}: got {actual:?}"),
            }
        }
    }

    /// `SQL_DBMS_VER` is a per-connection `Backend` hook now, so it is read
    /// through a connection rather than off the pre-connect path, which
    /// cannot answer it, having no data source to name the version of.
    #[test]
    fn dbms_ver_is_well_formed() {
        let s = SqliteBackend::dbms_version(&test_connection());
        let prefix = s.split(' ').next().unwrap_or("");
        let parts: Vec<&str> = prefix.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "SQL_DBMS_VER must start with ##.##.####, got {s:?}"
        );
        assert!(
            parts[0].len() >= 2 && parts[1].len() >= 2 && parts[2].len() >= 4,
            "SQL_DBMS_VER field widths wrong: {s:?}"
        );
        assert!(
            parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())),
            "SQL_DBMS_VER prefix must be all digits and dots: {s:?}"
        );
    }

    /// `SQL_QUOTED_IDENTIFIER_CASE` claims `SQL_IC_MIXED`, which asserts two
    /// separate things about quoted identifiers: that they are matched
    /// case-*insensitively*, and that the catalog stores them with the case
    /// they were written in. Both are probed against the bundled library,
    /// because the value this replaced (`SQL_IC_SENSITIVE`) was neither.
    ///
    /// A driver that claims `SQL_IC_SENSITIVE` here tells an application that
    /// `"T"` and `"t"` are different tables. In SQLite they are the same one:
    /// double quotes are a *delimiter*, letting a keyword or a name with
    /// punctuation be used as an identifier, and they do not switch on
    /// case-sensitive matching the way they do in a SQL-92 conformant DBMS.
    #[test]
    fn quoted_identifiers_are_not_case_sensitive() {
        let conn = test_connection();
        let db = conn.conn.lock().unwrap();
        db.execute_batch(r#"CREATE TABLE "MixedCase" (a INTEGER);"#)
            .unwrap();

        // Case-insensitive: a differently-cased quoted name finds the table.
        for spelling in [r#""mixedcase""#, r#""MIXEDCASE""#, r#""MiXeDcAsE""#] {
            db.execute_batch(&format!("SELECT * FROM {spelling};"))
                .unwrap_or_else(|e| {
                    panic!(
                        "SQLite resolved the quoted identifier {spelling} \
                         case-sensitively ({e}), so SQL_QUOTED_IDENTIFIER_CASE \
                         is not SQL_IC_MIXED"
                    )
                });
        }

        // Mixed *storage*: the catalog keeps the case it was created with,
        // which is what separates SQL_IC_MIXED from SQL_IC_UPPER/SQL_IC_LOWER.
        let stored: String = db
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored, "MixedCase",
            "SQLite folded a quoted identifier's stored case, so \
             SQL_QUOTED_IDENTIFIER_CASE is not SQL_IC_MIXED"
        );

        assert_eq!(
            SqliteBackend::quoted_identifier_case(&conn),
            SQL_IC_MIXED,
            "the probe above says SQL_IC_MIXED"
        );
    }

    /// Every character in [`SPECIAL_CHARACTER_CANDIDATES`] is executed inside
    /// an undelimited identifier, and the outcome is asserted against
    /// [`SQLITE_SPECIAL_CHARACTERS`] **both ways**.
    ///
    /// The negative half is the point, and is the same lesson
    /// `alter_table_capabilities_are_each_live_probed` records: a list that is
    /// only extended when someone notices can understate forever. `""` (core's
    /// old default, inherited rather than chosen) was exactly that, and had an
    /// application quoting `a$b`, a name SQLite parses bare.
    ///
    /// "Accepted" means more than "the CREATE parsed": the name must also come
    /// back out of `sqlite_master` unchanged. A character the tokenizer treats
    /// as punctuation could otherwise split the identifier and leave a
    /// differently-named table behind, which would be a *worse* answer than
    /// rejecting it.
    #[test]
    fn special_characters_are_each_live_probed() {
        let conn = test_connection();
        let db = conn.conn.lock().unwrap();

        for (index, ch) in SPECIAL_CHARACTER_CANDIDATES.chars().enumerate() {
            // A distinct table per candidate, and the character in the middle
            // so a leading-digit or leading-punctuation rule cannot be what is
            // actually being measured.
            let name = format!("probe{index}{ch}tail");
            let accepted = db
                .execute_batch(&format!("CREATE TABLE {name} (a INTEGER);"))
                .is_ok()
                && db
                    .query_row(
                        "SELECT 1 FROM sqlite_master WHERE name = ?1",
                        [&name],
                        |r| r.get::<_, i64>(0),
                    )
                    .is_ok();

            let claimed = SQLITE_SPECIAL_CHARACTERS.contains(ch);
            assert_eq!(
                accepted,
                claimed,
                "SQL_SPECIAL_CHARACTERS {}claims {ch:?}, but the bundled \
                 SQLite {} it in an undelimited identifier",
                if claimed { "" } else { "does not " },
                if accepted { "accepts" } else { "rejects" },
            );
        }

        assert_eq!(
            SqliteBackend::special_characters(&conn),
            SQLITE_SPECIAL_CHARACTERS,
            "the hook must report the probed list"
        );
    }

    /// SQL_DRIVER_VER is derived from Cargo.toml, so it cannot be asserted
    /// against a literal without reintroducing drift between the two.
    /// Assert the spec's shape instead.
    ///
    /// Unlike `SQL_DBMS_VER` this needs no connection: it describes the driver,
    /// which the Windows Driver Manager asks about before one exists.
    #[test]
    fn driver_ver_is_well_formed() {
        let v = SqliteBackend::driver_version();
        let parts: Vec<&str> = v.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "SQL_DRIVER_VER must be ##.##.####, got {v:?}"
        );
        assert!(
            parts[0].len() >= 2 && parts[1].len() >= 2 && parts[2].len() >= 4,
            "SQL_DRIVER_VER field widths wrong: {v:?}"
        );
        assert!(
            parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())),
            "SQL_DRIVER_VER must be all digits and dots: {v:?}"
        );
    }

    /// Guards `SQLITE_STRING_FUNCTIONS`/`SQLITE_NUMERIC_FUNCTIONS`'s three
    /// counter-intuitive claims by actually calling the functions on a live
    /// in-memory connection, rather than only asserting the bitmap constant
    /// against its own definition (`fixed_size_type_info_rows_use_the_backend_independent_formula`-
    /// style tests in this module do that for COLUMN_SIZE; this test is the
    /// one that exercises these live):
    ///
    /// - `sign()`: survives `SQLITE_ENABLE_MATH_FUNCTIONS` being compiled
    ///   out of the `bundled` feature because it lives on the *core*
    ///   functions page, not the math one.
    /// - `soundex()`: exists only because this build enables the
    ///   non-default `SQLITE_SOUNDEX` compile flag.
    /// - `octet_length()`: claimed as the `SQL_FN_STR_OCTET_LENGTH`
    ///   equivalent.
    ///
    /// If a future `rusqlite`/`libsqlite3-sys` bump silently drops one of
    /// these compile flags, this test fails with a clear "no such function"
    /// error instead of the bitmap silently overclaiming forever.
    /// Every part of the Integrity Enhancement Facility this driver claims via
    /// `SQL_INTEGRITY = "Y"`, proved by making the bundled library reject a
    /// violation rather than by reading its documentation.
    ///
    /// Referential integrity is the fragile one. Plain SQLite defaults
    /// `PRAGMA foreign_keys` to off for backward compatibility, so
    /// [`SqliteBackend::connect`] turns it on explicitly rather than relying on
    /// the bundled library's `SQLITE_DEFAULT_FOREIGN_KEYS`.
    ///
    /// This goes through `connect` rather than opening a `rusqlite` connection
    /// directly, because `connect` is where the guarantee lives. A raw
    /// connection would only re-test the dependency's build configuration,
    /// which is exactly what the driver stopped depending on.
    #[test]
    fn integrity_enhancement_facility_is_actually_enforced() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let sqlite_conn = SqliteBackend::connect(&params).expect("connect");
        let conn = sqlite_conn.conn.lock().expect("lock");

        let fk_on: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            fk_on, 1,
            "SqliteBackend::connect did not enable foreign keys, so \
             SQL_INTEGRITY = \"Y\" is a false claim"
        );

        conn.execute_batch(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY);
             CREATE TABLE child (pid INTEGER REFERENCES parent(id) ON DELETE CASCADE);
             CREATE TABLE con (v INTEGER CHECK (v > 0), u INTEGER UNIQUE, n INTEGER NOT NULL DEFAULT 7);",
        )
        .unwrap();

        for (feature, sql) in [
            ("FOREIGN KEY", "INSERT INTO child VALUES (999)"),
            ("CHECK", "INSERT INTO con (v, n) VALUES (-1, 1)"),
            ("NOT NULL", "INSERT INTO con (v, n) VALUES (1, NULL)"),
            (
                "UNIQUE",
                "INSERT INTO con (u, n) VALUES (1, 1); INSERT INTO con (u, n) VALUES (1, 2)",
            ),
        ] {
            assert!(
                conn.execute_batch(sql).is_err(),
                "{feature} is not enforced, so SQL_INTEGRITY = \"Y\" overstates\n  {sql}"
            );
        }

        // DEFAULT, and a referential action rather than mere rejection.
        conn.execute_batch("INSERT INTO con (v, u) VALUES (1, 42)")
            .expect("DEFAULT should supply the NOT NULL column");
        conn.execute_batch("INSERT INTO parent VALUES (1); INSERT INTO child VALUES (1);")
            .unwrap();
        conn.execute_batch("DELETE FROM parent WHERE id = 1")
            .unwrap();
        let orphans: i64 = conn
            .query_row("SELECT count(*) FROM child", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "ON DELETE CASCADE did not cascade");
    }

    /// The `SQL_MAX_*` values that SQLite can be asked for come from the
    /// connection, not from a constant.
    ///
    /// Asserted by *changing* the limit and watching the reported value follow.
    /// Checking it merely equals SQLite's default would pass just as well
    /// against a hardcoded 2000, which is the thing this is meant to rule out.
    #[test]
    fn max_limits_are_read_from_the_connection() {
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let sqlite_conn = SqliteBackend::connect(&params).expect("connect");

        let column_limited = [
            InfoType::MaxColumnsInSelect,
            InfoType::MaxColumnsInTable,
            InfoType::MaxColumnsInGroupBy,
            InfoType::MaxColumnsInOrderBy,
            InfoType::MaxColumnsInIndex,
        ];

        // Default: whatever the bundled library carries, but never core's 0.
        for info_type in column_limited {
            match get_info(&sqlite_conn, info_type) {
                Ok(InfoValue::U16(v)) => assert!(
                    v > 0,
                    "{info_type:?} reported 0; the connection limit was not read"
                ),
                other => panic!("{info_type:?} unexpected: {other:?}"),
            }
        }

        // Lower SQLITE_LIMIT_COLUMN and every one of them must move with it.
        {
            let db = sqlite_conn.conn.lock().expect("lock");
            db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_COLUMN, 42)
                .expect("set limit");
        }
        for info_type in column_limited {
            assert_eq!(
                get_info(&sqlite_conn, info_type).expect("info"),
                InfoValue::U16(42),
                "{info_type:?} did not follow SQLITE_LIMIT_COLUMN"
            );
        }

        // The two SQLUINTEGER limits, same argument.
        for (info_type, limit) in [
            (
                InfoType::MaxStatementLen,
                rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH,
            ),
            (
                InfoType::MaxRowSize,
                rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
            ),
        ] {
            {
                let db = sqlite_conn.conn.lock().expect("lock");
                db.set_limit(limit, 4096).expect("set limit");
            }
            assert_eq!(
                get_info(&sqlite_conn, info_type).expect("info"),
                InfoValue::U32(4096),
                "{info_type:?} did not follow its sqlite3_limit"
            );
        }

        // Not claimed: SQLite's 64-table join cap has no sqlite3_limit, so this
        // stays core's 0 rather than a transcribed constant.
        assert_eq!(
            get_info(&sqlite_conn, InfoType::MaxTablesInSelect).expect("info"),
            InfoValue::U16(0),
            "SQL_MAX_TABLES_IN_SELECT has no limit to read and should stay 0"
        );
    }

    /// A maximum name length for a namespace this driver says does not exist
    /// is a bound on nothing. Kept in step with the two hooks rather than
    /// pinned to 0, so it stays right if either ever flips.
    #[test]
    fn catalog_and_schema_name_lengths_follow_their_support_hooks() {
        let conn = test_connection();
        let max_catalog = sqlite_get_info(Some(&conn), InfoType::MaxCatalogNameLen).expect("info");
        let max_schema = sqlite_get_info(Some(&conn), InfoType::MaxSchemaNameLen).expect("info");

        if SqliteBackend::supports_catalogs(&conn) {
            assert_ne!(max_catalog, InfoValue::U16(0));
        } else {
            assert_eq!(
                max_catalog,
                InfoValue::U16(0),
                "SQL_MAX_CATALOG_NAME_LEN bounds a name that cannot exist"
            );
        }
        if SqliteBackend::supports_schemas(&conn) {
            assert_ne!(max_schema, InfoValue::U16(0));
        } else {
            assert_eq!(
                max_schema,
                InfoValue::U16(0),
                "SQL_MAX_SCHEMA_NAME_LEN bounds a name that cannot exist"
            );
        }
    }

    /// Every `SQL_SUBQUERIES` bit this driver claims, proved by preparing the
    /// subquery form it describes, and the one it does not claim, proved by
    /// the bundled library rejecting it.
    ///
    /// `SQL_SQ_QUANTIFIED` is the point. Claiming it while
    /// `SQL_SQL92_PREDICATES` denies `SQL_SP_QUANTIFIED_COMPARISON` would
    /// advertise and deny the same capability across two info types, and a BI
    /// tool reading `SQL_SUBQUERIES` would push down `< ALL` and get a syntax
    /// error.
    #[test]
    fn subqueries_are_each_live_probed() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (a INTEGER, b INTEGER); CREATE TABLE u (b INTEGER);")
            .unwrap();

        for (bit, sql) in [
            (
                SQL_SQ_COMPARISON,
                "SELECT * FROM t WHERE a < (SELECT max(b) FROM u)",
            ),
            (
                SQL_SQ_EXISTS,
                "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u)",
            ),
            (SQL_SQ_IN, "SELECT * FROM t WHERE a IN (SELECT b FROM u)"),
            (
                SQL_SQ_CORRELATED_SUBQUERIES,
                "SELECT * FROM t WHERE a IN (SELECT b FROM u WHERE u.b = t.b)",
            ),
        ] {
            assert!(
                SQLITE_SUBQUERIES & bit == bit,
                "probe listed for unclaimed bit {bit:#x}"
            );
            conn.prepare(sql).unwrap_or_else(|e| {
                panic!("SQL_SQ bit {bit:#x} claimed but SQLite rejected it: {e}\n  {sql}")
            });
        }

        assert_eq!(
            SQLITE_SUBQUERIES & SQL_SQ_QUANTIFIED,
            0,
            "SQL_SQ_QUANTIFIED must not be claimed while SQL_SQL92_PREDICATES \
             denies SQL_SP_QUANTIFIED_COMPARISON"
        );
        for sql in [
            "SELECT * FROM t WHERE a < ALL (SELECT b FROM u)",
            "SELECT * FROM t WHERE a < ANY (SELECT b FROM u)",
            "SELECT * FROM t WHERE a < SOME (SELECT b FROM u)",
        ] {
            assert!(
                conn.prepare(sql).is_err(),
                "SQLite now parses a quantified comparison, so SQL_SQ_QUANTIFIED \
                 and SQL_SP_QUANTIFIED_COMPARISON should both be claimed\n  {sql}"
            );
        }
    }

    /// The hook returns SQLite's **raw** keyword list, and core turns it into
    /// `SQL_KEYWORDS` by subtracting the ODBC reserved words.
    ///
    /// Both halves are asserted, because each can fail independently: a raw
    /// list missing SQLite's own words, or a wiring mistake that leaves core
    /// filtering something else. Properties rather than a fixed string, since
    /// a `rusqlite` bump may legitimately add a keyword, and pinning the value
    /// would turn that into a failure.
    #[test]
    fn keywords_hook_feeds_sql_keywords_with_odbc_words_removed() {
        let hook_conn = test_connection();
        let raw = SqliteBackend::keywords(&hook_conn);
        assert!(!raw.is_empty(), "SQLite reserves words of its own");

        // Raw means unfiltered: ODBC's words are still in here, because
        // removing them is core's job and doing it twice would be the
        // duplication this hook exists to avoid.
        assert!(
            raw.iter().any(|k| k.eq_ignore_ascii_case("SELECT")),
            "the raw list should still contain SELECT; filtering is core's"
        );
        for expected in ["AUTOINCREMENT", "PRAGMA", "VACUUM", "GLOB", "REGEXP"] {
            assert!(
                raw.iter().any(|k| k.eq_ignore_ascii_case(expected)),
                "{expected} is a SQLite keyword but the hook did not report it"
            );
        }

        // And what an application actually receives, through the real path.
        let params = ConnectParams::parse("Database=:memory:").unwrap();
        let conn = SqliteBackend::connect(&params).expect("connect");
        let value = match get_info_raw(&conn, SQL_KEYWORDS) {
            Some(Ok(InfoValue::String(s))) => s,
            other => panic!("SQL_KEYWORDS unexpected: {other:?}"),
        };
        let listed: Vec<&str> = value.split(',').filter(|s| !s.is_empty()).collect();

        for reserved in ["SELECT", "FROM", "WHERE", "PRIMARY", "TABLE"] {
            assert!(
                !listed.contains(&reserved),
                "{reserved} is ODBC-reserved and must not survive into SQL_KEYWORDS"
            );
        }
        for expected in ["AUTOINCREMENT", "PRAGMA", "VACUUM", "GLOB", "REGEXP"] {
            assert!(
                listed.contains(&expected),
                "{expected} should survive the ODBC subtraction"
            );
        }
        assert!(
            listed.len() < raw.len(),
            "nothing was subtracted, so the ODBC filter did not run"
        );
    }

    /// SQLite's `GROUP BY` is unrelated to the select list, which is what
    /// `SQL_GB_NO_RELATION` means and what rules out the SQL-92 entry level.
    ///
    /// The spec: "a SQL-92 Entry level-conformant driver will always return the
    /// SQL_GB_GROUP_BY_EQUALS_SELECT option as supported." SQLite does the
    /// opposite in both directions, so `SqliteBackend::sql_conformance` claims
    /// no level rather than one this contradicts.
    #[test]
    fn group_by_is_unrelated_to_the_select_list() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE gb (a INTEGER, b TEXT);
             INSERT INTO gb VALUES (1, 'x'), (1, 'y'), (2, 'z');",
        )
        .unwrap();

        // A non-aggregated column absent from GROUP BY: rejected by anything
        // stricter than SQL_GB_NO_RELATION.
        conn.prepare("SELECT a, b, count(*) FROM gb GROUP BY a")
            .expect("SQLite accepts a bare non-aggregated column");

        // And the converse: a GROUP BY column absent from the select list.
        conn.prepare("SELECT count(*) FROM gb GROUP BY b")
            .expect("SQLite accepts a GROUP BY column absent from the select list");

        let hook_conn = test_connection();
        assert_eq!(SqliteBackend::group_by(&hook_conn), SQL_GB_NO_RELATION);
        assert_eq!(
            SqliteBackend::sql_conformance(&hook_conn),
            0,
            "SQL_GB_NO_RELATION rules out the SQL-92 entry level"
        );
    }

    /// The five catalog info types and the two schema info types must agree
    /// with each other. Without this test nothing ties the two groups
    /// together, and `SQL_CATALOG_NAME`, `SQL_CATALOG_LOCATION` and
    /// `SQL_CATALOG_USAGE` can say catalogs do not exist while
    /// `SQL_CATALOG_TERM` and `SQL_CATALOG_NAME_SEPARATOR` name one.
    ///
    /// Asserts the spec's rule, not the current values, so it keeps holding if
    /// [`SqliteBackend::supports_catalogs`] or
    /// [`SqliteBackend::supports_schemas`] ever flips.
    #[test]
    fn catalog_and_schema_info_types_agree_with_each_other() {
        let conn = test_connection();
        let get = |t: InfoType| sqlite_get_info(Some(&conn), t).expect("info type answered");

        let catalogs_supported = matches!(
            get(InfoType::CatalogName),
            InfoValue::String(ref s) if s == "Y"
        );
        assert_eq!(
            catalogs_supported,
            SqliteBackend::supports_catalogs(&conn),
            "SQL_CATALOG_NAME must follow Backend::supports_catalogs"
        );

        if catalogs_supported {
            assert_ne!(get(InfoType::CatalogTerm), InfoValue::String(String::new()));
            assert_ne!(get(InfoType::CatalogLocation), InfoValue::U16(0));
        } else {
            // Spec: "An empty string is returned if catalogs are not supported
            // by the data source" (SQL_CATALOG_TERM, SQL_CATALOG_NAME_SEPARATOR);
            // "A value of 0 is returned if catalogs are not supported"
            // (SQL_CATALOG_LOCATION, SQL_CATALOG_USAGE).
            assert_eq!(
                get(InfoType::CatalogTerm),
                InfoValue::String(String::new()),
                "SQL_CATALOG_TERM must be empty when catalogs are unsupported"
            );
            assert_eq!(
                get(InfoType::CatalogNameSeparator),
                InfoValue::String(String::new()),
                "SQL_CATALOG_NAME_SEPARATOR must be empty when catalogs are unsupported"
            );
            assert_eq!(
                get(InfoType::CatalogLocation),
                InfoValue::U16(0),
                "SQL_CATALOG_LOCATION must be 0 when catalogs are unsupported"
            );
            assert_eq!(
                get(InfoType::CatalogUsage),
                InfoValue::U32(0),
                "SQL_CATALOG_USAGE must be 0 when catalogs are unsupported"
            );
        }

        if SqliteBackend::supports_schemas(&conn) {
            assert_ne!(get(InfoType::SchemaTerm), InfoValue::String(String::new()));
        } else {
            assert_eq!(
                get(InfoType::SchemaTerm),
                InfoValue::String(String::new()),
                "SQL_SCHEMA_TERM must be empty when schemas are unsupported"
            );
            assert_eq!(
                get(InfoType::SchemaUsage),
                InfoValue::U32(0),
                "SQL_SCHEMA_USAGE must be 0 when schemas are unsupported"
            );
        }
    }

    /// `SQL_TXN_CAPABLE` is measured rather than assumed, because the four
    /// non-`NONE` values differ only in what DDL does inside a transaction and
    /// nothing about the constant's name says which one SQLite is.
    ///
    /// The spec separates them by observable effect: `SQL_TC_DML` means DDL
    /// "cause[s] an error", `SQL_TC_DDL_COMMIT` that it commits the
    /// transaction, `SQL_TC_DDL_IGNORE` that it is ignored, and `SQL_TC_ALL`
    /// that DML and DDL are supported "in any order". So the probe runs a
    /// `CREATE TABLE` between two inserts and rolls back, which tells all four
    /// apart at once: no error rules out `SQL_TC_DML`, the inserts
    /// disappearing rules out `SQL_TC_DDL_COMMIT`, and the created table
    /// disappearing rules out `SQL_TC_DDL_IGNORE`.
    #[test]
    fn transaction_capability_is_live_probed() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();

        conn.execute_batch("BEGIN").unwrap();
        conn.execute_batch("INSERT INTO t VALUES (2)").unwrap();
        conn.execute_batch("CREATE TABLE mid (x TEXT)")
            .expect("DDL inside a transaction must not error, which is what SQL_TC_DML claims");
        conn.execute_batch("INSERT INTO t VALUES (3)").unwrap();
        conn.execute_batch("ROLLBACK").unwrap();

        let rows: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            rows, 1,
            "the rows around the DDL survived a ROLLBACK, so the DDL committed \
             the transaction and this is SQL_TC_DDL_COMMIT"
        );

        let mid: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'mid'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            mid, 0,
            "the table created inside the transaction survived a ROLLBACK, so the \
             DDL was not transactional and this is SQL_TC_DDL_IGNORE"
        );

        let reported = SqliteBackend::txn_capable(&test_connection());
        assert_eq!(
            reported, SQL_TC_ALL as u16,
            "SQLite runs DDL and DML in a transaction in any order, which is SQL_TC_ALL"
        );
    }

    /// `SQL_DEFAULT_TXN_ISOLATION` must name a level that
    /// `SQL_TXN_ISOLATION_OPTION` actually offers, and this driver offers
    /// exactly one.
    ///
    /// SQLite is serializable and has no way to be anything else here: READ
    /// COMMITTED and REPEATABLE READ are not SQLite concepts, and READ
    /// UNCOMMITTED needs shared-cache mode, which `SqliteBackend::connect`
    /// never enables.
    ///
    /// This matters more than an unused info value usually would, because
    /// nothing applies what an application sets: `SQL_ATTR_TXN_ISOLATION` is
    /// stored on the connection and read back unchanged, never pushed to
    /// SQLite. Advertising a level therefore promises something no code path
    /// delivers.
    #[test]
    fn transaction_isolation_offers_only_the_level_sqlite_implements() {
        let conn = test_connection();
        let supported = match sqlite_get_info(Some(&conn), InfoType::TransactionIsolationProtocol) {
            Ok(InfoValue::U32(v)) => v,
            other => panic!("unexpected shape: {other:?}"),
        };
        let default = match sqlite_get_info(Some(&conn), InfoType::DefaultTxnIsolation) {
            Ok(InfoValue::U32(v)) => v,
            other => panic!("unexpected shape: {other:?}"),
        };

        assert_eq!(
            supported, SQL_TXN_SERIALIZABLE,
            "SQLite is serializable and offers no other level reachable from this driver"
        );
        assert!(
            supported & default == default,
            "SQL_DEFAULT_TXN_ISOLATION ({default:#x}) is not in \
             SQL_TXN_ISOLATION_OPTION ({supported:#x})"
        );
        for absent in [
            SQL_TXN_READ_UNCOMMITTED,
            SQL_TXN_READ_COMMITTED,
            SQL_TXN_REPEATABLE_READ,
        ] {
            assert!(
                supported & absent == 0,
                "isolation level {absent:#x} advertised, but SQLite cannot provide it"
            );
        }
    }

    /// Every `SQL_AT_*` bit this driver claims, proved by running the
    /// `ALTER TABLE` it describes, and every bit it does *not* claim, proved
    /// by the bundled library rejecting that syntax.
    ///
    /// The negative half is the point. A bitmap that only checks what it claims
    /// can overclaim forever; these assertions fail the moment SQLite gains a
    /// clause the bitmap still denies, which is the cheapest possible reminder
    /// to widen it.
    #[test]
    fn alter_table_capabilities_are_each_live_probed() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER)").unwrap();

        // Claimed: each must be accepted.
        for (bit, sql) in [
            (SQL_AT_ADD_COLUMN_SINGLE, "ALTER TABLE t ADD COLUMN c1 TEXT"),
            (
                SQL_AT_ADD_COLUMN_DEFAULT,
                "ALTER TABLE t ADD COLUMN c2 TEXT DEFAULT 'x'",
            ),
            (
                SQL_AT_ADD_COLUMN_COLLATION,
                "ALTER TABLE t ADD COLUMN c3 TEXT COLLATE NOCASE",
            ),
            (
                SQL_AT_ADD_TABLE_CONSTRAINT | SQL_AT_CONSTRAINT_NAME_DEFINITION,
                "ALTER TABLE t ADD CONSTRAINT ck CHECK (id > 0)",
            ),
        ] {
            assert!(
                SQLITE_ALTER_TABLE & bit == bit,
                "probe listed for unclaimed bit {bit:#x}"
            );
            conn.execute_batch(sql).unwrap_or_else(|e| {
                panic!("SQL_AT bit {bit:#x} claimed but SQLite rejected it: {e}\n  {sql}")
            });
        }

        // ADD CONSTRAINT must produce a real table constraint, not a column
        // that merely parses. Without this, `ADD CONSTRAINT ck CHECK (...)`
        // could be read as a column named CONSTRAINT and the bit would be a
        // lie that still passes the acceptance probe above.
        let schema: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name = 't'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            schema.contains("CONSTRAINT ck CHECK"),
            "SQL_AT_ADD_TABLE_CONSTRAINT claimed, but the stored schema shows no \
             named table constraint: {schema}"
        );

        // Not claimed: each must be rejected. ALTER COLUMN is not SQLite
        // grammar at all; the CASCADE and RESTRICT qualifiers are not accepted
        // on either DROP form, which is why those four bits stay off even
        // though SQLite drops both columns and constraints.
        for (bit, sql) in [
            (
                SQL_AT_SET_COLUMN_DEFAULT,
                "ALTER TABLE t ALTER COLUMN c1 SET DEFAULT 'y'",
            ),
            (
                SQL_AT_DROP_COLUMN_DEFAULT,
                "ALTER TABLE t ALTER COLUMN c2 DROP DEFAULT",
            ),
            (
                SQL_AT_DROP_COLUMN_CASCADE,
                "ALTER TABLE t DROP COLUMN c1 CASCADE",
            ),
            (
                SQL_AT_DROP_COLUMN_RESTRICT,
                "ALTER TABLE t DROP COLUMN c1 RESTRICT",
            ),
            (
                SQL_AT_DROP_TABLE_CONSTRAINT_CASCADE,
                "ALTER TABLE t DROP CONSTRAINT ck CASCADE",
            ),
            (
                SQL_AT_DROP_TABLE_CONSTRAINT_RESTRICT,
                "ALTER TABLE t DROP CONSTRAINT ck RESTRICT",
            ),
        ] {
            assert!(
                SQLITE_ALTER_TABLE & bit == 0,
                "negative probe listed for claimed bit {bit:#x}"
            );
            assert!(
                conn.execute_batch(sql).is_err(),
                "SQL_AT bit {bit:#x} is not claimed, but SQLite accepted it; \
                 SQLITE_ALTER_TABLE now understates and should be widened\n  {sql}"
            );
        }

        // ADD CONSTRAINT only takes CHECK. The ODBC bit cannot express that
        // narrowing, so it is recorded here instead.
        assert!(
            conn.execute_batch("ALTER TABLE t ADD CONSTRAINT uq UNIQUE (id)")
                .is_err(),
            "SQLite gained ADD CONSTRAINT ... UNIQUE; the doc comment on \
             SQLITE_ALTER_TABLE says only CHECK is accepted and needs updating"
        );

        // Supported by SQLite but unrepresentable: neither unqualified form has
        // a SQL_AT_* bit, so both are absent by necessity rather than because
        // SQLite lacks them. Asserted so that is not mistaken for an oversight.
        conn.execute_batch("ALTER TABLE t DROP CONSTRAINT ck")
            .expect("SQLite supports unqualified DROP CONSTRAINT");
        conn.execute_batch("ALTER TABLE t DROP COLUMN c1")
            .expect("SQLite supports unqualified DROP COLUMN (3.35.0+)");
    }

    /// Every `SQL_OJ_*` bit this driver claims, proved by running the join it
    /// describes rather than by reading release notes. `RIGHT` and `FULL`
    /// arrived in SQLite 3.39.0; if a `rusqlite`/`libsqlite3-sys` downgrade
    /// ever took the bundled library below that, this fails with a parse error
    /// instead of the bitmap overclaiming forever.
    ///
    /// A `SQL_OUTER_JOIN_CAPABILITIES` of 0 would say SQLite supports no outer
    /// joins at all, while this driver's own `SQL_OUTER_JOINS` says "Y".
    #[test]
    fn outer_join_capabilities_are_each_live_probed() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE l (id INTEGER, v TEXT);
             CREATE TABLE r (id INTEGER, w TEXT);
             CREATE TABLE m (id INTEGER, x TEXT);
             INSERT INTO l VALUES (1, 'a'), (2, 'b');
             INSERT INTO r VALUES (2, 'B'), (3, 'C');
             -- m.id = 2 so it meets the row r contributes to the outer join;
             -- the SQL_OJ_INNER probe below needs a real match, not just a
             -- statement SQLite is willing to parse.
             INSERT INTO m VALUES (2, 'y'), (3, 'z');",
        )
        .unwrap();

        // SQL_OJ_LEFT / SQL_OJ_RIGHT / SQL_OJ_FULL: the three join forms.
        for (bit, sql) in [
            (
                SQL_OJ_LEFT,
                "SELECT COUNT(*) FROM l LEFT OUTER JOIN r ON l.id = r.id",
            ),
            (
                SQL_OJ_RIGHT,
                "SELECT COUNT(*) FROM l RIGHT OUTER JOIN r ON l.id = r.id",
            ),
            (
                SQL_OJ_FULL,
                "SELECT COUNT(*) FROM l FULL OUTER JOIN r ON l.id = r.id",
            ),
        ] {
            let n: i64 = conn
                .query_row(sql, [], |row| row.get(0))
                .unwrap_or_else(|e| {
                    panic!("SQL_OJ bit {bit:#x} claimed but the join failed: {e}\n  {sql}")
                });
            assert!(n > 0, "SQL_OJ bit {bit:#x}: {sql} returned no rows");
        }

        // SQL_OJ_NESTED: an outer join whose operand is itself an outer join.
        let nested: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (l LEFT OUTER JOIN r ON l.id = r.id) \
                 LEFT OUTER JOIN m ON r.id = m.id",
                [],
                |row| row.get(0),
            )
            .expect("SQL_OJ_NESTED claimed but a nested outer join failed");
        assert!(nested > 0, "SQL_OJ_NESTED probe returned no rows");

        // SQL_OJ_NOT_ORDERED: the ON-clause column order need not follow the
        // table order in the FROM clause.
        let not_ordered: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM l LEFT OUTER JOIN r ON r.id = l.id",
                [],
                |row| row.get(0),
            )
            .expect("SQL_OJ_NOT_ORDERED claimed but a reversed ON clause failed");
        assert!(not_ordered > 0, "SQL_OJ_NOT_ORDERED probe returned no rows");

        // SQL_OJ_INNER: the inner table of an outer join may also be used in
        // an inner join.
        let inner: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM l LEFT OUTER JOIN r ON l.id = r.id \
                 INNER JOIN m ON m.id = r.id",
                [],
                |row| row.get(0),
            )
            .expect("SQL_OJ_INNER claimed but mixing an inner join in failed");
        assert!(inner > 0, "SQL_OJ_INNER probe returned no rows");

        // SQL_OJ_ALL_COMPARISON_OPS: the ON clause takes any comparison
        // operator, not just equality.
        let any_op: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM l LEFT OUTER JOIN r ON l.id < r.id",
                [],
                |row| row.get(0),
            )
            .expect("SQL_OJ_ALL_COMPARISON_OPS claimed but a non-equality ON failed");
        assert!(
            any_op > 0,
            "SQL_OJ_ALL_COMPARISON_OPS probe returned no rows"
        );
    }

    #[test]
    fn live_sqlite_supports_sign_soundex_and_octet_length() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let sign: i64 = conn
            .query_row("SELECT sign(-5)", [], |row| row.get(0))
            .expect("sign() should be available (core function, not gated by SQLITE_ENABLE_MATH_FUNCTIONS)");
        assert_eq!(sign, -1, "sign(-5) should be -1");

        let soundex: String = conn
            .query_row("SELECT soundex('Robert')", [], |row| row.get(0))
            .expect("soundex() should be available (this build enables SQLITE_SOUNDEX)");
        assert_eq!(soundex, "R163", "soundex('Robert') should be R163");

        let octet_length: i64 = conn
            .query_row("SELECT octet_length('abc')", [], |row| row.get(0))
            .expect("octet_length() should be available");
        assert_eq!(octet_length, 3, "octet_length('abc') should be 3");
    }

    #[test]
    fn every_reportable_type_has_a_type_info_row() {
        // Every type SQLColumns can report (e.g. SQL_BIGINT for an INTEGER
        // column) must have a matching SQLGetTypeInfo row; otherwise the
        // advertised type is absent from the type list entirely.
        //
        // This iterates `SQLITE_DECLARED_TYPE_ALIASES` rather than a
        // hand-copied list of declared type strings, so it cannot miss a
        // *new* alias added to `sqlite_type_to_sql_data_type`'s mapping that
        // yields a `SqlDataType` with no row: it is the same table
        // `sqlite_type_to_sql_data_type` looks up (not a second transcription
        // of it), so a new alias added there is automatically exercised here
        // too.
        for (decl, expected_ty) in crate::type_conversion::SQLITE_DECLARED_TYPE_ALIASES {
            let reported = crate::type_conversion::sqlite_type_to_sql_data_type(decl);
            assert_eq!(
                reported, *expected_ty,
                "SQLITE_DECLARED_TYPE_ALIASES entry for {decl:?} does not match what \
                 sqlite_type_to_sql_data_type actually returns for it"
            );
            assert!(
                SQLITE_TYPE_INFO
                    .iter()
                    .any(|row| row.data_type() == reported),
                "declared type {decl:?} is reported as {reported:?}, \
                 which has no SQLGetTypeInfo row"
            );
        }

        // Residual gap: `SQLITE_DECLARED_TYPE_ALIASES` only covers spellings
        // this driver recognises explicitly. A declared type outside that
        // table falls back to `sqlite_affinity`'s five substring rules
        // instead (see that function's doc comment); its only possible
        // outputs are already covered by the loop above, so this pins a few
        // concrete affinity-fallback inputs rather than re-deriving that
        // closed output set.
        for decl in [
            "MADE UP TYPE",
            "",
            "UNSIGNED BIG INT",
            "NATIVE CHARACTER(70)",
        ] {
            let reported = crate::type_conversion::sqlite_type_to_sql_data_type(decl);
            assert!(
                SQLITE_TYPE_INFO
                    .iter()
                    .any(|row| row.data_type() == reported),
                "declared type {decl:?} is reported as {reported:?}, \
                 which has no SQLGetTypeInfo row"
            );
        }
    }

    #[test]
    fn sqlite_bare_type_name_rejects_the_ambiguous_ansi_varchar_data_type() {
        // `SqlDataType::VARCHAR` (the ANSI code, 12) has two
        // `SQLITE_TYPE_INFO` rows ("TEXT" and "VARCHAR") and no single right
        // answer. `sqlite_type_to_sql_data_type` never actually produces this
        // value (only `EXT_W_VARCHAR`, see the WVARCHAR row's comment), so
        // this case is unreachable in practice; pin here that it fails safe
        // (empty string) rather than silently picking whichever row table
        // order happens to put first.
        assert_eq!(sqlite_bare_type_name(SqlDataType::VARCHAR), "");
    }

    #[test]
    fn sqlite_bare_type_name_matches_a_type_info_row() {
        // The invariant this task establishes: SQL_DESC_TYPE_NAME
        // (`execute.rs`) and SQLColumns.TYPE_NAME (`metadata.rs`) both call
        // `sqlite_bare_type_name`, so pin here that its result always
        // matches a row's TYPE_NAME for that same DATA_TYPE, for every
        // declared-type alias this driver recognises, including ones whose
        // own spelling (e.g. "INT8", "DOUBLE PRECISION") differs from the
        // canonical `SQLGetTypeInfo` name it must resolve to ("BIGINT", "REAL").
        for (decl, _) in crate::type_conversion::SQLITE_DECLARED_TYPE_ALIASES {
            let sql_type = crate::type_conversion::sqlite_type_to_sql_data_type(decl);
            let name = sqlite_bare_type_name(sql_type);
            assert!(
                SQLITE_TYPE_INFO
                    .iter()
                    .any(|row| row.type_name() == name && row.data_type() == sql_type),
                "sqlite_bare_type_name({sql_type:?}) (for declared type {decl:?}) returned \
                 {name:?}, which is not a matching SQLGetTypeInfo row"
            );
        }
    }

    #[test]
    fn every_type_info_row_is_reachable_via_sqlite_bare_type_name() {
        // Inverse of `every_reportable_type_has_a_type_info_row` above: that
        // test guards that every declared-type alias maps to *some* row;
        // this one guards the opposite direction: that every
        // `SQLITE_TYPE_INFO` row's TYPE_NAME can actually be *produced* by
        // `sqlite_bare_type_name` for some `SqlDataType`, not merely
        // advertised in the catalog. A row that fails this check is a
        // half-truth: an application enumerating SQLGetTypeInfo sees a type
        // advertised that no real column can ever be reported under.
        //
        // Exceptions: "TEXT" and "VARCHAR" both share
        // DATA_TYPE=SqlDataType::VARCHAR (the ANSI code, 12), kept purely
        // for Windows DM/pyodbc ANSI compatibility (see their own row
        // comments in `SQLITE_TYPE_INFO` above).
        // `sqlite_type_to_sql_data_type` never actually returns the ANSI
        // code for any declared type (only `EXT_W_VARCHAR`, see the
        // WVARCHAR row), and `sqlite_bare_type_name` deliberately refuses to
        // guess which of the two ambiguous rows is "correct" for that
        // DATA_TYPE (see
        // `sqlite_bare_type_name_rejects_the_ambiguous_ansi_varchar_data_type`
        // below), so neither name is ever produced by the function.
        const DM_COMPAT_ONLY: &[&str] = &["TEXT", "VARCHAR"];

        for row in SQLITE_TYPE_INFO.iter() {
            if DM_COMPAT_ONLY.contains(&row.type_name()) {
                continue;
            }
            let produced = sqlite_bare_type_name(row.data_type());
            assert_eq!(
                produced,
                row.type_name(),
                "SQLITE_TYPE_INFO row {:?} (DATA_TYPE={:?}) is not reachable via \
                 sqlite_bare_type_name (got {produced:?} instead); no real column can \
                 ever be reported under this TYPE_NAME",
                row.type_name(),
                row.data_type()
            );
        }
    }

    #[test]
    fn type_info_rows_have_unique_data_types_per_name() {
        let mut seen = std::collections::HashSet::new();
        for row in SQLITE_TYPE_INFO.iter() {
            assert!(
                seen.insert(row.type_name()),
                "duplicate type_name in SQLITE_TYPE_INFO: {}",
                row.type_name()
            );
        }
    }

    #[test]
    fn type_info_column_size_matches_default_precision() {
        // Consistency requirement: a row's COLUMN_SIZE must agree with what
        // SQLColumns/SQLDescribeCol report for an undeclared column of that
        // type (`default_precision_for_type`), or a driver could advertise
        // one max length in SQLGetTypeInfo and a different one everywhere
        // else (the same category of defect as a type missing from the type
        // list, just for size instead of presence).
        //
        // This assertion is not masking a real possible divergence: both
        // sides of the TIME/TIMESTAMP comparison below
        // read the exact same `MAX_FRACTIONAL_SECONDS_PRECISION` constant,
        // by design (see that constant's doc comment): SQLite has no
        // schema-declarable temporal scale for a column to differ by, so
        // "the data source's maximum" and "an undeclared column's default"
        // are the same number *by construction*, not by coincidence. This
        // test therefore only guards against the two call sites drifting to
        // read *different* constants; it cannot catch the constant itself
        // being wrong for SQLite's actual format, which is why
        // `sqlite_temporal_default_precision_matches_documented_iso8601_format`
        // below pins the concrete expected numbers instead.
        let declared = [
            "INTEGER",
            "BIGINT",
            "SMALLINT",
            "TINYINT",
            "BOOLEAN",
            "REAL",
            "BLOB",
            "DECIMAL",
            "TEXT",
            "DATE",
            "TIME",
            "TIMESTAMP",
        ];
        for decl in declared {
            let sql_type = crate::type_conversion::sqlite_type_to_sql_data_type(decl);
            let expected =
                i32::try_from(crate::type_conversion::default_precision_for_type(sql_type))
                    .unwrap_or(i32::MAX);
            let row = SQLITE_TYPE_INFO
                .iter()
                .find(|row| row.data_type() == sql_type)
                .unwrap_or_else(|| panic!("no SQLGetTypeInfo row for {sql_type:?} ({decl:?})"));
            assert_eq!(
                row.column_size(),
                expected,
                "column_size for {decl:?} ({sql_type:?}) row {:?} does not match \
                 default_precision_for_type",
                row.type_name()
            );
        }
    }

    #[test]
    fn sqlite_temporal_default_precision_matches_documented_iso8601_format() {
        // Pins the concrete numbers independently of
        // `MAX_FRACTIONAL_SECONDS_PRECISION`'s value, so this fails if that
        // constant is ever 0 (or anything else) rather than only proving two
        // call sites agree with each other. 12 = 9 + 3 and
        // 23 = 20 + 3 per the ODBC "Column Size" appendix's TIME/TIMESTAMP
        // formulas, evaluated at SQLite's documented 3-fractional-digit
        // ISO-8601 format (`YYYY-MM-DD HH:MM:SS.SSS`, format 4/7 at
        // <https://www.sqlite.org/lang_datefunc.html>). See
        // `MAX_FRACTIONAL_SECONDS_PRECISION`'s doc comment in
        // `type_conversion.rs`.
        assert_eq!(
            crate::type_conversion::default_precision_for_type(SqlDataType::TIME),
            12,
            "TIME default precision should be 9 + 3 fractional digits (\"HH:MM:SS.SSS\")"
        );
        assert_eq!(
            crate::type_conversion::default_precision_for_type(SqlDataType::TIMESTAMP),
            23,
            "TIMESTAMP default precision should be 20 + 3 fractional digits \
             (\"YYYY-MM-DD HH:MM:SS.SSS\")"
        );
    }

    /// Core orders the result set (DATA_TYPE, preferred row, TYPE_NAME), so the
    /// declaration order here no longer matters. What does matter is that every
    /// DATA_TYPE shared by several rows names its closest match, rather than
    /// leaving the first row to the alphabet.
    #[test]
    fn every_shared_data_type_has_one_preferred_row() {
        let issues =
            stackable_odbc_core::conformance::type_info_preference_issues(&SQLITE_TYPE_INFO);
        assert!(
            issues.is_empty(),
            "SQLGetTypeInfo preference markers: {issues:#?}"
        );
    }

    /// The specific choice: `TEXT`, SQLite's own storage class, over its
    /// `VARCHAR` alias for SQL_VARCHAR.
    #[test]
    fn sql_varchar_prefers_text() {
        let preferred: Vec<&str> = SQLITE_TYPE_INFO
            .iter()
            .filter(|r| r.preferred())
            .map(|r| r.type_name())
            .collect();
        assert_eq!(preferred, vec!["TEXT"]);
    }

    /// Guards that SQL_DRIVER_VER is derived from the crate version rather
    /// than a hand-transcribed string that must be remembered whenever
    /// Cargo.toml changes.
    ///
    /// The macro's three components are cross-checked against the *full*
    /// `CARGO_PKG_VERSION` string rather than against the same three
    /// `CARGO_PKG_VERSION_*` variables the macro reads. Restating the macro's
    /// own expansion would assert nothing: it would pass even if the macro
    /// wired PATCH where MINOR belongs, because both sides would carry the
    /// same mistake. Going through the combined string catches exactly that.
    #[test]
    fn driver_version_tracks_the_crate_version() {
        let (major, minor, release) =
            stackable_odbc_core::types::parse_dotted_version(env!("CARGO_PKG_VERSION"))
                .expect("Cargo always supplies a parseable package version");
        assert_eq!(
            stackable_odbc_core::driver_version!(),
            stackable_odbc_core::types::format_odbc_version(major, minor, release)
        );
    }

    // --- SQLGetInfo capability bitmaps ---

    /// SQLite has every ODBC aggregate.
    #[test]
    fn aggregate_functions_covers_all_seven() {
        assert_eq!(
            SQLITE_AGGREGATE_FUNCTIONS,
            SQL_AF_AVG
                | SQL_AF_COUNT
                | SQL_AF_MAX
                | SQL_AF_MIN
                | SQL_AF_SUM
                | SQL_AF_DISTINCT
                | SQL_AF_ALL
        );
    }

    /// Quantified comparison is genuinely absent: `< ALL`, `< ANY` and
    /// `< SOME` all fail to prepare. SQLite's ALL/ANY are set quantifiers on
    /// compound selects, not comparison quantifiers. Claiming it would make a
    /// BI tool fold a predicate SQLite rejects.
    #[test]
    fn sql92_predicates_excludes_quantified_comparison_and_match() {
        assert_eq!(
            SQLITE_SQL92_PREDICATES,
            SQL_SP_EXISTS
                | SQL_SP_ISNOTNULL
                | SQL_SP_ISNULL
                | SQL_SP_LIKE
                | SQL_SP_IN
                | SQL_SP_BETWEEN
                | SQL_SP_COMPARISON
        );
        for absent in [
            SQL_SP_QUANTIFIED_COMPARISON,
            SQL_SP_MATCH_FULL,
            SQL_SP_MATCH_PARTIAL,
            SQL_SP_MATCH_UNIQUE_FULL,
            SQL_SP_MATCH_UNIQUE_PARTIAL,
            SQL_SP_OVERLAPS,
            SQL_SP_UNIQUE,
        ] {
            assert_eq!(SQLITE_SQL92_PREDICATES & absent, 0);
        }
    }

    /// RIGHT and FULL OUTER JOIN arrived in SQLite 3.39.0 and this build is
    /// 3.53.2, so both are claimed, verified by live probe rather than assumed.
    #[test]
    fn sql92_join_operators_includes_right_and_full_outer() {
        assert_eq!(
            SQLITE_SQL92_JOIN_OPERATORS,
            SQL_SRJO_CROSS_JOIN
                | SQL_SRJO_EXCEPT_JOIN
                | SQL_SRJO_FULL_OUTER_JOIN
                | SQL_SRJO_INNER_JOIN
                | SQL_SRJO_INTERSECT_JOIN
                | SQL_SRJO_LEFT_OUTER_JOIN
                | SQL_SRJO_NATURAL_JOIN
                | SQL_SRJO_RIGHT_OUTER_JOIN
        );
        assert_eq!(
            SQLITE_SQL92_JOIN_OPERATORS & SQL_SRJO_CORRESPONDING_CLAUSE,
            0
        );
        assert_eq!(SQLITE_SQL92_JOIN_OPERATORS & SQL_SRJO_UNION_JOIN, 0);
    }

    #[test]
    fn sql92_value_expressions_covers_all_four() {
        assert_eq!(
            SQLITE_SQL92_VALUE_EXPRESSIONS,
            SQL_SVE_CASE | SQL_SVE_CAST | SQL_SVE_COALESCE | SQL_SVE_NULLIF
        );
    }

    /// The bundled build compiles out SQLITE_ENABLE_MATH_FUNCTIONS, so the
    /// whole trig/log/power set is gone. `sign()` survives because it is a
    /// *core* function, not a math one, the one flag that would be wrong if
    /// inferred from "math functions are off".
    #[test]
    fn numeric_functions_is_only_the_core_three() {
        assert_eq!(
            SQLITE_NUMERIC_FUNCTIONS,
            SQL_FN_NUM_ABS | SQL_FN_NUM_SIGN | SQL_FN_NUM_ROUND
        );
        for absent in [
            SQL_FN_NUM_COS,
            SQL_FN_NUM_SQRT,
            SQL_FN_NUM_POWER,
            SQL_FN_NUM_LOG,
            SQL_FN_NUM_CEILING,
            SQL_FN_NUM_FLOOR,
            SQL_FN_NUM_TRUNCATE,
            SQL_FN_NUM_MOD,
            SQL_FN_NUM_RAND,
        ] {
            assert_eq!(SQLITE_NUMERIC_FUNCTIONS & absent, 0);
        }
    }

    /// LOCATE is excluded on purpose: `instr(haystack, needle)` reverses
    /// ODBC's `LOCATE(needle, haystack)`, so claiming it yields silently
    /// wrong answers rather than a clean failure.
    #[test]
    fn string_functions_excludes_reversed_locate() {
        assert_eq!(
            SQLITE_STRING_FUNCTIONS,
            SQL_FN_STR_CONCAT
                | SQL_FN_STR_LTRIM
                | SQL_FN_STR_LENGTH
                | SQL_FN_STR_LCASE
                | SQL_FN_STR_REPLACE
                | SQL_FN_STR_RTRIM
                | SQL_FN_STR_SUBSTRING
                | SQL_FN_STR_UCASE
                | SQL_FN_STR_ASCII
                | SQL_FN_STR_CHAR
                | SQL_FN_STR_SOUNDEX
                | SQL_FN_STR_OCTET_LENGTH
        );
        for absent in [
            SQL_FN_STR_LOCATE,
            SQL_FN_STR_LOCATE_2,
            SQL_FN_STR_LEFT,
            SQL_FN_STR_RIGHT,
            SQL_FN_STR_REPEAT,
            SQL_FN_STR_SPACE,
            SQL_FN_STR_INSERT,
            SQL_FN_STR_DIFFERENCE,
            SQL_FN_STR_CHAR_LENGTH,
            SQL_FN_STR_CHARACTER_LENGTH,
            SQL_FN_STR_BIT_LENGTH,
            SQL_FN_STR_POSITION,
        ] {
            assert_eq!(SQLITE_STRING_FUNCTIONS & absent, 0);
        }
    }

    /// SQLite has no user and no scalar database-name function.
    #[test]
    fn system_functions_is_only_ifnull() {
        assert_eq!(SQLITE_SYSTEM_FUNCTIONS, SQL_FN_SYS_IFNULL);
    }

    /// SQLite has no year()/month()/day(), only strftime() with a format
    /// string, which is not an equivalent function. Only the current-date and
    /// current-time family is claimed.
    #[test]
    fn timedate_functions_is_only_the_current_datetime_family() {
        assert_eq!(
            SQLITE_TIMEDATE_FUNCTIONS,
            SQL_FN_TD_NOW
                | SQL_FN_TD_CURDATE
                | SQL_FN_TD_CURTIME
                | SQL_FN_TD_CURRENT_DATE
                | SQL_FN_TD_CURRENT_TIME
                | SQL_FN_TD_CURRENT_TIMESTAMP
        );
        for absent in [
            SQL_FN_TD_YEAR,
            SQL_FN_TD_MONTH,
            SQL_FN_TD_DAYOFMONTH,
            SQL_FN_TD_QUARTER,
            SQL_FN_TD_EXTRACT,
            SQL_FN_TD_TIMESTAMPADD,
            SQL_FN_TD_TIMESTAMPDIFF,
            SQL_FN_TD_DAYNAME,
            SQL_FN_TD_MONTHNAME,
        ] {
            assert_eq!(SQLITE_TIMEDATE_FUNCTIONS & absent, 0);
        }
    }

    #[test]
    fn get_functions_advertises_data_at_execution() {
        let f = get_functions();
        assert!(f.contains(&FunctionId::ParamData), "SQLParamData missing");
        assert!(f.contains(&FunctionId::PutData), "SQLPutData missing");
    }

    /// Nothing this driver ever claimed to support is absent from what core
    /// exports.
    ///
    /// The check that matters is this direction. `SQLGetFunctions` is what the
    /// Windows Driver Manager builds its dispatch table from, so claiming a
    /// function core does not export hands it a null pointer to call, whereas
    /// staying silent about one merely means the DM does not use it.
    ///
    /// `SUPPORTED_FUNCTIONS` is the hand-written list `get_functions` used to
    /// return. It is kept as the historical claim so this assertion has
    /// something to check; the live answer is `CORE_EXPORTED_FUNCTIONS`, which
    /// core pins against its own `forward_ffi!` arms.
    #[test]
    fn supported_functions_are_all_exported_by_core() {
        for id in SUPPORTED_FUNCTIONS {
            assert!(
                CORE_EXPORTED_FUNCTIONS.contains(id),
                "{id:?} was advertised but core exports no entry point for it"
            );
        }
    }

    #[test]
    fn get_functions_has_no_duplicates() {
        let f = get_functions();
        let ids: Vec<u16> = f.iter().map(|id| *id as u16).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ids.len(),
            "duplicate FunctionId in get_functions"
        );
    }
}
