//! Postgres-to-Doris type mapping and DDL generation.

use std::borrow::Cow;

use etl::schema::{ColumnSchema, NumericModifiers, ReplicatedTableSchema, Type, numeric_modifiers};

use crate::doris::{DorisTableName, quote_identifier};

/// Maximum `DECIMAL` precision supported by Doris.
const DORIS_MAX_DECIMAL_PRECISION: i16 = 76;

/// Length for `VARCHAR` key columns (Doris requires bounded key columns).
const KEY_VARCHAR_LENGTH: u32 = 1024;

/// Hidden Doris column that marks a row as deleted under merge-on-write.
pub(super) const DELETE_SIGN_COLUMN: &str = "__DORIS_DELETE_SIGN__";

/// ETL-owned surrogate key for tables without replica identity.
pub(super) const SURROGATE_KEY_COLUMN: &str = "_etl_row_id";

/// Doris type of the surrogate key column.
const SURROGATE_KEY_TYPE: &str = "varchar(32)";

/// Returns the Doris SQL type for a Postgres column.
pub(super) fn postgres_type_to_doris_sql(
    typ: &Type,
    modifier: i32,
    is_key: bool,
) -> Cow<'static, str> {
    let fallback: Cow<'static, str> = if is_key {
        format!("varchar({KEY_VARCHAR_LENGTH})").into()
    } else {
        "varchar(65533)".into()
    };

    match typ {
        &Type::BOOL => "boolean".into(),
        &Type::INT2 => "smallint".into(),
        &Type::INT4 => "int".into(),
        &Type::INT8 => "bigint".into(),
        &Type::FLOAT4 => "float".into(),
        &Type::FLOAT8 => "double".into(),
        &Type::NUMERIC => match numeric_modifiers(modifier) {
            Some(NumericModifiers { p, s })
                if (1..=DORIS_MAX_DECIMAL_PRECISION).contains(&p) && s >= 0 && s <= p =>
            {
                format!("decimal({p}, {s})").into()
            }
            _ => fallback,
        },
        &Type::DATE => "date".into(),
        &Type::TIMESTAMP | &Type::TIMESTAMPTZ => "datetime(6)".into(),
        &Type::JSON | &Type::JSONB => "json".into(),
        _ => fallback,
    }
}

/// Layout of a Doris unique-key table derived from the source schema.
pub(super) struct DorisTableLayout {
    /// Key column names for the Doris `UNIQUE KEY(...)` clause.
    pub key_columns: Vec<String>,
    /// Whether this table is append-only (has a surrogate key).
    pub append_only: bool,
}

impl DorisTableLayout {
    /// Derives the table layout from the source replica identity.
    pub fn from_schema(schema: &ReplicatedTableSchema) -> Self {
        let identity_cols: Vec<&ColumnSchema> = schema.identity_column_schemas().collect();
        if identity_cols.is_empty() {
            Self { key_columns: vec![SURROGATE_KEY_COLUMN.to_owned()], append_only: true }
        } else {
            Self {
                key_columns: identity_cols.iter().map(|c| c.name.clone()).collect(),
                append_only: false,
            }
        }
    }
}

/// Generates a `CREATE TABLE` statement for a Doris unique-key table.
pub(super) fn build_create_table_sql(
    table_name: &DorisTableName,
    schema: &ReplicatedTableSchema,
    layout: &DorisTableLayout,
    replication_num: Option<u16>,
) -> String {
    let db = quote_identifier(table_name.database());
    let tbl = quote_identifier(table_name.table());

    let mut col_defs = Vec::new();

    if layout.append_only {
        col_defs.push(format!(
            "    {} {SURROGATE_KEY_TYPE} NOT NULL",
            quote_identifier(SURROGATE_KEY_COLUMN)
        ));
    }

    for col in schema.column_schemas() {
        let is_key = layout.key_columns.contains(&col.name);
        let col_name = quote_identifier(&col.name);
        let col_type = postgres_type_to_doris_sql(&col.typ, col.modifier, is_key);
        let null_clause = if is_key { " NOT NULL" } else { " NULL" };
        col_defs.push(format!("    {col_name} {col_type}{null_clause}"));
    }

    let key_list: Vec<String> = layout.key_columns.iter().map(|k| quote_identifier(k)).collect();
    let key_clause = key_list.join(", ");
    let columns_str = col_defs.join(",\n");

    let mut properties = vec!["\"enable_unique_key_merge_on_write\" = \"true\"".to_owned()];
    // Doris defaults to three replicas, which a cluster with fewer backends
    // rejects at create time.
    if let Some(replication_num) = replication_num {
        properties.push(format!("\"replication_num\" = \"{replication_num}\""));
    }
    let properties_str = properties.join(", ");

    format!(
        "CREATE TABLE IF NOT EXISTS {db}.{tbl} (\n{columns_str}\n)\nUNIQUE \
         KEY({key_clause})\nDISTRIBUTED BY HASH({key_clause}) BUCKETS AUTO\nPROPERTIES \
         ({properties_str})"
    )
}

/// Generates an `ALTER TABLE ... ADD COLUMN` statement.
pub(super) fn build_add_column_sql(table_name: &DorisTableName, col: &ColumnSchema) -> String {
    let db = quote_identifier(table_name.database());
    let tbl = quote_identifier(table_name.table());
    let col_name = quote_identifier(&col.name);
    let col_type = postgres_type_to_doris_sql(&col.typ, col.modifier, false);
    format!("ALTER TABLE {db}.{tbl} ADD COLUMN {col_name} {col_type} NULL")
}

/// Generates an `ALTER TABLE ... DROP COLUMN` statement.
pub(super) fn build_drop_column_sql(table_name: &DorisTableName, col_name: &str) -> String {
    let db = quote_identifier(table_name.database());
    let tbl = quote_identifier(table_name.table());
    let col = quote_identifier(col_name);
    format!("ALTER TABLE {db}.{tbl} DROP COLUMN {col}")
}

/// Generates an `ALTER TABLE ... RENAME COLUMN` statement.
pub(super) fn build_rename_column_sql(
    table_name: &DorisTableName,
    old_name: &str,
    new_name: &str,
) -> String {
    let db = quote_identifier(table_name.database());
    let tbl = quote_identifier(table_name.table());
    let old = quote_identifier(old_name);
    let new = quote_identifier(new_name);
    format!("ALTER TABLE {db}.{tbl} RENAME COLUMN {old} {new}")
}

/// Generates an `ALTER TABLE ... MODIFY COLUMN` for a type change.
pub(super) fn build_modify_column_type_sql(
    table_name: &DorisTableName,
    col: &ColumnSchema,
) -> String {
    let db = quote_identifier(table_name.database());
    let tbl = quote_identifier(table_name.table());
    let col_name = quote_identifier(&col.name);
    let col_type = postgres_type_to_doris_sql(&col.typ, col.modifier, false);
    format!("ALTER TABLE {db}.{tbl} MODIFY COLUMN {col_name} {col_type} NULL")
}

/// Generates an `ALTER TABLE ... RENAME` statement.
pub(super) fn build_rename_table_sql(table_name: &DorisTableName, new_table: &str) -> String {
    let db = quote_identifier(table_name.database());
    let tbl = quote_identifier(table_name.table());
    let new = quote_identifier(new_table);
    format!("ALTER TABLE {db}.{tbl} RENAME {new}")
}

/// Generates a `DROP TABLE IF EXISTS` statement.
pub(super) fn build_drop_table_sql(table_name: &DorisTableName) -> String {
    let db = quote_identifier(table_name.database());
    let tbl = quote_identifier(table_name.table());
    format!("DROP TABLE IF EXISTS {db}.{tbl}")
}

/// Generates a `TRUNCATE TABLE` statement.
pub(super) fn build_truncate_table_sql(table_name: &DorisTableName) -> String {
    let db = quote_identifier(table_name.database());
    let tbl = quote_identifier(table_name.table());
    format!("TRUNCATE TABLE {db}.{tbl}")
}

/// Builds a `columns` header for Stream Load with optional delete sign.
pub(super) fn build_columns_header(column_names: &[String], include_delete_sign: bool) -> String {
    let mut cols: Vec<&str> = column_names.iter().map(String::as_str).collect();
    if include_delete_sign {
        cols.push(DELETE_SIGN_COLUMN);
    }
    cols.join(", ")
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc};

    use etl::schema::{
        IdentityMask, ReplicatedTableSchema, ReplicationMask, TableId, TableName, TableSchema,
    };

    use super::*;

    fn schema_with_pk() -> ReplicatedTableSchema {
        let columns = vec![
            ColumnSchema {
                name: "id".to_owned(),
                typ: Type::INT4,
                modifier: -1,
                nullable: false,
                primary_key_ordinal_position: Some(1),
                ordinal_position: 1,
                default_expression: None,
            },
            ColumnSchema {
                name: "name".to_owned(),
                typ: Type::TEXT,
                modifier: -1,
                nullable: true,
                primary_key_ordinal_position: None,
                ordinal_position: 2,
                default_expression: None,
            },
        ];
        let ts = Arc::new(TableSchema::new(
            TableId::new(1),
            TableName::new("public".to_owned(), "users".to_owned()),
            columns,
        ));
        let rep = ReplicationMask::all(&ts);
        let id_names: HashSet<String> = ["id".to_owned()].into_iter().collect();
        let id_mask = IdentityMask::try_build(&ts, &id_names).unwrap();
        ReplicatedTableSchema::from_masks(ts, rep, id_mask)
    }

    fn schema_no_pk() -> ReplicatedTableSchema {
        let columns = vec![ColumnSchema {
            name: "data".to_owned(),
            typ: Type::TEXT,
            modifier: -1,
            nullable: true,
            primary_key_ordinal_position: None,
            ordinal_position: 1,
            default_expression: None,
        }];
        let ts = Arc::new(TableSchema::new(
            TableId::new(2),
            TableName::new("public".to_owned(), "events".to_owned()),
            columns,
        ));
        let rep = ReplicationMask::all(&ts);
        let id_mask = IdentityMask::from_bytes(vec![0]);
        ReplicatedTableSchema::from_masks(ts, rep, id_mask)
    }

    #[test]
    fn type_mapping_basic() {
        assert_eq!(postgres_type_to_doris_sql(&Type::BOOL, -1, false), "boolean");
        assert_eq!(postgres_type_to_doris_sql(&Type::INT2, -1, false), "smallint");
        assert_eq!(postgres_type_to_doris_sql(&Type::INT4, -1, false), "int");
        assert_eq!(postgres_type_to_doris_sql(&Type::INT8, -1, false), "bigint");
        assert_eq!(postgres_type_to_doris_sql(&Type::FLOAT4, -1, false), "float");
        assert_eq!(postgres_type_to_doris_sql(&Type::FLOAT8, -1, false), "double");
        assert_eq!(postgres_type_to_doris_sql(&Type::DATE, -1, false), "date");
        assert_eq!(postgres_type_to_doris_sql(&Type::TIMESTAMP, -1, false), "datetime(6)");
        assert_eq!(postgres_type_to_doris_sql(&Type::TIMESTAMPTZ, -1, false), "datetime(6)");
        assert_eq!(postgres_type_to_doris_sql(&Type::JSON, -1, false), "json");
        assert_eq!(postgres_type_to_doris_sql(&Type::JSONB, -1, false), "json");
        assert_eq!(postgres_type_to_doris_sql(&Type::UUID, -1, false), "varchar(65533)");
    }

    #[test]
    fn type_mapping_numeric_in_range() {
        let modifier = (10 << 16) | (2 + 4);
        assert_eq!(postgres_type_to_doris_sql(&Type::NUMERIC, modifier, false), "decimal(10, 2)");
    }

    #[test]
    fn type_mapping_numeric_out_of_range() {
        let modifier = (77 << 16) | (2 + 4);
        assert_eq!(postgres_type_to_doris_sql(&Type::NUMERIC, modifier, false), "varchar(65533)");
    }

    #[test]
    fn create_table_with_pk() {
        let schema = schema_with_pk();
        let layout = DorisTableLayout::from_schema(&schema);
        let table_name = DorisTableName::new("db", "public_users");
        let sql = build_create_table_sql(&table_name, &schema, &layout, None);
        assert!(sql.contains("UNIQUE KEY(`id`)"));
        assert!(sql.contains("DISTRIBUTED BY HASH(`id`) BUCKETS AUTO"));
        assert!(sql.contains("`id` int NOT NULL"));
        assert!(sql.contains("`name` varchar(65533) NULL"));
        assert!(sql.contains("enable_unique_key_merge_on_write"));
    }

    #[test]
    fn create_table_without_pk_uses_surrogate() {
        let schema = schema_no_pk();
        let layout = DorisTableLayout::from_schema(&schema);
        let table_name = DorisTableName::new("db", "public_events");
        let sql = build_create_table_sql(&table_name, &schema, &layout, None);
        assert!(sql.contains(SURROGATE_KEY_COLUMN));
        assert!(sql.contains("UNIQUE KEY(`_etl_row_id`)"));
    }

    #[test]
    fn columns_header_with_delete_sign() {
        let cols = vec!["id".to_owned(), "name".to_owned()];
        let header = build_columns_header(&cols, true);
        assert_eq!(header, "id, name, __DORIS_DELETE_SIGN__");
    }
}
