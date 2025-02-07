#![allow(dead_code)]
#![allow(unreachable_patterns)]
#![feature(maybe_uninit_array_assume_init)]
#![feature(maybe_uninit_uninit_array)]

use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
pub use compaction::check::vnode_table_checksum_schema;
use compaction::{CompactTask, FlushReq};
use context::GlobalContext;
use datafusion::arrow::record_batch::RecordBatch;
use models::meta_data::{NodeId, VnodeId};
use models::predicate::domain::{ColumnDomains, ResolvedPredicate};
use models::schema::Precision;
use models::{SeriesId, SeriesKey, TagKey, TagValue, Timestamp};
use protos::kv_service::{WritePointsRequest, WritePointsResponse};
use serde::{Deserialize, Serialize};
use summary::SummaryTask;
use tokio::sync::mpsc::Sender;
use tokio::sync::RwLock;
use trace::SpanContext;
use version_set::VersionSet;
use vnode_store::VnodeStorage;
use wal::WalTask;

pub use crate::error::{Error, Result};
pub use crate::kv_option::Options;
use crate::kv_option::StorageOptions;
pub use crate::kvcore::TsKv;
use crate::summary::CompactMeta;
pub use crate::summary::{print_summary_statistics, Summary, VersionEdit};
use crate::tseries_family::SuperVersion;
pub use crate::tsm::print_tsm_statistics;
pub use crate::wal::print_wal_statistics;

pub mod byte_utils;
mod compaction;
mod compute;
mod context;
pub mod database;
pub mod engine_mock;
pub mod error;
pub mod file_system;
pub mod file_utils;
pub mod index;
pub mod kv_option;
mod kvcore;
mod memcache;
// TODO supposedly private
pub mod reader;
mod record_file;
mod schema;
mod summary;
mod tseries_family;
pub mod tsm;
mod version_set;
pub mod vnode_store;
pub mod wal;

/// The column file ID is unique in a KV instance
/// and uniquely corresponds to one column file.
pub type ColumnFileId = u64;
type TseriesFamilyId = VnodeId;
type LevelId = u32;

pub type EngineRef = Arc<dyn Engine>;

#[derive(PartialEq, Eq, Hash)]
pub struct UpdateSetValue<K, V> {
    pub key: K,
    pub value: Option<V>,
}

#[async_trait]
pub trait Engine: Send + Sync + Debug {
    /// Tskv engine write the gRPC message `WritePointsRequest`(which contains
    /// the tenant, user, database, some tables, and each table has some rows)
    /// into a `Vnode` managed by engine.
    ///
    /// - span_ctx - The trace span.
    /// - vnode_id - ID of the storage unit(caches and files).
    /// - precision - The timestamp precision of table rows.
    ///
    /// Data will be written to the write-ahead-log first.
    async fn write(
        &self,
        span_ctx: Option<&SpanContext>,
        vnode_id: VnodeId,
        precision: Precision,
        write_batch: WritePointsRequest,
    ) -> Result<WritePointsResponse>;

    /// Remove all storage unit(caches and files) in specified database,
    /// then remove directory of the database.
    async fn drop_database(&self, tenant: &str, database: &str) -> Result<()>;

    /// Delete all data of a table.
    async fn drop_table(&self, tenant: &str, database: &str, table: &str) -> Result<()>;

    /// open a tsfamily, if already exist just return.
    async fn open_tsfamily(
        &self,
        tenant: &str,
        db_name: &str,
        vnode_id: VnodeId,
    ) -> Result<VnodeStorage>;

    /// Remove the storage unit(caches and files) managed by engine,
    /// then remove directory of the storage unit.
    async fn remove_tsfamily(&self, tenant: &str, database: &str, vnode_id: VnodeId) -> Result<()>;

    /// Mark the storage unit as `Copying` and flush caches.
    async fn prepare_copy_vnode(
        &self,
        tenant: &str,
        database: &str,
        vnode_id: VnodeId,
    ) -> Result<()>;

    /// Flush all caches of the storage unit into a file.
    async fn flush_tsfamily(&self, tenant: &str, database: &str, vnode_id: VnodeId) -> Result<()>;

    // TODO this method is not completed,
    async fn drop_table_column(
        &self,
        tenant: &str,
        database: &str,
        table: &str,
        column: &str,
    ) -> Result<()>;

    /// Update the value of the tag type columns of the specified table
    ///
    /// `new_tags` is the new tags, and the tag key must be included in all series
    ///
    /// # Parameters
    /// - `tenant` - The tenant name.
    /// - `database` - The database name.
    /// - `new_tags` - The tags and its new tag value.
    /// - `matched_series` - The series that need to be updated.
    /// - `dry_run` - Whether to only check if the `update_tags_value` is successful, if it is true, the update will not be performed.
    ///
    /// # Examples
    ///
    /// We have a table `tbl` as follows
    ///
    /// ```text
    /// +----+-----+-----+-----+
    /// | ts | tag1| tag2|field|
    /// +----+-----+-----+-----+
    /// | 1  | t1a | t2b | f1  |
    /// +----+-----+-----+-----+
    /// | 2  | t1a | t2c | f2  |
    /// +----+-----+-----+-----+
    /// | 3  | t1b | t2c | f3  |
    /// +----+-----+-----+-----+
    /// ```
    ///
    /// Execute the following update statement
    ///
    /// ```sql
    /// UPDATE tbl SET tag1 = 't1c' WHERE tag2 = 't2c';
    /// ```
    ///
    /// The `new_tags` is `[tag1 = 't1c']`, and the `matched_series` is `[(tag1 = 't1a', tag2 = 't2c'), (tag1 = 't1b', tag2 = 't2c')]`
    ///
    /// TODO Specify vnode id
    async fn update_tags_value(
        &self,
        tenant: &str,
        database: &str,
        new_tags: &[UpdateSetValue<TagKey, TagValue>],
        matched_series: &[SeriesKey],
        dry_run: bool,
    ) -> Result<()>;

    // TODO this method is not completed,
    // TODO(zipper): Delete data on table
    async fn delete_from_table(
        &self,
        vnode_id: VnodeId,
        tenant: &str,
        database: &str,
        table: &str,
        predicate: &ResolvedPredicate,
    ) -> Result<()>;

    /// Read index of a storage unit, find series ids that matches the filter.
    async fn get_series_id_by_filter(
        &self,
        tenant: &str,
        database: &str,
        table: &str,
        vnode_id: VnodeId,
        filter: &ColumnDomains<String>,
    ) -> Result<Vec<SeriesId>>;

    /// Read index of a storage unit, get `SeriesKey` of the geiven series id.
    async fn get_series_key(
        &self,
        tenant: &str,
        database: &str,
        table: &str,
        vnode_id: VnodeId,
        series_id: &[SeriesId],
    ) -> Result<Vec<SeriesKey>>;

    /// Get a `SuperVersion` that contains the latest version of caches and files
    /// of the storage unit.
    async fn get_db_version(
        &self,
        tenant: &str,
        database: &str,
        vnode_id: u32,
    ) -> Result<Option<Arc<SuperVersion>>>;

    /// Get the storage options which was used to install the engine.
    fn get_storage_options(&self) -> Arc<StorageOptions>;

    /// Get the summary(information of files) of the storage unit.
    async fn get_vnode_summary(
        &self,
        tenant: &str,
        database: &str,
        vnode_id: u32,
    ) -> Result<Option<VersionEdit>>;

    /// Try to build a new storage unit from the summary(information of files),
    /// if it already exists, delete first.
    async fn apply_vnode_summary(
        &self,
        tenant: &str,
        database: &str,
        vnode_id: u32,
        summary: VersionEdit,
    ) -> Result<()>;

    // TODO this method is the same as remove_tsfamily and not be referenced,
    // we can delete it.
    #[deprecated]
    async fn drop_vnode(&self, id: TseriesFamilyId) -> Result<()>;

    /// For the specified storage units, flush all caches into files, then compact
    /// files into larger files.
    async fn compact(&self, vnode_ids: Vec<TseriesFamilyId>) -> Result<()>;

    /// Get a compressed hash_tree(ID and checksum of each vnode) of engine.
    async fn get_vnode_hash_tree(&self, vnode_id: VnodeId) -> Result<RecordBatch>;

    /// Close all background jobs of engine.
    async fn close(&self);
}

#[derive(Debug, Clone)]
pub struct TsKvContext {
    pub options: Arc<Options>,
    pub global_ctx: Arc<GlobalContext>,
    pub version_set: Arc<RwLock<VersionSet>>,

    pub wal_sender: Sender<WalTask>,
    pub flush_task_sender: Sender<FlushReq>,
    pub compact_task_sender: Sender<CompactTask>,
    pub summary_task_sender: Sender<SummaryTask>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VnodeSnapshot {
    pub snapshot_id: String,
    pub node_id: NodeId,
    pub tenant: String,
    pub database: String,
    pub vnode_id: VnodeId,
    pub files: Vec<SnapshotFileMeta>,
    pub last_seq_no: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SnapshotFileMeta {
    pub file_id: ColumnFileId,
    pub file_size: u64,
    pub level: LevelId,
    pub min_ts: Timestamp,
    pub max_ts: Timestamp,
}

impl From<&CompactMeta> for SnapshotFileMeta {
    fn from(cm: &CompactMeta) -> Self {
        Self {
            file_id: cm.file_id,
            file_size: cm.file_size,
            level: cm.level,
            min_ts: cm.min_ts,
            max_ts: cm.max_ts,
        }
    }
}

pub mod test {
    pub use crate::memcache::test::{get_one_series_cache_data, put_rows_to_cache};
}
