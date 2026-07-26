//! Parameter binding for the SQLite backend is handled inline in `execute.rs`
//! via the rusqlite params API (see `execute`/`prepare`). The generic
//! SQLBindParameter / SQLNumParams / SQLDescribeParam FFI entry points live in
//! stackable-odbc-core (`ffi/params.rs`) and need no SQLite-specific override, so this
//! module is intentionally empty.
