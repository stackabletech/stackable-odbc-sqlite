//! Catalog metadata for the SQLite backend (`tables`, `columns`,
//! `primary_keys`, `foreign_keys`, `statistics`, `special_columns`), derived
//! from SQLite's `PRAGMA` introspection and `sqlite_master`, plus the private
//! query helpers those functions share.

use stackable_odbc_core::backend::Backend;
use stackable_odbc_core::types::{
    ColumnDescriptor, ColumnValue, ColumnsResultCol, ForeignKeysResultCol, IdentifierType,
    Nullable, PrimaryKeysResultCol, SQL_CASCADE, SQL_INDEX_OTHER, SQL_NO_ACTION, SQL_PC_NOT_PSEUDO,
    SQL_PC_PSEUDO, SQL_RESTRICT, SQL_SET_DEFAULT, SQL_SET_NULL, SQL_TABLE_STAT, Scope, SqlDataType,
    TablesResultCol, special_columns_columns, statistics_columns,
};

/// Column indices for `PRAGMA table_info(table)`.
///
/// Columns: cid (0), name (1), type (2), notnull (3), dflt_value (4), pk (5)
mod pragma_table_info_col {
    pub const CID: usize = 0;
    pub const NAME: usize = 1;
    pub const TYPE: usize = 2;
    pub const NOT_NULL: usize = 3;
    pub const DFT_VALUE: usize = 4;
    pub const PK: usize = 5;
}

/// Column ordinals of `PRAGMA index_list(<table>)`.
mod pragma_index_list_col {
    pub const NAME: usize = 1;
    pub const UNIQUE: usize = 2;
    pub const PARTIAL: usize = 4;
}

/// Column ordinals of `PRAGMA index_xinfo(<index>)`.
mod pragma_index_xinfo_col {
    pub const NAME: usize = 2;
    pub const DESC: usize = 3;
    pub const KEY: usize = 5;
}

/// Column indices for `PRAGMA foreign_key_list(table)`.
///
/// Columns: id (0), seq (1), table (2), from (3), to (4), on_update (5), on_delete (6), match (7)
mod pragma_fk_col {
    pub const SEQ: usize = 1;
    pub const TABLE: usize = 2;
    pub const FROM: usize = 3;
    pub const TO: usize = 4;
    pub const ON_UPDATE: usize = 5;
    pub const ON_DELETE: usize = 6;
}

use super::SqliteError;
use super::info::sqlite_bare_type_name;
use super::{SqliteBackend, SqliteConnection, SqliteStatement, map_sqlite_error};
use crate::type_conversion::{
    default_precision_for_type, sqlite_declared_type_precision, sqlite_declared_type_scale,
    sqlite_type_to_sql_data_type,
};

/// Bytes per character used for `CHAR_OCTET_LENGTH`. UTF-16 encodes a character
/// in at most 4 bytes (a surrogate pair).
const BYTES_PER_CHAR: i32 = 4;

/// Convert a SQLite foreign key action string to its ODBC numeric constant.
fn fk_action_to_odbc(action: &str) -> i16 {
    match action.to_ascii_uppercase().as_str() {
        "CASCADE" => SQL_CASCADE,
        "RESTRICT" => SQL_RESTRICT,
        "SET NULL" => SQL_SET_NULL,
        "SET DEFAULT" => SQL_SET_DEFAULT,
        _ => SQL_NO_ACTION, // "NO ACTION" and anything else
    }
}

/// Column descriptors for the `SQLTables` result set.
///
/// Shared with every other driver via [`TablesResultCol::all_descriptors`].
/// Widths come from `catalog_result_column_widths()` so they stay consistent
/// with this driver's `SQL_MAX_TABLE_NAME_LEN` of 128.
fn tables_columns() -> Vec<ColumnDescriptor> {
    TablesResultCol::all_descriptors(&SqliteBackend::catalog_result_column_widths())
}

/// Query `sqlite_master` for table **and view** names, filtered by an optional
/// LIKE pattern. Used by `SQLColumns`, whose `TableName` argument is a search
/// pattern and whose result set is defined over tables and views alike.
///
/// Contrast with [`tables_to_inspect`], which matches an exact name and
/// excludes views; the two are deliberately not interchangeable.
fn collect_matching_tables(
    db: &rusqlite::Connection,
    table_filter: Option<&str>,
) -> Result<Vec<String>, rusqlite::Error> {
    let table_sql = if table_filter.is_some() {
        "SELECT name FROM sqlite_master WHERE type IN ('table', 'view') AND name LIKE ?1 ESCAPE '\\' ORDER BY name"
    } else {
        "SELECT name FROM sqlite_master WHERE type IN ('table', 'view') ORDER BY name"
    };
    let mut table_stmt = db.prepare(table_sql)?;
    let mut table_rows = if let Some(t) = table_filter {
        table_stmt.query(rusqlite::params![t])?
    } else {
        table_stmt.query([])?
    };
    let mut table_names = Vec::new();
    while let Some(row) = table_rows.next()? {
        table_names.push(row.get(0)?);
    }
    Ok(table_names)
}

/// Convert one row from `PRAGMA table_info(table_name)` into an ODBC SQLColumns row.
/// `table_name` is the table the column belongs to.
/// `col_name` is the column name, `col_type` is the declared type, `not_null` is true if NOT NULL,
/// `ordinal` is the 0-based column index from PRAGMA (cid), `dflt_value` is the default value.
fn build_column_row(
    table_name: &str,
    col_name: &str,
    col_type: &str,
    not_null: bool,
    ordinal: i64,
    dflt_value: Option<&str>,
) -> Vec<ColumnValue> {
    let sql_type = sqlite_type_to_sql_data_type(col_type);
    let precision = sqlite_declared_type_precision(col_type);
    let scale = sqlite_declared_type_scale(col_type);
    let nullable = if not_null {
        Nullable::SqlNoNulls
    } else {
        Nullable::SqlNullable
    };
    let is_numeric = matches!(
        sql_type,
        SqlDataType::SMALLINT
            | SqlDataType::DOUBLE
            | SqlDataType::EXT_BIG_INT
            | SqlDataType::EXT_TINY_INT
            | SqlDataType::DECIMAL
    );
    // sqlite_type_to_sql_data_type() only ever returns SqlDataType::EXT_W_VARCHAR for
    // character types (both the explicit CHAR/VARCHAR/TEXT/CLOB/... match arm and the
    // sqlite_affinity() fallback map to EXT_W_VARCHAR; there is no case that returns
    // the plain SqlDataType::VARCHAR for SQLite-declared columns).
    let is_char = sql_type == SqlDataType::EXT_W_VARCHAR;
    // Likewise, BLOB (both the explicit match arm and the affinity fallback)
    // is the only SQLite-declared type mapped to SqlDataType::EXT_VAR_BINARY.
    let is_binary = sql_type == SqlDataType::EXT_VAR_BINARY;

    let column_size = i32::try_from(precision).unwrap_or_else(|_| {
        tracing::warn!(precision, "declared column size exceeds i32");
        i32::MAX
    });

    // CHAR_OCTET_LENGTH (ODBC 3.0 SQLColumns column 16): the maximum length in
    // bytes of a character or binary column; NULL for all other data types.
    //
    // Character columns declare their length in UTF-16 characters (up to
    // BYTES_PER_CHAR bytes per character). SQLite lets a declared length reach
    // e.g. VARCHAR(2000000000), so the product must be checked rather than
    // wrapping (release builds) or panicking (debug builds); report NULL when
    // it does not fit i32.
    //
    // Binary columns (BLOB) declare their length in bytes already, so it is
    // passed through unmultiplied, but the u32 -> i32 conversion still needs
    // to be checked for the same overflow reason.
    let char_octet_length = if is_char {
        i32::try_from(precision)
            .ok()
            .and_then(|p| p.checked_mul(BYTES_PER_CHAR))
            .map(ColumnValue::I32)
            .unwrap_or(ColumnValue::Null)
    } else if is_binary {
        i32::try_from(precision)
            .ok()
            .map(ColumnValue::I32)
            .unwrap_or(ColumnValue::Null)
    } else {
        ColumnValue::Null
    };

    let ordinal_position = i32::try_from(ordinal + 1).unwrap_or_else(|_| {
        tracing::warn!(ordinal, "ordinal position exceeds i32");
        i32::MAX
    });

    vec![
        ColumnValue::Null,                           // TABLE_CAT
        ColumnValue::Null,                           // TABLE_SCHEM
        ColumnValue::String(table_name.to_string()), // TABLE_NAME
        ColumnValue::String(col_name.to_string()),   // COLUMN_NAME
        ColumnValue::I16(sql_type.0),                // DATA_TYPE
        // Spec (SQLColumns.TYPE_NAME / SQL_DESC_TYPE_NAME): both list bare
        // examples ("CHAR", "VARCHAR", ...), not declarations, so `col_type`
        // ("VARCHAR(50)") matches no `SQLGetTypeInfo` row.
        // `sqlite_bare_type_name` (also used by `SQL_DESC_TYPE_NAME` in
        // execute.rs) returns the bare name that does; the declared length
        // is still carried above via COLUMN_SIZE (`precision`), just not the
        // name.
        ColumnValue::String(sqlite_bare_type_name(sql_type).to_string()), // TYPE_NAME
        ColumnValue::I32(column_size),                                    // COLUMN_SIZE
        ColumnValue::I32(0),                                              // BUFFER_LENGTH
        ColumnValue::I16(scale),                                          // DECIMAL_DIGITS
        if is_numeric {
            ColumnValue::I16(10)
        } else {
            ColumnValue::Null
        }, // NUM_PREC_RADIX
        ColumnValue::I16(nullable.into()),                                // NULLABLE
        ColumnValue::Null,                                                // REMARKS
        match dflt_value {
            Some(v) => ColumnValue::String(v.to_string()),
            None => ColumnValue::Null,
        }, // COLUMN_DEF
        ColumnValue::I16(sql_type.0),                                     // SQL_DATA_TYPE
        ColumnValue::Null,                                                // SQL_DATETIME_SUB
        char_octet_length,                                                // CHAR_OCTET_LENGTH
        ColumnValue::I32(ordinal_position),                               // ORDINAL_POSITION
        ColumnValue::String(nullable.as_is_nullable_str().to_string()),   // IS_NULLABLE
    ]
}

/// Return the base tables to inspect: the exact named table if one is given,
/// otherwise every `type='table'` entry in `sqlite_master`. Used by
/// `SQLPrimaryKeys`/`SQLForeignKeys`, which take an exact table name (not a
/// pattern) and for which views are out of scope.
///
/// Contrast with [`collect_matching_tables`], which treats its argument as a
/// LIKE pattern and includes views; the two are deliberately not
/// interchangeable.
fn tables_to_inspect(
    db: &rusqlite::Connection,
    table_name: Option<&str>,
) -> Result<Vec<String>, rusqlite::Error> {
    if let Some(t) = table_name {
        return Ok(vec![t.to_string()]);
    }
    let mut stmt = db.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
    let mut rows = stmt.query([])?;
    let mut names = Vec::new();
    while let Some(row) = rows.next()? {
        names.push(row.get(0)?);
    }
    Ok(names)
}

pub(super) fn tables(
    conn: &SqliteConnection,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
    table_type: Option<&str>,
) -> Result<SqliteStatement, SqliteError> {
    // ODBC spec: empty string is a valid (but useless for SQLite) filter; treat as no-filter.
    let catalog = catalog.filter(|s| !s.is_empty());
    let schema = schema.filter(|s| !s.is_empty());
    // The SQLTables TableType="%" discovery case requires an EMPTY TableName;
    // evaluate that before the "%"-stripping normalization below, so a literal
    // TableName="%" (meaning "all tables") does not masquerade as discovery.
    let table_name_is_empty = table.is_none_or(|s| s.is_empty());
    // Treat "%" (match-all wildcard) as no-filter too, to avoid LIKE '%' overhead.
    let table = table.filter(|s| !s.is_empty() && *s != "%");

    // ODBC spec §SQLTables: TableType="%" with empty catalog/schema/table returns
    // the list of valid table types for the data source. SQLite exposes tables
    // and views.
    if table_type == Some("%") && catalog.is_none() && schema.is_none() && table_name_is_empty {
        let type_rows = ["TABLE", "VIEW"]
            .into_iter()
            .map(|t| {
                vec![
                    ColumnValue::Null,                  // TABLE_CAT
                    ColumnValue::Null,                  // TABLE_SCHEM
                    ColumnValue::Null,                  // TABLE_NAME
                    ColumnValue::String(t.to_string()), // TABLE_TYPE
                    ColumnValue::Null,                  // REMARKS
                ]
            })
            .collect();
        return Ok(SqliteStatement::new(tables_columns(), type_rows));
    }

    let table_type = table_type.filter(|s| !s.is_empty() && *s != "%");

    // ODBC spec §8.3: special single-argument discovery calls.
    // catalog="%" with empty schema/table → return list of valid catalogs.
    // SQLite has no catalogs (TABLE_CAT is always NULL), so return empty result.
    if catalog == Some("%") && schema.is_none() && table.is_none() {
        return Ok(SqliteStatement::new(tables_columns(), vec![]));
    }
    // schema="%" with empty catalog/table → return list of valid schemas.
    // SQLite has no schemas, so return empty result.
    if schema == Some("%") && catalog.is_none() && table.is_none() {
        return Ok(SqliteStatement::new(tables_columns(), vec![]));
    }

    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;

    // TableName is a search pattern (ODBC §8.3); use LIKE so "%" works as wildcard.
    let sql = if table.is_some() {
        "SELECT name, type FROM sqlite_master WHERE type IN ('table', 'view') AND name LIKE ?1 ESCAPE '\\' ORDER BY type, name"
    } else {
        "SELECT name, type FROM sqlite_master WHERE type IN ('table', 'view') ORDER BY type, name"
    };

    let mut stmt = db.prepare(sql).map_err(map_sqlite_error)?;
    let mut rows = Vec::new();
    let mut raw_rows = if let Some(t) = table {
        stmt.query(rusqlite::params![t]).map_err(map_sqlite_error)?
    } else {
        stmt.query([]).map_err(map_sqlite_error)?
    };
    while let Some(row) = raw_rows.next().map_err(map_sqlite_error)? {
        let name: String = row.get(0).map_err(map_sqlite_error)?;
        let type_str: String = row.get(1).map_err(map_sqlite_error)?;
        let odbc_type = if type_str == "view" { "VIEW" } else { "TABLE" };

        // Filter by table_type if specified (comma-separated list, not a pattern).
        if let Some(tt) = table_type {
            let allowed: Vec<&str> = tt.split(',').map(|s| s.trim().trim_matches('\'')).collect();
            if !allowed.iter().any(|a| a.eq_ignore_ascii_case(odbc_type)) {
                continue;
            }
        }

        rows.push(vec![
            ColumnValue::Null,                          // TABLE_CAT
            ColumnValue::Null,                          // TABLE_SCHEM
            ColumnValue::String(name),                  // TABLE_NAME
            ColumnValue::String(odbc_type.to_string()), // TABLE_TYPE
            ColumnValue::Null,                          // REMARKS
        ]);
    }

    Ok(SqliteStatement::new(tables_columns(), rows))
}

pub(super) fn columns(
    conn: &SqliteConnection,
    _catalog: Option<&str>,
    _schema: Option<&str>,
    table: Option<&str>,
    column: Option<&str>,
) -> Result<SqliteStatement, SqliteError> {
    // Same normalization as tables(): empty string and "%" both mean "no filter".
    let table = table.filter(|s| !s.is_empty() && *s != "%");
    let column = column.filter(|s| !s.is_empty() && *s != "%");

    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;

    let table_names = collect_matching_tables(&db, table).map_err(map_sqlite_error)?;

    let mut rows = Vec::new();
    for table_name in &table_names {
        // The pragma_table_info table-valued function binds its argument (the
        // `PRAGMA table_info(...)` statement form does not); use it so the name
        // needs no manual escaping. Same columns, same order as the PRAGMA.
        //
        // ColumnName is an ODBC search pattern (spec §SQLColumns): filter with
        // LIKE ... ESCAPE '\' in SQL so `%`/`_`/`\` follow ODBC semantics.
        let pragma_sql = if column.is_some() {
            "SELECT * FROM pragma_table_info(?1) WHERE name LIKE ?2 ESCAPE '\\'"
        } else {
            "SELECT * FROM pragma_table_info(?1)"
        };
        let mut pragma_stmt = db.prepare(pragma_sql).map_err(map_sqlite_error)?;
        let mut pragma_rows = if let Some(c) = column {
            pragma_stmt
                .query(rusqlite::params![table_name, c])
                .map_err(map_sqlite_error)?
        } else {
            pragma_stmt
                .query(rusqlite::params![table_name])
                .map_err(map_sqlite_error)?
        };

        while let Some(row) = pragma_rows.next().map_err(map_sqlite_error)? {
            let cid: i64 = row
                .get(pragma_table_info_col::CID)
                .map_err(map_sqlite_error)?;
            let col_name: String = row
                .get(pragma_table_info_col::NAME)
                .map_err(map_sqlite_error)?;
            let decl_type: String = row
                .get::<_, Option<String>>(pragma_table_info_col::TYPE)
                .map_err(map_sqlite_error)?
                .unwrap_or_else(|| "TEXT".to_string());
            let notnull: i64 = row
                .get(pragma_table_info_col::NOT_NULL)
                .map_err(map_sqlite_error)?;
            let dflt_value: Option<String> = row
                .get(pragma_table_info_col::DFT_VALUE)
                .map_err(map_sqlite_error)?;

            rows.push(build_column_row(
                table_name,
                &col_name,
                &decl_type,
                notnull != 0,
                cid,
                dflt_value.as_deref(),
            ));
        }
    }

    let columns = ColumnsResultCol::all_descriptors(&SqliteBackend::catalog_result_column_widths());

    Ok(SqliteStatement::new(columns, rows))
}

/// Return primary key columns for the given table.
///
/// Uses `PRAGMA table_info(table)` and filters rows where `pk > 0`.
/// The `pk` column is the 1-based key sequence number.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprimarykeys-function>
pub(super) fn primary_keys(
    conn: &SqliteConnection,
    _catalog: Option<&str>,
    _schema: Option<&str>,
    table: Option<&str>,
) -> Result<SqliteStatement, SqliteError> {
    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;

    // Collect table names to query (either the specific one or all tables).
    let table_names = tables_to_inspect(&db, table).map_err(map_sqlite_error)?;

    let mut result_rows: Vec<Vec<ColumnValue>> = Vec::new();
    for table_name in &table_names {
        // The pragma_table_info table-valued function binds its argument (the
        // `PRAGMA table_info(...)` statement form does not); use it so the name
        // needs no manual escaping. Same columns, same order as the PRAGMA.
        let mut pragma_stmt = db
            .prepare("SELECT * FROM pragma_table_info(?1)")
            .map_err(map_sqlite_error)?;
        let mut pragma_rows = pragma_stmt
            .query(rusqlite::params![table_name])
            .map_err(map_sqlite_error)?;

        // Collect pk columns: (key_seq, col_name)
        let mut pk_cols: Vec<(i64, String)> = Vec::new();
        while let Some(row) = pragma_rows.next().map_err(map_sqlite_error)? {
            let pk_seq: i64 = row
                .get(pragma_table_info_col::PK)
                .map_err(map_sqlite_error)?;
            if pk_seq > 0 {
                let col_name: String = row
                    .get(pragma_table_info_col::NAME)
                    .map_err(map_sqlite_error)?;
                pk_cols.push((pk_seq, col_name));
            }
        }

        // Sort by KEY_SEQ (the pk column from PRAGMA is already 1-based).
        pk_cols.sort_by_key(|(seq, _)| *seq);

        for (key_seq, col_name) in pk_cols {
            result_rows.push(vec![
                ColumnValue::Null,                       // TABLE_CAT
                ColumnValue::Null,                       // TABLE_SCHEM
                ColumnValue::String(table_name.clone()), // TABLE_NAME
                ColumnValue::String(col_name),           // COLUMN_NAME
                ColumnValue::I16(i16::try_from(key_seq).unwrap_or_else(|_| {
                    tracing::warn!(key_seq, "key sequence exceeds i16");
                    i16::MAX
                })), // KEY_SEQ
                ColumnValue::Null,                       // PK_NAME (not available in SQLite)
            ]);
        }
    }

    let columns =
        PrimaryKeysResultCol::all_descriptors(&SqliteBackend::catalog_result_column_widths());

    Ok(SqliteStatement::new(columns, result_rows))
}

/// Return foreign key relationships involving the given tables.
///
/// If `fk_table` is given, uses `PRAGMA foreign_key_list(fk_table)` to enumerate the FKs
/// defined on that table. If only `pk_table` is given, all tables are scanned.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlforeignkeys-function>
pub(super) fn foreign_keys(
    conn: &SqliteConnection,
    _pk_catalog: Option<&str>,
    _pk_schema: Option<&str>,
    pk_table: Option<&str>,
    _fk_catalog: Option<&str>,
    _fk_schema: Option<&str>,
    fk_table: Option<&str>,
) -> Result<SqliteStatement, SqliteError> {
    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;

    // Which FK tables do we query?
    let fk_table_names = tables_to_inspect(&db, fk_table).map_err(map_sqlite_error)?;

    let mut result_rows: Vec<Vec<ColumnValue>> = Vec::new();

    for fk_tbl in &fk_table_names {
        let pragma_sql = format!("PRAGMA foreign_key_list('{}')", fk_tbl.replace('\'', "''"));
        let mut pragma_stmt = db.prepare(&pragma_sql).map_err(map_sqlite_error)?;
        let mut pragma_rows = pragma_stmt.query([]).map_err(map_sqlite_error)?;

        while let Some(row) = pragma_rows.next().map_err(map_sqlite_error)? {
            let seq: i64 = row.get(pragma_fk_col::SEQ).map_err(map_sqlite_error)?;
            let referenced_table: String =
                row.get(pragma_fk_col::TABLE).map_err(map_sqlite_error)?;
            let from_col: String = row.get(pragma_fk_col::FROM).map_err(map_sqlite_error)?;
            let to_col: Option<String> = row.get(pragma_fk_col::TO).map_err(map_sqlite_error)?;
            let on_update: String = row
                .get(pragma_fk_col::ON_UPDATE)
                .map_err(map_sqlite_error)?;
            let on_delete: String = row
                .get(pragma_fk_col::ON_DELETE)
                .map_err(map_sqlite_error)?;

            // Filter by pk_table if specified.
            if let Some(pkt) = pk_table
                && !referenced_table.eq_ignore_ascii_case(pkt)
            {
                continue;
            }

            let pk_col = match to_col {
                Some(c) => ColumnValue::String(c),
                // SQLite allows FK without explicit column; treat as NULL.
                None => ColumnValue::Null,
            };

            result_rows.push(vec![
                ColumnValue::Null,                     // PKTABLE_CAT
                ColumnValue::Null,                     // PKTABLE_SCHEM
                ColumnValue::String(referenced_table), // PKTABLE_NAME
                pk_col,                                // PKCOLUMN_NAME
                ColumnValue::Null,                     // FKTABLE_CAT
                ColumnValue::Null,                     // FKTABLE_SCHEM
                ColumnValue::String(fk_tbl.clone()),   // FKTABLE_NAME
                ColumnValue::String(from_col),         // FKCOLUMN_NAME
                ColumnValue::I16(i16::try_from(seq + 1).unwrap_or_else(|_| {
                    tracing::warn!(seq, "key sequence exceeds i16");
                    i16::MAX
                })), // KEY_SEQ (1-based)
                ColumnValue::I16(fk_action_to_odbc(&on_update)), // UPDATE_RULE
                ColumnValue::I16(fk_action_to_odbc(&on_delete)), // DELETE_RULE
                ColumnValue::Null,                     // FK_NAME (not in SQLite PRAGMA)
                ColumnValue::Null,                     // PK_NAME (not in SQLite PRAGMA)
                ColumnValue::Null,                     // DEFERRABILITY
            ]);
        }
    }

    let columns =
        ForeignKeysResultCol::all_descriptors(&SqliteBackend::catalog_result_column_widths());

    Ok(SqliteStatement::new(columns, result_rows))
}

/// Return index statistics for a single table (SQLStatistics).
///
/// Emits a leading `SQL_TABLE_STAT` row (CARDINALITY from `sqlite_stat1` when
/// `ANALYZE` has populated it, else NULL; PAGES always NULL, honoring
/// SQL_QUICK), then one row per key column of each index from
/// `PRAGMA index_list` / `PRAGMA index_xinfo`. Rows are ordered per spec by
/// NON_UNIQUE, TYPE, INDEX_QUALIFIER (always NULL here), INDEX_NAME,
/// ORDINAL_POSITION, with the NULL NON_UNIQUE table-stat row first.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlstatistics-function>
pub(super) fn statistics(
    conn: &SqliteConnection,
    _catalog: Option<&str>,
    _schema: Option<&str>,
    table: Option<&str>,
    unique_only: bool,
) -> Result<SqliteStatement, SqliteError> {
    use stackable_odbc_core::types::{SQL_FALSE, SQL_TRUE};

    let widths = SqliteBackend::catalog_result_column_widths();
    let columns = statistics_columns(&widths);

    // SQLStatistics.TableName cannot be a search pattern; an absent name has no
    // table to describe, so return an empty (but correctly-shaped) result set.
    let Some(table) = table.filter(|s| !s.is_empty()) else {
        return Ok(SqliteStatement::new(columns, Vec::new()));
    };

    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;

    // CARDINALITY for the table-stat row: read sqlite_stat1 only if present.
    let cardinality = table_cardinality_from_stat1(&db, table);

    // Table-stat row (leading). TABLE_NAME (3) and TYPE (7) are the NOT NULL
    // columns; everything index-specific is NULL.
    let mut rows: Vec<Vec<ColumnValue>> = vec![vec![
        ColumnValue::Null,                      // TABLE_CAT
        ColumnValue::Null,                      // TABLE_SCHEM
        ColumnValue::String(table.to_string()), // TABLE_NAME
        ColumnValue::Null,                      // NON_UNIQUE
        ColumnValue::Null,                      // INDEX_QUALIFIER
        ColumnValue::Null,                      // INDEX_NAME
        ColumnValue::I16(SQL_TABLE_STAT),       // TYPE
        ColumnValue::Null,                      // ORDINAL_POSITION
        ColumnValue::Null,                      // COLUMN_NAME
        ColumnValue::Null,                      // ASC_OR_DESC
        cardinality,                            // CARDINALITY
        ColumnValue::Null,                      // PAGES
        ColumnValue::Null,                      // FILTER_CONDITION
    ]];

    // Enumerate indexes. Use the pragma_ TVF form so the name binds safely.
    let mut list_stmt = db
        .prepare("SELECT * FROM pragma_index_list(?1)")
        .map_err(map_sqlite_error)?;
    let mut list_rows = list_stmt
        .query(rusqlite::params![table])
        .map_err(map_sqlite_error)?;

    // (index_name, is_unique, is_partial)
    let mut indexes: Vec<(String, bool, bool)> = Vec::new();
    while let Some(r) = list_rows.next().map_err(map_sqlite_error)? {
        let name: String = r
            .get(pragma_index_list_col::NAME)
            .map_err(map_sqlite_error)?;
        let unique: i64 = r
            .get(pragma_index_list_col::UNIQUE)
            .map_err(map_sqlite_error)?;
        let partial: i64 = r
            .get(pragma_index_list_col::PARTIAL)
            .map_err(map_sqlite_error)?;
        let is_unique = unique != 0;
        if unique_only && !is_unique {
            continue;
        }
        indexes.push((name, is_unique, partial != 0));
    }
    drop(list_rows);
    drop(list_stmt);

    for (index_name, is_unique, is_partial) in &indexes {
        let mut xinfo_stmt = db
            .prepare("SELECT * FROM pragma_index_xinfo(?1)")
            .map_err(map_sqlite_error)?;
        let mut xinfo_rows = xinfo_stmt
            .query(rusqlite::params![index_name])
            .map_err(map_sqlite_error)?;

        let mut ordinal: i16 = 0;
        while let Some(r) = xinfo_rows.next().map_err(map_sqlite_error)? {
            let key: i64 = r
                .get(pragma_index_xinfo_col::KEY)
                .map_err(map_sqlite_error)?;
            if key == 0 {
                continue; // auxiliary column (e.g. trailing rowid), not part of the key
            }
            ordinal += 1;
            // COLUMN_NAME is NULL for an expression index; spec wants "" then.
            let col_name: Option<String> = r
                .get(pragma_index_xinfo_col::NAME)
                .map_err(map_sqlite_error)?;
            let desc: i64 = r
                .get(pragma_index_xinfo_col::DESC)
                .map_err(map_sqlite_error)?;

            rows.push(vec![
                ColumnValue::Null,                      // TABLE_CAT
                ColumnValue::Null,                      // TABLE_SCHEM
                ColumnValue::String(table.to_string()), // TABLE_NAME
                ColumnValue::I16(if *is_unique {
                    SQL_FALSE as i16
                } else {
                    SQL_TRUE as i16
                }), // NON_UNIQUE
                ColumnValue::Null,                      // INDEX_QUALIFIER
                ColumnValue::String(index_name.clone()), // INDEX_NAME
                ColumnValue::I16(SQL_INDEX_OTHER),      // TYPE
                ColumnValue::I16(ordinal),              // ORDINAL_POSITION
                ColumnValue::String(col_name.unwrap_or_default()), // COLUMN_NAME ("" for expression)
                ColumnValue::String(if desc != 0 { "D" } else { "A" }.into()), // ASC_OR_DESC
                ColumnValue::Null,                                 // CARDINALITY
                ColumnValue::Null,                                 // PAGES
                if *is_partial {
                    ColumnValue::String(String::new())
                } else {
                    ColumnValue::Null
                }, // FILTER_CONDITION
            ]);
        }
    }

    // Order: table-stat row first (NON_UNIQUE NULL), then by NON_UNIQUE, TYPE,
    // INDEX_NAME, ORDINAL_POSITION. Sort key extracts those columns.
    rows.sort_by_key(|r| statistics_sort_key(r));

    Ok(SqliteStatement::new(columns, rows))
}

/// Sort key implementing the SQLStatistics ordering. NULL NON_UNIQUE sorts
/// first (the table-stat row); NON_UNIQUE ascending (unique before non-unique);
/// then TYPE, INDEX_NAME, ORDINAL_POSITION.
fn statistics_sort_key(row: &[ColumnValue]) -> (i16, i16, String, i16) {
    let non_unique = match &row[3] {
        ColumnValue::I16(v) => *v,
        _ => -1, // NULL -> before 0 (unique) and 1 (non-unique)
    };
    let ty = match &row[6] {
        ColumnValue::I16(v) => *v,
        _ => 0,
    };
    let index_name = match &row[5] {
        ColumnValue::String(s) => s.clone(),
        _ => String::new(),
    };
    let ordinal = match &row[7] {
        ColumnValue::I16(v) => *v,
        _ => 0,
    };
    (non_unique, ty, index_name, ordinal)
}

/// Read the table row count from `sqlite_stat1` if `ANALYZE` has populated it.
/// The `stat` column's first whitespace-delimited token is the table row count.
/// Returns `ColumnValue::Null` when the stat table or row is absent.
fn table_cardinality_from_stat1(db: &rusqlite::Connection, table: &str) -> ColumnValue {
    let query = "SELECT stat FROM sqlite_stat1 WHERE tbl = ?1 AND idx IS NULL LIMIT 1";
    let stat: Result<String, _> = db.query_row(query, rusqlite::params![table], |r| r.get(0));
    match stat {
        Ok(s) => s
            .split_whitespace()
            .next()
            .and_then(|tok| tok.parse::<i32>().ok())
            .map(ColumnValue::I32)
            .unwrap_or(ColumnValue::Null),
        Err(_) => ColumnValue::Null, // no sqlite_stat1 (no ANALYZE) or no row
    }
}

/// Return the SQL_BEST_ROWID / SQL_ROWVER special columns for a table
/// (SQLSpecialColumns).
///
/// SQLite has no engine-updated columns, so `SQL_ROWVER` is always empty.
/// For `SQL_BEST_ROWID` the identifier is, in priority order: a declared
/// `INTEGER PRIMARY KEY` column (a nameable rowid alias); otherwise the
/// `rowid` pseudo-column of an ordinary rowid table; otherwise the PRIMARY KEY
/// columns of a WITHOUT ROWID table. A request whose minimum `Scope` exceeds
/// the identifier's guaranteed scope yields an empty result set.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlspecialcolumns-function>
pub(super) fn special_columns(
    conn: &SqliteConnection,
    identifier_type: IdentifierType,
    _catalog: Option<&str>,
    _schema: Option<&str>,
    table: Option<&str>,
    scope: Scope,
    _nullable: Nullable, // our identifiers are all NOT NULL -> Nullable never filters
) -> Result<SqliteStatement, SqliteError> {
    let widths = SqliteBackend::catalog_result_column_widths();
    let columns = special_columns_columns(&widths);
    let empty = || Ok(SqliteStatement::new(columns.clone(), Vec::new()));

    // ROWVER: SQLite has no auto-updated columns.
    if matches!(identifier_type, IdentifierType::RowVer) {
        return empty();
    }
    let Some(table) = table.filter(|s| !s.is_empty()) else {
        return empty();
    };

    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;

    // Gather (name, decl_type, pk_seq) for every column via the pragma TVF.
    let mut info_stmt = db
        .prepare("SELECT * FROM pragma_table_info(?1)")
        .map_err(map_sqlite_error)?;
    let mut info_rows = info_stmt
        .query(rusqlite::params![table])
        .map_err(map_sqlite_error)?;

    struct Col {
        name: String,
        decl_type: String,
        pk: i64,
    }
    let mut cols: Vec<Col> = Vec::new();
    while let Some(r) = info_rows.next().map_err(map_sqlite_error)? {
        let name: String = r
            .get(pragma_table_info_col::NAME)
            .map_err(map_sqlite_error)?;
        let decl_type: Option<String> = r
            .get(pragma_table_info_col::TYPE)
            .map_err(map_sqlite_error)?;
        let pk: i64 = r.get(pragma_table_info_col::PK).map_err(map_sqlite_error)?;
        cols.push(Col {
            name,
            decl_type: decl_type.unwrap_or_default(),
            pk,
        });
    }
    drop(info_rows);
    drop(info_stmt);

    if cols.is_empty() {
        return empty(); // table does not exist / has no columns
    }

    let pk_cols: Vec<&Col> = {
        let mut v: Vec<&Col> = cols.iter().filter(|c| c.pk > 0).collect();
        v.sort_by_key(|c| c.pk);
        v
    };

    // Case 1: single declared INTEGER PRIMARY KEY -> the nameable rowid alias.
    let integer_pk =
        if pk_cols.len() == 1 && pk_cols[0].decl_type.trim().eq_ignore_ascii_case("INTEGER") {
            Some(pk_cols[0])
        } else {
            None
        };

    // Decide identifier + guaranteed scope.
    // guaranteed scope: TRANSACTION for the volatile rowid pseudo-column,
    // SESSION for a declared key column.
    let (rows, guaranteed): (Vec<Vec<ColumnValue>>, Scope) = if let Some(col) = integer_pk {
        // A declared INTEGER PRIMARY KEY is an alias for the 8-byte 64-bit
        // rowid, not a plain INTEGER column, so describe it with the same
        // BIGINT/COLUMN_SIZE 19/BUFFER_LENGTH 8 shape as the rowid
        // pseudo-column below, just under the real column name.
        (
            vec![special_column_row_bigint(
                &col.name,
                SQL_PC_NOT_PSEUDO,
                Scope::Session,
            )],
            Scope::Session,
        )
    } else if table_is_rowid(&db, table)? {
        // rowid pseudo-column (BIGINT).
        (
            vec![special_column_row_bigint(
                "rowid",
                SQL_PC_PSEUDO,
                Scope::Transaction,
            )],
            Scope::Transaction,
        )
    } else {
        // WITHOUT ROWID: the PRIMARY KEY columns.
        if pk_cols.is_empty() {
            return empty();
        }
        (
            pk_cols
                .iter()
                .map(|c| {
                    special_column_row(&c.name, &c.decl_type, SQL_PC_NOT_PSEUDO, Scope::Session)
                })
                .collect(),
            Scope::Session,
        )
    };

    // Scope is a minimum; if we cannot meet it, return empty (spec).
    if scope > guaranteed {
        return empty();
    }

    Ok(SqliteStatement::new(columns, rows))
}

/// Build one SQLSpecialColumns row for a declared column, deriving its SQL type
/// from the same mapping SQLColumns uses.
fn special_column_row(name: &str, decl_type: &str, pseudo: i16, scope: Scope) -> Vec<ColumnValue> {
    let sql_type = sqlite_type_to_sql_data_type(decl_type);
    let column_size = i32::try_from(sqlite_declared_type_precision(decl_type)).unwrap_or(i32::MAX);
    let scale = sqlite_declared_type_scale(decl_type);
    vec![
        ColumnValue::I16(scope.into()),        // SCOPE
        ColumnValue::String(name.to_string()), // COLUMN_NAME
        ColumnValue::I16(sql_type.0),          // DATA_TYPE
        ColumnValue::String(sqlite_bare_type_name(sql_type).to_string()), // TYPE_NAME
        ColumnValue::I32(column_size),         // COLUMN_SIZE
        ColumnValue::I32(column_size),         // BUFFER_LENGTH (approx: transfer octet length)
        if scale > 0 {
            ColumnValue::I16(scale)
        } else {
            ColumnValue::Null
        }, // DECIMAL_DIGITS
        ColumnValue::I16(pseudo),              // PSEUDO_COLUMN
    ]
}

/// Build one SQLSpecialColumns row for the 64-bit rowid pseudo-column / an
/// INTEGER PRIMARY KEY reported as BIGINT.
fn special_column_row_bigint(name: &str, pseudo: i16, scope: Scope) -> Vec<ColumnValue> {
    let sql_type = SqlDataType::EXT_BIG_INT;
    let column_size = i32::try_from(default_precision_for_type(sql_type)).unwrap_or(i32::MAX);
    vec![
        ColumnValue::I16(scope.into()),
        ColumnValue::String(name.to_string()),
        ColumnValue::I16(sql_type.0),
        ColumnValue::String(sqlite_bare_type_name(sql_type).to_string()),
        ColumnValue::I32(column_size),
        ColumnValue::I32(8), // BUFFER_LENGTH: 8 bytes for a 64-bit integer
        ColumnValue::Null,   // DECIMAL_DIGITS: not applicable to integers
        ColumnValue::I16(pseudo),
    ]
}

/// True if `table` is an ordinary rowid table. Probes `SELECT rowid`: a
/// WITHOUT ROWID table has no `rowid` column, so the prepare fails with
/// "no such column: rowid".
///
/// In this rusqlite version (0.40.1) that prepare-time failure arrives as
/// `Error::SqlInputError { msg, .. }` (not `Error::SqliteFailure`); see
/// `map_sqlite_error`'s handling of the same shape in `backend.rs`. Only the
/// "no such column" message is treated as "not a rowid table"; any other
/// error (a genuine failure, not the WITHOUT ROWID case) is routed through
/// `map_sqlite_error`.
fn table_is_rowid(db: &rusqlite::Connection, table: &str) -> Result<bool, SqliteError> {
    // Identifier cannot be bound; quote it, doubling embedded quotes.
    let quoted = format!("\"{}\"", table.replace('"', "\"\""));
    match db.prepare(&format!("SELECT rowid FROM {quoted} LIMIT 0")) {
        Ok(_) => Ok(true),
        Err(rusqlite::Error::SqlInputError { ref msg, .. })
            if msg.starts_with("no such column") =>
        {
            Ok(false)
        }
        // Any other error shape is a genuine failure.
        Err(e) => Err(map_sqlite_error(e)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::backend::SqliteConnection;
    use stackable_odbc_core::backend::StatementBackend;
    use stackable_odbc_core::types::{CDataType, FetchResult, SQL_FALSE};

    fn setup_test_db() -> SqliteConnection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE empty_table (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE types_test (id INTEGER, val REAL, label TEXT);
             CREATE VIEW types_view AS SELECT id, label FROM types_test;
             CREATE TABLE parent (pk INTEGER PRIMARY KEY, info TEXT);
             CREATE TABLE child (
                 id INTEGER PRIMARY KEY,
                 parent_pk INTEGER NOT NULL,
                 FOREIGN KEY (parent_pk) REFERENCES parent(pk)
             );",
        )
        .unwrap();
        SqliteConnection {
            conn: Mutex::new(conn),
            manual_commit: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[test]
    fn tables_returns_all_tables_and_views() {
        let conn = setup_test_db();
        let mut stmt = tables(&conn, None, None, None, None).unwrap();
        assert_eq!(stmt.column_count(), 5);

        let mut names = Vec::new();
        let mut types = Vec::new();
        while stmt.fetch().unwrap() == FetchResult::Row {
            if let ColumnValue::String(n) =
                stmt.get_data(3, CDataType::Default).unwrap().into_owned()
            {
                names.push(n);
            }
            if let ColumnValue::String(t) =
                stmt.get_data(4, CDataType::Default).unwrap().into_owned()
            {
                types.push(t);
            }
        }
        assert!(names.contains(&"empty_table".to_string()));
        assert!(names.contains(&"types_test".to_string()));
        assert!(names.contains(&"types_view".to_string()));
        assert!(names.contains(&"parent".to_string()));
        assert!(names.contains(&"child".to_string()));
        assert_eq!(types.iter().filter(|t| *t == "TABLE").count(), 4);
        assert_eq!(types.iter().filter(|t| *t == "VIEW").count(), 1);
    }

    #[test]
    fn tables_filter_by_table_type() {
        let conn = setup_test_db();
        let mut stmt = tables(&conn, None, None, None, Some("TABLE")).unwrap();
        let mut names = Vec::new();
        while stmt.fetch().unwrap() == FetchResult::Row {
            if let ColumnValue::String(n) =
                stmt.get_data(3, CDataType::Default).unwrap().into_owned()
            {
                names.push(n);
            }
        }
        assert!(names.contains(&"empty_table".to_string()));
        assert!(names.contains(&"types_test".to_string()));
        assert!(!names.contains(&"types_view".to_string()));
    }

    #[test]
    fn tables_filter_by_name() {
        let conn = setup_test_db();
        let mut stmt = tables(&conn, None, None, Some("types_test"), None).unwrap();
        let mut count = 0;
        while stmt.fetch().unwrap() == FetchResult::Row {
            count += 1;
            assert_eq!(
                stmt.get_data(3, CDataType::Default).unwrap().into_owned(),
                ColumnValue::String("types_test".to_string())
            );
        }
        assert_eq!(count, 1);
    }

    #[test]
    fn tables_table_type_percent_lists_table_types() {
        let conn = setup_test_db();
        // SQL_ALL_TABLE_TYPES discovery: TableType="%", others empty.
        let mut stmt = tables(&conn, Some(""), Some(""), Some(""), Some("%")).unwrap();
        let mut types = Vec::new();
        while stmt.fetch().unwrap() == FetchResult::Row {
            // TABLE_NAME (col 3) must be NULL for the discovery result set.
            assert_eq!(
                stmt.get_data(3, CDataType::Default).unwrap().into_owned(),
                ColumnValue::Null
            );
            if let ColumnValue::String(s) =
                stmt.get_data(4, CDataType::Default).unwrap().into_owned()
            {
                types.push(s);
            }
        }
        types.sort();
        assert_eq!(types, vec!["TABLE".to_string(), "VIEW".to_string()]);
    }

    #[test]
    fn columns_returns_correct_columns() {
        let conn = setup_test_db();
        let mut stmt = columns(&conn, None, None, Some("types_test"), None).unwrap();

        let mut col_names = Vec::new();
        while stmt.fetch().unwrap() == FetchResult::Row {
            if let ColumnValue::String(n) =
                stmt.get_data(4, CDataType::Default).unwrap().into_owned()
            {
                col_names.push(n);
            }
        }
        assert_eq!(col_names, vec!["id", "val", "label"]);
    }

    #[test]
    fn columns_filter_by_column_name() {
        let conn = setup_test_db();
        let mut stmt = columns(&conn, None, None, Some("types_test"), Some("val")).unwrap();

        let mut count = 0;
        while stmt.fetch().unwrap() == FetchResult::Row {
            count += 1;
            assert_eq!(
                stmt.get_data(4, CDataType::Default).unwrap().into_owned(),
                ColumnValue::String("val".to_string())
            );
        }
        assert_eq!(count, 1);
    }

    #[test]
    fn columns_nullable_flag() {
        let conn = setup_test_db();
        // empty_table: id INTEGER PRIMARY KEY, name TEXT NOT NULL
        // SQLite PRAGMA table_info reports notnull=0 for INTEGER PRIMARY KEY (PK does not imply
        // NOT NULL in SQLite's PRAGMA), and notnull=1 for the explicit NOT NULL constraint.
        let mut stmt = columns(&conn, None, None, Some("empty_table"), None).unwrap();

        let mut nullability: Vec<(String, i16)> = Vec::new();
        while stmt.fetch().unwrap() == FetchResult::Row {
            let col_name = match stmt.get_data(4, CDataType::Default).unwrap().into_owned() {
                ColumnValue::String(s) => s,
                other => panic!("unexpected column name value: {other:?}"),
            };
            let nullable = match stmt.get_data(11, CDataType::Default).unwrap().into_owned() {
                ColumnValue::I16(v) => v,
                other => panic!("unexpected nullable value: {other:?}"),
            };
            nullability.push((col_name, nullable));
        }

        assert_eq!(nullability.len(), 2);
        let id_nullable = nullability.iter().find(|(n, _)| n == "id").unwrap().1;
        let name_nullable = nullability.iter().find(|(n, _)| n == "name").unwrap().1;
        // id INTEGER PRIMARY KEY: PRAGMA notnull=0, so reported as nullable
        assert_eq!(id_nullable, i16::from(Nullable::SqlNullable));
        // name TEXT NOT NULL: PRAGMA notnull=1, so reported as not null
        assert_eq!(name_nullable, i16::from(Nullable::SqlNoNulls));
    }

    #[test]
    fn build_column_row_text_column_has_char_octet_length() {
        // sqlite_type_to_sql_data_type() maps every character declared type
        // (including VARCHAR) to SqlDataType::EXT_W_VARCHAR, never to the bare
        // SqlDataType::VARCHAR, so build_column_row()'s `is_char` compares
        // against EXT_W_VARCHAR. CHAR_OCTET_LENGTH (column index 15) must then
        // be declared_length * BYTES_PER_CHAR for a text column, not NULL.
        let row = build_column_row("t", "label", "VARCHAR(50)", false, 0, None);
        assert_eq!(row[15], ColumnValue::I32(50 * BYTES_PER_CHAR));
    }

    #[test]
    fn build_column_row_blob_column_has_declared_length_char_octet_length() {
        // CHAR_OCTET_LENGTH also applies to binary columns (ODBC 3.0 SQLColumns
        // column 16), but a BLOB's declared length is already a byte count and
        // must be passed through as-is, not multiplied by BYTES_PER_CHAR.
        let row = build_column_row("t", "data", "BLOB(50)", false, 0, None);
        assert_eq!(row[15], ColumnValue::I32(50));
    }

    #[test]
    fn build_column_row_non_character_non_binary_column_has_null_char_octet_length() {
        // INTEGER is neither character nor binary data, so CHAR_OCTET_LENGTH
        // is NULL per the ODBC spec.
        let row = build_column_row("t", "id", "INTEGER", false, 0, None);
        assert_eq!(row[15], ColumnValue::Null);
    }

    #[test]
    fn build_column_row_huge_declared_length_does_not_overflow() {
        // sqlite_declared_type_precision() parses the declared length out of
        // the type string. A column declared VARCHAR(2000000000) yields a
        // precision whose product with BYTES_PER_CHAR (4) overflows i32
        // (2_000_000_000 * 4 = 8_000_000_000). The checked multiplication
        // reports NULL instead of wrapping or panicking.
        let row = build_column_row("t", "label", "VARCHAR(2000000000)", false, 0, None);
        assert_eq!(row[15], ColumnValue::Null);
    }

    #[test]
    fn primary_keys_returns_pk_column() {
        let conn = setup_test_db();
        let mut stmt = primary_keys(&conn, None, None, Some("parent")).unwrap();

        let mut pk_cols = Vec::new();
        while stmt.fetch().unwrap() == FetchResult::Row {
            // Column 3 = TABLE_NAME, Column 4 = COLUMN_NAME, Column 5 = KEY_SEQ
            let table = match stmt.get_data(3, CDataType::Default).unwrap().into_owned() {
                ColumnValue::String(s) => s,
                other => panic!("unexpected table name: {other:?}"),
            };
            let col = match stmt.get_data(4, CDataType::Default).unwrap().into_owned() {
                ColumnValue::String(s) => s,
                other => panic!("unexpected column name: {other:?}"),
            };
            let seq = match stmt.get_data(5, CDataType::Default).unwrap().into_owned() {
                ColumnValue::I16(v) => v,
                other => panic!("unexpected key_seq: {other:?}"),
            };
            pk_cols.push((table, col, seq));
        }
        assert_eq!(pk_cols.len(), 1);
        assert_eq!(pk_cols[0], ("parent".to_string(), "pk".to_string(), 1));
    }

    #[test]
    fn primary_keys_no_pk_returns_empty() {
        let conn = setup_test_db();
        // types_test has no PRIMARY KEY constraint
        let mut stmt = primary_keys(&conn, None, None, Some("types_test")).unwrap();
        assert_eq!(stmt.fetch().unwrap(), FetchResult::NoData);
    }

    #[test]
    fn foreign_keys_by_fk_table() {
        let conn = setup_test_db();
        let mut stmt = foreign_keys(
            &conn,
            None,
            None,
            None, // pk table: unfiltered
            None,
            None,
            Some("child"), // fk table: child
        )
        .unwrap();

        let mut fks = Vec::new();
        while stmt.fetch().unwrap() == FetchResult::Row {
            // Column 3 = PKTABLE_NAME, Column 4 = PKCOLUMN_NAME
            // Column 7 = FKTABLE_NAME, Column 8 = FKCOLUMN_NAME
            let pk_table = match stmt.get_data(3, CDataType::Default).unwrap().into_owned() {
                ColumnValue::String(s) => s,
                other => panic!("unexpected pk_table: {other:?}"),
            };
            let pk_col = match stmt.get_data(4, CDataType::Default).unwrap().into_owned() {
                ColumnValue::String(s) => s,
                other => panic!("unexpected pk_col: {other:?}"),
            };
            let fk_table = match stmt.get_data(7, CDataType::Default).unwrap().into_owned() {
                ColumnValue::String(s) => s,
                other => panic!("unexpected fk_table: {other:?}"),
            };
            let fk_col = match stmt.get_data(8, CDataType::Default).unwrap().into_owned() {
                ColumnValue::String(s) => s,
                other => panic!("unexpected fk_col: {other:?}"),
            };
            fks.push((pk_table, pk_col, fk_table, fk_col));
        }
        assert_eq!(fks.len(), 1);
        assert_eq!(
            fks[0],
            (
                "parent".to_string(),
                "pk".to_string(),
                "child".to_string(),
                "parent_pk".to_string(),
            )
        );
    }

    #[test]
    fn foreign_keys_no_fk_returns_empty() {
        let conn = setup_test_db();
        // parent has no outgoing foreign keys
        let mut stmt = foreign_keys(&conn, None, None, None, None, None, Some("parent")).unwrap();
        assert_eq!(stmt.fetch().unwrap(), FetchResult::NoData);
    }

    #[test]
    fn foreign_keys_by_pk_table() {
        let conn = setup_test_db();
        let mut stmt = foreign_keys(
            &conn,
            None,
            None,
            Some("parent"), // pk table: parent
            None,
            None,
            None, // fk table: unfiltered
        )
        .unwrap();

        let mut count = 0;
        while stmt.fetch().unwrap() == FetchResult::Row {
            count += 1;
            // FK should point from child.parent_pk to parent.pk
            assert_eq!(
                stmt.get_data(3, CDataType::Default).unwrap().into_owned(),
                ColumnValue::String("parent".to_string())
            );
            assert_eq!(
                stmt.get_data(7, CDataType::Default).unwrap().into_owned(),
                ColumnValue::String("child".to_string())
            );
        }
        assert_eq!(count, 1);
    }

    #[test]
    fn tables_table_name_honors_escape_character() {
        let conn = setup_test_db();
        // `empty\_table` with ESCAPE '\' means a literal underscore: matches
        // exactly "empty_table". Without ESCAPE the `_` is a wildcard and the
        // stray backslash matches nothing.
        let mut stmt = tables(&conn, None, None, Some("empty\\_table"), None).unwrap();
        let mut names = Vec::new();
        while stmt.fetch().unwrap() == FetchResult::Row {
            if let ColumnValue::String(s) =
                stmt.get_data(3, CDataType::Default).unwrap().into_owned()
            {
                names.push(s);
            }
        }
        assert_eq!(names, vec!["empty_table".to_string()]);
    }

    #[test]
    fn columns_column_name_is_a_like_pattern() {
        let conn = setup_test_db();
        // types_test columns: id, val, label. "%l%" matches val and label.
        // Under the old exact-match filter this returned zero rows.
        let mut stmt = columns(&conn, None, None, Some("types_test"), Some("%l%")).unwrap();
        let mut names = Vec::new();
        while stmt.fetch().unwrap() == FetchResult::Row {
            if let ColumnValue::String(s) =
                stmt.get_data(4, CDataType::Default).unwrap().into_owned()
            {
                names.push(s);
            }
        }
        names.sort();
        assert_eq!(names, vec!["label".to_string(), "val".to_string()]);
    }

    #[test]
    fn tables_table_type_percent_with_table_wildcard_lists_tables() {
        let conn = setup_test_db();
        // TableType="%" with TableName="%" is NOT the type-discovery case (that
        // requires an empty TableName): it must list actual tables/views.
        let mut stmt = tables(&conn, Some(""), Some(""), Some("%"), Some("%")).unwrap();
        let mut names = Vec::new();
        while stmt.fetch().unwrap() == FetchResult::Row {
            if let ColumnValue::String(s) =
                stmt.get_data(3, CDataType::Default).unwrap().into_owned()
            {
                names.push(s);
            }
        }
        assert!(
            names.contains(&"types_test".to_string()),
            "expected real table listing, got {names:?}"
        );
    }

    fn setup_stats_db() -> SqliteConnection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (a INTEGER, b TEXT, c REAL);
             CREATE UNIQUE INDEX ux_t_a ON t(a);
             CREATE INDEX ix_t_bc ON t(b, c DESC);",
        )
        .unwrap();
        SqliteConnection {
            conn: Mutex::new(conn),
            manual_commit: std::sync::atomic::AtomicBool::new(false),
        }
    }

    // column ordinals in the 13-column SQLStatistics result set (1-based get_data)
    const NON_UNIQUE: u16 = 4;
    const TYPE_COL: u16 = 7;
    const ORDINAL_POSITION: u16 = 8;
    const COLUMN_NAME: u16 = 9;
    const ASC_OR_DESC: u16 = 10;
    const FILTER_CONDITION: u16 = 13;

    /// (TYPE, NON_UNIQUE, COLUMN_NAME, ORDINAL_POSITION, ASC_OR_DESC) subset of
    /// each fetched row, in the order `get_data` is called below.
    fn collect_stats(
        stmt: &mut SqliteStatement,
    ) -> Vec<(
        ColumnValue,
        ColumnValue,
        ColumnValue,
        ColumnValue,
        ColumnValue,
    )> {
        let mut out = Vec::new();
        while stmt.fetch().unwrap() == FetchResult::Row {
            out.push((
                stmt.get_data(TYPE_COL, CDataType::Default)
                    .unwrap()
                    .into_owned(),
                stmt.get_data(NON_UNIQUE, CDataType::Default)
                    .unwrap()
                    .into_owned(),
                stmt.get_data(COLUMN_NAME, CDataType::Default)
                    .unwrap()
                    .into_owned(),
                stmt.get_data(ORDINAL_POSITION, CDataType::Default)
                    .unwrap()
                    .into_owned(),
                stmt.get_data(ASC_OR_DESC, CDataType::Default)
                    .unwrap()
                    .into_owned(),
            ));
        }
        out
    }

    #[test]
    fn statistics_reports_table_stat_row_and_indexes_in_order() {
        let conn = setup_stats_db();
        let mut stmt = statistics(&conn, None, None, Some("t"), false).unwrap();
        assert_eq!(stmt.column_count(), 13);
        let rows = collect_stats(&mut stmt);
        // Row 0: table-stat row (TYPE = SQL_TABLE_STAT, NON_UNIQUE NULL, COLUMN_NAME NULL).
        assert_eq!(rows[0].0, ColumnValue::I16(SQL_TABLE_STAT));
        assert_eq!(rows[0].1, ColumnValue::Null);
        assert_eq!(rows[0].2, ColumnValue::Null);
        // Next: the UNIQUE index (NON_UNIQUE = SQL_FALSE = 0) before the non-unique one.
        assert_eq!(rows[1].0, ColumnValue::I16(SQL_INDEX_OTHER));
        assert_eq!(rows[1].1, ColumnValue::I16(SQL_FALSE as i16));
        assert_eq!(rows[1].2, ColumnValue::String("a".into()));
        // Then the non-unique composite index (b, c DESC): 2 rows, NON_UNIQUE = SQL_TRUE = 1.
        assert_eq!(rows[2].1, ColumnValue::I16(1));
        assert_eq!(rows[2].2, ColumnValue::String("b".into()));
        assert_eq!(rows[2].3, ColumnValue::I16(1)); // ORDINAL_POSITION
        assert_eq!(rows[2].4, ColumnValue::String("A".into()));
        assert_eq!(rows[3].2, ColumnValue::String("c".into()));
        assert_eq!(rows[3].3, ColumnValue::I16(2));
        assert_eq!(rows[3].4, ColumnValue::String("D".into())); // c DESC
    }

    #[test]
    fn statistics_unique_only_drops_non_unique_indexes() {
        let conn = setup_stats_db();
        let mut stmt = statistics(&conn, None, None, Some("t"), true).unwrap();
        let rows = collect_stats(&mut stmt);
        // table-stat row + the unique index's single column only.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, ColumnValue::I16(SQL_TABLE_STAT));
        assert_eq!(rows[1].2, ColumnValue::String("a".into()));
    }

    #[test]
    fn statistics_table_without_indexes_returns_only_table_stat_row() {
        let conn = setup_stats_db();
        conn.conn
            .lock()
            .unwrap()
            .execute_batch("CREATE TABLE plain (x INTEGER);")
            .unwrap();
        let mut stmt = statistics(&conn, None, None, Some("plain"), false).unwrap();
        let rows = collect_stats(&mut stmt);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, ColumnValue::I16(SQL_TABLE_STAT));
    }

    #[test]
    fn statistics_with_no_table_returns_empty() {
        let conn = setup_stats_db();
        let mut stmt = statistics(&conn, None, None, None, false).unwrap();
        assert_eq!(stmt.column_count(), 13);
        assert_eq!(stmt.fetch().unwrap(), FetchResult::NoData);
    }

    fn setup_partial_index_db() -> SqliteConnection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tp (a INTEGER, b TEXT);
             CREATE INDEX ix_tp_partial ON tp(a) WHERE a > 0;",
        )
        .unwrap();
        SqliteConnection {
            conn: Mutex::new(conn),
            manual_commit: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[test]
    fn statistics_partial_index_reports_empty_filter_condition() {
        let conn = setup_partial_index_db();
        let mut stmt = statistics(&conn, None, None, Some("tp"), false).unwrap();
        // table-stat row + a single index-column row: exactly one index.
        assert_eq!(stmt.column_count(), 13);
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        // Row 0: table-stat row; skip it.
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        // Row 1: the partial index's single key column.
        assert_eq!(
            stmt.get_data(FILTER_CONDITION, CDataType::Default)
                .unwrap()
                .into_owned(),
            ColumnValue::String(String::new())
        );
        assert_eq!(stmt.fetch().unwrap(), FetchResult::NoData);
    }

    fn setup_expression_index_db() -> SqliteConnection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE te (a INTEGER, b INTEGER);
             CREATE INDEX ix_te_expr ON te(a + b);",
        )
        .unwrap();
        SqliteConnection {
            conn: Mutex::new(conn),
            manual_commit: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[test]
    fn statistics_expression_index_reports_empty_column_name() {
        let conn = setup_expression_index_db();
        let mut stmt = statistics(&conn, None, None, Some("te"), false).unwrap();
        // table-stat row + a single index-column row: exactly one index.
        assert_eq!(stmt.column_count(), 13);
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        // Row 0: table-stat row; skip it.
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        // Row 1: the expression index's key column (key=1, name=NULL).
        assert_eq!(
            stmt.get_data(COLUMN_NAME, CDataType::Default)
                .unwrap()
                .into_owned(),
            ColumnValue::String(String::new())
        );
        assert_eq!(stmt.fetch().unwrap(), FetchResult::NoData);
    }

    fn setup_specialcols_db() -> SqliteConnection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE with_int_pk (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE no_pk (a TEXT, b TEXT);
             CREATE TABLE without_rowid (k TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID;",
        )
        .unwrap();
        SqliteConnection {
            conn: Mutex::new(conn),
            manual_commit: std::sync::atomic::AtomicBool::new(false),
        }
    }

    const SC_SCOPE: u16 = 1;
    const SC_COLUMN_NAME: u16 = 2;
    const SC_DATA_TYPE: u16 = 3;
    const SC_BUFFER_LENGTH: u16 = 6;
    const SC_PSEUDO_COLUMN: u16 = 8;

    #[test]
    fn special_columns_integer_pk_is_reported_as_real_column() {
        let conn = setup_specialcols_db();
        let mut stmt = special_columns(
            &conn,
            IdentifierType::BestRowId,
            None,
            None,
            Some("with_int_pk"),
            Scope::CurRow,
            Nullable::SqlNullable,
        )
        .unwrap();
        assert_eq!(stmt.column_count(), 8);
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            stmt.get_data(SC_COLUMN_NAME, CDataType::Default)
                .unwrap()
                .into_owned(),
            ColumnValue::String("id".into())
        );
        // A declared INTEGER PRIMARY KEY is the 8-byte 64-bit rowid alias, not
        // a plain INTEGER column: DATA_TYPE must be SQL_BIGINT and
        // BUFFER_LENGTH must be 8 (not the 19-byte COLUMN_SIZE-derived value
        // a generic INTEGER column would get).
        assert_eq!(
            stmt.get_data(SC_DATA_TYPE, CDataType::Default)
                .unwrap()
                .into_owned(),
            ColumnValue::I16(SqlDataType::EXT_BIG_INT.0)
        );
        assert_eq!(
            stmt.get_data(SC_BUFFER_LENGTH, CDataType::Default)
                .unwrap()
                .into_owned(),
            ColumnValue::I32(8)
        );
        assert_eq!(
            stmt.get_data(SC_PSEUDO_COLUMN, CDataType::Default)
                .unwrap()
                .into_owned(),
            ColumnValue::I16(SQL_PC_NOT_PSEUDO)
        );
        assert_eq!(stmt.fetch().unwrap(), FetchResult::NoData);
    }

    #[test]
    fn special_columns_rowid_table_reports_rowid_pseudo_column() {
        let conn = setup_specialcols_db();
        let mut stmt = special_columns(
            &conn,
            IdentifierType::BestRowId,
            None,
            None,
            Some("no_pk"),
            Scope::CurRow,
            Nullable::SqlNullable,
        )
        .unwrap();
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            stmt.get_data(SC_COLUMN_NAME, CDataType::Default)
                .unwrap()
                .into_owned(),
            ColumnValue::String("rowid".into())
        );
        assert_eq!(
            stmt.get_data(SC_PSEUDO_COLUMN, CDataType::Default)
                .unwrap()
                .into_owned(),
            ColumnValue::I16(SQL_PC_PSEUDO)
        );
        // The volatile rowid pseudo-column only guarantees TRANSACTION scope.
        assert_eq!(
            stmt.get_data(SC_SCOPE, CDataType::Default)
                .unwrap()
                .into_owned(),
            ColumnValue::I16(Scope::Transaction.into())
        );
    }

    #[test]
    fn special_columns_without_rowid_reports_pk_columns() {
        let conn = setup_specialcols_db();
        let mut stmt = special_columns(
            &conn,
            IdentifierType::BestRowId,
            None,
            None,
            Some("without_rowid"),
            Scope::CurRow,
            Nullable::SqlNullable,
        )
        .unwrap();
        assert_eq!(stmt.fetch().unwrap(), FetchResult::Row);
        assert_eq!(
            stmt.get_data(SC_COLUMN_NAME, CDataType::Default)
                .unwrap()
                .into_owned(),
            ColumnValue::String("k".into())
        );
        assert_eq!(
            stmt.get_data(SC_PSEUDO_COLUMN, CDataType::Default)
                .unwrap()
                .into_owned(),
            ColumnValue::I16(SQL_PC_NOT_PSEUDO)
        );
    }

    #[test]
    fn special_columns_rowver_is_empty() {
        let conn = setup_specialcols_db();
        let mut stmt = special_columns(
            &conn,
            IdentifierType::RowVer,
            None,
            None,
            Some("with_int_pk"),
            Scope::CurRow,
            Nullable::SqlNullable,
        )
        .unwrap();
        assert_eq!(stmt.column_count(), 8);
        assert_eq!(stmt.fetch().unwrap(), FetchResult::NoData);
    }

    #[test]
    fn special_columns_requested_session_scope_on_rowid_is_empty() {
        // The rowid pseudo-column only guarantees TRANSACTION scope; a request for
        // SESSION cannot be met, so the result set is empty (per spec).
        let conn = setup_specialcols_db();
        let mut stmt = special_columns(
            &conn,
            IdentifierType::BestRowId,
            None,
            None,
            Some("no_pk"),
            Scope::Session,
            Nullable::SqlNullable,
        )
        .unwrap();
        assert_eq!(stmt.fetch().unwrap(), FetchResult::NoData);
    }
}
