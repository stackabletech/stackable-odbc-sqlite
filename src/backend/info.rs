//! `SQLGetInfo`, `SQLGetTypeInfo` and `SQLGetFunctions` support for the SQLite
//! backend: the `get_info` / `get_info_pre_connect` / `get_info_raw`
//! handlers, the exported-function bitmap, the static type-info rows mapping
//! SQLite's storage classes onto ODBC types, and the SQLite capability
//! bitmaps (`SQLITE_*`).

use stackable_odbc_core::backend::{Backend, common_get_info_raw, default_get_info};
use stackable_odbc_core::errors::OdbcError;
use stackable_odbc_core::function_id::FunctionId;
use stackable_odbc_core::types::{
    InfoType, InfoValue, MaxPrecision, MaxScale, Nullable, SQL_AF_ALL, SQL_AF_AVG, SQL_AF_COUNT,
    SQL_AF_DISTINCT, SQL_AF_MAX, SQL_AF_MIN, SQL_AF_SUM, SQL_AGGREGATE_FUNCTIONS,
    SQL_AT_ADD_COLUMN_COLLATION, SQL_AT_ADD_COLUMN_DEFAULT, SQL_AT_ADD_COLUMN_SINGLE,
    SQL_AT_ADD_TABLE_CONSTRAINT, SQL_AT_CONSTRAINT_NAME_DEFINITION, SQL_CL_START, SQL_CODE_DATE,
    SQL_CODE_TIME, SQL_CODE_TIMESTAMP, SQL_CU_DML_STATEMENTS, SQL_CU_INDEX_DEFINITION,
    SQL_CU_TABLE_DEFINITION, SQL_FN_NUM_ABS, SQL_FN_NUM_ROUND, SQL_FN_NUM_SIGN, SQL_FN_STR_ASCII,
    SQL_FN_STR_CHAR, SQL_FN_STR_CONCAT, SQL_FN_STR_LCASE, SQL_FN_STR_LENGTH, SQL_FN_STR_LTRIM,
    SQL_FN_STR_OCTET_LENGTH, SQL_FN_STR_REPLACE, SQL_FN_STR_RTRIM, SQL_FN_STR_SOUNDEX,
    SQL_FN_STR_SUBSTRING, SQL_FN_STR_UCASE, SQL_FN_SYS_IFNULL, SQL_FN_TD_CURDATE,
    SQL_FN_TD_CURRENT_DATE, SQL_FN_TD_CURRENT_TIME, SQL_FN_TD_CURRENT_TIMESTAMP, SQL_FN_TD_CURTIME,
    SQL_FN_TD_NOW, SQL_GD_ANY_COLUMN, SQL_GD_ANY_ORDER, SQL_GD_BOUND, SQL_IC_MIXED,
    SQL_LIKE_ESCAPE_CLAUSE, SQL_NC_LOW, SQL_NUMERIC_FUNCTIONS, SQL_OJ_ALL_COMPARISON_OPS,
    SQL_OJ_FULL, SQL_OJ_INNER, SQL_OJ_LEFT, SQL_OJ_NESTED, SQL_OJ_NOT_ORDERED, SQL_OJ_RIGHT,
    SQL_OUTER_JOINS, SQL_SEARCHABLE, SQL_SP_BETWEEN, SQL_SP_COMPARISON, SQL_SP_EXISTS, SQL_SP_IN,
    SQL_SP_ISNOTNULL, SQL_SP_ISNULL, SQL_SP_LIKE, SQL_SQL92_PREDICATES,
    SQL_SQL92_RELATIONAL_JOIN_OPERATORS, SQL_SQL92_VALUE_EXPRESSIONS, SQL_SRJO_CROSS_JOIN,
    SQL_SRJO_EXCEPT_JOIN, SQL_SRJO_FULL_OUTER_JOIN, SQL_SRJO_INNER_JOIN, SQL_SRJO_INTERSECT_JOIN,
    SQL_SRJO_LEFT_OUTER_JOIN, SQL_SRJO_NATURAL_JOIN, SQL_SRJO_RIGHT_OUTER_JOIN,
    SQL_STRING_FUNCTIONS, SQL_SU_DML_STATEMENTS, SQL_SU_INDEX_DEFINITION, SQL_SU_TABLE_DEFINITION,
    SQL_SVE_CASE, SQL_SVE_CAST, SQL_SVE_COALESCE, SQL_SVE_NULLIF, SQL_SYSTEM_FUNCTIONS, SQL_TC_DML,
    SQL_TIMEDATE_FUNCTIONS, SQL_TXN_READ_COMMITTED, SQL_TXN_READ_UNCOMMITTED,
    SQL_TXN_REPEATABLE_READ, SQL_TXN_SERIALIZABLE, SqlDataType, TypeInfoRow, catalog_column_size,
    format_odbc_version, parse_dotted_version,
};

use super::SqliteBackend;
use super::SqliteConnection;
use super::SqliteError;
use crate::type_conversion::{
    BLOB_DEFAULT_COLUMN_SIZE, DECIMAL_DEFAULT_COLUMN_SIZE, MAX_FRACTIONAL_SECONDS_PRECISION,
    VARCHAR_DEFAULT_COLUMN_SIZE,
};

/// Whether this driver exposes ODBC catalogs. It does not: `metadata::tables`
/// reports `TABLE_CAT` as NULL for every row, and a `catalog = "%"` enumeration
/// returns an empty result set.
///
/// The `SQLGetInfo` specification defines five separate info types in terms of
/// this single fact — `SQL_CATALOG_NAME`, `SQL_CATALOG_TERM`,
/// `SQL_CATALOG_NAME_SEPARATOR`, `SQL_CATALOG_LOCATION` and
/// `SQL_CATALOG_USAGE` — so all five are derived from it here rather than
/// answered independently.
///
/// That independence is what went wrong before: the driver answered
/// `SQL_CATALOG_NAME`, `SQL_CATALOG_LOCATION` and `SQL_CATALOG_USAGE` itself
/// and let `SQL_CATALOG_TERM` and `SQL_CATALOG_NAME_SEPARATOR` fall through to
/// `stackable-odbc-core`'s defaults, which name a catalog and a separator. An
/// application was told catalogs do not exist and given their name in the same
/// breath. The spec is explicit for both: "An empty string is returned if
/// catalogs are not supported by the data source."
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetinfo-function>
const SUPPORTS_CATALOGS: bool = false;

/// Whether this driver exposes ODBC schemas. It does not: a `schema = "%"`
/// enumeration returns an empty result set and `TABLE_SCHEM` is always NULL.
///
/// Derives `SQL_SCHEMA_TERM` and `SQL_SCHEMA_USAGE`, for the same reason
/// [`SUPPORTS_CATALOGS`] derives its five. The spec: "An empty string is
/// returned if schemas are not supported by the data source."
const SUPPORTS_SCHEMAS: bool = false;

/// ODBC function IDs for functions this driver implements.
/// Used by `SQLGetFunctions` to report supported capabilities.
/// Reference: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetfunctions-function>
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
// Rows are sorted by DATA_TYPE ascending (as signed i16, so ODBC extension
// types with negative codes sort first), then by TYPE_NAME ascending within
// an equal DATA_TYPE, per the SQLGetTypeInfo spec's "ordered by DATA_TYPE and
// then ... TYPE_NAME" requirement. This invariant is asserted directly by
// `type_info_rows_sorted_by_data_type_then_type_name` below; keep new rows
// in the correct sorted position rather than appending them.
static SQLITE_TYPE_INFO: &[TypeInfoRow] = &[
    // WVARCHAR — sqlite_type_to_sql_data_type maps VARCHAR/CHAR/CHARACTER/
    // NCHAR/NVARCHAR/VARYING CHARACTER/NATIVE CHARACTER/TEXT/CLOB here, and
    // it is the CHAR/CLOB/TEXT-affinity fallback too. This is the row that
    // actually satisfies the invariant for every text-affinity declared
    // type; the SQL_VARCHAR/SQL_CHAR rows further down this list
    // exist only for Windows DM/pyodbc ANSI compatibility.
    TypeInfoRow {
        type_name: "WVARCHAR",
        data_type: SqlDataType::EXT_W_VARCHAR,
        column_size: catalog_column_size(
            SqlDataType::EXT_W_VARCHAR,
            MaxPrecision(VARCHAR_DEFAULT_COLUMN_SIZE),
            MaxScale(0),
        ),
        literal_prefix: Some("'"),
        literal_suffix: Some("'"),
        create_params: Some("max length"),
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: true,
        searchable: SQL_SEARCHABLE,
        unsigned: None,
        fixed_prec_scale: false,
        auto_unique_value: None,
        local_type_name: None,
        minimum_scale: None,
        maximum_scale: None,
        sql_data_type: SqlDataType::EXT_W_VARCHAR.0,
        sql_datetime_sub: None,
        num_prec_radix: None,
        interval_precision: None,
    },
    // WCHAR — Unicode counterpart to the CHAR row further down this list,
    // included for symmetry per the Windows DM checklist even though
    // sqlite_type_to_sql_data_type itself never produces EXT_W_CHAR (declared
    // CHAR(n) collapses into the WVARCHAR affinity above, matching real
    // SQLite semantics where CHAR(n) is not length-limited).
    TypeInfoRow {
        type_name: "WCHAR",
        data_type: SqlDataType::EXT_W_CHAR,
        column_size: catalog_column_size(
            SqlDataType::EXT_W_CHAR,
            MaxPrecision(WCHAR_COLUMN_SIZE_ROW),
            MaxScale(0),
        ),
        literal_prefix: Some("'"),
        literal_suffix: Some("'"),
        create_params: Some("length"),
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: true,
        searchable: SQL_SEARCHABLE,
        unsigned: None,
        fixed_prec_scale: false,
        auto_unique_value: None,
        local_type_name: None,
        minimum_scale: None,
        maximum_scale: None,
        sql_data_type: SqlDataType::EXT_W_CHAR.0,
        sql_datetime_sub: None,
        num_prec_radix: None,
        interval_precision: None,
    },
    // BIT — sqlite_type_to_sql_data_type maps BOOLEAN/BOOL here.
    TypeInfoRow {
        type_name: "BIT",
        data_type: SqlDataType::EXT_BIT,
        column_size: catalog_column_size(SqlDataType::EXT_BIT, MaxPrecision(0), MaxScale(0)),
        literal_prefix: None,
        literal_suffix: None,
        create_params: None,
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: false,
        searchable: SQL_SEARCHABLE,
        unsigned: None,
        fixed_prec_scale: false,
        auto_unique_value: None,
        local_type_name: None,
        minimum_scale: None,
        maximum_scale: None,
        sql_data_type: SqlDataType::EXT_BIT.0,
        sql_datetime_sub: None,
        num_prec_radix: None,
        interval_precision: None,
    },
    // TINYINT — sqlite_type_to_sql_data_type maps TINYINT here.
    TypeInfoRow {
        type_name: "TINYINT",
        data_type: SqlDataType::EXT_TINY_INT,
        column_size: catalog_column_size(SqlDataType::EXT_TINY_INT, MaxPrecision(0), MaxScale(0)),
        literal_prefix: None,
        literal_suffix: None,
        create_params: None,
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: false,
        searchable: SQL_SEARCHABLE,
        unsigned: Some(false),
        fixed_prec_scale: false,
        auto_unique_value: Some(false),
        local_type_name: None,
        minimum_scale: Some(0),
        maximum_scale: Some(0),
        sql_data_type: SqlDataType::EXT_TINY_INT.0,
        sql_datetime_sub: None,
        num_prec_radix: Some(10),
        interval_precision: None,
    },
    // BIGINT — sqlite_type_to_sql_data_type maps INTEGER/INT/BIGINT/INT8 here
    // (and the "INT"-substring affinity fallback), since SQLite integers are
    // always 64-bit storage. This is the row an INTEGER column's reported
    // type (SQL_BIGINT) actually resolves to.
    TypeInfoRow {
        type_name: "BIGINT",
        data_type: SqlDataType::EXT_BIG_INT,
        column_size: catalog_column_size(SqlDataType::EXT_BIG_INT, MaxPrecision(0), MaxScale(0)),
        literal_prefix: None,
        literal_suffix: None,
        create_params: None,
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: false,
        searchable: SQL_SEARCHABLE,
        unsigned: Some(false),
        fixed_prec_scale: false,
        auto_unique_value: Some(false),
        local_type_name: None,
        minimum_scale: Some(0),
        maximum_scale: Some(0),
        sql_data_type: SqlDataType::EXT_BIG_INT.0,
        sql_datetime_sub: None,
        num_prec_radix: Some(10),
        interval_precision: None,
    },
    TypeInfoRow {
        type_name: "BLOB",
        data_type: SqlDataType::EXT_VAR_BINARY,
        column_size: catalog_column_size(
            SqlDataType::EXT_VAR_BINARY,
            MaxPrecision(BLOB_DEFAULT_COLUMN_SIZE),
            MaxScale(0),
        ),
        literal_prefix: Some("X'"),
        literal_suffix: Some("'"),
        create_params: Some("max length"),
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: false,
        searchable: SQL_SEARCHABLE,
        unsigned: None,
        fixed_prec_scale: false,
        auto_unique_value: None,
        local_type_name: None,
        minimum_scale: None,
        maximum_scale: None,
        sql_data_type: SqlDataType::EXT_VAR_BINARY.0,
        sql_datetime_sub: None,
        num_prec_radix: None,
        interval_precision: None,
    },
    // SQL_CHAR (1) — ANSI alias. See the SQL_VARCHAR comment further down
    // this list; same rationale for why this is a distinct row from the
    // WCHAR row above.
    TypeInfoRow {
        type_name: "CHAR",
        data_type: SqlDataType::CHAR,
        column_size: catalog_column_size(
            SqlDataType::CHAR,
            MaxPrecision(CHAR_COLUMN_SIZE_ROW),
            MaxScale(0),
        ),
        literal_prefix: Some("'"),
        literal_suffix: Some("'"),
        create_params: Some("length"),
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: true,
        searchable: SQL_SEARCHABLE,
        unsigned: None,
        fixed_prec_scale: false,
        auto_unique_value: None,
        local_type_name: None,
        minimum_scale: None,
        maximum_scale: None,
        sql_data_type: SqlDataType::CHAR.0,
        sql_datetime_sub: None,
        num_prec_radix: None,
        interval_precision: None,
    },
    // DECIMAL — sqlite_type_to_sql_data_type maps DECIMAL/NUMERIC here, and
    // it is also the NUMERIC-affinity fallback for any declared type that
    // SQLite's own affinity rules do not otherwise classify.
    TypeInfoRow {
        type_name: "DECIMAL",
        data_type: SqlDataType::DECIMAL,
        column_size: catalog_column_size(
            SqlDataType::DECIMAL,
            MaxPrecision(DECIMAL_DEFAULT_COLUMN_SIZE),
            MaxScale(DECIMAL_MAX_SCALE),
        ),
        literal_prefix: None,
        literal_suffix: None,
        create_params: Some("precision,scale"),
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: false,
        searchable: SQL_SEARCHABLE,
        unsigned: Some(false),
        fixed_prec_scale: false,
        auto_unique_value: Some(false),
        local_type_name: None,
        minimum_scale: Some(0),
        maximum_scale: Some(DECIMAL_MAX_SCALE),
        sql_data_type: SqlDataType::DECIMAL.0,
        sql_datetime_sub: None,
        num_prec_radix: Some(10),
        interval_precision: None,
    },
    TypeInfoRow {
        type_name: "INTEGER",
        data_type: SqlDataType::INTEGER,
        column_size: catalog_column_size(SqlDataType::INTEGER, MaxPrecision(0), MaxScale(0)),
        literal_prefix: None,
        literal_suffix: None,
        create_params: None,
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: false,
        searchable: SQL_SEARCHABLE,
        unsigned: Some(false),
        fixed_prec_scale: false,
        auto_unique_value: Some(false),
        local_type_name: None,
        minimum_scale: Some(0),
        maximum_scale: Some(0),
        sql_data_type: SqlDataType::INTEGER.0,
        sql_datetime_sub: None,
        num_prec_radix: Some(10),
        interval_precision: None,
    },
    // SMALLINT — sqlite_type_to_sql_data_type maps SMALLINT/INT2 here.
    TypeInfoRow {
        type_name: "SMALLINT",
        data_type: SqlDataType::SMALLINT,
        column_size: catalog_column_size(SqlDataType::SMALLINT, MaxPrecision(0), MaxScale(0)),
        literal_prefix: None,
        literal_suffix: None,
        create_params: None,
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: false,
        searchable: SQL_SEARCHABLE,
        unsigned: Some(false),
        fixed_prec_scale: false,
        auto_unique_value: Some(false),
        local_type_name: None,
        minimum_scale: Some(0),
        maximum_scale: Some(0),
        sql_data_type: SqlDataType::SMALLINT.0,
        sql_datetime_sub: None,
        num_prec_radix: Some(10),
        interval_precision: None,
    },
    TypeInfoRow {
        type_name: "REAL",
        data_type: SqlDataType::DOUBLE,
        column_size: catalog_column_size(SqlDataType::DOUBLE, MaxPrecision(0), MaxScale(0)),
        literal_prefix: None,
        literal_suffix: None,
        create_params: None,
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: false,
        searchable: SQL_SEARCHABLE,
        unsigned: Some(false),
        fixed_prec_scale: false,
        auto_unique_value: None,
        local_type_name: None,
        minimum_scale: None,
        maximum_scale: None,
        sql_data_type: SqlDataType::DOUBLE.0,
        sql_datetime_sub: None,
        num_prec_radix: Some(2),
        interval_precision: None,
    },
    // TEXT — column_size matches VARCHAR_DEFAULT_COLUMN_SIZE (255), the
    // same default `default_precision_for_type` reports for both VARCHAR and
    // EXT_W_VARCHAR (see type_conversion.rs). This row and the VARCHAR row
    // immediately below both describe SQLite's single, unbounded TEXT
    // storage class under the shared ANSI DATA_TYPE=12, so they must report
    // the same size. 255 is the value the rest of the driver treats as
    // authoritative for this DATA_TYPE (`default_precision_for_type`, and the
    // WVARCHAR row below), so both rows use it.
    TypeInfoRow {
        type_name: "TEXT",
        data_type: SqlDataType::VARCHAR,
        column_size: catalog_column_size(
            SqlDataType::VARCHAR,
            MaxPrecision(VARCHAR_DEFAULT_COLUMN_SIZE),
            MaxScale(0),
        ),
        literal_prefix: Some("'"),
        literal_suffix: Some("'"),
        create_params: Some("max length"),
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: true,
        searchable: SQL_SEARCHABLE,
        unsigned: None,
        fixed_prec_scale: false,
        auto_unique_value: None,
        local_type_name: None,
        minimum_scale: None,
        maximum_scale: None,
        sql_data_type: SqlDataType::VARCHAR.0,
        sql_datetime_sub: None,
        num_prec_radix: None,
        interval_precision: None,
    },
    // SQL_VARCHAR (12) — ANSI alias needed for Windows DM / pyodbc type
    // conversion (AGENTS.md "Windows Driver Manager compatibility
    // checklist"). sqlite_type_to_sql_data_type never actually returns this
    // ANSI code (only EXT_W_VARCHAR, see the WVARCHAR row above); this row
    // exists purely so SQLGetTypeInfo(SQL_VARCHAR) finds a match. TYPE_NAME
    // differs from the TEXT row immediately above (same DATA_TYPE) because
    // SQLite itself treats VARCHAR as a recognised alias of TEXT, and the
    // spec explicitly allows multiple rows sharing a DATA_TYPE; column_size
    // matches the TEXT row above for the same reason (see that row's
    // comment).
    TypeInfoRow {
        type_name: "VARCHAR",
        data_type: SqlDataType::VARCHAR,
        column_size: catalog_column_size(
            SqlDataType::VARCHAR,
            MaxPrecision(VARCHAR_DEFAULT_COLUMN_SIZE),
            MaxScale(0),
        ),
        literal_prefix: Some("'"),
        literal_suffix: Some("'"),
        create_params: Some("max length"),
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: true,
        searchable: SQL_SEARCHABLE,
        unsigned: None,
        fixed_prec_scale: false,
        auto_unique_value: None,
        local_type_name: None,
        minimum_scale: None,
        maximum_scale: None,
        sql_data_type: SqlDataType::VARCHAR.0,
        sql_datetime_sub: None,
        num_prec_radix: None,
        interval_precision: None,
    },
    // DATE — sqlite_type_to_sql_data_type maps DATE here. SQLite has no DATE
    // literal syntax; a date value is just a quoted ISO-8601 string, hence
    // the plain quote prefix/suffix (matching the TEXT row's convention)
    // rather than a typed `DATE '...'` literal.
    // DATA_TYPE=91 (SQL_TYPE_DATE), SQL_DATA_TYPE=9 (SQL_DATETIME), SQL_DATETIME_SUB=1 (SQL_CODE_DATE)
    TypeInfoRow {
        type_name: "DATE",
        data_type: SqlDataType::DATE,
        column_size: catalog_column_size(SqlDataType::DATE, MaxPrecision(0), MaxScale(0)), // 'YYYY-MM-DD'
        literal_prefix: Some("'"),
        literal_suffix: Some("'"),
        create_params: None,
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: false,
        searchable: SQL_SEARCHABLE,
        unsigned: None,
        fixed_prec_scale: false,
        auto_unique_value: None,
        local_type_name: None,
        minimum_scale: None,
        maximum_scale: None,
        sql_data_type: SqlDataType::DATETIME.0,
        sql_datetime_sub: Some(SQL_CODE_DATE),
        num_prec_radix: None,
        interval_precision: None,
    },
    // TIME — sqlite_type_to_sql_data_type maps TIME here. SQLite stores time
    // values as plain "HH:MM:SS" text with no fractional-seconds field (see
    // column_value_to_rusqlite), so scale is fixed at 0.
    // DATA_TYPE=92 (SQL_TYPE_TIME), SQL_DATA_TYPE=9 (SQL_DATETIME), SQL_DATETIME_SUB=2 (SQL_CODE_TIME)
    TypeInfoRow {
        type_name: "TIME",
        data_type: SqlDataType::TIME,
        // 'HH:MM:SS': SQLite has no fractional-seconds capability to report
        // as a maximum (MAX_FRACTIONAL_SECONDS_PRECISION = 0), so this is
        // the plain (scale-0) form of the TIME formula.
        column_size: catalog_column_size(
            SqlDataType::TIME,
            MaxPrecision(0),
            MaxScale(MAX_FRACTIONAL_SECONDS_PRECISION),
        ),
        literal_prefix: Some("'"),
        literal_suffix: Some("'"),
        create_params: None,
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: false,
        searchable: SQL_SEARCHABLE,
        unsigned: None,
        fixed_prec_scale: false,
        auto_unique_value: None,
        local_type_name: None,
        minimum_scale: Some(0),
        maximum_scale: Some(MAX_FRACTIONAL_SECONDS_PRECISION),
        sql_data_type: SqlDataType::DATETIME.0,
        sql_datetime_sub: Some(SQL_CODE_TIME),
        num_prec_radix: None,
        interval_precision: None,
    },
    // TIMESTAMP — sqlite_type_to_sql_data_type maps DATETIME/TIMESTAMP here.
    // column_size intentionally excludes a fractional-seconds allowance: it
    // is computed via catalog_column_size at MAX_FRACTIONAL_SECONDS_PRECISION
    // (0), the same constant sqlite_declared_type_precision uses as the
    // fallback for an undeclared TIMESTAMP column (see the consistency test
    // below), so minimum/maximum scale are reported as fixed at 0 rather
    // than claiming precision the column size does not budget for.
    // DATA_TYPE=93 (SQL_TYPE_TIMESTAMP), SQL_DATA_TYPE=9 (SQL_DATETIME), SQL_DATETIME_SUB=3 (SQL_CODE_TIMESTAMP)
    TypeInfoRow {
        type_name: "TIMESTAMP",
        data_type: SqlDataType::TIMESTAMP,
        // 'YYYY-MM-DD HH:MM:SS': same no-fractional-capability rationale
        // as the TIME row above.
        column_size: catalog_column_size(
            SqlDataType::TIMESTAMP,
            MaxPrecision(0),
            MaxScale(MAX_FRACTIONAL_SECONDS_PRECISION),
        ),
        literal_prefix: Some("'"),
        literal_suffix: Some("'"),
        create_params: None,
        nullable: Nullable::SqlNullable as i16,
        case_sensitive: false,
        searchable: SQL_SEARCHABLE,
        unsigned: None,
        fixed_prec_scale: false,
        auto_unique_value: None,
        local_type_name: None,
        minimum_scale: Some(0),
        maximum_scale: Some(MAX_FRACTIONAL_SECONDS_PRECISION),
        sql_data_type: SqlDataType::DATETIME.0,
        sql_datetime_sub: Some(SQL_CODE_TIMESTAMP),
        num_prec_radix: None,
        interval_precision: None,
    },
];

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

/// All values here are connection-independent (driver-level constants).
/// Extracted so that both the connected and pre-connect paths can use it
/// without duplicating the match.
fn sqlite_get_info(info_type: InfoType) -> Result<InfoValue, SqliteError> {
    // Driver-specific overrides
    match info_type {
        InfoType::DriverName => return Ok(InfoValue::String("stackable-odbc-sqlite".into())),
        InfoType::DriverVer => {
            return Ok(InfoValue::String(stackable_odbc_core::driver_version!()));
        }
        InfoType::DbmsName => return Ok(InfoValue::String("SQLite".into())),
        InfoType::DbmsVer => {
            let raw = rusqlite::version();
            // The spec permits appending the data source's own version string
            // after the ##.##.#### prefix, which keeps SQLite's native
            // spelling visible to anyone reading the value by eye.
            return Ok(InfoValue::String(match parse_dotted_version(raw) {
                Some((major, minor, release)) => {
                    format!("{} ({raw})", format_odbc_version(major, minor, release))
                }
                None => {
                    tracing::warn!(
                        raw,
                        "could not parse the SQLite version; reporting it verbatim"
                    );
                    raw.to_string()
                }
            }));
        }
        // Catalogs and schemas: every value below is derived from
        // SUPPORTS_CATALOGS / SUPPORTS_SCHEMAS rather than restated, because
        // the SQLGetInfo spec defines each of them in terms of that one fact.
        InfoType::CatalogName => {
            return Ok(InfoValue::String(
                if SUPPORTS_CATALOGS { "Y" } else { "N" }.into(),
            ));
        }
        InfoType::CatalogTerm => {
            return Ok(InfoValue::String(
                if SUPPORTS_CATALOGS { "catalog" } else { "" }.into(),
            ));
        }
        InfoType::CatalogNameSeparator => {
            return Ok(InfoValue::String(
                if SUPPORTS_CATALOGS { "." } else { "" }.into(),
            ));
        }
        InfoType::CatalogLocation => {
            return Ok(InfoValue::U16(if SUPPORTS_CATALOGS {
                SQL_CL_START
            } else {
                0
            }));
        }
        InfoType::CatalogUsage => {
            return Ok(InfoValue::U32(if SUPPORTS_CATALOGS {
                SQL_CU_DML_STATEMENTS | SQL_CU_TABLE_DEFINITION | SQL_CU_INDEX_DEFINITION
            } else {
                0
            }));
        }
        InfoType::SchemaTerm => {
            return Ok(InfoValue::String(
                if SUPPORTS_SCHEMAS { "schema" } else { "" }.into(),
            ));
        }
        InfoType::SchemaUsage => {
            return Ok(InfoValue::U32(if SUPPORTS_SCHEMAS {
                SQL_SU_DML_STATEMENTS | SQL_SU_TABLE_DEFINITION | SQL_SU_INDEX_DEFINITION
            } else {
                0
            }));
        }
        // Every outer-join form SQLite implements, and every relaxation of
        // the ON clause the spec asks about. Core's default is 0, which
        // contradicted this driver's own SQL_OUTER_JOINS = "Y". Each bit is
        // exercised by `outer_join_capabilities_are_each_live_probed`.
        InfoType::AlterTable => return Ok(InfoValue::U32(SQLITE_ALTER_TABLE)),
        InfoType::OuterJoinCapabilities => {
            return Ok(InfoValue::U32(
                SQL_OJ_LEFT
                    | SQL_OJ_RIGHT
                    | SQL_OJ_FULL
                    | SQL_OJ_NESTED
                    | SQL_OJ_NOT_ORDERED
                    | SQL_OJ_INNER
                    | SQL_OJ_ALL_COMPARISON_OPS,
            ));
        }
        InfoType::IdentifierCase => return Ok(InfoValue::U16(SQL_IC_MIXED)),
        InfoType::NullCollation => return Ok(InfoValue::U16(SQL_NC_LOW)),
        InfoType::DefaultTxnIsolation => return Ok(InfoValue::U32(SQL_TXN_SERIALIZABLE)),
        InfoType::TransactionIsolationProtocol => {
            return Ok(InfoValue::U32(
                SQL_TXN_READ_UNCOMMITTED
                    | SQL_TXN_READ_COMMITTED
                    | SQL_TXN_REPEATABLE_READ
                    | SQL_TXN_SERIALIZABLE,
            ));
        }
        // SQL_TXN_CAPABLE is `An SQLUSMALLINT value` per the SQLGetInfo spec,
        // not SQLUINTEGER -- found by the info-type conformance test
        // (`stackable_odbc_core::conformance`). `SQL_TC_DML` is a small fixed constant
        // (1), so the narrowing `as u16` cannot lose information.
        InfoType::TransactionCapable => return Ok(InfoValue::U16(SQL_TC_DML as u16)),
        // SQL_GD_BLOCK is deliberately not claimed: it means SQLGetData can
        // be called for a row in a block cursor after a bulk fetch, but this
        // driver has no block cursors to speak of -- `SQLSetStmtAttrW`
        // (`stackable-odbc-core/src/ffi/stmt_attr.rs`) rejects any
        // SQL_ATTR_ROW_ARRAY_SIZE other than 1, substituting 1 back with
        // 01S02, so no application can ever get a multi-row rowset out of
        // this driver to begin with. SQL_GD_BOUND, by contrast, genuinely
        // holds: `sql_get_data` (`stackable-odbc-core/src/ffi/fetch.rs`) never checks
        // `stmt.bindings` before reading a column, so a column bound via
        // `SQLBindCol` can still be fetched again through `SQLGetData`.
        // Reporting the exact capability set (rather than a blanket 0x0F) is
        // what the Windows DM checklist in AGENTS.md requires.
        InfoType::GetDataExtensions => {
            return Ok(InfoValue::U32(
                SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER | SQL_GD_BOUND,
            ));
        }
        _ => {}
    }

    // Fall through to shared defaults
    default_get_info::<SqliteBackend>(info_type, &SqliteBackend::catalog_result_column_widths())
        .ok_or_else(|| SqliteError::NotImplemented {
            feature: format!("get_info({info_type:?})"),
        })
}

pub(super) fn get_info(
    _conn: &SqliteConnection,
    info_type: InfoType,
) -> Result<InfoValue, SqliteError> {
    sqlite_get_info(info_type)
}

pub(super) fn get_info_pre_connect(info_type: InfoType) -> Result<InfoValue, OdbcError> {
    sqlite_get_info(info_type).map_err(Into::into)
}

/// `SQL_AGGREGATE_FUNCTIONS` — SQLite has every ODBC aggregate, and accepts
/// both `DISTINCT` and `ALL` as set quantifiers.
/// <https://sqlite.org/lang_aggfunc.html>
pub(crate) const SQLITE_AGGREGATE_FUNCTIONS: u32 =
    SQL_AF_AVG | SQL_AF_COUNT | SQL_AF_MAX | SQL_AF_MIN | SQL_AF_SUM | SQL_AF_DISTINCT | SQL_AF_ALL;

/// `SQL_ALTER_TABLE` (86) — the `ALTER TABLE` clauses SQLite accepts, of those
/// the ODBC bitmap can express.
///
/// Every bit here was established by executing the clause against the bundled
/// library (3.53.2), not read off the documentation —
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
///   while SQLite accepts only `CHECK` here — `UNIQUE`, `PRIMARY KEY` and
///   `FOREIGN KEY` are still syntax errors.
/// - `SQL_AT_CONSTRAINT_NAME_DEFINITION`, since that `CONSTRAINT <name>` clause
///   is exactly what the bit describes.
///
/// Supported by SQLite but *unrepresentable*, so absent by necessity rather
/// than because SQLite lacks them: unqualified `DROP COLUMN` (3.35.0+) and
/// unqualified `DROP CONSTRAINT`, for which the bitmap offers only `CASCADE`
/// and `RESTRICT` variants — and SQLite rejects both keywords, so claiming
/// either would advertise a syntax an application would send and have refused.
/// `RENAME TO` and `RENAME COLUMN` have no `SQL_AT_*` bit at all.
///
/// Genuinely absent: `ALTER COLUMN ... SET DEFAULT` and
/// `ALTER COLUMN ... DROP DEFAULT` are not SQLite grammar.
///
/// Core previously defaulted this to 0, which said SQLite cannot alter a table
/// in any way.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetinfo-function>
/// SQLite: <https://www.sqlite.org/lang_altertable.html>
pub(crate) const SQLITE_ALTER_TABLE: u32 = SQL_AT_ADD_COLUMN_SINGLE
    | SQL_AT_ADD_COLUMN_DEFAULT
    | SQL_AT_ADD_COLUMN_COLLATION
    | SQL_AT_ADD_TABLE_CONSTRAINT
    | SQL_AT_CONSTRAINT_NAME_DEFINITION;

/// `SQL_SQL92_PREDICATES`.
///
/// Deliberately absent: quantified comparison (`< ALL` / `< ANY` / `< SOME`
/// all fail to prepare -- SQLite's `ALL`/`ANY` are set quantifiers on
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

/// `SQL_SQL92_VALUE_EXPRESSIONS` — all four present.
/// <https://sqlite.org/lang_expr.html>
pub(crate) const SQLITE_SQL92_VALUE_EXPRESSIONS: u32 =
    SQL_SVE_CASE | SQL_SVE_CAST | SQL_SVE_COALESCE | SQL_SVE_NULLIF;

/// `SQL_NUMERIC_FUNCTIONS` — only three.
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
/// function, and is integer-only -- `7.5 % 2` yields `1`) and `RAND`
/// (`random()` returns a signed 64-bit integer, not ODBC's float in `[0,1)`,
/// and takes no seed).
/// <https://sqlite.org/lang_corefunc.html>
pub(crate) const SQLITE_NUMERIC_FUNCTIONS: u32 =
    SQL_FN_NUM_ABS | SQL_FN_NUM_SIGN | SQL_FN_NUM_ROUND;

/// `SQL_STRING_FUNCTIONS` — SQLite equivalents, several under other names:
/// `LCASE` is `lower()`, `UCASE` is `upper()`, `SUBSTRING` is `substr()`,
/// `ASCII` is `unicode()`, `CHAR` is `char()`.
///
/// `SOUNDEX` is claimed because this build enables `SQLITE_SOUNDEX`, which is
/// **not** the SQLite default -- verified by probe (`soundex('Robert')` gives
/// `R163`). A future `rusqlite` bump could silently drop it, which is why
/// `tests::live_sqlite_supports_sign_soundex_and_octet_length` opens a real
/// in-memory connection and calls it (along with `sign()` and
/// `octet_length()`, the other two counter-intuitive entries in this
/// bitmap) rather than only asserting the constant against its own
/// definition.
///
/// Deliberately absent: `LOCATE` and `LOCATE_2`, because `instr(haystack,
/// needle)` reverses ODBC's `LOCATE(needle, haystack)` -- claiming it would
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

/// `SQL_SYSTEM_FUNCTIONS` — only `IFNULL`, which SQLite spells the same way.
///
/// SQLite has no user concept, so no `USERNAME`; and no scalar
/// database-name function, only the `pragma_database_list` table-valued
/// function, which is not an equivalent.
/// <https://sqlite.org/lang_corefunc.html>
pub(crate) const SQLITE_SYSTEM_FUNCTIONS: u32 = SQL_FN_SYS_IFNULL;

/// `SQL_TIMEDATE_FUNCTIONS` — only the current-date/time family.
///
/// `date()`, `time()` and `datetime()` take no arguments and return the
/// current value, so they are genuine equivalents of `CURDATE`, `CURTIME` and
/// `NOW`, and the three `CURRENT_*` keywords work directly.
///
/// Everything else is deliberately absent. SQLite has no `year()`,
/// `month()`, `day()`, `quarter()` or `extract()` -- only `strftime()` with a
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

pub(super) fn get_info_raw(
    _conn: &SqliteConnection,
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
    // `SqliteBackend::escape_dialect()` -- see `crate::escape_dialect`), so
    // `{fn ABS(x)}` becomes `ABS(x)` and succeeds, and the "under other
    // names" entries documented above are remapped (`UCASE`->`upper`,
    // `LCASE`->`lower`, `SUBSTRING`->`substr`, `ASCII`->`unicode`, plus
    // `NOW`->`datetime`, `CURDATE`->`date`, `CURTIME`->`time` from the
    // `SQL_TIMEDATE_FUNCTIONS` bitmap below). Names SQLite spells identically
    // to ODBC (`ABS`, `ROUND`, `CONCAT`, `LENGTH`, `IFNULL`, `CHAR`, ...) pass
    // through unchanged and already worked. Still deliberately untranslated:
    // `CURRENT_DATE`/`CURRENT_TIME`/`CURRENT_TIMESTAMP` -- SQLite treats these
    // as bare keywords (`SELECT CURRENT_DATE();` is a syntax error), and a
    // name-only remap cannot drop the trailing `()` the `{fn ...()}` escape
    // always includes; see the `crate::escape_dialect` module doc comment.
    // None of this is version-gated -- SQLite's version is fixed at compile
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
        _ => common_get_info_raw::<SqliteBackend>(info_type).map(Ok),
    }
}

pub(super) fn get_functions() -> &'static [FunctionId] {
    SUPPORTED_FUNCTIONS
}

pub(super) fn get_type_info() -> &'static [TypeInfoRow] {
    SQLITE_TYPE_INFO
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
        .find(|row| row.data_type == sql_type)
        .map(|row| row.type_name)
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
    /// value from `row.data_type` rather than repeating the table's own
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

        for row in SQLITE_TYPE_INFO {
            if !BACKEND_INDEPENDENT.contains(&row.data_type) {
                continue;
            }
            let expected = catalog_column_size(row.data_type, IGNORED_PRECISION, IGNORED_SCALE);
            assert_eq!(
                row.column_size, expected,
                "{} (DATA_TYPE {:?}): COLUMN_SIZE is {} but the \
                 backend-independent appendix formula for that DATA_TYPE \
                 gives {} — the row is built from a different SqlDataType \
                 than it reports",
                row.type_name, row.data_type, row.column_size, expected
            );
        }
    }
    use super::*;
    use stackable_odbc_core::types::{
        DEFAULT_IDENTIFIER_LEN, InfoType, InfoValue, SQL_AM_NONE, SQL_AT_DROP_COLUMN_CASCADE,
        SQL_AT_DROP_COLUMN_DEFAULT, SQL_AT_DROP_COLUMN_RESTRICT,
        SQL_AT_DROP_TABLE_CONSTRAINT_CASCADE, SQL_AT_DROP_TABLE_CONSTRAINT_RESTRICT,
        SQL_AT_SET_COLUMN_DEFAULT, SQL_CA1_NEXT, SQL_CB_PRESERVE, SQL_DRIVER_ODBC_VER_STRING,
        SQL_FN_CVT_CAST, SQL_FN_NUM_CEILING, SQL_FN_NUM_COS, SQL_FN_NUM_FLOOR, SQL_FN_NUM_LOG,
        SQL_FN_NUM_MOD, SQL_FN_NUM_POWER, SQL_FN_NUM_RAND, SQL_FN_NUM_SQRT, SQL_FN_NUM_TRUNCATE,
        SQL_FN_STR_BIT_LENGTH, SQL_FN_STR_CHAR_LENGTH, SQL_FN_STR_CHARACTER_LENGTH,
        SQL_FN_STR_DIFFERENCE, SQL_FN_STR_INSERT, SQL_FN_STR_LEFT, SQL_FN_STR_LOCATE,
        SQL_FN_STR_LOCATE_2, SQL_FN_STR_POSITION, SQL_FN_STR_REPEAT, SQL_FN_STR_RIGHT,
        SQL_FN_STR_SPACE, SQL_FN_TD_DAYNAME, SQL_FN_TD_DAYOFMONTH, SQL_FN_TD_EXTRACT,
        SQL_FN_TD_MONTH, SQL_FN_TD_MONTHNAME, SQL_FN_TD_QUARTER, SQL_FN_TD_TIMESTAMPADD,
        SQL_FN_TD_TIMESTAMPDIFF, SQL_FN_TD_YEAR, SQL_GB_NO_RELATION, SQL_GD_ANY_COLUMN,
        SQL_GD_ANY_ORDER, SQL_GD_BOUND, SQL_IC_MIXED, SQL_INSENSITIVE, SQL_MAX_CURSOR_NAME_LEN,
        SQL_NC_LOW, SQL_OIC_CORE, SQL_SC_SQL92_ENTRY, SQL_SO_FORWARD_ONLY, SQL_SP_MATCH_FULL,
        SQL_SP_MATCH_PARTIAL, SQL_SP_MATCH_UNIQUE_FULL, SQL_SP_MATCH_UNIQUE_PARTIAL,
        SQL_SP_OVERLAPS, SQL_SP_QUANTIFIED_COMPARISON, SQL_SP_UNIQUE, SQL_SQ_COMPARISON,
        SQL_SQ_CORRELATED_SUBQUERIES, SQL_SQ_EXISTS, SQL_SQ_IN, SQL_SQ_QUANTIFIED,
        SQL_SRJO_CORRESPONDING_CLAUSE, SQL_SRJO_UNION_JOIN, SQL_TC_DML, SQL_TXN_READ_COMMITTED,
        SQL_TXN_READ_UNCOMMITTED, SQL_TXN_REPEATABLE_READ, SQL_TXN_SERIALIZABLE, SQL_U_UNION,
        SQL_U_UNION_ALL,
    };

    enum Expected {
        Str(&'static str),
        U16(u16),
        U32(u32),
    }

    #[rustfmt::skip]
    const EXPECTED: &[(InfoType, Expected)] = &[
        // --- String values ---
        (InfoType::DriverName,                    Expected::Str("stackable-odbc-sqlite")),
        (InfoType::DbmsName,                      Expected::Str("SQLite")),
        (InfoType::DriverOdbcVer,                 Expected::Str(SQL_DRIVER_ODBC_VER_STRING)),
        (InfoType::SearchPatternEscape,            Expected::Str("\\")),
        (InfoType::IdentifierQuoteChar,            Expected::Str("\"")),
        // Empty, not "catalog": derived from SUPPORTS_CATALOGS. The spec
        // requires an empty string when catalogs are unsupported, which
        // SQL_CATALOG_NAME = "N" declares.
        (InfoType::CatalogTerm,                   Expected::Str("")),
        // Empty, not "schema": derived from SUPPORTS_SCHEMAS, same spec rule.
        (InfoType::SchemaTerm,                    Expected::Str("")),
        // Empty, not ".": derived from SUPPORTS_CATALOGS, same spec rule.
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
        (InfoType::Integrity,                     Expected::Str("N")),
        (InfoType::SpecialCharacters,             Expected::Str("")),
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
        (InfoType::MaxSchemaNameLen,              Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::MaxCatalogNameLen,             Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::MaxTableNameLen,               Expected::U16(DEFAULT_IDENTIFIER_LEN)),
        (InfoType::NullCollation,                 Expected::U16(SQL_NC_LOW)),
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
        // TransactionCapable is SQLUSMALLINT per spec, not SQLUINTEGER -- see
        // the matching comment on its arm in sqlite_get_info.
        (InfoType::TransactionCapable,            Expected::U16(SQL_TC_DML as u16)),
        // --- U32 values ---
        // CursorSensitivity is SQLUINTEGER per spec, not SQLUSMALLINT -- see
        // the matching comment in stackable-odbc-core's default_get_info.
        (InfoType::CursorSensitivity,             Expected::U32(SQL_INSENSITIVE as u32)),
        (InfoType::Subqueries,                    Expected::U32(SQL_SQ_COMPARISON | SQL_SQ_EXISTS | SQL_SQ_IN | SQL_SQ_QUANTIFIED | SQL_SQ_CORRELATED_SUBQUERIES)),
        (InfoType::UnionStatement,                Expected::U32(SQL_U_UNION | SQL_U_UNION_ALL)),
        (InfoType::DefaultTxnIsolation,           Expected::U32(SQL_TXN_SERIALIZABLE)),
        (InfoType::ScrollOptions,                 Expected::U32(SQL_SO_FORWARD_ONLY)),
        (InfoType::ConvertFunctions,              Expected::U32(SQL_FN_CVT_CAST)),
        (InfoType::TransactionIsolationProtocol,  Expected::U32(SQL_TXN_READ_UNCOMMITTED | SQL_TXN_READ_COMMITTED | SQL_TXN_REPEATABLE_READ | SQL_TXN_SERIALIZABLE)),
        (InfoType::AlterTable,                    Expected::U32(SQLITE_ALTER_TABLE)),
        (InfoType::MaxIndexSize,                  Expected::U32(0)),
        (InfoType::MaxRowSize,                    Expected::U32(0)),
        (InfoType::MaxStatementLen,               Expected::U32(0)),
        // Not 0: SQLite implements every outer-join form the spec asks
        // about. Core's default of 0 contradicted SQL_OUTER_JOINS = "Y".
        (InfoType::OuterJoinCapabilities,         Expected::U32(
            SQL_OJ_LEFT | SQL_OJ_RIGHT | SQL_OJ_FULL | SQL_OJ_NESTED
                | SQL_OJ_NOT_ORDERED | SQL_OJ_INNER | SQL_OJ_ALL_COMPARISON_OPS)),
        (InfoType::SqlConformance,                Expected::U32(SQL_SC_SQL92_ENTRY)),
        (InfoType::OdbcInterfaceConformance,      Expected::U32(SQL_OIC_CORE)),
        (InfoType::AsyncMode,                     Expected::U32(SQL_AM_NONE)),
        (InfoType::AsyncDbcFunctions,             Expected::U32(0)),
        (InfoType::SchemaUsage,                   Expected::U32(0)),
        (InfoType::CatalogUsage,                  Expected::U32(0)),
        (InfoType::GetDataExtensions,             Expected::U32(SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER | SQL_GD_BOUND)),
        (InfoType::DynamicCursorAttributes1,      Expected::U32(0)),
        (InfoType::DynamicCursorAttributes2,      Expected::U32(0)),
        (InfoType::ForwardOnlyCursorAttributes1,  Expected::U32(SQL_CA1_NEXT)),
        (InfoType::ForwardOnlyCursorAttributes2,  Expected::U32(0)),
        (InfoType::KeysetCursorAttributes1,       Expected::U32(0)),
        (InfoType::KeysetCursorAttributes2,       Expected::U32(0)),
        (InfoType::StaticCursorAttributes1,       Expected::U32(0)),
        (InfoType::StaticCursorAttributes2,       Expected::U32(0)),
    ];

    #[test]
    fn get_info_snapshot() {
        for (info_type, expected) in EXPECTED {
            let actual = sqlite_get_info(*info_type)
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

    #[test]
    fn dbms_ver_is_well_formed() {
        let InfoValue::String(s) = sqlite_get_info(InfoType::DbmsVer).unwrap() else {
            panic!("expected String for DbmsVer");
        };
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

    /// SQL_DRIVER_VER is derived from Cargo.toml, so it cannot be asserted
    /// against a literal without reintroducing drift between the two.
    /// Assert the spec's shape instead.
    #[test]
    fn driver_ver_is_well_formed() {
        let InfoValue::String(v) = sqlite_get_info(InfoType::DriverVer).unwrap() else {
            panic!("expected String for DriverVer");
        };
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
    /// - `sign()` -- survives `SQLITE_ENABLE_MATH_FUNCTIONS` being compiled
    ///   out of the `bundled` feature because it lives on the *core*
    ///   functions page, not the math one.
    /// - `soundex()` -- exists only because this build enables the
    ///   non-default `SQLITE_SOUNDEX` compile flag.
    /// - `octet_length()` -- claimed as the `SQL_FN_STR_OCTET_LENGTH`
    ///   equivalent.
    ///
    /// If a future `rusqlite`/`libsqlite3-sys` bump silently drops one of
    /// these compile flags, this test fails with a clear "no such function"
    /// error instead of the bitmap silently overclaiming forever.
    /// The five catalog info types and the two schema info types must agree
    /// with each other. This is the test the previous arrangement lacked:
    /// `SQL_CATALOG_NAME`, `SQL_CATALOG_LOCATION` and `SQL_CATALOG_USAGE` said
    /// catalogs do not exist while `SQL_CATALOG_TERM` and
    /// `SQL_CATALOG_NAME_SEPARATOR` fell through to core's defaults and named
    /// one, and nothing tied the two groups together.
    ///
    /// Asserts the spec's rule, not the current values, so it keeps holding if
    /// [`SUPPORTS_CATALOGS`] or [`SUPPORTS_SCHEMAS`] ever flips.
    #[test]
    fn catalog_and_schema_info_types_agree_with_each_other() {
        let get = |t: InfoType| sqlite_get_info(t).expect("info type answered");

        let catalogs_supported = matches!(
            get(InfoType::CatalogName),
            InfoValue::String(ref s) if s == "Y"
        );
        assert_eq!(
            catalogs_supported, SUPPORTS_CATALOGS,
            "SQL_CATALOG_NAME must follow SUPPORTS_CATALOGS"
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

        if SUPPORTS_SCHEMAS {
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

    /// Every `SQL_AT_*` bit this driver claims, proved by running the
    /// `ALTER TABLE` it describes — and every bit it does *not* claim, proved
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
                "SQL_AT bit {bit:#x} is not claimed, but SQLite accepted it -- \
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
    /// Core's default for `SQL_OUTER_JOIN_CAPABILITIES` is 0, which said
    /// SQLite supports no outer joins at all while this driver's own
    /// `SQL_OUTER_JOINS` said "Y".
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

        // SQL_OJ_LEFT / SQL_OJ_RIGHT / SQL_OJ_FULL — the three join forms.
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

        // SQL_OJ_NESTED — an outer join whose operand is itself an outer join.
        let nested: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (l LEFT OUTER JOIN r ON l.id = r.id) \
                 LEFT OUTER JOIN m ON r.id = m.id",
                [],
                |row| row.get(0),
            )
            .expect("SQL_OJ_NESTED claimed but a nested outer join failed");
        assert!(nested > 0, "SQL_OJ_NESTED probe returned no rows");

        // SQL_OJ_NOT_ORDERED — the ON-clause column order need not follow the
        // table order in the FROM clause.
        let not_ordered: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM l LEFT OUTER JOIN r ON r.id = l.id",
                [],
                |row| row.get(0),
            )
            .expect("SQL_OJ_NOT_ORDERED claimed but a reversed ON clause failed");
        assert!(not_ordered > 0, "SQL_OJ_NOT_ORDERED probe returned no rows");

        // SQL_OJ_INNER — the inner table of an outer join may also be used in
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

        // SQL_OJ_ALL_COMPARISON_OPS — the ON clause takes any comparison
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
                SQLITE_TYPE_INFO.iter().any(|row| row.data_type == reported),
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
                SQLITE_TYPE_INFO.iter().any(|row| row.data_type == reported),
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
                    .any(|row| row.type_name == name && row.data_type == sql_type),
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

        for row in SQLITE_TYPE_INFO {
            if DM_COMPAT_ONLY.contains(&row.type_name) {
                continue;
            }
            let produced = sqlite_bare_type_name(row.data_type);
            assert_eq!(
                produced, row.type_name,
                "SQLITE_TYPE_INFO row {:?} (DATA_TYPE={:?}) is not reachable via \
                 sqlite_bare_type_name (got {produced:?} instead) — no real column can \
                 ever be reported under this TYPE_NAME",
                row.type_name, row.data_type
            );
        }
    }

    #[test]
    fn type_info_rows_have_unique_data_types_per_name() {
        let mut seen = std::collections::HashSet::new();
        for row in SQLITE_TYPE_INFO {
            assert!(
                seen.insert(row.type_name),
                "duplicate type_name in SQLITE_TYPE_INFO: {}",
                row.type_name
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
        // by design (see that constant's doc comment) -- SQLite has no
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
                .find(|row| row.data_type == sql_type)
                .unwrap_or_else(|| panic!("no SQLGetTypeInfo row for {sql_type:?} ({decl:?})"));
            assert_eq!(
                row.column_size, expected,
                "column_size for {decl:?} ({sql_type:?}) row {:?} does not match \
                 default_precision_for_type",
                row.type_name
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
        // <https://www.sqlite.org/lang_datefunc.html>) -- see
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

    #[test]
    fn type_info_rows_sorted_by_data_type_then_type_name() {
        // Spec (SQLGetTypeInfo): "ordered by DATA_TYPE and then ... TYPE_NAME,
        // both ascending." DATA_TYPE is a signed i16 (negative for ODBC
        // extension types), so the comparison must not treat it as unsigned.
        // This walks adjacent pairs rather than asserting a fixed sequence,
        // so it keeps holding as rows are added or reordered.
        for pair in SQLITE_TYPE_INFO.windows(2) {
            let (prev, next) = (&pair[0], &pair[1]);
            assert!(
                prev.data_type.0 <= next.data_type.0,
                "SQLITE_TYPE_INFO not sorted by DATA_TYPE: {:?} (DATA_TYPE={}) \
                 appears before {:?} (DATA_TYPE={})",
                prev.type_name,
                prev.data_type.0,
                next.type_name,
                next.data_type.0
            );
            if prev.data_type == next.data_type {
                assert!(
                    prev.type_name <= next.type_name,
                    "rows sharing DATA_TYPE={} not sorted by TYPE_NAME: {:?} appears \
                     before {:?}",
                    prev.data_type.0,
                    prev.type_name,
                    next.type_name
                );
            }
        }
    }

    /// Guards that SQL_DRIVER_VER is derived from the crate version rather
    /// than a hand-transcribed string that must be remembered whenever
    /// Cargo.toml changes.
    ///
    /// The macro's three components are cross-checked against the *full*
    /// `CARGO_PKG_VERSION` string rather than against the same three
    /// `CARGO_PKG_VERSION_*` variables the macro reads. Restating the macro's
    /// own expansion would assert nothing -- it would pass even if the macro
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
    /// 3.53.2, so both are claimed -- verified by live probe, not assumed.
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
    /// *core* function, not a math one -- the one flag that would be wrong if
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

    /// SQLite has no year()/month()/day() -- only strftime() with a format
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
