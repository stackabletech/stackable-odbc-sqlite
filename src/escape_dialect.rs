//! SQLite escape-translation dialect: `"`/`` ` ``/`[...]`-quoted identifiers
//! (SQLite accepts all three quoting styles), bare-string date/time/timestamp
//! literals (SQLite has no date/time storage classes, so a "date" is just a
//! quoted text value), and the `{fn}` scalar-function remap for the names
//! the bundled 3.53.2 build spells differently from ODBC.
//!
//! The remap table is traceable to the `SQL_*_FUNCTIONS` bitmaps
//! `src/backend/info.rs` advertises for SQLite.
//! Every arm below corresponds to one advertised `SQL_FN_*`
//! bit whose ODBC name SQLite spells differently *and* for which a bare name
//! substitution (`stackable_odbc_core::escape` only ever swaps the identifier in front
//! of the parentheses, it does not rewrite argument syntax or values) still
//! produces valid, semantically equivalent SQLite SQL.
//!
//! - `SQL_FN_STR_UCASE` / `SQL_FN_STR_LCASE`: SQLite's `upper()` / `lower()`.
//! - `SQL_FN_STR_SUBSTRING`: SQLite's `substr(string, start, length)` takes
//!   the same argument order and 1-based indexing as ODBC's `SUBSTRING`, so
//!   a bare name swap is exact.
//! - `SQL_FN_STR_ASCII`: SQLite's `unicode(x)` returns the code point of the
//!   first character of `x`, the same one-argument shape as ODBC's `ASCII`.
//! - `SQL_FN_TD_NOW` / `SQL_FN_TD_CURDATE` / `SQL_FN_TD_CURTIME`: SQLite's
//!   `datetime()` / `date()` / `time()` take no arguments and return the
//!   current value (see the `SQL_TIMEDATE_FUNCTIONS` doc comment in
//!   `backend/info.rs`). They are real callable functions, so `{fn NOW()}` /
//!   `{fn CURDATE()}` / `{fn CURTIME()}`'s trailing `()` remains valid SQLite
//!   syntax after the name swap.
//!
//! Advertised names that are NOT remapped here, and why:
//!
//! - `SQL_FN_STR_CONCAT`, `LTRIM`, `LENGTH`, `REPLACE`, `RTRIM`, `CHAR`,
//!   `SOUNDEX`, `OCTET_LENGTH`; `SQL_FN_NUM_ABS`, `SIGN`, `ROUND`;
//!   `SQL_FN_SYS_IFNULL`: SQLite spells every one of these identically to
//!   ODBC (case-insensitively): `concat()`, `ltrim()`, `length()`,
//!   `replace()`, `rtrim()`, `char()`, `soundex()`, `octet_length()`,
//!   `abs()`, `sign()`, `round()`, `ifnull()`, so they pass through
//!   unchanged (`None`). SQLite has `ifnull()` natively, so no substitution
//!   is needed for `SQL_FN_SYS_IFNULL`.
//!
//! Names handled by [`rewrite_scalar_fn`] rather than the remap table:
//!
//! - `SQL_FN_TD_CURRENT_DATE` / `SQL_FN_TD_CURRENT_TIME` /
//!   `SQL_FN_TD_CURRENT_TIMESTAMP`: SQLite's `CURRENT_DATE` / `CURRENT_TIME`
//!   / `CURRENT_TIMESTAMP` are bare keywords, not callable functions.
//!   `SELECT CURRENT_DATE();` is a syntax error (confirmed live: "near '(':
//!   syntax error"). The ODBC escape always includes `()` (e.g.
//!   `{fn CURRENT_DATE()}`), and a name-only rename appends whatever follows
//!   the name verbatim, so it cannot drop that trailing `()`.
//!
//!   These three were advertised in `SQL_TIMEDATE_FUNCTIONS` while no
//!   translation existed for them, so `{fn CURRENT_DATE()}` reached SQLite as
//!   `CURRENT_DATE()` and failed to prepare. `rewrite_scalar_fn` replaces the
//!   whole escape, which is what emitting a bare keyword requires.
use stackable_odbc_core::escape::EscapeDialect;

/// Remap an ODBC `{fn NAME(...)}` scalar-function name to SQLite's spelling.
/// `None` passes the name through unchanged (same spelling in both).
pub(crate) fn remap_scalar_fn(name: &str) -> Option<&'static str> {
    match name.to_ascii_uppercase().as_str() {
        // SQL_FN_STR_UCASE / SQL_FN_STR_LCASE
        "UCASE" => Some("upper"),
        "LCASE" => Some("lower"),
        // SQL_FN_STR_SUBSTRING
        "SUBSTRING" => Some("substr"),
        // SQL_FN_STR_ASCII
        "ASCII" => Some("unicode"),
        // SQL_FN_TD_NOW / SQL_FN_TD_CURDATE / SQL_FN_TD_CURTIME
        "NOW" => Some("datetime"),
        "CURDATE" => Some("date"),
        "CURTIME" => Some("time"),
        _ => None,
    }
}

/// Rewrite a whole `{fn NAME(args)}` escape, for the calls a name swap cannot
/// express.
///
/// Only the three bare-keyword date/time forms need this: SQLite spells them
/// `CURRENT_DATE` / `CURRENT_TIME` / `CURRENT_TIMESTAMP` with no parentheses,
/// and `SELECT CURRENT_DATE();` is a syntax error. Returning the keyword alone
/// replaces the escape including its `()`.
///
/// Everything else returns `None` and falls back to [`remap_scalar_fn`]. The
/// argument text is checked rather than ignored: `{fn CURRENT_DATE(x)}` is not
/// a call SQLite has any spelling for, so it is left alone to fail as the
/// malformed call it is, instead of being silently rewritten to a keyword that
/// discards `x`.
pub(crate) fn rewrite_scalar_fn(name: &str, args: &str) -> Option<String> {
    if !args.trim().is_empty() {
        return None;
    }
    match name.to_ascii_uppercase().as_str() {
        // SQL_FN_TD_CURRENT_DATE / _CURRENT_TIME / _CURRENT_TIMESTAMP
        "CURRENT_DATE" => Some("CURRENT_DATE".to_string()),
        "CURRENT_TIME" => Some("CURRENT_TIME".to_string()),
        "CURRENT_TIMESTAMP" => Some("CURRENT_TIMESTAMP".to_string()),
        _ => None,
    }
}

/// SQLite has no date/time/timestamp storage classes, a date/time value is
/// just quoted text, so `{d/t/ts '...'}` render to the bare string literal
/// with no leading type keyword.
fn render_bare(x: &str) -> String {
    x.to_string()
}

/// SQLite's `EscapeDialect`: all three SQLite identifier-quoting styles
/// (`"`, `` ` ``, `[...]`) and bare-string date/time/timestamp literals.
pub(crate) fn dialect() -> EscapeDialect {
    EscapeDialect::ansi_default()
        .with_identifier_quotes(&[('"', '"'), ('`', '`'), ('[', ']')])
        .with_remap_scalar_fn(remap_scalar_fn)
        .with_rewrite_scalar_fn(rewrite_scalar_fn)
        .with_datetime_renderers(render_bare, render_bare, render_bare)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ucase_maps_to_upper() {
        assert_eq!(remap_scalar_fn("UCASE"), Some("upper"));
        assert_eq!(remap_scalar_fn("ucase"), Some("upper"));
    }

    #[test]
    fn lcase_maps_to_lower() {
        assert_eq!(remap_scalar_fn("LCASE"), Some("lower"));
    }

    #[test]
    fn substring_maps_to_substr() {
        assert_eq!(remap_scalar_fn("SUBSTRING"), Some("substr"));
    }

    #[test]
    fn ascii_maps_to_unicode() {
        assert_eq!(remap_scalar_fn("ASCII"), Some("unicode"));
    }

    #[test]
    fn now_maps_to_datetime() {
        assert_eq!(remap_scalar_fn("NOW"), Some("datetime"));
    }

    #[test]
    fn curdate_maps_to_date() {
        assert_eq!(remap_scalar_fn("CURDATE"), Some("date"));
    }

    #[test]
    fn curtime_maps_to_time() {
        assert_eq!(remap_scalar_fn("CURTIME"), Some("time"));
    }

    #[test]
    fn abs_passes_through() {
        assert_eq!(remap_scalar_fn("ABS"), None);
    }

    #[test]
    fn concat_passes_through() {
        assert_eq!(remap_scalar_fn("CONCAT"), None);
    }

    #[test]
    fn char_passes_through() {
        assert_eq!(remap_scalar_fn("CHAR"), None);
    }

    #[test]
    fn ifnull_passes_through() {
        // SQLite spells IFNULL the same way ODBC does, so it passes through.
        assert_eq!(remap_scalar_fn("IFNULL"), None);
    }

    #[test]
    fn round_passes_through() {
        assert_eq!(remap_scalar_fn("ROUND"), None);
    }

    #[test]
    fn sign_passes_through() {
        assert_eq!(remap_scalar_fn("SIGN"), None);
    }

    // The three bare-keyword forms are handled by rewrite_scalar_fn, not by
    // the name-only remap table, which cannot drop the escape's trailing `()`.
    #[test]
    fn current_date_is_not_a_name_only_remap() {
        assert_eq!(remap_scalar_fn("CURRENT_DATE"), None);
        assert_eq!(remap_scalar_fn("CURRENT_TIME"), None);
        assert_eq!(remap_scalar_fn("CURRENT_TIMESTAMP"), None);
    }

    #[test]
    fn bare_keyword_datetime_forms_are_rewritten_without_parentheses() {
        for name in ["CURRENT_DATE", "CURRENT_TIME", "CURRENT_TIMESTAMP"] {
            assert_eq!(rewrite_scalar_fn(name, ""), Some(name.to_string()));
            // Case-insensitive, like the remap table.
            assert_eq!(
                rewrite_scalar_fn(&name.to_ascii_lowercase(), ""),
                Some(name.to_string())
            );
        }
    }

    /// A call with arguments is left alone rather than rewritten to a keyword
    /// that would silently discard them.
    #[test]
    fn bare_keyword_rewrite_declines_a_call_with_arguments() {
        assert_eq!(rewrite_scalar_fn("CURRENT_DATE", "x"), None);
        assert_eq!(rewrite_scalar_fn("CURRENT_TIMESTAMP", "1, 2"), None);
    }

    /// Everything else falls through to the remap table.
    #[test]
    fn rewrite_declines_names_the_remap_table_owns() {
        for name in ["UCASE", "SUBSTRING", "NOW", "CURDATE", "ABS"] {
            assert_eq!(rewrite_scalar_fn(name, ""), None);
        }
    }

    #[test]
    fn date_literal_is_bare_string() {
        assert_eq!(render_bare("'2020-01-01'"), "'2020-01-01'");
    }

    #[test]
    fn time_literal_is_bare_string() {
        assert_eq!(render_bare("'10:00:00'"), "'10:00:00'");
    }

    #[test]
    fn timestamp_literal_is_bare_string() {
        assert_eq!(
            render_bare("'2020-01-01 00:00:00'"),
            "'2020-01-01 00:00:00'"
        );
    }

    #[test]
    fn identifier_quotes_include_brackets_and_backticks() {
        let d = dialect();
        assert!(d.identifier_quotes.contains(&('[', ']')));
        assert!(d.identifier_quotes.contains(&('`', '`')));
        assert!(d.identifier_quotes.contains(&('"', '"')));
    }

    #[test]
    fn end_to_end_fn_and_date_translate() {
        let out = stackable_odbc_core::escape::translate_escapes(
            "SELECT {fn UCASE(name)} FROM t WHERE d = {d '2020-01-01'}",
            &dialect(),
        )
        .unwrap();
        assert_eq!(out, "SELECT upper(name) FROM t WHERE d = '2020-01-01'");
    }
}
