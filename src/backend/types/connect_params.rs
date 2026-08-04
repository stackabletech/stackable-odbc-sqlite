//! `SqliteConnectParams`: the SQLite connection settings (the database file
//! path, `:memory:` for an in-memory database) parsed from the generic
//! `stackable-odbc-core` connection-string key/value map.

use stackable_odbc_core::types::ConnectParams;

use super::super::SqliteError;

// ---------------------------------------------------------------------------
// Connection string parameter keys (SQLite-specific)
// ---------------------------------------------------------------------------

/// Database file path, or `":memory:"` for an in-memory database.
pub(crate) const PARAM_DATABASE: &str = "database";

// ---------------------------------------------------------------------------
// Typed connection parameters
// ---------------------------------------------------------------------------

/// Parsed and validated SQLite connection parameters.
#[derive(Debug)]
pub(crate) struct SqliteConnectParams {
    database: String,
}

impl SqliteConnectParams {
    pub fn database(&self) -> &str {
        &self.database
    }
}

impl TryFrom<&ConnectParams> for SqliteConnectParams {
    type Error = SqliteError;

    fn try_from(params: &ConnectParams) -> Result<Self, SqliteError> {
        let database = params
            .get(PARAM_DATABASE)
            .ok_or_else(|| SqliteError::MissingParam {
                name: PARAM_DATABASE.into(),
            })?;
        Ok(SqliteConnectParams {
            database: database.to_string(),
        })
    }
}
