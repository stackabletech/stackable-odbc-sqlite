//! Conversion between SQLite's `rusqlite::types::Value` and `stackable-odbc-core`'s
//! [`ColumnValue`], including SQLite's numeric datetime storage encodings and
//! the declared fractional-seconds precision this driver reports.

use rusqlite::types::Value;
use stackable_odbc_core::types::{ColumnValue, SqlDataType, column_size};

/// This driver's declared maximum fractional-seconds precision for
/// TIME/TIMESTAMP columns. SQLite has no native temporal type system at all:
/// `TIME`/`TIMESTAMP` values are plain `TEXT` (or the numeric encodings
/// decoded below) with no declared scale in the schema, so there is no
/// larger, separately-declarable server maximum to report (as there would be
/// for a backend with a `time(N)`/`timestamp(N)` schema type): "the maximum
/// this data source supports" and "what a column actually delivers" are the
/// same number by construction here.
///
/// The number itself comes from SQLite's own documented datetime format.
/// SQLite's date-and-time-functions page
/// (<https://www.sqlite.org/lang_datefunc.html>) lists
/// `YYYY-MM-DD HH:MM:SS.SSS` (format 4/7) as one of the ISO-8601 formats it
/// both accepts and produces, and documents `strftime`'s `%f` substitution
/// as "fractional seconds: SS.SSS" (three digits), the format SQLite's own
/// date/time functions (e.g. `datetime('now')`) render by default. That
/// makes 3 the honest "this is what a SQLite TIME/TIMESTAMP column
/// ordinarily looks like" figure for a driver with no schema-level scale to
/// read a tighter or looser bound from, matching what
/// `column_value_to_rusqlite`'s `Time`/`Timestamp` arms below produce for
/// this driver's own bound parameters.
///
/// A value with a genuinely finer fraction than 3 digits (e.g. hand-written
/// as `'...:15.123456'`, or read back through
/// `column_value_to_rusqlite`'s current 9-digit-nanosecond rendering) still
/// round-trips as data: SQLite text storage is unbounded, so the extra
/// digits are neither rejected nor truncated in storage, only
/// under-reported by `SQL_DESC_DISPLAY_SIZE`/`COLUMN_SIZE`. That is the same
/// kind of "declared vs. actual" gap every other undeclared-length default in
/// `default_precision_for_type` already carries, for the same reason (no
/// real schema constraint to consult). It is an accepted, general
/// limitation of describing a dynamically typed column ahead of fetching
/// it, not something this specific constant introduces.
pub(crate) const MAX_FRACTIONAL_SECONDS_PRECISION: i16 = 3;

// ---------------------------------------------------------------------------
// SQLite's numeric datetime storage encodings
// ---------------------------------------------------------------------------
//
// SQLite is dynamically typed and has no dedicated DATE/TIME/DATETIME storage
// class: a column declared as one of those types (and reported to the
// application as `SQL_TYPE_DATE` / `SQL_TYPE_TIME` / `SQL_TYPE_TIMESTAMP` via
// [`sqlite_type_to_sql_data_type`]) may still physically hold any of the three
// formats SQLite's own date/time functions document and produce: ISO-8601
// text, an `INTEGER` count of seconds since the Unix epoch, or a `REAL`
// Julian day number (days since noon, proleptic Gregorian -4713-11-24). Text
// is already handled generically by `stackable-odbc-core` (`ColumnValue::String` converts
// to any C datetime type per the ODBC conversion matrix); the two numeric
// encodings are a SQLite-specific convention, so they are decoded here, at
// fetch time, where the column's declared type is known. `stackable-odbc-core`
// must not carry this backend-specific knowledge (see its `write_column_value`
// doc comment).

/// Convert a [`ColumnValue`] (from ODBC parameter binding) to a [`rusqlite::types::Value`]
/// so it can be passed to `params_from_iter` in parameterized queries.
///
/// Date/Time/Timestamp values are formatted as ISO-8601 strings, which SQLite
/// stores and compares correctly via its built-in date functions.
/// GUID values are formatted as the standard hyphenated hex string.
pub fn column_value_to_rusqlite(value: &ColumnValue) -> Value {
    match value {
        ColumnValue::Null => Value::Null,
        ColumnValue::String(s) => Value::Text(s.clone()),
        ColumnValue::I8(i) => Value::Integer(*i as i64),
        ColumnValue::I16(i) => Value::Integer(*i as i64),
        ColumnValue::I32(i) => Value::Integer(*i as i64),
        ColumnValue::I64(i) => Value::Integer(*i),
        ColumnValue::F32(f) => Value::Real(*f as f64),
        ColumnValue::F64(f) => Value::Real(*f),
        ColumnValue::Bool(b) => Value::Integer(*b as i64),
        ColumnValue::Bytes(b) => Value::Blob(b.clone()),
        ColumnValue::Date { year, month, day } => {
            Value::Text(format!("{year:04}-{month:02}-{day:02}"))
        }
        ColumnValue::Time {
            hour,
            minute,
            second,
            fraction,
        } => Value::Text(format!("{hour:02}:{minute:02}:{second:02}.{fraction:09}")),
        ColumnValue::Timestamp {
            year,
            month,
            day,
            hour,
            minute,
            second,
            fraction,
        } => Value::Text(format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{fraction:09}"
        )),
        ColumnValue::Guid(bytes) => {
            let b = bytes;
            Value::Text(format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                b[0],
                b[1],
                b[2],
                b[3],
                b[4],
                b[5],
                b[6],
                b[7],
                b[8],
                b[9],
                b[10],
                b[11],
                b[12],
                b[13],
                b[14],
                b[15]
            ))
        }
        // DECIMAL/NUMERIC has no native SQLite storage class; keep the exact
        // decimal text so precision is never lost (SQLite compares numeric
        // text correctly in arithmetic contexts).
        ColumnValue::Decimal(s) => Value::Text(s.clone()),
        // New ColumnValue variants are not natively representable in SQLite.
        // TODO(spec): HYC00 (optional feature not implemented); cannot store complex types in SQLite.
        _ => {
            tracing::warn!(
                value = ?value,
                "column_value_to_rusqlite: unhandled ColumnValue variant, storing as empty text"
            );
            Value::Text(String::new())
        }
    }
}

/// Convert a [`rusqlite::types::Value`] (from a query result row) to a [`ColumnValue`],
/// given the column's declared ODBC SQL type (as computed by
/// [`sqlite_type_to_sql_data_type`] from the same `decl_type` string).
///
/// Takes `Value` by ownership so that `Text` / `Blob` can move their heap
/// buffers directly into `ColumnValue::String` / `ColumnValue::Bytes` without
/// cloning: rusqlite gives us a freshly-owned `Value` per cell, so a clone
/// here would be pure waste.
///
/// A column declared `DATE`/`TIME`/`DATETIME`/`TIMESTAMP` is described to the
/// application as the corresponding ODBC datetime SQL type, but SQLite may
/// still have stored the value as `INTEGER` (epoch seconds) or `REAL` (Julian
/// day) rather than text (see the module-level doc comment above). Those two
/// cases are decoded here into a proper `ColumnValue::Date`/`Time`/`Timestamp`
/// so `stackable-odbc-core`, which holds no SQLite-specific knowledge, only ever sees a
/// correctly typed value.
pub fn sqlite_value_to_column_value(value: Value, sql_type: SqlDataType) -> ColumnValue {
    match (value, sql_type) {
        (
            Value::Integer(epoch_seconds),
            SqlDataType::DATE | SqlDataType::TIME | SqlDataType::TIMESTAMP,
        ) => decode_epoch_seconds(epoch_seconds, sql_type).unwrap_or_else(|| {
            tracing::warn!(
                epoch_seconds,
                ?sql_type,
                "sqlite_value_to_column_value: epoch-seconds value does not decode to a \
                 representable datetime (year out of i16 range); returning the raw integer"
            );
            ColumnValue::I64(epoch_seconds)
        }),
        (
            Value::Real(julian_day),
            SqlDataType::DATE | SqlDataType::TIME | SqlDataType::TIMESTAMP,
        ) => decode_julian_day(julian_day, sql_type).unwrap_or_else(|| {
            tracing::warn!(
                julian_day,
                ?sql_type,
                "sqlite_value_to_column_value: Julian day value does not decode to a \
                 representable datetime (non-finite, or year out of i16 range); returning the \
                 raw float"
            );
            ColumnValue::F64(julian_day)
        }),
        (Value::Null, _) => ColumnValue::Null,
        (Value::Integer(i), _) => ColumnValue::I64(i),
        (Value::Real(f), _) => ColumnValue::F64(f),
        (Value::Text(s), _) => ColumnValue::String(s),
        (Value::Blob(b), _) => ColumnValue::Bytes(b),
    }
}

/// A decoded (year, month, day, hour, minute, second, nanosecond) civil
/// timestamp, before it is narrowed to whichever of `ColumnValue::Date` /
/// `Time` / `Timestamp` the column's declared SQL type calls for.
struct DecodedDateTime {
    year: i16,
    month: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    fraction: u32,
}

impl DecodedDateTime {
    /// Narrow to whichever `ColumnValue` variant `sql_type` calls for.
    ///
    /// `sql_type` is always one of `DATE`/`TIME`/`TIMESTAMP` here (the only
    /// values the caller matches on before reaching this point), so the
    /// fallback arm is unreachable in practice; it maps to `Timestamp` rather
    /// than panicking, since `SqlDataType` is not our enum to exhaustively
    /// match without a wildcard.
    fn into_column_value(self, sql_type: SqlDataType) -> ColumnValue {
        match sql_type {
            SqlDataType::DATE => ColumnValue::Date {
                year: self.year,
                month: self.month,
                day: self.day,
            },
            SqlDataType::TIME => ColumnValue::Time {
                hour: self.hour,
                minute: self.minute,
                second: self.second,
                // `decode_epoch_seconds` always passes 0 nanos (an INTEGER
                // epoch-seconds value has no sub-second part), but
                // `decode_julian_day`'s REAL encoding can carry a genuine
                // fraction, so `self.fraction` is real data, not a placeholder.
                fraction: self.fraction,
            },
            _ => ColumnValue::Timestamp {
                year: self.year,
                month: self.month,
                day: self.day,
                hour: self.hour,
                minute: self.minute,
                second: self.second,
                fraction: self.fraction,
            },
        }
    }
}

/// Decompose a day count since the Unix epoch (1970-01-01) into a proleptic
/// Gregorian (year, month, day).
///
/// This is the well-known "civil_from_days" algorithm (Howard Hinnant,
/// public domain: <http://howardhinnant.github.io/date_algorithms.html>),
/// valid for the entire range of `i64` day counts. All intermediate
/// arithmetic is carried out in `i128` so it cannot overflow regardless of
/// the input magnitude; the caller is responsible for range-checking the
/// resulting year against the target field width.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = i128::from(days) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u128; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i128 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as i64, m, d)
}

/// Decode a count of seconds since the Unix epoch, plus a nanosecond fraction
/// already isolated by the caller, into a [`DecodedDateTime`].
///
/// Returns `None` if the resulting year does not fit `SQL_TIMESTAMP_STRUCT.year`
/// (`i16`). See [`decode_epoch_seconds`] for how callers handle that.
fn timestamp_from_epoch_seconds(total_seconds: i64, nanos: u32) -> Option<DecodedDateTime> {
    let days = total_seconds.div_euclid(86_400);
    let secs_of_day = total_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let year = i16::try_from(year).ok()?;
    let hour = (secs_of_day / 3600) as u16;
    let minute = ((secs_of_day % 3600) / 60) as u16;
    let second = (secs_of_day % 60) as u16;
    Some(DecodedDateTime {
        year,
        month: month as u16,
        day: day as u16,
        hour,
        minute,
        second,
        fraction: nanos,
    })
}

/// Decode a SQLite `INTEGER` datetime column (epoch seconds) into the
/// [`ColumnValue`] variant `sql_type` (`DATE`/`TIME`/`TIMESTAMP`) calls for.
///
/// Returns `None` if the decoded year does not fit `SQL_TIMESTAMP_STRUCT.year`
/// (`i16`); the caller falls back to the raw `ColumnValue::I64` in that case
/// (see [`sqlite_value_to_column_value`]'s doc comment) rather than failing
/// the fetch outright.
fn decode_epoch_seconds(epoch_seconds: i64, sql_type: SqlDataType) -> Option<ColumnValue> {
    let decoded = timestamp_from_epoch_seconds(epoch_seconds, 0)?;
    Some(decoded.into_column_value(sql_type))
}

/// Decode a SQLite `REAL` datetime column (Julian day number, days since noon
/// on proleptic-Gregorian -4713-11-24, the convention SQLite's own
/// `julianday()` function uses) into the [`ColumnValue`] variant `sql_type`
/// calls for.
///
/// Julian day 2440587.5 is the Unix epoch, so the value is first rebased to
/// epoch seconds (with a fractional part) and then decoded the same way as
/// [`decode_epoch_seconds`]. Returns `None` for non-finite input or a value
/// whose implied year does not fit `i16`; the caller falls back to the raw
/// `ColumnValue::F64` in that case.
fn decode_julian_day(julian_day: f64, sql_type: SqlDataType) -> Option<ColumnValue> {
    const JULIAN_DAY_UNIX_EPOCH: f64 = 2_440_587.5;
    if !julian_day.is_finite() {
        return None;
    }
    let unix_seconds = (julian_day - JULIAN_DAY_UNIX_EPOCH) * 86_400.0;
    // i64 as f64 is inexact at the extremes, but comparing against the exact
    // bounds is enough to reject anything that would saturate on cast below.
    if !unix_seconds.is_finite() || unix_seconds < i64::MIN as f64 || unix_seconds > i64::MAX as f64
    {
        return None;
    }
    let whole_seconds = unix_seconds.floor();
    let frac_seconds = unix_seconds - whole_seconds;
    let nanos = (frac_seconds * 1_000_000_000.0)
        .round()
        .clamp(0.0, 999_999_999.0) as u32;
    let decoded = timestamp_from_epoch_seconds(whole_seconds as i64, nanos)?;
    Some(decoded.into_column_value(sql_type))
}

/// Split a declared type into its base name and parenthesised arguments.
///
/// `"DECIMAL(10,2)"` → `("DECIMAL", ["10", "2"])`. SQLite stores the declared
/// type verbatim, so `VARCHAR(50)` arrives with its length attached.
fn split_declared_type(decl_type: &str) -> (String, Vec<String>) {
    let t = decl_type.trim();
    match t.split_once('(') {
        Some((base, rest)) => {
            let args = rest
                .trim_end()
                .strip_suffix(')')
                .unwrap_or(rest)
                .split(',')
                .map(|a| a.trim().to_string())
                .collect();
            (base.trim().to_uppercase(), args)
        }
        None => (t.to_uppercase(), Vec::new()),
    }
}

/// SQLite's column affinity algorithm.
///
/// <https://www.sqlite.org/datatype3.html#determination_of_column_affinity>
/// The five rules are applied in order as substring matches; the first hit
/// wins. Used for declared types we do not recognise explicitly, so that an
/// unknown type is still described sensibly rather than as VARCHAR.
///
/// Its only possible outputs (`EXT_BIG_INT`, `EXT_W_VARCHAR`,
/// `EXT_VAR_BINARY`, `DOUBLE`, `DECIMAL`) are all already reachable through
/// [`SQLITE_DECLARED_TYPE_ALIASES`] too, so no separate `SQLGetTypeInfo`
/// completeness coverage is needed for this fallback path specifically (see
/// `every_reportable_type_has_a_type_info_row` in `backend/info.rs`).
fn sqlite_affinity(upper: &str) -> SqlDataType {
    if upper.contains("INT") {
        SqlDataType::EXT_BIG_INT
    } else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
        SqlDataType::EXT_W_VARCHAR
    } else if upper.contains("BLOB") || upper.is_empty() {
        SqlDataType::EXT_VAR_BINARY
    } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        SqlDataType::DOUBLE
    } else {
        SqlDataType::DECIMAL
    }
}

/// Every declared-type spelling this driver recognises explicitly, paired
/// with the `SqlDataType` it maps to.
///
/// [`sqlite_type_to_sql_data_type`] looks this table up directly instead of
/// a `match` with the same spellings transcribed a second time, so a
/// completeness test (`every_reportable_type_has_a_type_info_row` in
/// `backend/info.rs`) can iterate this table itself (the same data the
/// mapping uses) rather than a hand-copied list that can silently omit a
/// spelling added here later. Anything not in this table falls back to
/// [`sqlite_affinity`] (see its doc comment for why that needs no separate
/// coverage).
pub(crate) const SQLITE_DECLARED_TYPE_ALIASES: &[(&str, SqlDataType)] = &[
    ("INTEGER", SqlDataType::EXT_BIG_INT),
    ("INT", SqlDataType::EXT_BIG_INT),
    ("BIGINT", SqlDataType::EXT_BIG_INT),
    ("INT8", SqlDataType::EXT_BIG_INT),
    ("SMALLINT", SqlDataType::SMALLINT),
    ("INT2", SqlDataType::SMALLINT),
    ("TINYINT", SqlDataType::EXT_TINY_INT),
    ("REAL", SqlDataType::DOUBLE),
    ("DOUBLE", SqlDataType::DOUBLE),
    ("DOUBLE PRECISION", SqlDataType::DOUBLE),
    ("FLOAT", SqlDataType::DOUBLE),
    ("BOOLEAN", SqlDataType::EXT_BIT),
    ("BOOL", SqlDataType::EXT_BIT),
    ("BLOB", SqlDataType::EXT_VAR_BINARY),
    ("DECIMAL", SqlDataType::DECIMAL),
    ("NUMERIC", SqlDataType::DECIMAL),
    ("DATE", SqlDataType::DATE),
    ("TIME", SqlDataType::TIME),
    ("DATETIME", SqlDataType::TIMESTAMP),
    ("TIMESTAMP", SqlDataType::TIMESTAMP),
    ("VARCHAR", SqlDataType::EXT_W_VARCHAR),
    ("CHAR", SqlDataType::EXT_W_VARCHAR),
    ("CHARACTER", SqlDataType::EXT_W_VARCHAR),
    ("NCHAR", SqlDataType::EXT_W_VARCHAR),
    ("NVARCHAR", SqlDataType::EXT_W_VARCHAR),
    ("VARYING CHARACTER", SqlDataType::EXT_W_VARCHAR),
    ("NATIVE CHARACTER", SqlDataType::EXT_W_VARCHAR),
    ("TEXT", SqlDataType::EXT_W_VARCHAR),
    ("CLOB", SqlDataType::EXT_W_VARCHAR),
];

/// Map a SQLite declared column type to an ODBC `SqlDataType`.
///
/// SQLite does not constrain declared types, so this recognises the common SQL
/// spellings explicitly (via [`SQLITE_DECLARED_TYPE_ALIASES`]) and falls back
/// to SQLite's own affinity rules ([`sqlite_affinity`]) for anything else.
pub fn sqlite_type_to_sql_data_type(decl_type: &str) -> SqlDataType {
    let (base, _) = split_declared_type(decl_type);
    SQLITE_DECLARED_TYPE_ALIASES
        .iter()
        .find(|(name, _)| *name == base.as_str())
        .map(|(_, ty)| *ty)
        .unwrap_or_else(|| sqlite_affinity(&base))
}

/// Column size for a declared type, using the declared length where present.
pub fn sqlite_declared_type_precision(decl_type: &str) -> u32 {
    let (_, args) = split_declared_type(decl_type);
    if let Some(first) = args.first()
        && let Ok(n) = first.parse::<u32>()
    {
        return n;
    }
    default_precision_for_type(sqlite_type_to_sql_data_type(decl_type))
}

/// Decimal digits for a declared type, from the second parenthesised argument.
pub fn sqlite_declared_type_scale(decl_type: &str) -> i16 {
    let (_, args) = split_declared_type(decl_type);
    args.get(1).and_then(|s| s.parse::<i16>().ok()).unwrap_or(0)
}

// Backend policy constants used as the `precision`/`max_precision` input to
// `column_size`/`catalog_column_size` below. Unlike the fixed-size integer
// and float types (whose column size the ODBC appendix defines as a
// constant regardless of what precision is supplied), these three represent
// an actual choice this driver makes for an *undeclared* column of the type;
// they are not themselves derived from the appendix.
pub(crate) const VARCHAR_DEFAULT_COLUMN_SIZE: i32 = 255; // SQLite default text column size
pub(crate) const DECIMAL_DEFAULT_COLUMN_SIZE: i32 = 38; // conventional max precision for undeclared DECIMAL/NUMERIC
pub(crate) const BLOB_DEFAULT_COLUMN_SIZE: i32 = i32::MAX; // matches the BLOB row in backend/info.rs

/// Narrow a [`column_size`] result (`i32`, always non-negative for every
/// type this driver reports) to the `u32` this function has always
/// returned. Defensive: none of the arms below can actually produce a
/// negative value, but the fallback keeps this panic-free rather than
/// relying on that invariant silently.
fn precision_as_u32(n: i32) -> u32 {
    u32::try_from(n).unwrap_or_else(|_| {
        tracing::warn!(
            value = n,
            "column size formula produced a value outside u32 range"
        );
        0
    })
}

/// Return a reasonable default precision for a given SQL data type.
pub fn default_precision_for_type(sql_type: SqlDataType) -> u32 {
    match sql_type {
        SqlDataType::EXT_BIG_INT => precision_as_u32(column_size(SqlDataType::EXT_BIG_INT, 0, 0)),
        SqlDataType::SMALLINT => precision_as_u32(column_size(SqlDataType::SMALLINT, 0, 0)),
        SqlDataType::EXT_TINY_INT => precision_as_u32(column_size(SqlDataType::EXT_TINY_INT, 0, 0)),
        SqlDataType::DOUBLE => precision_as_u32(column_size(SqlDataType::DOUBLE, 0, 0)),
        SqlDataType::EXT_BIT => precision_as_u32(column_size(SqlDataType::EXT_BIT, 0, 0)),
        SqlDataType::DECIMAL => precision_as_u32(column_size(
            SqlDataType::DECIMAL,
            DECIMAL_DEFAULT_COLUMN_SIZE,
            0,
        )),
        SqlDataType::DATE => precision_as_u32(column_size(SqlDataType::DATE, 0, 0)),
        SqlDataType::TIME => precision_as_u32(column_size(
            SqlDataType::TIME,
            0,
            MAX_FRACTIONAL_SECONDS_PRECISION,
        )),
        SqlDataType::TIMESTAMP => precision_as_u32(column_size(
            SqlDataType::TIMESTAMP,
            0,
            MAX_FRACTIONAL_SECONDS_PRECISION,
        )),
        SqlDataType::EXT_VAR_BINARY => precision_as_u32(column_size(
            SqlDataType::EXT_VAR_BINARY,
            BLOB_DEFAULT_COLUMN_SIZE,
            0,
        )),
        SqlDataType::EXT_W_VARCHAR | SqlDataType::VARCHAR => {
            precision_as_u32(column_size(sql_type, VARCHAR_DEFAULT_COLUMN_SIZE, 0))
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_integer() {
        assert_eq!(
            sqlite_value_to_column_value(Value::Integer(42), SqlDataType::EXT_BIG_INT),
            ColumnValue::I64(42)
        );
    }

    #[test]
    fn convert_real() {
        assert_eq!(
            sqlite_value_to_column_value(Value::Real(std::f64::consts::PI), SqlDataType::DOUBLE),
            ColumnValue::F64(std::f64::consts::PI)
        );
    }

    #[test]
    fn convert_text() {
        assert_eq!(
            sqlite_value_to_column_value(Value::Text("hi".into()), SqlDataType::EXT_W_VARCHAR),
            ColumnValue::String("hi".into())
        );
    }

    #[test]
    fn convert_blob() {
        assert_eq!(
            sqlite_value_to_column_value(Value::Blob(vec![1, 2, 3]), SqlDataType::EXT_VAR_BINARY),
            ColumnValue::Bytes(vec![1, 2, 3])
        );
    }

    #[test]
    fn convert_null() {
        assert_eq!(
            sqlite_value_to_column_value(Value::Null, SqlDataType::EXT_W_VARCHAR),
            ColumnValue::Null
        );
    }

    // -----------------------------------------------------------------------
    // Numeric datetime encodings (epoch seconds / Julian day)
    // -----------------------------------------------------------------------
    //
    // SQLite is dynamically typed: a column declared DATE/TIME/DATETIME/
    // TIMESTAMP is not guaranteed to hold text even though it is described to
    // the application as an ODBC datetime SQL type. It may physically store
    // an integer count of seconds since the epoch, or a floating point Julian
    // day number. Both must decode to the same value a text representation
    // would have produced.

    #[test]
    fn epoch_seconds_integer_decodes_to_timestamp() {
        // 1_700_000_000 == 2023-11-14 22:13:20 UTC (verified against Python's
        // datetime.utcfromtimestamp).
        let value =
            sqlite_value_to_column_value(Value::Integer(1_700_000_000), SqlDataType::TIMESTAMP);
        assert_eq!(
            value,
            ColumnValue::Timestamp {
                year: 2023,
                month: 11,
                day: 14,
                hour: 22,
                minute: 13,
                second: 20,
                fraction: 0,
            }
        );
    }

    #[test]
    fn epoch_seconds_integer_decodes_to_date_and_time() {
        let date = sqlite_value_to_column_value(Value::Integer(1_700_000_000), SqlDataType::DATE);
        assert_eq!(
            date,
            ColumnValue::Date {
                year: 2023,
                month: 11,
                day: 14,
            }
        );
        let time = sqlite_value_to_column_value(Value::Integer(1_700_000_000), SqlDataType::TIME);
        assert_eq!(
            time,
            ColumnValue::Time {
                hour: 22,
                minute: 13,
                second: 20,
                fraction: 0,
            }
        );
    }

    #[test]
    fn julian_day_real_decodes_to_time_with_fraction() {
        // 2451545.0 is 2000-01-01 12:00:00 UTC exactly (see
        // julian_day_real_decodes_to_timestamp below); adding a quarter of a
        // second's worth of days exercises the fractional-seconds path that
        // only the REAL (Julian day) encoding can produce for TIME.
        // `decode_epoch_seconds` (INTEGER) never has a nonzero fraction to
        // decode, so `DecodedDateTime::fraction` must be threaded through
        // rather than dropped.
        let julian_day = 2_451_545.0 + 0.25 / 86_400.0;
        let time = sqlite_value_to_column_value(Value::Real(julian_day), SqlDataType::TIME);
        match time {
            ColumnValue::Time {
                hour,
                minute,
                second,
                fraction,
            } => {
                assert_eq!((hour, minute, second), (12, 0, 0));
                // f64 only has ~15-16 significant decimal digits; at this
                // magnitude (~2.45 million), the round trip through
                // `julian_day` cannot land on an exact nanosecond, so allow a
                // small tolerance rather than asserting bit-for-bit equality.
                assert!(
                    fraction.abs_diff(250_000_000) < 20_000,
                    "expected a fraction close to 250_000_000 ns, got {fraction}"
                );
            }
            other => panic!("expected ColumnValue::Time, got {other:?}"),
        }
    }

    #[test]
    fn epoch_seconds_negative_decodes_pre_1970_dates() {
        // -86_400 == exactly one day before the epoch: 1969-12-31 00:00:00 UTC.
        let value = sqlite_value_to_column_value(Value::Integer(-86_400), SqlDataType::TIMESTAMP);
        assert_eq!(
            value,
            ColumnValue::Timestamp {
                year: 1969,
                month: 12,
                day: 31,
                hour: 0,
                minute: 0,
                second: 0,
                fraction: 0,
            }
        );
    }

    #[test]
    fn julian_day_real_decodes_to_timestamp() {
        // 2451545.0 == 2000-01-01 12:00:00 UTC exactly (SQLite:
        // `SELECT julianday('2000-01-01 12:00:00')` returns 2451545.0), and
        // the offset from the Unix epoch (10957.5 days) multiplies back to a
        // whole number of seconds with no floating point rounding loss, so
        // this case can assert exact fields.
        let value = sqlite_value_to_column_value(Value::Real(2_451_545.0), SqlDataType::TIMESTAMP);
        assert_eq!(
            value,
            ColumnValue::Timestamp {
                year: 2000,
                month: 1,
                day: 1,
                hour: 12,
                minute: 0,
                second: 0,
                fraction: 0,
            }
        );
    }

    #[test]
    fn julian_day_before_day_zero_still_decodes_when_year_fits_i16() {
        // Julian day 0 is -4713-11-24 (proleptic Gregorian). A negative
        // Julian day is a date further in the past still; this does not
        // overflow SQL_TIMESTAMP_STRUCT.year (i16) since -4713 is well
        // within range, and the decoder must not special-case it.
        let value = sqlite_value_to_column_value(Value::Real(0.0), SqlDataType::DATE);
        match value {
            ColumnValue::Date { year, .. } => assert!(year < 0, "expected a BC year, got {year}"),
            other => panic!("expected ColumnValue::Date, got {other:?}"),
        }
    }

    #[test]
    fn epoch_seconds_year_overflow_falls_back_to_raw_integer() {
        // i64::MAX seconds implies a year vastly beyond i16::MAX (32767);
        // SQL_TIMESTAMP_STRUCT.year cannot represent it, so the fetch must
        // not fail: the raw integer is returned instead, per the design
        // decision that an unrepresentable value should still be readable
        // (e.g. as SQL_C_SBIGINT) rather than aborting the whole fetch.
        let value = sqlite_value_to_column_value(Value::Integer(i64::MAX), SqlDataType::TIMESTAMP);
        assert_eq!(value, ColumnValue::I64(i64::MAX));
    }

    #[test]
    fn julian_day_year_overflow_falls_back_to_raw_real() {
        // An enormous Julian day number implies a year far beyond i16::MAX.
        let value = sqlite_value_to_column_value(Value::Real(1.0e18), SqlDataType::TIMESTAMP);
        assert_eq!(value, ColumnValue::F64(1.0e18));
    }

    #[test]
    fn julian_day_non_finite_falls_back_to_raw_real() {
        for jd in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let value = sqlite_value_to_column_value(Value::Real(jd), SqlDataType::TIMESTAMP);
            match value {
                ColumnValue::F64(f) if f.is_nan() && jd.is_nan() => {}
                ColumnValue::F64(f) => assert_eq!(f, jd),
                other => panic!("expected ColumnValue::F64({jd}), got {other:?}"),
            }
        }
    }

    #[test]
    fn non_temporal_integer_column_is_not_decoded_as_datetime() {
        // A column that is NOT declared DATE/TIME/DATETIME/TIMESTAMP must
        // never be reinterpreted as an encoded datetime, no matter what its
        // numeric value looks like.
        let value =
            sqlite_value_to_column_value(Value::Integer(1_700_000_000), SqlDataType::EXT_BIG_INT);
        assert_eq!(value, ColumnValue::I64(1_700_000_000));
    }

    #[test]
    fn type_mapping_integer_variants() {
        assert_eq!(
            sqlite_type_to_sql_data_type("INTEGER"),
            SqlDataType::EXT_BIG_INT
        );
        assert_eq!(
            sqlite_type_to_sql_data_type("INT"),
            SqlDataType::EXT_BIG_INT
        );
        assert_eq!(
            sqlite_type_to_sql_data_type("BIGINT"),
            SqlDataType::EXT_BIG_INT
        );
    }

    #[test]
    fn type_mapping_text_fallback() {
        assert_eq!(
            sqlite_type_to_sql_data_type("TEXT"),
            SqlDataType::EXT_W_VARCHAR
        );
        // "UNKNOWN" matches none of SQLite's affinity substrings (INT, CHAR/CLOB/TEXT,
        // BLOB, REAL/FLOA/DOUB), so it falls through to NUMERIC affinity.
        assert_eq!(
            sqlite_type_to_sql_data_type("UNKNOWN"),
            SqlDataType::DECIMAL
        );
    }

    #[test]
    fn type_mapping_real_variants() {
        assert_eq!(sqlite_type_to_sql_data_type("REAL"), SqlDataType::DOUBLE);
        assert_eq!(sqlite_type_to_sql_data_type("DOUBLE"), SqlDataType::DOUBLE);
        assert_eq!(sqlite_type_to_sql_data_type("FLOAT"), SqlDataType::DOUBLE);
    }

    #[test]
    fn parameterised_types_are_recognised() {
        assert_eq!(
            sqlite_type_to_sql_data_type("VARCHAR(50)"),
            SqlDataType::EXT_W_VARCHAR
        );
        assert_eq!(sqlite_declared_type_precision("VARCHAR(50)"), 50);
        assert_eq!(
            sqlite_type_to_sql_data_type("DECIMAL(10,2)"),
            SqlDataType::DECIMAL
        );
        assert_eq!(sqlite_declared_type_precision("DECIMAL(10,2)"), 10);
        assert_eq!(sqlite_declared_type_scale("DECIMAL(10,2)"), 2);
    }

    #[test]
    fn temporal_declared_types_are_recognised() {
        // SqlDataType::DATE/TIME/TIMESTAMP are the ODBC 3.x concise types
        // (91/92/93). odbc-sys has no EXT_TYPE_* spelling for them.
        assert_eq!(sqlite_type_to_sql_data_type("DATE"), SqlDataType::DATE);
        assert_eq!(
            sqlite_type_to_sql_data_type("DATETIME"),
            SqlDataType::TIMESTAMP
        );
        assert_eq!(
            sqlite_type_to_sql_data_type("TIMESTAMP"),
            SqlDataType::TIMESTAMP
        );
    }

    #[test]
    fn declared_type_matching_is_case_insensitive_and_trims() {
        assert_eq!(
            sqlite_type_to_sql_data_type("  varchar(50)  "),
            SqlDataType::EXT_W_VARCHAR
        );
    }

    #[test]
    fn unrecognised_types_use_sqlite_affinity_rules() {
        // SQLite's published affinity algorithm, applied in order as substring
        // matches on the declared type.
        assert_eq!(
            sqlite_type_to_sql_data_type("UNSIGNED BIG INT"), // contains INT
            SqlDataType::EXT_BIG_INT
        );
        assert_eq!(
            sqlite_type_to_sql_data_type("NATIVE CHARACTER(70)"), // contains CHAR
            SqlDataType::EXT_W_VARCHAR
        );
        assert_eq!(
            sqlite_type_to_sql_data_type("DOUBLE PRECISION"), // contains DOUB
            SqlDataType::DOUBLE
        );
        assert_eq!(
            sqlite_type_to_sql_data_type(""), // empty → BLOB affinity
            SqlDataType::EXT_VAR_BINARY
        );
        assert_eq!(
            sqlite_type_to_sql_data_type("MADE UP TYPE"), // → NUMERIC affinity
            SqlDataType::DECIMAL
        );
    }

    #[test]
    fn affinity_rules_are_applied_in_order() {
        // "INT" is checked before "CHAR", so a type containing both is INTEGER.
        assert_eq!(
            sqlite_type_to_sql_data_type("INTCHAR"),
            SqlDataType::EXT_BIG_INT
        );
        // "CHAR" is checked before "BLOB".
        assert_eq!(
            sqlite_type_to_sql_data_type("CHARBLOB"),
            SqlDataType::EXT_W_VARCHAR
        );
    }

    #[test]
    fn blob_has_a_non_zero_column_size() {
        // A BLOB's declared-type precision must be non-zero, matching the
        // BLOB row SQLGetTypeInfo reports; a COLUMN_SIZE of 0 would disagree.
        assert!(sqlite_declared_type_precision("BLOB") > 0);
    }

    #[test]
    fn column_value_to_rusqlite_null() {
        assert_eq!(column_value_to_rusqlite(&ColumnValue::Null), Value::Null);
    }

    #[test]
    fn column_value_to_rusqlite_decimal() {
        // A bound SQL_C_NUMERIC arrives as ColumnValue::Decimal and must be
        // stored as its exact text, never coerced to empty text.
        assert_eq!(
            column_value_to_rusqlite(&ColumnValue::Decimal("-123.45".into())),
            Value::Text("-123.45".into())
        );
    }

    #[test]
    fn column_value_to_rusqlite_string() {
        assert_eq!(
            column_value_to_rusqlite(&ColumnValue::String("hi".into())),
            Value::Text("hi".into())
        );
    }

    #[test]
    fn column_value_to_rusqlite_integers() {
        assert_eq!(
            column_value_to_rusqlite(&ColumnValue::I8(1)),
            Value::Integer(1)
        );
        assert_eq!(
            column_value_to_rusqlite(&ColumnValue::I16(-5)),
            Value::Integer(-5)
        );
        assert_eq!(
            column_value_to_rusqlite(&ColumnValue::I32(1000)),
            Value::Integer(1000)
        );
        assert_eq!(
            column_value_to_rusqlite(&ColumnValue::I64(i64::MAX)),
            Value::Integer(i64::MAX)
        );
    }

    #[test]
    fn column_value_to_rusqlite_floats() {
        assert_eq!(
            column_value_to_rusqlite(&ColumnValue::F32(1.5)),
            Value::Real(1.5f32 as f64)
        );
        assert_eq!(
            column_value_to_rusqlite(&ColumnValue::F64(1.5_f64)),
            Value::Real(1.5_f64)
        );
    }

    #[test]
    fn column_value_to_rusqlite_bool() {
        assert_eq!(
            column_value_to_rusqlite(&ColumnValue::Bool(true)),
            Value::Integer(1)
        );
        assert_eq!(
            column_value_to_rusqlite(&ColumnValue::Bool(false)),
            Value::Integer(0)
        );
    }

    #[test]
    fn column_value_to_rusqlite_bytes() {
        assert_eq!(
            column_value_to_rusqlite(&ColumnValue::Bytes(vec![0xDE, 0xAD])),
            Value::Blob(vec![0xDE, 0xAD])
        );
    }

    #[test]
    fn column_value_to_rusqlite_date() {
        let v = ColumnValue::Date {
            year: 2024,
            month: 3,
            day: 15,
        };
        assert_eq!(
            column_value_to_rusqlite(&v),
            Value::Text("2024-03-15".into())
        );
    }

    #[test]
    fn column_value_to_rusqlite_time() {
        let v = ColumnValue::Time {
            hour: 14,
            minute: 30,
            second: 5,
            fraction: 0,
        };
        assert_eq!(
            column_value_to_rusqlite(&v),
            Value::Text("14:30:05.000000000".into())
        );
    }

    #[test]
    fn column_value_to_rusqlite_time_with_fraction() {
        let v = ColumnValue::Time {
            hour: 14,
            minute: 30,
            second: 5,
            fraction: 123_000_000,
        };
        assert_eq!(
            column_value_to_rusqlite(&v),
            Value::Text("14:30:05.123000000".into())
        );
    }

    #[test]
    fn column_value_to_rusqlite_timestamp() {
        let v = ColumnValue::Timestamp {
            year: 2024,
            month: 1,
            day: 2,
            hour: 10,
            minute: 0,
            second: 0,
            fraction: 123_000_000,
        };
        assert_eq!(
            column_value_to_rusqlite(&v),
            Value::Text("2024-01-02 10:00:00.123000000".into())
        );
    }
}

#[cfg(test)]
mod proptests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        // The declared-type parsers must never panic on any input.
        #[test]
        fn declared_type_parsers_never_panic(s in ".*") {
            let _ = sqlite_type_to_sql_data_type(&s);
            let _ = sqlite_declared_type_precision(&s);
            let _ = sqlite_declared_type_scale(&s);
        }

        // A declared `VARCHAR(n)` reports n as its precision.
        #[test]
        fn declared_precision_round_trips(n in 0u32..1_000_000) {
            prop_assert_eq!(sqlite_declared_type_precision(&format!("VARCHAR({n})")), n);
        }

        // `DECIMAL(p,s)` reports s as its scale (the second parenthesised arg).
        #[test]
        fn declared_scale_round_trips(p in 0u32..1000, s in 0i16..1000) {
            prop_assert_eq!(sqlite_declared_type_scale(&format!("DECIMAL({p},{s})")), s);
        }
    }
}
