use std::sync::Arc;

use async_trait::async_trait;
use coordinator::service::CoordinatorRef;
use memory_pool::MemoryPoolRef;
use meta::error::MetaError;
use meta::model::MetaClientRef;
use models::auth::user::admin_user;
use models::oid::Oid;
use spi::query::ast::ExtStatement;
use spi::query::datasource::stream::StreamProviderManagerRef;
use spi::query::dispatcher::{QueryDispatcher, QueryInfo, QueryStatus};
use spi::query::execution::{Output, QueryStateMachine};
use spi::query::function::FuncMetaManagerRef;
use spi::query::logical_planner::{LogicalPlanner, Plan};
use spi::query::parser::Parser;
use spi::query::session::{SessionCtx, SessionCtxFactory};
use spi::service::protocol::{ContextBuilder, Query, QueryId};
use spi::{QueryError, Result};
use trace::{info, SpanContext, SpanExt, SpanRecorder, TraceExporter};

use super::query_tracker::QueryTracker;
use crate::data_source::split::SplitManagerRef;
use crate::execution::factory::QueryExecutionFactoryRef;
use crate::metadata::{
    BaseTableProvider, ContextProviderExtension, MetadataProvider, TableHandleProviderRef,
};
use crate::sql::logical::planner::DefaultLogicalPlanner;

#[derive(Clone)]
pub struct SimpleQueryDispatcher {
    coord: CoordinatorRef,
    // client for default tenant
    default_table_provider: TableHandleProviderRef,
    split_manager: SplitManagerRef,
    session_factory: Arc<SessionCtxFactory>,
    // memory pool
    memory_pool: MemoryPoolRef,
    // query tracker
    query_tracker: Arc<QueryTracker>,
    // parser
    parser: Arc<dyn Parser + Send + Sync>,
    // get query execution factory
    query_execution_factory: QueryExecutionFactoryRef,
    func_manager: FuncMetaManagerRef,
    stream_provider_manager: StreamProviderManagerRef,
    trace_collector: Option<Arc<dyn TraceExporter>>,
}

#[async_trait]
impl QueryDispatcher for SimpleQueryDispatcher {
    async fn start(&self) -> Result<()> {
        // 执行被持久化的任务
        let queries = self.query_tracker.persistent_queries().await?;

        for query in queries {
            let query_id = query.query_id();
            let sql = query.query();
            let database_name = query.database_name();
            let tenant_name = query.tenant_name();
            let tenant_id = query.tenant_id();
            let user_desc = query.user_desc();
            let user = admin_user(user_desc.to_owned());
            let ctx = ContextBuilder::new(user)
                .with_tenant(Some(tenant_name.to_owned()))
                .with_database(Some(database_name.to_owned()))
                .build();
            let query = Query::new(ctx, sql.to_owned());
            let span_context = self.trace_collector.clone().map(SpanContext::new);
            match self
                .execute_query(tenant_id, query_id, &query, span_context.as_ref())
                .await
            {
                Ok(_) => {
                    info!("Re-execute persistent query: {}", query.content());
                }
                Err(err) => {
                    trace::warn!("Ignore, failed to re-execute persistent query: {}", err)
                }
            }
        }

        Ok(())
    }

    fn stop(&self) {
        // TODO
    }

    fn create_query_id(&self) -> QueryId {
        QueryId::next_id()
    }

    fn query_info(&self, _id: &QueryId) {
        // TODO
    }

    async fn execute_query(
        &self,
        tenant_id: Oid,
        query_id: QueryId,
        query: &Query,
        span_ctx: Option<&SpanContext>,
    ) -> Result<Output> {
        let query_state_machine = {
            let _span_recorder = SpanRecorder::new(span_ctx.child_span("init session ctx"));
            self.build_query_state_machine(tenant_id, query_id, query.clone(), span_ctx)
                .await?
        };

        let logical_plan = self.build_logical_plan(query_state_machine.clone()).await?;
        let logical_plan = match logical_plan {
            Some(plan) => plan,
            None => return Ok(Output::Nil(())),
        };
        let result = self
            .execute_logical_plan(logical_plan, query_state_machine)
            .await?;
        Ok(result)
    }

    async fn build_logical_plan(
        &self,
        query_state_machine: Arc<QueryStateMachine>,
    ) -> Result<Option<Plan>> {
        let session = &query_state_machine.session;
        let query = &query_state_machine.query;

        let scheme_provider = self.build_scheme_provider(session).await?;

        let logical_planner = DefaultLogicalPlanner::new(&scheme_provider);

        let span_recorder = session.get_child_span_recorder("parse sql");
        let statements = self.parser.parse(query.content())?;

        // not allow multi statement
        if statements.len() > 1 {
            return Err(QueryError::MultiStatement {
                num: statements.len(),
                sql: query_state_machine.query.content().to_string(),
            });
        }

        let stmt = match statements.front() {
            Some(stmt) => stmt.clone(),
            None => return Ok(None),
        };

        drop(span_recorder);

        let logical_plan = self
            .statement_to_logical_plan(stmt, &logical_planner, query_state_machine)
            .await?;
        Ok(Some(logical_plan))
    }

    async fn execute_logical_plan(
        &self,
        logical_plan: Plan,
        query_state_machine: Arc<QueryStateMachine>,
    ) -> Result<Output> {
        self.execute_logical_plan(logical_plan, query_state_machine)
            .await
    }

    async fn build_query_state_machine(
        &self,
        tenant_id: Oid,
        query_id: QueryId,
        query: Query,
        span_ctx: Option<&SpanContext>,
    ) -> Result<Arc<QueryStateMachine>> {
        let session = self.session_factory.create_session_ctx(
            query_id.to_string(),
            query.context(),
            tenant_id,
            self.memory_pool.clone(),
            span_ctx.cloned(),
        )?;

        let query_state_machine = Arc::new(QueryStateMachine::begin(
            query_id,
            query,
            session,
            self.coord.clone(),
        ));
        Ok(query_state_machine)
    }

    fn running_query_infos(&self) -> Vec<QueryInfo> {
        self.query_tracker
            .running_queries()
            .iter()
            .map(|e| e.info())
            .collect()
    }

    fn running_query_status(&self) -> Vec<QueryStatus> {
        self.query_tracker
            .running_queries()
            .iter()
            .map(|e| e.status())
            .collect()
    }

    fn cancel_query(&self, id: &QueryId) {
        self.query_tracker.query(id).map(|e| e.cancel());
    }
}

impl SimpleQueryDispatcher {
    async fn statement_to_logical_plan<S: ContextProviderExtension + Send + Sync>(
        &self,
        stmt: ExtStatement,
        logical_planner: &DefaultLogicalPlanner<'_, S>,
        query_state_machine: Arc<QueryStateMachine>,
    ) -> Result<Plan> {
        // begin analyze
        query_state_machine.begin_analyze();
        let logical_plan = logical_planner
            .create_logical_plan(stmt, &query_state_machine.session)
            .await?;
        query_state_machine.end_analyze();

        Ok(logical_plan)
    }

    async fn execute_logical_plan(
        &self,
        logical_plan: Plan,
        query_state_machine: Arc<QueryStateMachine>,
    ) -> Result<Output> {
        let execution = self
            .query_execution_factory
            .create_query_execution(logical_plan, query_state_machine.clone())?;

        // TrackedQuery.drop() is called implicitly when the value goes out of scope,
        self.query_tracker
            .try_track_query(query_state_machine.query_id, execution)
            .await?
            .start()
            .await
    }

    async fn build_scheme_provider(&self, session: &SessionCtx) -> Result<MetadataProvider> {
        let meta_client = self.build_current_session_meta_client(session).await?;
        let current_session_table_provider =
            self.build_table_handle_provider(meta_client.clone())?;
        let metadata_provider = MetadataProvider::new(
            self.coord.clone(),
            meta_client,
            current_session_table_provider,
            self.default_table_provider.clone(),
            self.func_manager.clone(),
            self.query_tracker.clone(),
            session.clone(),
        );

        Ok(metadata_provider)
    }

    async fn build_current_session_meta_client(
        &self,
        session: &SessionCtx,
    ) -> Result<MetaClientRef> {
        let meta_client = self
            .coord
            .tenant_meta(session.tenant())
            .await
            .ok_or_else(|| MetaError::TenantNotFound {
                tenant: session.tenant().to_string(),
            })?;

        Ok(meta_client)
    }

    fn build_table_handle_provider(
        &self,
        meta_client: MetaClientRef,
    ) -> Result<TableHandleProviderRef> {
        let current_session_table_provider: Arc<BaseTableProvider> =
            Arc::new(BaseTableProvider::new(
                self.coord.clone(),
                self.split_manager.clone(),
                meta_client,
                self.stream_provider_manager.clone(),
            ));

        Ok(current_session_table_provider)
    }
}

#[derive(Default, Clone)]
pub struct SimpleQueryDispatcherBuilder {
    coord: Option<CoordinatorRef>,
    default_table_provider: Option<TableHandleProviderRef>,
    split_manager: Option<SplitManagerRef>,
    session_factory: Option<Arc<SessionCtxFactory>>,
    parser: Option<Arc<dyn Parser + Send + Sync>>,

    query_execution_factory: Option<QueryExecutionFactoryRef>,
    query_tracker: Option<Arc<QueryTracker>>,
    memory_pool: Option<MemoryPoolRef>, // memory

    func_manager: Option<FuncMetaManagerRef>,
    stream_provider_manager: Option<StreamProviderManagerRef>,
    trace_collector: Option<Arc<dyn TraceExporter>>,
}

impl SimpleQueryDispatcherBuilder {
    pub fn with_coord(mut self, coord: CoordinatorRef) -> Self {
        self.coord = Some(coord);
        self
    }

    pub fn with_default_table_provider(
        mut self,
        default_table_provider: TableHandleProviderRef,
    ) -> Self {
        self.default_table_provider = Some(default_table_provider);
        self
    }

    pub fn with_session_factory(mut self, session_factory: Arc<SessionCtxFactory>) -> Self {
        self.session_factory = Some(session_factory);
        self
    }

    pub fn with_split_manager(mut self, split_manager: SplitManagerRef) -> Self {
        self.split_manager = Some(split_manager);
        self
    }

    pub fn with_parser(mut self, parser: Arc<dyn Parser + Send + Sync>) -> Self {
        self.parser = Some(parser);
        self
    }

    pub fn with_query_execution_factory(
        mut self,
        query_execution_factory: QueryExecutionFactoryRef,
    ) -> Self {
        self.query_execution_factory = Some(query_execution_factory);
        self
    }

    pub fn with_query_tracker(mut self, query_tracker: Arc<QueryTracker>) -> Self {
        self.query_tracker = Some(query_tracker);
        self
    }

    pub fn with_memory_pool(mut self, memory_pool: MemoryPoolRef) -> Self {
        self.memory_pool = Some(memory_pool);
        self
    }

    pub fn with_func_manager(mut self, func_manager: FuncMetaManagerRef) -> Self {
        self.func_manager = Some(func_manager);
        self
    }

    pub fn with_stream_provider_manager(
        mut self,
        stream_provider_manager: StreamProviderManagerRef,
    ) -> Self {
        self.stream_provider_manager = Some(stream_provider_manager);
        self
    }

    pub fn with_trace_collector(mut self, trace_collector: Arc<dyn TraceExporter>) -> Self {
        self.trace_collector = Some(trace_collector);
        self
    }

    pub fn build(self) -> Result<SimpleQueryDispatcher> {
        let coord = self.coord.ok_or_else(|| QueryError::BuildQueryDispatcher {
            err: "lost of coord".to_string(),
        })?;

        let split_manager = self
            .split_manager
            .ok_or_else(|| QueryError::BuildQueryDispatcher {
                err: "lost of split manager".to_string(),
            })?;

        let session_factory =
            self.session_factory
                .ok_or_else(|| QueryError::BuildQueryDispatcher {
                    err: "lost of session_factory".to_string(),
                })?;

        let parser = self
            .parser
            .ok_or_else(|| QueryError::BuildQueryDispatcher {
                err: "lost of parser".to_string(),
            })?;

        let query_execution_factory =
            self.query_execution_factory
                .ok_or_else(|| QueryError::BuildQueryDispatcher {
                    err: "lost of query_execution_factory".to_string(),
                })?;

        let query_tracker = self
            .query_tracker
            .ok_or_else(|| QueryError::BuildQueryDispatcher {
                err: "lost of query_tracker".to_string(),
            })?;

        let func_manager = self
            .func_manager
            .ok_or_else(|| QueryError::BuildQueryDispatcher {
                err: "lost of func_manager".to_string(),
            })?;

        let stream_provider_manager =
            self.stream_provider_manager
                .ok_or_else(|| QueryError::BuildQueryDispatcher {
                    err: "lost of stream_provider_manager".to_string(),
                })?;

        let memory_pool = self
            .memory_pool
            .ok_or_else(|| QueryError::BuildQueryDispatcher {
                err: "lost of memory pool".to_string(),
            })?;

        let default_table_provider =
            self.default_table_provider
                .ok_or_else(|| QueryError::BuildQueryDispatcher {
                    err: "lost of default_table_provider".to_string(),
                })?;

        let trace_collector = self.trace_collector;

        Ok(SimpleQueryDispatcher {
            coord,
            default_table_provider,
            split_manager,
            session_factory,
            memory_pool,
            parser,
            query_execution_factory,
            query_tracker,
            func_manager,
            stream_provider_manager,
            trace_collector,
        })
    }
}
