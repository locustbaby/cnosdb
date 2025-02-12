use std::collections::HashMap;
use std::sync::Arc;

use memory_pool::MemoryPoolRef;
use meta::model::MetaRef;
use metrics::metric_register::MetricsRegister;
use models::schema::{make_owner, split_owner, DatabaseSchema};
use snafu::ResultExt;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::Sender;
use tokio::sync::RwLock;
use utils::BloomFilter;

use crate::compaction::CompactTask;
use crate::database::{Database, DatabaseFactory};
use crate::error::{MetaSnafu, Result};
use crate::index::ts_index::TSIndex;
use crate::summary::VersionEdit;
use crate::tseries_family::{TseriesFamily, Version};
use crate::{ColumnFileId, Options, TseriesFamilyId};

#[derive(Debug)]
pub struct VersionSet {
    /// Maps DBName -> DB
    dbs: HashMap<String, Arc<RwLock<Database>>>,
    runtime: Arc<Runtime>,
    db_factory: DatabaseFactory,
}

impl VersionSet {
    pub fn empty(
        meta: MetaRef,
        opt: Arc<Options>,
        runtime: Arc<Runtime>,
        memory_pool: MemoryPoolRef,
        metrics_register: Arc<MetricsRegister>,
    ) -> Self {
        let db_factory = DatabaseFactory::new(meta, memory_pool.clone(), metrics_register, opt);

        Self {
            dbs: HashMap::new(),
            runtime,
            db_factory,
        }
    }

    #[cfg(test)]
    pub fn build_empty_test(runtime: Arc<Runtime>) -> Self {
        use meta::model::meta_admin::AdminMeta;
        let opt = Arc::new(Options::from(&config::Config::default()));
        let register = Arc::new(MetricsRegister::default());
        let memory_pool = Arc::new(memory_pool::GreedyMemoryPool::default());
        let meta = Arc::new(AdminMeta::mock());
        let db_factory = DatabaseFactory::new(meta, memory_pool, register, opt);
        Self {
            dbs: HashMap::new(),
            runtime,
            db_factory,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        meta: MetaRef,
        opt: Arc<Options>,
        runtime: Arc<Runtime>,
        memory_pool: MemoryPoolRef,
        ver_set: HashMap<TseriesFamilyId, Arc<Version>>,
        compact_task_sender: Sender<CompactTask>,
        metrics_register: Arc<MetricsRegister>,
    ) -> Result<Self> {
        let mut dbs = HashMap::new();
        let db_factory = DatabaseFactory::new(meta.clone(), memory_pool, metrics_register, opt);

        for ver in ver_set.into_values() {
            let owner = ver.tenant_database().to_string();
            let (tenant, database) = split_owner(&owner);

            let schema = match meta.tenant_meta(tenant).await {
                None => DatabaseSchema::new(tenant, database),
                Some(client) => match client.get_db_schema(database).context(MetaSnafu)? {
                    None => DatabaseSchema::new(tenant, database),
                    Some(schema) => schema,
                },
            };

            let db: &mut Arc<RwLock<Database>> = dbs.entry(owner).or_insert(Arc::new(RwLock::new(
                db_factory.create_database(schema).await?,
            )));

            let tf_id = ver.tf_id();
            db.write()
                .await
                .open_tsfamily(runtime.clone(), ver, compact_task_sender.clone());
            db.write().await.get_ts_index_or_add(tf_id).await?;
        }

        Ok(Self {
            dbs,
            runtime,
            db_factory,
        })
    }

    pub async fn create_db(&mut self, schema: DatabaseSchema) -> Result<Arc<RwLock<Database>>> {
        let db = self
            .dbs
            .entry(schema.owner())
            .or_insert(Arc::new(RwLock::new(
                self.db_factory.create_database(schema).await?,
            )))
            .clone();
        Ok(db)
    }

    pub fn delete_db(&mut self, tenant: &str, database: &str) -> Option<Arc<RwLock<Database>>> {
        let owner = make_owner(tenant, database);
        self.dbs.remove(&owner)
    }

    pub fn db_exists(&self, tenant: &str, database: &str) -> bool {
        let owner = make_owner(tenant, database);
        self.dbs.get(&owner).is_some()
    }

    pub async fn get_db_schema(
        &self,
        tenant: &str,
        database: &str,
    ) -> Result<Option<DatabaseSchema>> {
        let owner = make_owner(tenant, database);
        let db = self.dbs.get(&owner);
        match db {
            None => Ok(None),
            Some(db) => Ok(Some(db.read().await.get_schema()?)),
        }
    }

    pub fn get_all_db(&self) -> &HashMap<String, Arc<RwLock<Database>>> {
        &self.dbs
    }

    pub fn get_db(&self, tenant_name: &str, db_name: &str) -> Option<Arc<RwLock<Database>>> {
        let owner_name = make_owner(tenant_name, db_name);
        if let Some(v) = self.dbs.get(&owner_name) {
            return Some(v.clone());
        }

        None
    }

    pub async fn get_tsfamily_by_tf_id(&self, tf_id: u32) -> Option<Arc<RwLock<TseriesFamily>>> {
        // FIXME: add tsf_id -> db HashTable
        for db in self.dbs.values() {
            if let Some(v) = db.read().await.get_tsfamily(tf_id) {
                return Some(v);
            }
        }

        None
    }

    pub async fn get_tsfamily_tsindex_by_tf_id(
        &self,
        tf_id: u32,
    ) -> (Option<Arc<RwLock<TseriesFamily>>>, Option<Arc<TSIndex>>) {
        let mut vnode = None;
        let mut vnode_index = None;
        for db in self.dbs.values() {
            let db = db.read().await;
            if let Some(v) = db.get_tsfamily(tf_id) {
                vnode = Some(v);
                if let Some(v) = db.get_ts_index(tf_id) {
                    vnode_index = Some(v);
                }
                break;
            }
        }

        (vnode, vnode_index)
    }

    pub async fn get_tsfamily_by_name_id(
        &self,
        tenant: &str,
        database: &str,
        tf_id: u32,
    ) -> Option<Arc<RwLock<TseriesFamily>>> {
        let owner = make_owner(tenant, database);
        if let Some(db) = self.dbs.get(&owner) {
            return db.read().await.get_tsfamily(tf_id);
        }

        None
    }

    /// Snapshots last version before `last_seq` of system state.
    ///
    /// Generated data is `VersionEdit`s for all vnodes and db-files,
    /// and `HashMap<ColumnFileId, Arc<BloomFilter>>` for index data
    /// (field-id filter) of db-files.
    pub async fn snapshot(&self) -> (Vec<VersionEdit>, HashMap<ColumnFileId, Arc<BloomFilter>>) {
        let mut version_edits = vec![];
        let mut file_metas: HashMap<ColumnFileId, Arc<BloomFilter>> = HashMap::new();
        for db in self.dbs.values() {
            db.read()
                .await
                .snapshot(None, &mut version_edits, &mut file_metas)
                .await;
        }
        (version_edits, file_metas)
    }

    pub async fn get_tsfamily_seq_no_map(&self) -> HashMap<TseriesFamilyId, u64> {
        let mut r = HashMap::with_capacity(self.dbs.len());
        for db in self.dbs.values() {
            let db = db.read().await;
            for tsf in db.ts_families().values() {
                let tsf = tsf.read().await;
                r.insert(tsf.tf_id(), tsf.super_version().version.last_seq());
            }
        }
        r
    }
}

#[cfg(test)]
impl VersionSet {
    pub async fn tsf_num(&self) -> usize {
        let mut size = 0;
        for db in self.dbs.values() {
            size += db.read().await.tsf_num();
        }

        size
    }

    pub async fn get_database_tsfs(
        &self,
        tenant: &str,
        database: &str,
    ) -> Option<Vec<Arc<RwLock<TseriesFamily>>>> {
        let owner = make_owner(tenant, database);
        if let Some(db) = self.dbs.get(&owner) {
            return Some(db.read().await.ts_families().values().cloned().collect());
        }

        None
    }
}
