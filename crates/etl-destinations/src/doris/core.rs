//! Doris destination implementation.
//!
//! Writes are synchronous: Stream Load commits before a call returns, so every
//! write reports [`DestinationWriteStatus::Durable`] and ETL can advance
//! replication progress immediately. Streaming load labels are derived from the
//! source position, which turns ETL's at-least-once retries into idempotent
//! loads because Doris refuses a repeated label.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use etl::{
    data::{OldTableRow, PartialTableRow, TableRow, UpdatedTableRow},
    destination::{
        Destination, DestinationTableMetadata, DestinationTableSchemaStatus,
        DestinationWriteStatus, DropTableForCopyResult, WriteEventsDurability, WriteEventsResult,
        WriteTableRowsResult,
    },
    error::{ErrorKind, EtlError, EtlResult},
    etl_error,
    event::Event,
    schema::{ColumnModification, ReplicatedTableSchema, SchemaDiff, TableId},
    store::DestinationStore,
};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::doris::{
    DorisTableName,
    client::{
        DorisStreamLoadClient, StreamLoadOutcome, build_copy_stream_load_label,
        build_stream_load_label,
    },
    config::DorisConfig,
    ddl::DorisDdlClient,
    encoding::cell_to_json,
    schema::{
        DELETE_SIGN_COLUMN, DorisTableLayout, SURROGATE_KEY_COLUMN, build_add_column_sql,
        build_columns_header, build_create_table_sql, build_drop_column_sql, build_drop_table_sql,
        build_modify_column_type_sql, build_rename_column_sql, build_rename_table_sql,
        build_truncate_table_sql,
    },
    table_name_to_doris_table_name,
};

/// Delay before retrying a load whose label is still owned by a running job.
const IN_PROGRESS_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Number of times a load waits for an earlier attempt of the same label.
const IN_PROGRESS_MAX_ATTEMPTS: usize = 30;

/// Replicates a Postgres source into Doris unique-key tables.
#[derive(Clone)]
pub struct DorisDestination<S> {
    config: DorisConfig,
    stream_load: DorisStreamLoadClient,
    ddl: DorisDdlClient,
    store: S,
    /// Serializes DDL so two tables never race one Doris schema-change job.
    ddl_lock: Arc<Mutex<()>>,
    /// Sequence that keeps table-copy load labels unique across batches.
    copy_batch_sequence: Arc<AtomicU64>,
    /// Separates this run's table-copy labels from an earlier run's.
    ///
    /// A copy always starts from a dropped table, so a label Doris still
    /// remembers must not make it skip the load and leave the table empty.
    copy_run_nonce: u64,
}

impl<S> DorisDestination<S>
where
    S: DestinationStore,
{
    /// Connects to Doris and creates the target database when it is missing.
    pub async fn new(config: DorisConfig, store: S) -> EtlResult<Self> {
        let stream_load = DorisStreamLoadClient::new(config.clone())?;
        let ddl = DorisDdlClient::connect(&config).await?;
        ddl.ensure_database(&config.database).await?;

        Ok(Self {
            config,
            stream_load,
            ddl,
            store,
            ddl_lock: Arc::new(Mutex::new(())),
            copy_batch_sequence: Arc::new(AtomicU64::new(0)),
            copy_run_nonce: rand::random(),
        })
    }

    /// Returns the Doris table that mirrors a source table.
    fn table_name(&self, replicated_table_schema: &ReplicatedTableSchema) -> DorisTableName {
        table_name_to_doris_table_name(&self.config.database, replicated_table_schema.name())
    }

    /// Creates or migrates the Doris table for a replicated source schema.
    ///
    /// Destination metadata records which source snapshot the Doris table was
    /// built from, so a restart in the middle of a schema change resumes from
    /// the stored snapshot instead of guessing from the live table shape.
    async fn ensure_table_ready(
        &self,
        replicated_table_schema: &ReplicatedTableSchema,
    ) -> EtlResult<DorisTableName> {
        let table_id = replicated_table_schema.id();
        let snapshot_id = replicated_table_schema.inner().snapshot_id;
        let replication_mask = replicated_table_schema.replication_mask().clone();
        let desired_table_name = self.table_name(replicated_table_schema);

        let Some(metadata) = self.store.get_destination_table_metadata(table_id).await? else {
            return self.create_table(replicated_table_schema, &desired_table_name).await;
        };

        let stored_table_name = parse_metadata_table_name(&metadata.destination_table_id)?;
        let table_name = self
            .follow_table_rename(table_id, &metadata, stored_table_name, &desired_table_name)
            .await?;

        if metadata.is_applied()
            && metadata.snapshot_id == snapshot_id
            && metadata.replication_mask == replication_mask
        {
            return Ok(table_name);
        }

        self.apply_schema_change(table_id, &table_name, metadata, replicated_table_schema).await?;

        Ok(table_name)
    }

    /// Creates the Doris table and records applied destination metadata.
    async fn create_table(
        &self,
        replicated_table_schema: &ReplicatedTableSchema,
        table_name: &DorisTableName,
    ) -> EtlResult<DorisTableName> {
        let layout = DorisTableLayout::from_schema(replicated_table_schema);
        if layout.append_only {
            warn!(
                table = %table_name,
                "doris table has no source replica identity, replicating as an append-only log \
                 keyed by a surrogate column"
            );
        }

        let metadata = DestinationTableMetadata::new_applying(
            metadata_table_name(table_name),
            replicated_table_schema.inner().snapshot_id,
            replicated_table_schema.replication_mask().clone(),
        );
        self.store
            .store_destination_table_metadata(replicated_table_schema.id(), metadata.clone())
            .await?;

        let sql = build_create_table_sql(
            table_name,
            replicated_table_schema,
            &layout,
            self.config.replication_num,
        );
        {
            let _ddl_permit = self.ddl_lock.lock().await;
            self.ddl.execute(&sql, "Doris create table failed").await?;
        }

        self.store
            .store_destination_table_metadata(replicated_table_schema.id(), metadata.to_applied())
            .await?;

        Ok(table_name.clone())
    }

    /// Renames the Doris table when the source table name changed.
    ///
    /// Postgres keeps the table OID across a rename, so without this the
    /// destination table name would drift from the source forever.
    async fn follow_table_rename(
        &self,
        table_id: TableId,
        metadata: &DestinationTableMetadata,
        stored_table_name: DorisTableName,
        desired_table_name: &DorisTableName,
    ) -> EtlResult<DorisTableName> {
        if stored_table_name == *desired_table_name {
            return Ok(stored_table_name);
        }

        info!(
            table_id = %table_id,
            old_table = %stored_table_name,
            new_table = %desired_table_name,
            "doris following source table rename"
        );

        {
            let _ddl_permit = self.ddl_lock.lock().await;
            let sql = build_rename_table_sql(&stored_table_name, desired_table_name.table());
            self.ddl.execute(&sql, "Doris rename table failed").await?;
        }

        let mut renamed = metadata.clone();
        renamed.destination_table_id = metadata_table_name(desired_table_name);
        self.store.store_destination_table_metadata(table_id, renamed).await?;

        Ok(desired_table_name.clone())
    }

    /// Applies the diff between the stored and the new source schema.
    async fn apply_schema_change(
        &self,
        table_id: TableId,
        table_name: &DorisTableName,
        metadata: DestinationTableMetadata,
        new_replicated_table_schema: &ReplicatedTableSchema,
    ) -> EtlResult<()> {
        let current_snapshot_id = metadata.snapshot_id;
        let current_table_schema =
            self.store.get_table_schema(&table_id, current_snapshot_id).await?.ok_or_else(
                || {
                    etl_error!(
                        ErrorKind::CorruptedTableSchema,
                        "Stored schema snapshot missing for Doris schema change",
                        format!("table={table_name} snapshot_id={current_snapshot_id}")
                    )
                },
            )?;
        let current_schema = ReplicatedTableSchema::from_mask(
            current_table_schema,
            metadata.replication_mask.clone(),
        );
        let diff = current_schema.diff(new_replicated_table_schema);

        let applying = metadata.with_schema_change(
            new_replicated_table_schema.inner().snapshot_id,
            new_replicated_table_schema.replication_mask().clone(),
            DestinationTableSchemaStatus::Applying,
        );
        self.store.store_destination_table_metadata(table_id, applying.clone()).await?;

        self.apply_schema_diff(table_name, new_replicated_table_schema, &diff).await?;

        self.store.store_destination_table_metadata(table_id, applying.to_applied()).await
    }

    /// Executes the Doris DDL for one schema diff.
    ///
    /// Adding, dropping, and renaming a value column are lightweight changes in
    /// Doris, while a type change is queued as a background schema-change job
    /// that must finish before the next load, so those are awaited.
    async fn apply_schema_diff(
        &self,
        table_name: &DorisTableName,
        new_replicated_table_schema: &ReplicatedTableSchema,
        diff: &SchemaDiff,
    ) -> EtlResult<()> {
        if diff.is_empty() {
            debug!(table = %table_name, "doris schema diff is empty");
            return Ok(());
        }

        let layout = DorisTableLayout::from_schema(new_replicated_table_schema);
        let _ddl_permit = self.ddl_lock.lock().await;
        let existing_columns = self.ddl.column_names(table_name).await?;

        for column_schema in &diff.columns_to_add {
            if existing_columns.contains(&column_schema.name) {
                continue;
            }

            let sql = build_add_column_sql(table_name, column_schema);
            self.ddl.execute_async_schema_change(&sql, table_name).await?;
        }

        for column_schema in &diff.columns_to_remove {
            // Doris refuses to drop a key column, and a key change cannot be
            // expressed in place on a unique-key table.
            if layout.key_columns.contains(&column_schema.name) {
                warn!(
                    table = %table_name,
                    column = %column_schema.name,
                    "skipping drop of a doris key column"
                );
                continue;
            }
            if !existing_columns.contains(&column_schema.name) {
                continue;
            }

            let sql = build_drop_column_sql(table_name, &column_schema.name);
            self.ddl.execute_async_schema_change(&sql, table_name).await?;
        }

        for change in &diff.columns_to_change {
            for modification in &change.modifications {
                match modification {
                    ColumnModification::Rename { old_name, new_name } => {
                        if !existing_columns.contains(old_name) {
                            continue;
                        }

                        let sql = build_rename_column_sql(table_name, old_name, new_name);
                        self.ddl.execute_async_schema_change(&sql, table_name).await?;
                    }
                    ColumnModification::Type { .. } => {
                        if layout.key_columns.contains(&change.new_column.name) {
                            warn!(
                                table = %table_name,
                                column = %change.new_column.name,
                                "skipping type change of a doris key column"
                            );
                            continue;
                        }

                        let sql = build_modify_column_type_sql(table_name, &change.new_column);
                        self.ddl.execute_async_schema_change(&sql, table_name).await?;
                    }
                    // Doris value columns stay nullable so a relaxed source keeps
                    // loading, and the replica mirrors source data rather than
                    // source constraints or defaults.
                    ColumnModification::Nullability { .. } | ColumnModification::Default { .. } => {
                    }
                }
            }
        }

        info!(table = %table_name, "doris schema change applied");

        Ok(())
    }

    /// Loads one initial-copy batch.
    async fn write_copy_rows(
        &self,
        replicated_table_schema: &ReplicatedTableSchema,
        table_rows: Vec<TableRow>,
    ) -> EtlResult<()> {
        let table_name = self.ensure_table_ready(replicated_table_schema).await?;
        if table_rows.is_empty() {
            return Ok(());
        }

        let layout = DorisTableLayout::from_schema(replicated_table_schema);
        let columns = column_names(replicated_table_schema, &layout);
        let mut rows = Vec::with_capacity(table_rows.len());
        for table_row in &table_rows {
            // A table copy always starts from a dropped table, so an
            // append-only surrogate key only has to be unique within this copy.
            rows.push(row_to_json(
                replicated_table_schema,
                &layout,
                table_row,
                &new_surrogate_key(),
            )?);
        }

        let sequence = self.copy_batch_sequence.fetch_add(1, Ordering::Relaxed);
        let label = build_copy_stream_load_label(
            self.config.pipeline_id,
            table_name.table(),
            self.copy_run_nonce,
            sequence,
        );

        self.load(&table_name, &label, &columns, false, rows).await
    }

    /// Applies one streaming batch, preserving per-table event order.
    async fn write_stream_events(&self, events: Vec<Event>) -> EtlResult<()> {
        let mut buffers: HashMap<TableId, TableBuffer> = HashMap::new();
        let mut batch_index = 0u32;

        for event in events {
            match event {
                Event::Relation(relation) => {
                    // A schema change has to land before the rows that follow
                    // it, so buffered work is flushed under the old shape first.
                    self.flush(&mut buffers, &mut batch_index).await?;
                    self.ensure_table_ready(&relation.replicated_table_schema).await?;
                }
                Event::Insert(insert) => {
                    let commit_lsn = insert.commit_lsn.into();
                    let buffer = Self::buffer_for(&mut buffers, insert.replicated_table_schema);
                    buffer.position = (commit_lsn, insert.tx_ordinal);
                    let surrogate_key = surrogate_key_for_event(commit_lsn, insert.tx_ordinal);
                    let row = row_to_json(
                        &buffer.schema,
                        &buffer.layout,
                        &insert.table_row,
                        &surrogate_key,
                    )?;
                    let key = merge_key(&buffer.layout, &row);
                    buffer.push_full(key, row);
                }
                Event::Update(update) => {
                    let commit_lsn = update.commit_lsn.into();
                    let old_table_row = update.old_table_row;
                    let buffer = Self::buffer_for(&mut buffers, update.replicated_table_schema);
                    buffer.position = (commit_lsn, update.tx_ordinal);
                    let surrogate_key = surrogate_key_for_event(commit_lsn, update.tx_ordinal);

                    match update.updated_table_row {
                        UpdatedTableRow::Full(table_row) => {
                            if buffer.layout.append_only {
                                warn!(
                                    table_name = %buffer.schema.name(),
                                    "appending updated row because the source table has no \
                                     replica identity"
                                );
                            }

                            let row = row_to_json(
                                &buffer.schema,
                                &buffer.layout,
                                &table_row,
                                &surrogate_key,
                            )?;
                            let key = merge_key(&buffer.layout, &row);

                            // Postgres keeps a row's identity in the old image.
                            // When the key value changed, the destination still
                            // holds the row under the old key and has to drop
                            // it, otherwise the update leaves two rows behind.
                            if let Some(old_row) = &old_table_row
                                && !buffer.layout.append_only
                            {
                                let delete =
                                    delete_row_to_json(&buffer.schema, &buffer.layout, old_row)?;
                                let old_key = merge_key(&buffer.layout, &delete);
                                if old_key != key {
                                    buffer.push_full(old_key, delete);
                                }
                            }

                            buffer.push_full(key, row);
                        }
                        UpdatedTableRow::Partial(partial_row) => {
                            if buffer.layout.append_only {
                                warn!(
                                    table_name = %buffer.schema.name(),
                                    "skipping partial update because the source table has no \
                                     replica identity"
                                );
                            } else {
                                let (columns, row) = partial_row_to_json(
                                    &buffer.schema,
                                    &buffer.layout,
                                    &partial_row,
                                )?;
                                let key = merge_key(&buffer.layout, &row);
                                buffer.push_partial(columns, key, row);
                            }
                        }
                    }
                }
                Event::Delete(delete) => {
                    let commit_lsn = delete.commit_lsn.into();
                    let old_table_row = delete.old_table_row;
                    let buffer = Self::buffer_for(&mut buffers, delete.replicated_table_schema);
                    buffer.position = (commit_lsn, delete.tx_ordinal);

                    if buffer.layout.append_only {
                        warn!(
                            table_name = %buffer.schema.name(),
                            "skipping delete because the source table has no replica identity"
                        );
                        continue;
                    }

                    let Some(old_row) = old_table_row else {
                        return Err(etl_error!(
                            ErrorKind::SourceReplicaIdentityError,
                            "Doris delete requires an old row image",
                            format!(
                                "Table '{}' emitted a delete without an old row",
                                buffer.schema.name()
                            )
                        ));
                    };

                    let row = delete_row_to_json(&buffer.schema, &buffer.layout, &old_row)?;
                    let key = merge_key(&buffer.layout, &row);
                    buffer.push_full(key, row);
                }
                Event::Truncate(truncate) => {
                    self.flush(&mut buffers, &mut batch_index).await?;
                    for replicated_table_schema in &truncate.truncated_tables {
                        let table_name = self.ensure_table_ready(replicated_table_schema).await?;
                        let _ddl_permit = self.ddl_lock.lock().await;
                        let sql = build_truncate_table_sql(&table_name);
                        self.ddl.execute(&sql, "Doris truncate table failed").await?;
                    }
                }
                Event::Begin(_) | Event::Commit(_) | Event::Unsupported => {}
            }
        }

        self.flush(&mut buffers, &mut batch_index).await
    }

    /// Returns the buffer for one table, creating it on first use.
    fn buffer_for(
        buffers: &mut HashMap<TableId, TableBuffer>,
        replicated_table_schema: ReplicatedTableSchema,
    ) -> &mut TableBuffer {
        buffers
            .entry(replicated_table_schema.id())
            .or_insert_with(|| TableBuffer::new(replicated_table_schema))
    }

    /// Loads every buffered table write.
    async fn flush(
        &self,
        buffers: &mut HashMap<TableId, TableBuffer>,
        batch_index: &mut u32,
    ) -> EtlResult<()> {
        for buffer in buffers.drain().map(|(_, buffer)| buffer) {
            let table_name = self.table_name(&buffer.schema);
            let (commit_lsn, tx_ordinal) = buffer.position;
            let next_label = |batch_index: &mut u32| {
                let label = build_stream_load_label(
                    self.config.pipeline_id,
                    table_name.table(),
                    commit_lsn,
                    tx_ordinal,
                    *batch_index,
                );
                *batch_index += 1;
                label
            };

            // Segments load in event order, so a later conflicting row always
            // gets the higher Doris version.
            for segment in buffer.segments {
                let label = next_label(batch_index);
                match segment.partial_columns {
                    Some(columns) => {
                        self.load(&table_name, &label, &columns, true, segment.rows).await?;
                    }
                    None => {
                        let columns = column_names(&buffer.schema, &buffer.layout);
                        self.load(&table_name, &label, &columns, false, segment.rows).await?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Serializes rows and loads them, waiting out a duplicate in-flight label.
    async fn load(
        &self,
        table_name: &DorisTableName,
        label: &str,
        column_names: &[String],
        partial_columns: bool,
        rows: Vec<Value>,
    ) -> EtlResult<()> {
        if rows.is_empty() {
            return Ok(());
        }

        let columns_header = build_columns_header(column_names, true);
        let body = serde_json::to_vec(&rows).map_err(|source| {
            etl_error!(
                ErrorKind::DestinationError,
                "Doris stream load payload serialization failed",
                format!("table={table_name} label={label}"),
                source: source
            )
        })?;

        for _ in 0..IN_PROGRESS_MAX_ATTEMPTS {
            let outcome = self
                .stream_load
                .stream_load(table_name, label, &columns_header, partial_columns, body.clone())
                .await?;
            if outcome == StreamLoadOutcome::Committed {
                return Ok(());
            }

            debug!(
                table = %table_name,
                label,
                "waiting for an earlier doris load of the same label"
            );
            tokio::time::sleep(IN_PROGRESS_RETRY_DELAY).await;
        }

        Err(etl_error!(
            ErrorKind::DestinationError,
            "Doris stream load label stayed in progress",
            format!("table={table_name} label={label} attempts={IN_PROGRESS_MAX_ATTEMPTS}")
        ))
    }
}

impl<S> Destination for DorisDestination<S>
where
    S: DestinationStore,
{
    fn name() -> &'static str {
        "doris"
    }

    async fn shutdown(&self) -> EtlResult<()> {
        self.ddl.shutdown().await;

        Ok(())
    }

    async fn drop_table_for_copy(
        &self,
        replicated_table_schema: &ReplicatedTableSchema,
        async_result: DropTableForCopyResult<()>,
    ) -> EtlResult<()> {
        let table_name = self.table_name(replicated_table_schema);
        let sql = build_drop_table_sql(&table_name);
        let result = {
            let _ddl_permit = self.ddl_lock.lock().await;
            self.ddl.execute(&sql, "Doris drop table failed").await
        };

        async_result.send(result.clone());

        result
    }

    async fn write_table_rows(
        &self,
        replicated_table_schema: &ReplicatedTableSchema,
        table_rows: Vec<TableRow>,
        async_result: WriteTableRowsResult,
    ) -> EtlResult<()> {
        let result = self.write_copy_rows(replicated_table_schema, table_rows).await;
        async_result.send(result.clone().map(|()| DestinationWriteStatus::Durable));

        result
    }

    async fn write_events(
        &self,
        events: Vec<Event>,
        _durability: WriteEventsDurability,
        async_result: WriteEventsResult,
    ) -> EtlResult<()> {
        let result = self.write_stream_events(events).await;
        async_result.send(result.clone().map(|()| DestinationWriteStatus::Durable));

        result
    }
}

/// Buffered writes for one Doris table inside a streaming batch.
struct TableBuffer {
    schema: ReplicatedTableSchema,
    layout: DorisTableLayout,
    /// Loads to issue in event order.
    segments: Vec<LoadSegment>,
    /// Source position of the last buffered event, used to derive load labels.
    position: (u64, u64),
}

impl TableBuffer {
    /// Creates an empty buffer for one replicated schema.
    fn new(schema: ReplicatedTableSchema) -> Self {
        let layout = DorisTableLayout::from_schema(&schema);

        Self { schema, layout, segments: Vec::new(), position: (0, 0) }
    }

    /// Appends a row that carries the full declared column set.
    fn push_full(&mut self, key: Option<String>, row: Value) {
        push_segment_row(&mut self.segments, None, key, row);
    }

    /// Appends a row that carries only the columns the source sent.
    fn push_partial(&mut self, columns: Vec<String>, key: Option<String>, row: Value) {
        push_segment_row(&mut self.segments, Some(columns), key, row);
    }
}

/// Appends a row to the last segment, starting a new one when it cannot take
/// it.
fn push_segment_row(
    segments: &mut Vec<LoadSegment>,
    partial_columns: Option<Vec<String>>,
    key: Option<String>,
    row: Value,
) {
    let reusable = segments.last().is_some_and(|segment| {
        segment.partial_columns == partial_columns
            && key.as_ref().is_none_or(|key| !segment.keys.contains(key))
    });
    if !reusable {
        segments.push(LoadSegment { partial_columns, rows: Vec::new(), keys: HashSet::new() });
    }

    let segment = segments.last_mut().expect("a segment exists because one was just ensured");
    if let Some(key) = key {
        segment.keys.insert(key);
    }
    segment.rows.push(row);
}

/// One Stream Load worth of rows for a single table.
///
/// Doris does not define which row wins when one load carries several rows with
/// the same key, because the unique-key model resolves a conflict by load
/// version rather than by position in the payload. A segment therefore holds at
/// most one row per key, and a repeated key opens a new segment. Segments are
/// loaded in event order, so the later load carries the higher version and the
/// outcome matches the source.
struct LoadSegment {
    /// Column set this load declares, or [`None`] for the full set.
    partial_columns: Option<Vec<String>>,
    rows: Vec<Value>,
    /// Keys already in this segment, used to detect a repeat.
    keys: HashSet<String>,
}

/// Returns the merge key of a rendered row.
///
/// An append-only table has no source key, and its surrogate key is unique per
/// event, so those rows never collide and need no key tracking.
fn merge_key(layout: &DorisTableLayout, row: &Value) -> Option<String> {
    if layout.append_only {
        return None;
    }

    let object = row.as_object()?;
    let mut key = String::new();
    for column in &layout.key_columns {
        // The separator keeps distinct column tuples distinct.
        key.push('\u{1}');
        key.push_str(&object.get(column)?.to_string());
    }

    Some(key)
}

/// Builds the surrogate key of one streaming event.
///
/// The source position identifies the event, so replaying it produces the same
/// key and merge-on-write overwrites the row instead of duplicating it.
fn surrogate_key_for_event(commit_lsn: u64, tx_ordinal: u64) -> String {
    format!("{commit_lsn:016x}{tx_ordinal:016x}")
}

/// Returns the Doris column names for a replicated schema, surrogate key first.
fn column_names(
    replicated_table_schema: &ReplicatedTableSchema,
    layout: &DorisTableLayout,
) -> Vec<String> {
    let mut names = Vec::new();
    if layout.append_only {
        names.push(SURROGATE_KEY_COLUMN.to_owned());
    }
    names.extend(
        replicated_table_schema.column_schemas().map(|column_schema| column_schema.name.clone()),
    );

    names
}

/// Serializes a Doris table reference for durable destination metadata.
fn metadata_table_name(table_name: &DorisTableName) -> String {
    format!("{}.{}", table_name.database(), table_name.table())
}

/// Parses a Doris table reference from durable destination metadata.
fn parse_metadata_table_name(value: &str) -> EtlResult<DorisTableName> {
    value.split_once('.').map(|(database, table)| DorisTableName::new(database, table)).ok_or_else(
        || {
            etl_error!(
                ErrorKind::InvalidState,
                "Doris destination table metadata is invalid",
                format!("destination_table_id={value}")
            )
        },
    )
}

/// Generates one value of the ETL surrogate key column.
fn new_surrogate_key() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// Renders one source row as a Stream Load JSON object.
fn row_to_json(
    replicated_table_schema: &ReplicatedTableSchema,
    layout: &DorisTableLayout,
    table_row: &TableRow,
    surrogate_key: &str,
) -> EtlResult<Value> {
    let column_schemas: Vec<_> = replicated_table_schema.column_schemas().collect();
    if column_schemas.len() != table_row.values().len() {
        return Err(row_shape_error(
            replicated_table_schema,
            column_schemas.len(),
            table_row.values().len(),
        ));
    }

    let mut object = Map::with_capacity(column_schemas.len() + 2);
    if layout.append_only {
        object.insert(SURROGATE_KEY_COLUMN.to_owned(), Value::String(surrogate_key.to_owned()));
    }
    for (column_schema, cell) in column_schemas.iter().zip(table_row.values()) {
        object.insert(column_schema.name.clone(), cell_to_json(cell));
    }
    object.insert(DELETE_SIGN_COLUMN.to_owned(), json!(0));

    Ok(Value::Object(object))
}

/// Renders a delete as a row carrying the Doris delete sign.
///
/// Merge-on-write matches the row by its key columns, so the value columns are
/// sent as null to keep the payload shaped like the declared column set.
fn delete_row_to_json(
    replicated_table_schema: &ReplicatedTableSchema,
    layout: &DorisTableLayout,
    old_row: &OldTableRow,
) -> EtlResult<Value> {
    let table_row = match old_row {
        OldTableRow::Full(row) | OldTableRow::Key(row) => row,
    };
    let column_schemas: Vec<_> = replicated_table_schema.column_schemas().collect();
    let mut object = Map::with_capacity(column_schemas.len() + 2);
    let mut present_keys = 0;

    for (column_schema, cell) in column_schemas.iter().zip(table_row.values()) {
        if layout.key_columns.contains(&column_schema.name) {
            present_keys += 1;
            object.insert(column_schema.name.clone(), cell_to_json(cell));
        } else {
            object.insert(column_schema.name.clone(), Value::Null);
        }
    }

    if present_keys != layout.key_columns.len() {
        return Err(etl_error!(
            ErrorKind::SourceReplicaIdentityError,
            "Doris delete is missing key columns",
            format!(
                "table='{}' expected_keys={} present_keys={present_keys}",
                replicated_table_schema.name(),
                layout.key_columns.len()
            )
        ));
    }

    object.insert(DELETE_SIGN_COLUMN.to_owned(), json!(1));

    Ok(Value::Object(object))
}

/// Renders a partial update as a row limited to the columns the source sent.
fn partial_row_to_json(
    replicated_table_schema: &ReplicatedTableSchema,
    layout: &DorisTableLayout,
    partial_row: &PartialTableRow,
) -> EtlResult<(Vec<String>, Value)> {
    let column_schemas: Vec<_> = replicated_table_schema.column_schemas().collect();
    let missing = partial_row.missing_column_indexes();
    let mut present_values = partial_row.values().iter();
    let mut object = Map::new();
    let mut columns = Vec::new();

    for (column_index, column_schema) in column_schemas.iter().enumerate() {
        if missing.contains(&column_index) {
            continue;
        }

        let Some(cell) = present_values.next() else {
            return Err(row_shape_error(
                replicated_table_schema,
                column_schemas.len() - missing.len(),
                partial_row.values().len(),
            ));
        };
        object.insert(column_schema.name.clone(), cell_to_json(cell));
        columns.push(column_schema.name.clone());
    }

    for key_column in &layout.key_columns {
        if !columns.contains(key_column) {
            return Err(etl_error!(
                ErrorKind::SourceReplicaIdentityError,
                "Doris partial update is missing key columns",
                format!("table='{}' key_column='{key_column}'", replicated_table_schema.name())
            ));
        }
    }

    object.insert(DELETE_SIGN_COLUMN.to_owned(), json!(0));

    Ok((columns, Value::Object(object)))
}

/// Builds the error raised when a row does not match its schema width.
fn row_shape_error(
    replicated_table_schema: &ReplicatedTableSchema,
    expected: usize,
    actual: usize,
) -> EtlError {
    etl_error!(
        ErrorKind::InvalidState,
        "Doris row shape does not match schema",
        format!("table='{}' expected={expected} actual={actual}", replicated_table_schema.name())
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn keyed_layout() -> DorisTableLayout {
        DorisTableLayout { key_columns: vec!["id".to_owned()], append_only: false }
    }

    fn append_only_layout() -> DorisTableLayout {
        DorisTableLayout { key_columns: vec![SURROGATE_KEY_COLUMN.to_owned()], append_only: true }
    }

    /// Runs the real segment logic and returns the row count of each segment.
    fn segments_of(
        layout: DorisTableLayout,
        rows: Vec<(Option<Vec<String>>, Value)>,
    ) -> Vec<usize> {
        let mut segments: Vec<LoadSegment> = Vec::new();
        for (partial_columns, row) in rows {
            let key = merge_key(&layout, &row);
            push_segment_row(&mut segments, partial_columns, key, row);
        }

        segments.iter().map(|segment| segment.rows.len()).collect()
    }

    #[test]
    fn distinct_keys_share_one_segment() {
        let rows = vec![(None, json!({ "id": 1 })), (None, json!({ "id": 2 }))];
        assert_eq!(segments_of(keyed_layout(), rows), vec![2]);
    }

    #[test]
    fn a_repeated_key_opens_a_new_segment() {
        // Doris does not define which row wins inside one load, so the second
        // change to the same key has to become its own higher-version load.
        let rows = vec![
            (None, json!({ "id": 1 })),
            (None, json!({ "id": 2 })),
            (None, json!({ "id": 1 })),
        ];
        assert_eq!(segments_of(keyed_layout(), rows), vec![2, 1]);
    }

    #[test]
    fn a_partial_column_set_opens_a_new_segment() {
        // A partial load declares its own column set and uses a different write
        // mode, so mixing it into a full load would reorder the two.
        let rows = vec![
            (None, json!({ "id": 1 })),
            (Some(vec!["id".to_owned(), "name".to_owned()]), json!({ "id": 2 })),
            (None, json!({ "id": 3 })),
        ];
        assert_eq!(segments_of(keyed_layout(), rows), vec![1, 1, 1]);
    }

    #[test]
    fn append_only_rows_never_split_a_segment() {
        let rows = vec![
            (None, json!({ SURROGATE_KEY_COLUMN: "a" })),
            (None, json!({ SURROGATE_KEY_COLUMN: "a" })),
        ];
        assert_eq!(segments_of(append_only_layout(), rows), vec![2]);
    }

    #[test]
    fn the_surrogate_key_is_derived_from_the_source_position() {
        assert_eq!(surrogate_key_for_event(0x1234, 5), "00000000000012340000000000000005");
        assert_eq!(surrogate_key_for_event(0x1234, 5), surrogate_key_for_event(0x1234, 5));
        assert_ne!(surrogate_key_for_event(0x1234, 5), surrogate_key_for_event(0x1234, 6));
    }
}
