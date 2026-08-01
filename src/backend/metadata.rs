//! Catalog metadata for the SQLite backend (`tables`, `columns`,
//! `primary_keys`, `foreign_keys`, `statistics`, `special_columns`), derived
//! from SQLite's `PRAGMA` introspection and `sqlite_master`, plus the private
//! query helpers those functions share.

use stackable_odbc_core::types::{
    ColumnRow, ColumnsQuery, ForeignKeyRow, ForeignKeysQuery, IdentifierType, Nullable,
    PrimaryKeyRow, PrimaryKeysQuery, SQL_CASCADE, SQL_INDEX_OTHER, SQL_NO_ACTION,
    SQL_PC_NOT_PSEUDO, SQL_PC_PSEUDO, SQL_RESTRICT, SQL_SET_DEFAULT, SQL_SET_NULL, SQL_TABLE_STAT,
    Scope, SpecialColumnRow, SpecialColumnsQuery, SqlDataType, StatisticsQuery, StatisticsRow,
    TableRow, TablesQuery,
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
use super::{SqliteConnection, map_sqlite_error};
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
) -> ColumnRow {
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
    } else if is_binary {
        i32::try_from(precision).ok()
    } else {
        None
    };

    let ordinal_position = i32::try_from(ordinal + 1).unwrap_or_else(|_| {
        tracing::warn!(ordinal, "ordinal position exceeds i32");
        i32::MAX
    });

    // `catalog`, `schema`, `remarks` and `sql_datetime_sub` are left at their
    // NULL default: SQLite has no catalogs or schemas, no column comments, and
    // no datetime subcode to report.
    ColumnRow::default()
        .table_name(table_name)
        .column_name(col_name)
        .data_type(sql_type.0)
        // Spec (SQLColumns.TYPE_NAME / SQL_DESC_TYPE_NAME): both list bare
        // examples ("CHAR", "VARCHAR", ...), not declarations, so `col_type`
        // ("VARCHAR(50)") matches no `SQLGetTypeInfo` row.
        // `sqlite_bare_type_name` (also used by `SQL_DESC_TYPE_NAME` in
        // execute.rs) returns the bare name that does; the declared length
        // is still carried above via COLUMN_SIZE (`precision`), just not the
        // name.
        .type_name(sqlite_bare_type_name(sql_type))
        .column_size(column_size)
        .buffer_length(0)
        .decimal_digits(scale)
        .num_prec_radix(if is_numeric { Some(10) } else { None })
        .nullable(i16::from(nullable))
        .column_def(dflt_value.map(str::to_string))
        .sql_data_type(sql_type.0)
        .char_octet_length(char_octet_length)
        .ordinal_position(ordinal_position)
        .is_nullable(nullable.as_is_nullable_str().to_string())
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

/// The table types SQLite exposes, for `SQLTables`' `SQL_ALL_TABLE_TYPES`
/// enumeration.
///
/// `sqlite_master.type` also carries `index` and `trigger`, but neither is a
/// table type: `SQLTables`' result set is defined over tables and views, and
/// this list must name exactly the values [`tables`] can put in `TABLE_TYPE`.
///
/// Upper case per the spec, which has applications specify table types in
/// upper case and the driver map them to whatever the data source needs --
/// SQLite spells its own lower case, and [`tables`] does that mapping.
pub(super) fn table_types() -> Vec<std::borrow::Cow<'static, str>> {
    vec![
        std::borrow::Cow::Borrowed(TABLE_TYPE_TABLE),
        std::borrow::Cow::Borrowed(TABLE_TYPE_VIEW),
    ]
}

/// The two `TABLE_TYPE` values this driver reports, named so [`table_types`]
/// and [`tables`] cannot drift apart.
const TABLE_TYPE_TABLE: &str = "TABLE";
const TABLE_TYPE_VIEW: &str = "VIEW";

/// Rows for `SQLTables`.
///
/// The `SQL_ALL_CATALOGS` / `SQL_ALL_SCHEMAS` / `SQL_ALL_TABLE_TYPES`
/// enumerations no longer reach here: core detects them from the raw arguments
/// and answers them from `supports_catalogs`, `supports_schemas` and
/// [`table_types`]. Rows are returned unsorted; core orders them by
/// TABLE_TYPE, TABLE_CAT, TABLE_SCHEM, TABLE_NAME.
pub(super) fn tables(
    conn: &SqliteConnection,
    query: &TablesQuery<'_>,
) -> Result<Vec<TableRow>, SqliteError> {
    // ODBC spec: empty string is a valid (but useless for SQLite) filter; treat
    // as no-filter. Treat "%" (match-all wildcard) as no-filter too, to avoid
    // LIKE '%' overhead -- an ordinary query is all that can arrive now.
    let table = query.table().filter(|s| !s.is_empty() && *s != "%");

    // `TableType` is a value list, not a pattern, and core has already split it
    // on commas and stripped the optional single quotes -- so what arrives is
    // the parsed values, with empty ones already dropped. A lone "%" still
    // reaches here: the `SQL_ALL_TABLE_TYPES` enumeration core answers itself
    // additionally requires the other three arguments to be empty strings, so
    // "%" alongside a table pattern is an ordinary query. It is not a pattern
    // either, and no table type is literally named "%", so read it as the
    // no-filter an application sending "%" everywhere means.
    let table_types: &[String] = match query.table_types() {
        [only] if only == "%" => &[],
        types => types,
    };

    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;

    // TableName is a search pattern (ODBC §8.3); use LIKE so "%" works as
    // wildcard. No ORDER BY: core sorts the result set.
    let sql = if table.is_some() {
        "SELECT name, type FROM sqlite_master WHERE type IN ('table', 'view') AND name LIKE ?1 ESCAPE '\\'"
    } else {
        "SELECT name, type FROM sqlite_master WHERE type IN ('table', 'view')"
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
        let odbc_type = if type_str == "view" {
            TABLE_TYPE_VIEW
        } else {
            TABLE_TYPE_TABLE
        };

        // Filter by table type if the value list named any.
        if !table_types.is_empty()
            && !table_types
                .iter()
                .any(|a| a.eq_ignore_ascii_case(odbc_type))
        {
            continue;
        }

        rows.push(
            TableRow::default()
                .name(name)
                .table_type(odbc_type.to_string()),
        );
    }

    Ok(rows)
}

pub(super) fn columns(
    conn: &SqliteConnection,
    query: &ColumnsQuery<'_>,
) -> Result<Vec<ColumnRow>, SqliteError> {
    // Same normalization as tables(): empty string and "%" both mean "no filter".
    let table = query.table().filter(|s| !s.is_empty() && *s != "%");
    let column = query.column().filter(|s| !s.is_empty() && *s != "%");

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

    Ok(rows)
}

/// Return primary key columns for the given table.
///
/// Uses `PRAGMA table_info(table)` and filters rows where `pk > 0`.
/// The `pk` column is the 1-based key sequence number.
///
/// Rows are returned unsorted; core orders them by TABLE_CAT, TABLE_SCHEM,
/// TABLE_NAME, KEY_SEQ.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprimarykeys-function>
pub(super) fn primary_keys(
    conn: &SqliteConnection,
    query: &PrimaryKeysQuery<'_>,
) -> Result<Vec<PrimaryKeyRow>, SqliteError> {
    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;

    // Collect table names to query (either the specific one or all tables).
    let table_names = tables_to_inspect(&db, query.table()).map_err(map_sqlite_error)?;

    let mut result_rows: Vec<PrimaryKeyRow> = Vec::new();
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

        // Not sorted by KEY_SEQ here: core sorts the result set on that key,
        // and a second ordering in the backend is one more place for it to be
        // wrong.
        for (key_seq, col_name) in pk_cols {
            // `pk_name` is left NULL: SQLite does not record a primary key
            // constraint name.
            result_rows.push(
                PrimaryKeyRow::default()
                    .table_name(table_name.clone())
                    .column_name(col_name)
                    .key_seq(i16::try_from(key_seq).unwrap_or_else(|_| {
                        tracing::warn!(key_seq, "key sequence exceeds i16");
                        i16::MAX
                    })),
            );
        }
    }

    Ok(result_rows)
}

/// Return foreign key relationships involving the given tables.
///
/// If `fk_table` is given, uses `PRAGMA foreign_key_list(fk_table)` to enumerate the FKs
/// defined on that table. If only `pk_table` is given, all tables are scanned.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlforeignkeys-function>
pub(super) fn foreign_keys(
    conn: &SqliteConnection,
    query: &ForeignKeysQuery<'_>,
) -> Result<Vec<ForeignKeyRow>, SqliteError> {
    let pk_table = query.pk_table();

    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;

    // Which FK tables do we query?
    let fk_table_names = tables_to_inspect(&db, query.fk_table()).map_err(map_sqlite_error)?;

    let mut result_rows: Vec<ForeignKeyRow> = Vec::new();

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

            // PKCOLUMN_NAME is one of the four columns the spec marks "not
            // NULL", which `ForeignKeyRow` enforces. `PRAGMA foreign_key_list`
            // leaves `to` NULL for an implicit reference (`REFERENCES parent`
            // with no column list), which SQLite defines as referencing the
            // parent's PRIMARY KEY -- so the name is recoverable, and is
            // resolved rather than reported as a NULL the column cannot hold.
            let pk_column_name = match to_col {
                Some(c) => c,
                None => parent_pk_column(&db, &referenced_table, seq)?.unwrap_or_else(|| {
                    // Reachable only for a schema SQLite itself rejects at DML
                    // time ("foreign key mismatch"), so there is no correct
                    // name to report.
                    tracing::warn!(
                        parent = %referenced_table,
                        seq,
                        "foreign key references a parent with no primary key column at \
                         this position; reporting PKCOLUMN_NAME as an empty string"
                    );
                    String::new()
                }),
            };

            // `fk_name`, `pk_name` and `deferrability` are left NULL: SQLite's
            // PRAGMA reports none of the three.
            result_rows.push(
                ForeignKeyRow::default()
                    .pk_table_name(referenced_table)
                    .pk_column_name(pk_column_name)
                    .fk_table_name(fk_tbl.clone())
                    .fk_column_name(from_col)
                    // 1-based
                    .key_seq(i16::try_from(seq + 1).unwrap_or_else(|_| {
                        tracing::warn!(seq, "key sequence exceeds i16");
                        i16::MAX
                    }))
                    .update_rule(fk_action_to_odbc(&on_update))
                    .delete_rule(fk_action_to_odbc(&on_delete)),
            );
        }
    }

    Ok(result_rows)
}

/// The parent table's primary key column at position `seq` (0-based), for a
/// foreign key declared without an explicit column list.
///
/// `PRAGMA table_info`'s `pk` column is the 1-based position within the
/// primary key, so the column wanted is the one with `pk == seq + 1`.
fn parent_pk_column(
    db: &rusqlite::Connection,
    parent: &str,
    seq: i64,
) -> Result<Option<String>, SqliteError> {
    let mut stmt = db
        .prepare("SELECT name FROM pragma_table_info(?1) WHERE pk = ?2")
        .map_err(map_sqlite_error)?;
    let mut rows = stmt
        .query(rusqlite::params![parent, seq + 1])
        .map_err(map_sqlite_error)?;
    match rows.next().map_err(map_sqlite_error)? {
        Some(row) => Ok(Some(row.get(0).map_err(map_sqlite_error)?)),
        None => Ok(None),
    }
}

/// Return index statistics for a single table (SQLStatistics).
///
/// Emits a `SQL_TABLE_STAT` row (CARDINALITY from `sqlite_stat1` when
/// `ANALYZE` has populated it, else NULL; PAGES always NULL, honoring
/// SQL_QUICK), then one row per key column of each index from
/// `PRAGMA index_list` / `PRAGMA index_xinfo`.
///
/// Rows are returned unsorted. Core orders them per spec by NON_UNIQUE, TYPE,
/// INDEX_QUALIFIER, INDEX_NAME, ORDINAL_POSITION, and the table-stat row still
/// comes first because its NON_UNIQUE is NULL and this driver reports
/// `SQL_NC_LOW` for `SQL_NULL_COLLATION` -- core's sorter takes NULL placement
/// from that hook rather than choosing for itself.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlstatistics-function>
pub(super) fn statistics(
    conn: &SqliteConnection,
    query: &StatisticsQuery<'_>,
) -> Result<Vec<StatisticsRow>, SqliteError> {
    use stackable_odbc_core::types::{SQL_FALSE, SQL_TRUE};

    let unique_only = query.unique_only();

    // SQLStatistics.TableName cannot be a search pattern; an absent name has no
    // table to describe, so there are no rows to report.
    let Some(table) = query.table().filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };

    let db = conn.conn.lock().map_err(|e| SqliteError::General {
        message: format!("Mutex poisoned: {e}"),
    })?;

    // CARDINALITY for the table-stat row: read sqlite_stat1 only if present.
    let cardinality = table_cardinality_from_stat1(&db, table);

    // The table-stat row. TABLE_NAME and TYPE are the NOT NULL columns;
    // everything index-specific is NULL.
    let mut rows: Vec<StatisticsRow> = vec![
        StatisticsRow::default()
            .table_name(table)
            .index_type(SQL_TABLE_STAT)
            .cardinality(cardinality),
    ];

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

            // `index_qualifier`, `cardinality` and `pages` are left NULL: the
            // first has no meaning without catalogs, and the latter two belong
            // to the table-stat row above.
            rows.push(
                StatisticsRow::default()
                    .table_name(table)
                    .non_unique(if *is_unique {
                        SQL_FALSE as i16
                    } else {
                        SQL_TRUE as i16
                    })
                    .index_name(index_name.clone())
                    .index_type(SQL_INDEX_OTHER)
                    .ordinal_position(ordinal)
                    // "" for an expression index, which has no column name.
                    .column_name(col_name.unwrap_or_default())
                    .asc_or_desc(if desc != 0 { "D" } else { "A" }.to_string())
                    .filter_condition(if *is_partial {
                        Some(String::new())
                    } else {
                        None
                    }),
            );
        }
    }

    Ok(rows)
}

/// Read the table row count from `sqlite_stat1` if `ANALYZE` has populated it.
/// The `stat` column's first whitespace-delimited token is the table row count.
/// Returns `None` when the stat table or row is absent.
fn table_cardinality_from_stat1(db: &rusqlite::Connection, table: &str) -> Option<i32> {
    let query = "SELECT stat FROM sqlite_stat1 WHERE tbl = ?1 AND idx IS NULL LIMIT 1";
    let stat: Result<String, _> = db.query_row(query, rusqlite::params![table], |r| r.get(0));
    match stat {
        Ok(s) => s
            .split_whitespace()
            .next()
            .and_then(|tok| tok.parse::<i32>().ok()),
        Err(_) => None, // no sqlite_stat1 (no ANALYZE) or no row
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
/// `query.nullable()` is not read: every identifier this reports is NOT NULL,
/// so the `Nullable` filter can never exclude a row.
pub(super) fn special_columns(
    conn: &SqliteConnection,
    query: &SpecialColumnsQuery<'_>,
) -> Result<Vec<SpecialColumnRow>, SqliteError> {
    let empty = || Ok(Vec::new());

    let scope = query.scope();

    // ROWVER: SQLite has no auto-updated columns.
    if matches!(query.identifier_type(), IdentifierType::RowVer) {
        return empty();
    }
    let Some(table) = query.table().filter(|s| !s.is_empty()) else {
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
    let (rows, guaranteed): (Vec<SpecialColumnRow>, Scope) = if let Some(col) = integer_pk {
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

    Ok(rows)
}

/// Build one SQLSpecialColumns row for a declared column, deriving its SQL type
/// from the same mapping SQLColumns uses.
fn special_column_row(name: &str, decl_type: &str, pseudo: i16, scope: Scope) -> SpecialColumnRow {
    let sql_type = sqlite_type_to_sql_data_type(decl_type);
    let column_size = i32::try_from(sqlite_declared_type_precision(decl_type)).unwrap_or(i32::MAX);
    let scale = sqlite_declared_type_scale(decl_type);
    SpecialColumnRow::default()
        .scope(i16::from(scope))
        .column_name(name)
        .data_type(sql_type.0)
        .type_name(sqlite_bare_type_name(sql_type))
        .column_size(column_size)
        // BUFFER_LENGTH (approx: transfer octet length)
        .buffer_length(column_size)
        .decimal_digits(if scale > 0 { Some(scale) } else { None })
        .pseudo_column(pseudo)
}

/// Build one SQLSpecialColumns row for the 64-bit rowid pseudo-column / an
/// INTEGER PRIMARY KEY reported as BIGINT.
fn special_column_row_bigint(name: &str, pseudo: i16, scope: Scope) -> SpecialColumnRow {
    let sql_type = SqlDataType::EXT_BIG_INT;
    let column_size = i32::try_from(default_precision_for_type(sql_type)).unwrap_or(i32::MAX);
    // `decimal_digits` is left NULL: not applicable to integers.
    SpecialColumnRow::default()
        .scope(i16::from(scope))
        .column_name(name)
        .data_type(sql_type.0)
        .type_name(sqlite_bare_type_name(sql_type))
        .column_size(column_size)
        .buffer_length(8) // 8 bytes for a 64-bit integer
        .pseudo_column(pseudo)
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
    use stackable_odbc_core::types::SQL_FALSE;

    /// Wrap a raw `rusqlite::Connection` the way [`SqliteBackend::connect`]
    /// does, including the interrupt handle every statement's cancel token is
    /// cloned from.
    fn wrap(conn: rusqlite::Connection) -> SqliteConnection {
        let interrupt = std::sync::Arc::new(conn.get_interrupt_handle());
        SqliteConnection {
            conn: Mutex::new(conn),
            interrupt,
            manual_commit: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// These tests assert what the backend now owns: which rows exist and what
    /// each column holds. Column *order* and row *order* moved to core, which
    /// converts these structs to the spec's layout and sorts them — so an
    /// ordering assertion belongs at the FFI level, where core's sort has
    /// actually run, not here. See `ffi_integration_tests.rs`.
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
        wrap(conn)
    }

    #[test]
    fn tables_returns_all_tables_and_views() {
        let conn = setup_test_db();
        let rows = tables(&conn, &TablesQuery::default()).unwrap();

        let names: Vec<&str> = rows.iter().filter_map(|r| r.name.as_deref()).collect();
        for expected in ["empty_table", "types_test", "types_view", "parent", "child"] {
            assert!(names.contains(&expected), "missing {expected} in {names:?}");
        }

        let types: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.table_type.as_deref())
            .collect();
        assert_eq!(types.iter().filter(|t| **t == TABLE_TYPE_TABLE).count(), 4);
        assert_eq!(types.iter().filter(|t| **t == TABLE_TYPE_VIEW).count(), 1);

        // SQLite has neither, and every row says so.
        assert!(
            rows.iter()
                .all(|r| r.catalog.is_none() && r.schema.is_none())
        );
    }

    /// Every value [`table_types`] declares must be one [`tables`] can actually
    /// put in `TABLE_TYPE`, and vice versa. Core serves `SQL_ALL_TABLE_TYPES`
    /// from the first and the result set from the second, so a mismatch is a
    /// data source that lists a type no query returns.
    #[test]
    fn declared_table_types_are_exactly_the_ones_tables_reports() {
        let conn = setup_test_db();
        let declared: Vec<String> = table_types().iter().map(|t| t.to_string()).collect();

        let mut reported: Vec<String> = tables(&conn, &TablesQuery::default())
            .unwrap()
            .into_iter()
            .filter_map(|r| r.table_type)
            .collect();
        reported.sort();
        reported.dedup();

        let mut declared_sorted = declared.clone();
        declared_sorted.sort();
        assert_eq!(
            declared_sorted, reported,
            "table_types() and the TABLE_TYPE values tables() emits disagree"
        );
    }

    #[test]
    fn tables_filter_by_table_type() {
        let conn = setup_test_db();
        // Core hands the backend the already-parsed value list, so the test
        // supplies one rather than the raw comma-separated argument.
        let only_tables = [TABLE_TYPE_TABLE.to_string()];
        let rows = tables(
            &conn,
            &TablesQuery::default().with_table_types(&only_tables[..]),
        )
        .unwrap();
        let names: Vec<&str> = rows.iter().filter_map(|r| r.name.as_deref()).collect();
        assert!(names.contains(&"empty_table"));
        assert!(names.contains(&"types_test"));
        assert!(!names.contains(&"types_view"));
    }

    #[test]
    fn tables_filter_by_name() {
        let conn = setup_test_db();
        let rows = tables(&conn, &TablesQuery::default().with_table("types_test")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name.as_deref(), Some("types_test"));
    }

    #[test]
    fn tables_table_name_honors_escape_character() {
        let conn = setup_test_db();
        // `empty\_table` with ESCAPE '\' means a literal underscore: matches
        // exactly "empty_table". Without ESCAPE the `_` is a wildcard and the
        // stray backslash matches nothing.
        let rows = tables(&conn, &TablesQuery::default().with_table("empty\\_table")).unwrap();
        let names: Vec<&str> = rows.iter().filter_map(|r| r.name.as_deref()).collect();
        assert_eq!(names, vec!["empty_table"]);
    }

    #[test]
    fn tables_table_type_percent_with_table_wildcard_lists_tables() {
        let conn = setup_test_db();
        // TableType="%" with TableName="%" is not an enumeration — core only
        // treats "%" as `SQL_ALL_TABLE_TYPES` when the other three arguments
        // are empty strings, so this reaches the backend as an ordinary query
        // and must list actual tables and views.
        let percent = ["%".to_string()];
        let rows = tables(
            &conn,
            &TablesQuery::default()
                .with_catalog("")
                .with_schema("")
                .with_table("%")
                .with_table_types(&percent[..]),
        )
        .unwrap();
        let names: Vec<&str> = rows.iter().filter_map(|r| r.name.as_deref()).collect();
        assert!(
            names.contains(&"types_test"),
            "expected real table listing, got {names:?}"
        );
    }

    #[test]
    fn columns_returns_correct_columns() {
        let conn = setup_test_db();
        let rows = columns(&conn, &ColumnsQuery::default().with_table("types_test")).unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.column_name.as_str()).collect();
        assert_eq!(names, vec!["id", "val", "label"]);
        // ORDINAL_POSITION is 1-based and is what core sorts on.
        let ordinals: Vec<i32> = rows.iter().map(|r| r.ordinal_position).collect();
        assert_eq!(ordinals, vec![1, 2, 3]);
    }

    #[test]
    fn columns_filter_by_column_name() {
        let conn = setup_test_db();
        let rows = columns(
            &conn,
            &ColumnsQuery::default()
                .with_table("types_test")
                .with_column("val"),
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].column_name, "val");
    }

    #[test]
    fn columns_column_name_is_a_like_pattern() {
        let conn = setup_test_db();
        // types_test columns: id, val, label. "%l%" matches val and label.
        // Under the old exact-match filter this returned zero rows.
        let rows = columns(
            &conn,
            &ColumnsQuery::default()
                .with_table("types_test")
                .with_column("%l%"),
        )
        .unwrap();
        let mut names: Vec<&str> = rows.iter().map(|r| r.column_name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["label", "val"]);
    }

    #[test]
    fn columns_nullable_flag() {
        let conn = setup_test_db();
        // empty_table: id INTEGER PRIMARY KEY, name TEXT NOT NULL
        // SQLite PRAGMA table_info reports notnull=0 for INTEGER PRIMARY KEY (PK does not imply
        // NOT NULL in SQLite's PRAGMA), and notnull=1 for the explicit NOT NULL constraint.
        let rows = columns(&conn, &ColumnsQuery::default().with_table("empty_table")).unwrap();
        assert_eq!(rows.len(), 2);

        let nullable_of = |name: &str| {
            rows.iter()
                .find(|r| r.column_name == name)
                .unwrap_or_else(|| panic!("no column {name}"))
                .nullable
        };
        // id INTEGER PRIMARY KEY: PRAGMA notnull=0, so reported as nullable
        assert_eq!(nullable_of("id"), i16::from(Nullable::SqlNullable));
        // name TEXT NOT NULL: PRAGMA notnull=1, so reported as not null
        assert_eq!(nullable_of("name"), i16::from(Nullable::SqlNoNulls));
    }

    #[test]
    fn build_column_row_text_column_has_char_octet_length() {
        // sqlite_type_to_sql_data_type() maps every character declared type
        // (including VARCHAR) to SqlDataType::EXT_W_VARCHAR, never to the bare
        // SqlDataType::VARCHAR, so build_column_row()'s `is_char` compares
        // against EXT_W_VARCHAR. CHAR_OCTET_LENGTH must then be
        // declared_length * BYTES_PER_CHAR for a text column, not NULL.
        let row = build_column_row("t", "label", "VARCHAR(50)", false, 0, None);
        assert_eq!(row.char_octet_length, Some(50 * BYTES_PER_CHAR));
    }

    #[test]
    fn build_column_row_blob_column_has_declared_length_char_octet_length() {
        // CHAR_OCTET_LENGTH also applies to binary columns (ODBC 3.0 SQLColumns
        // column 16), but a BLOB's declared length is already a byte count and
        // must be passed through as-is, not multiplied by BYTES_PER_CHAR.
        let row = build_column_row("t", "data", "BLOB(50)", false, 0, None);
        assert_eq!(row.char_octet_length, Some(50));
    }

    #[test]
    fn build_column_row_non_character_non_binary_column_has_null_char_octet_length() {
        // INTEGER is neither character nor binary data, so CHAR_OCTET_LENGTH
        // is NULL per the ODBC spec.
        let row = build_column_row("t", "id", "INTEGER", false, 0, None);
        assert_eq!(row.char_octet_length, None);
    }

    #[test]
    fn build_column_row_huge_declared_length_does_not_overflow() {
        // sqlite_declared_type_precision() parses the declared length out of
        // the type string. A column declared VARCHAR(2000000000) yields a
        // precision whose product with BYTES_PER_CHAR (4) overflows i32
        // (2_000_000_000 * 4 = 8_000_000_000). The checked multiplication
        // reports NULL instead of wrapping or panicking.
        let row = build_column_row("t", "label", "VARCHAR(2000000000)", false, 0, None);
        assert_eq!(row.char_octet_length, None);
    }

    #[test]
    fn primary_keys_returns_pk_column() {
        let conn = setup_test_db();
        let rows = primary_keys(&conn, &PrimaryKeysQuery::default().with_table("parent")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].table_name, "parent");
        assert_eq!(rows[0].column_name, "pk");
        assert_eq!(rows[0].key_seq, 1);
    }

    #[test]
    fn primary_keys_no_pk_returns_empty() {
        let conn = setup_test_db();
        // types_test has no PRIMARY KEY constraint
        let rows =
            primary_keys(&conn, &PrimaryKeysQuery::default().with_table("types_test")).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn foreign_keys_by_fk_table() {
        let conn = setup_test_db();
        // pk table unfiltered; fk table `child`.
        let rows =
            foreign_keys(&conn, &ForeignKeysQuery::default().with_fk_table("child")).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pk_table_name, "parent");
        assert_eq!(rows[0].pk_column_name, "pk");
        assert_eq!(rows[0].fk_table_name, "child");
        assert_eq!(rows[0].fk_column_name, "parent_pk");
        assert_eq!(rows[0].key_seq, 1);
    }

    #[test]
    fn foreign_keys_no_fk_returns_empty() {
        let conn = setup_test_db();
        // parent has no outgoing foreign keys
        let rows =
            foreign_keys(&conn, &ForeignKeysQuery::default().with_fk_table("parent")).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn foreign_keys_by_pk_table() {
        let conn = setup_test_db();
        // pk table `parent`; fk table unfiltered.
        let rows =
            foreign_keys(&conn, &ForeignKeysQuery::default().with_pk_table("parent")).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pk_table_name, "parent");
        assert_eq!(rows[0].fk_table_name, "child");
    }

    /// `PKCOLUMN_NAME` is one of the columns the spec marks "not NULL", and
    /// `ForeignKeyRow` enforces that. `REFERENCES parent` with no column list
    /// leaves `PRAGMA foreign_key_list`'s `to` NULL, which this driver used to
    /// report as a NULL `PKCOLUMN_NAME` — a value the column cannot hold.
    /// SQLite defines the implicit target as the parent's primary key, so the
    /// name is recovered rather than dropped.
    #[test]
    fn foreign_keys_implicit_reference_resolves_the_parent_primary_key() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE p (pk INTEGER PRIMARY KEY, info TEXT);
             CREATE TABLE c (id INTEGER PRIMARY KEY, p_ref INTEGER REFERENCES p);",
        )
        .unwrap();
        let conn = wrap(conn);

        let rows = foreign_keys(&conn, &ForeignKeysQuery::default().with_fk_table("c")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pk_table_name, "p");
        assert_eq!(
            rows[0].pk_column_name, "pk",
            "an implicit REFERENCES must resolve to the parent's primary key column"
        );
    }

    /// A composite implicit reference resolves each position to the parent
    /// primary key column at the same position, not always the first.
    #[test]
    fn foreign_keys_implicit_composite_reference_resolves_per_position() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE p (a INTEGER, b INTEGER, PRIMARY KEY (a, b));
             CREATE TABLE c (x INTEGER, y INTEGER, FOREIGN KEY (x, y) REFERENCES p);",
        )
        .unwrap();
        let conn = wrap(conn);

        let mut rows =
            foreign_keys(&conn, &ForeignKeysQuery::default().with_fk_table("c")).unwrap();
        rows.sort_by_key(|r| r.key_seq);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .map(|r| (r.fk_column_name.as_str(), r.pk_column_name.as_str()))
                .collect::<Vec<_>>(),
            vec![("x", "a"), ("y", "b")]
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
        wrap(conn)
    }

    /// The rows [`statistics`] produces, keyed by index name and column so the
    /// assertions do not depend on an order core owns.
    #[test]
    fn statistics_reports_a_table_stat_row_and_one_row_per_index_key_column() {
        let conn = setup_stats_db();
        let rows = statistics(&conn, &StatisticsQuery::new(false).with_table("t")).unwrap();

        let stat_rows: Vec<&StatisticsRow> = rows
            .iter()
            .filter(|r| r.index_type == SQL_TABLE_STAT)
            .collect();
        assert_eq!(stat_rows.len(), 1, "exactly one table-stat row");
        assert_eq!(stat_rows[0].table_name, "t");
        // The table-stat row's NULL NON_UNIQUE is what puts it first once core
        // sorts, given this driver's SQL_NC_LOW null collation.
        assert_eq!(stat_rows[0].non_unique, None);
        assert_eq!(stat_rows[0].column_name, None);

        let index_row = |index: &str, column: &str| {
            rows.iter()
                .find(|r| {
                    r.index_name.as_deref() == Some(index)
                        && r.column_name.as_deref() == Some(column)
                })
                .unwrap_or_else(|| panic!("no row for {index}.{column}"))
        };

        let unique = index_row("ux_t_a", "a");
        assert_eq!(unique.index_type, SQL_INDEX_OTHER);
        assert_eq!(unique.non_unique, Some(SQL_FALSE as i16));
        assert_eq!(unique.ordinal_position, Some(1));

        // The non-unique composite index (b, c DESC).
        let b = index_row("ix_t_bc", "b");
        assert_eq!(b.non_unique, Some(1));
        assert_eq!(b.ordinal_position, Some(1));
        assert_eq!(b.asc_or_desc.as_deref(), Some("A"));

        let c = index_row("ix_t_bc", "c");
        assert_eq!(c.ordinal_position, Some(2));
        assert_eq!(c.asc_or_desc.as_deref(), Some("D"));
    }

    #[test]
    fn statistics_unique_only_drops_non_unique_indexes() {
        let conn = setup_stats_db();
        let rows = statistics(&conn, &StatisticsQuery::new(true).with_table("t")).unwrap();
        // table-stat row + the unique index's single column only.
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .filter(|r| r.index_type == SQL_TABLE_STAT)
                .count(),
            1
        );
        assert!(rows.iter().any(
            |r| r.column_name.as_deref() == Some("a") && r.non_unique == Some(SQL_FALSE as i16)
        ));
    }

    #[test]
    fn statistics_table_without_indexes_returns_only_table_stat_row() {
        let conn = setup_stats_db();
        conn.conn
            .lock()
            .unwrap()
            .execute_batch("CREATE TABLE plain (x INTEGER);")
            .unwrap();
        let rows = statistics(&conn, &StatisticsQuery::new(false).with_table("plain")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].index_type, SQL_TABLE_STAT);
    }

    #[test]
    fn statistics_with_no_table_returns_empty() {
        let conn = setup_stats_db();
        assert!(
            statistics(&conn, &StatisticsQuery::new(false))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn statistics_partial_index_reports_empty_filter_condition() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tp (a INTEGER, b TEXT);
             CREATE INDEX ix_tp_partial ON tp(a) WHERE a > 0;",
        )
        .unwrap();
        let conn = wrap(conn);

        let rows = statistics(&conn, &StatisticsQuery::new(false).with_table("tp")).unwrap();
        // table-stat row + a single index-column row: exactly one index.
        assert_eq!(rows.len(), 2);
        let index = rows
            .iter()
            .find(|r| r.index_type == SQL_INDEX_OTHER)
            .expect("the partial index's key column");
        assert_eq!(index.filter_condition.as_deref(), Some(""));
    }

    #[test]
    fn statistics_expression_index_reports_empty_column_name() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE te (a INTEGER, b INTEGER);
             CREATE INDEX ix_te_expr ON te(a + b);",
        )
        .unwrap();
        let conn = wrap(conn);

        let rows = statistics(&conn, &StatisticsQuery::new(false).with_table("te")).unwrap();
        // table-stat row + a single index-column row: exactly one index.
        assert_eq!(rows.len(), 2);
        let index = rows
            .iter()
            .find(|r| r.index_type == SQL_INDEX_OTHER)
            .expect("the expression index's key column (key=1, name=NULL)");
        assert_eq!(index.column_name.as_deref(), Some(""));
    }

    fn setup_specialcols_db() -> SqliteConnection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE with_int_pk (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE no_pk (a TEXT, b TEXT);
             CREATE TABLE without_rowid (k TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID;",
        )
        .unwrap();
        wrap(conn)
    }

    #[test]
    fn special_columns_integer_pk_is_reported_as_real_column() {
        let conn = setup_specialcols_db();
        let rows = special_columns(
            &conn,
            &SpecialColumnsQuery::new(
                IdentifierType::BestRowId,
                Scope::CurRow,
                Nullable::SqlNullable,
            )
            .with_table("with_int_pk"),
        )
        .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].column_name, "id");
        // A declared INTEGER PRIMARY KEY is the 8-byte 64-bit rowid alias, not
        // a plain INTEGER column: DATA_TYPE must be SQL_BIGINT and
        // BUFFER_LENGTH must be 8 (not the 19-byte COLUMN_SIZE-derived value
        // a generic INTEGER column would get).
        assert_eq!(rows[0].data_type, SqlDataType::EXT_BIG_INT.0);
        assert_eq!(rows[0].buffer_length, Some(8));
        assert_eq!(rows[0].pseudo_column, Some(SQL_PC_NOT_PSEUDO));
    }

    #[test]
    fn special_columns_rowid_table_reports_rowid_pseudo_column() {
        let conn = setup_specialcols_db();
        let rows = special_columns(
            &conn,
            &SpecialColumnsQuery::new(
                IdentifierType::BestRowId,
                Scope::CurRow,
                Nullable::SqlNullable,
            )
            .with_table("no_pk"),
        )
        .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].column_name, "rowid");
        assert_eq!(rows[0].pseudo_column, Some(SQL_PC_PSEUDO));
        // The volatile rowid pseudo-column only guarantees TRANSACTION scope.
        assert_eq!(rows[0].scope, Some(Scope::Transaction.into()));
    }

    #[test]
    fn special_columns_without_rowid_reports_pk_columns() {
        let conn = setup_specialcols_db();
        let rows = special_columns(
            &conn,
            &SpecialColumnsQuery::new(
                IdentifierType::BestRowId,
                Scope::CurRow,
                Nullable::SqlNullable,
            )
            .with_table("without_rowid"),
        )
        .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].column_name, "k");
        assert_eq!(rows[0].pseudo_column, Some(SQL_PC_NOT_PSEUDO));
    }

    #[test]
    fn special_columns_rowver_is_empty() {
        let conn = setup_specialcols_db();
        assert!(
            special_columns(
                &conn,
                &SpecialColumnsQuery::new(
                    IdentifierType::RowVer,
                    Scope::CurRow,
                    Nullable::SqlNullable,
                )
                .with_table("with_int_pk"),
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn special_columns_requested_session_scope_on_rowid_is_empty() {
        // The rowid pseudo-column only guarantees TRANSACTION scope; a request for
        // SESSION cannot be met, so the result set is empty (per spec).
        let conn = setup_specialcols_db();
        assert!(
            special_columns(
                &conn,
                &SpecialColumnsQuery::new(
                    IdentifierType::BestRowId,
                    Scope::Session,
                    Nullable::SqlNullable,
                )
                .with_table("no_pk"),
            )
            .unwrap()
            .is_empty()
        );
    }
}
