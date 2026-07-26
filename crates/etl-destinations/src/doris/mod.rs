//! Apache Doris destination.
//!
//! Replicates a Postgres source into Doris unique-key tables with
//! merge-on-write, so the destination converges on the current state of the
//! source rather than keeping an audit log. Rows are loaded through Stream Load
//! and DDL is applied over the MySQL protocol.

use std::fmt;

use etl::schema::TableName;

mod client;
mod config;
mod core;
mod ddl;
mod encoding;
mod schema;

pub use core::DorisDestination;

pub use config::DorisConfig;

/// A table reference inside a Doris database.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DorisTableName {
    database: String,
    table: String,
}

impl DorisTableName {
    /// Creates a Doris table reference.
    fn new(database: impl Into<String>, table: impl Into<String>) -> Self {
        Self { database: database.into(), table: table.into() }
    }

    /// Returns the Doris database name.
    fn database(&self) -> &str {
        &self.database
    }

    /// Returns the Doris table name.
    fn table(&self) -> &str {
        &self.table
    }
}

impl fmt::Display for DorisTableName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.database, self.table)
    }
}

/// Maps a source Postgres table onto a Doris table inside one database.
///
/// The source schema is folded into the table name with a separator.
pub(crate) fn table_name_to_doris_table_name(
    database: &str,
    table_name: &TableName,
) -> DorisTableName {
    DorisTableName::new(database, format!("{}_{}", table_name.schema, table_name.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_tables_map_to_doris_names() {
        let name = table_name_to_doris_table_name(
            "analytics",
            &TableName::new("public".to_owned(), "users".to_owned()),
        );

        assert_eq!(name.table(), "public_users");
        assert_eq!(name.database(), "analytics");
    }

    #[test]
    fn display_is_database_qualified() {
        let table_name = DorisTableName::new("analytics", "public_users");
        assert_eq!(table_name.to_string(), "analytics.public_users");
    }
}
