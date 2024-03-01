//! Ring buffer of queries that have been run with some brief information

use data_types::NamespaceId;
use datafusion::physical_plan::ExecutionPlan;
use iox_time::{Time, TimeProvider};
use observability_deps::tracing::{info, warn};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    fmt::Debug,
    sync::{
        atomic::{self, AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use trace::ctx::TraceId;
use uuid::Uuid;

/// The query duration used for queries still running.
const UNCOMPLETED_DURATION: i64 = -1;

/// Phase of a query entry.
///
/// ```text
///     +-------------------------------+---> fail
///     |                               |
/// received ---> planned ---> permit ---+
///     |           |           |       |
///     |           |           |       +---> success
///     |           |           |
///     |           |           |
///     +-----------+-----------+-----------> cancel
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum QueryPhase {
    /// Query was received but not processed.
    ///
    /// This is the initial state.
    ///
    /// # Done
    /// - The query has been received (and potentially authenticated) by the server.
    ///
    /// # To Do
    /// - The query is not planned.
    /// - The concurrency-limiting semaphore has NOT yet issued a permit.
    /// - The query has not been executed.
    Received,

    /// Query was planned and is waiting for a semaphore permit.
    ///
    /// # Done
    /// - The query has been received (and potentially authenticated) by the server.
    /// - The query was planned.
    ///
    /// # To Do
    /// - The concurrency-limiting semaphore has NOT yet issued a permit.
    /// - The query has not been executed.
    Planned,

    /// Query has the permit to be executed and is likely being executed.
    ///
    /// # Done
    /// - The query has been received (and potentially authenticated) by the server.
    /// - The query was planned.
    /// - The concurrency-limiting semaphore has issued a permit.
    ///
    /// # To Do
    /// - The query has not been executed.
    Permit,

    /// Query was cancelled (likely by the user or a downstream component).
    ///
    /// This is a terminal state.
    Cancel,

    /// Query was fully executed successfully.
    ///
    /// This is a terminal state.
    Success,

    /// Query failed due to an error, e.g. during planning or during execution.
    ///
    /// This is a terminal state.
    Fail,
}

impl QueryPhase {
    /// Name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Planned => "planned",
            Self::Permit => "permit",
            Self::Cancel => "cancel",
            Self::Success => "success",
            Self::Fail => "fail",
        }
    }

    /// ID.
    fn id(&self) -> u8 {
        match self {
            Self::Received => 0,
            Self::Planned => 1,
            Self::Permit => 2,
            Self::Cancel => 3,
            Self::Success => 4,
            Self::Fail => 5,
        }
    }

    /// Create from ID.
    fn from_id(id: u8) -> Self {
        match id {
            0 => Self::Received,
            1 => Self::Planned,
            2 => Self::Permit,
            3 => Self::Cancel,
            4 => Self::Success,
            5 => Self::Fail,
            _ => unreachable!(),
        }
    }
}

impl std::fmt::Debug for QueryPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl std::fmt::Display for QueryPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Information about a single query that was executed
pub struct QueryLogEntry {
    /// Unique ID.
    pub id: Uuid,

    /// Namespace ID.
    pub namespace_id: NamespaceId,

    /// Namespace name.
    pub namespace_name: Arc<str>,

    /// The type of query
    pub query_type: &'static str,

    /// The text of the query (SQL for sql queries, pbjson for storage rpc queries)
    pub query_text: QueryText,

    /// The trace ID if any
    pub trace_id: Option<TraceId>,

    /// Time at which the query was run
    pub issue_time: Time,

    /// Duration it took to acquire a semaphore permit, relative to [`issue_time`](Self::issue_time).
    permit_duration: AtomicDuration,

    /// Duration it took to plan the query, relative to [`issue_time`](Self::issue_time) + [`permit_duration`](Self::permit_duration).
    plan_duration: AtomicDuration,

    /// Duration it took to execute the query, relative to [`issue_time`](Self::issue_time) +
    /// [`permit_duration`](Self::permit_duration) + [`plan_duration`](Self::plan_duration).
    execute_duration: AtomicDuration,

    /// Duration from [`issue_time`](Self::issue_time) til the query ended somehow.
    end2end_duration: AtomicDuration,

    /// CPU duration spend for computation.
    compute_duration: AtomicDuration,

    /// If the query completed successfully
    success: AtomicBool,

    /// If the query is currently running (in any state).
    running: AtomicBool,

    /// Phase.
    phase: AtomicU8,
}

impl Debug for QueryLogEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryLogEntry")
            .field("id", &self.id)
            .field("phase", &self.phase().name())
            .field("namespace_id", &self.namespace_id)
            .field("namespace_name", &self.namespace_name)
            .field("query_type", &self.query_type)
            .field("query_text", &self.query_text.to_string())
            .field("trace_id", &self.trace_id)
            .field("issue_time", &self.issue_time)
            .field("permit_duration", &self.permit_duration())
            .field("plan_duration", &self.plan_duration())
            .field("execute_duration", &self.execute_duration())
            .field("end2end_duration", &self.end2end_duration())
            .field("compute_duration", &self.compute_duration())
            .field("success", &self.success())
            .field("running", &self.running())
            .field("cancelled", &self.cancelled())
            .finish()
    }
}

impl QueryLogEntry {
    /// Current phase.
    pub fn phase(&self) -> QueryPhase {
        QueryPhase::from_id(self.phase.load(Ordering::SeqCst))
    }

    /// Duration it took to acquire a semaphore permit, relative to [`issue_time`](Self::issue_time).
    pub fn permit_duration(&self) -> Option<Duration> {
        self.permit_duration.get()
    }

    /// Duration it took to plan the query, relative to [`issue_time`](Self::issue_time) + [`permit_duration`](Self::permit_duration).
    pub fn plan_duration(&self) -> Option<Duration> {
        self.plan_duration.get()
    }

    /// Duration it took to execute the query, relative to [`issue_time`](Self::issue_time) +
    /// [`permit_duration`](Self::permit_duration) + [`plan_duration`](Self::plan_duration).
    pub fn execute_duration(&self) -> Option<Duration> {
        self.execute_duration.get()
    }

    /// Duration from [`issue_time`](Self::issue_time) til the query ended somehow.
    pub fn end2end_duration(&self) -> Option<Duration> {
        self.end2end_duration.get()
    }

    /// CPU duration spend for computation.
    pub fn compute_duration(&self) -> Option<Duration> {
        self.compute_duration.get()
    }

    /// Returns true if `set_completed` was called with `success=true`
    pub fn success(&self) -> bool {
        self.success.load(Ordering::SeqCst)
    }

    /// If the query is currently running (in any state).
    pub fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// If the query was cancelled.
    pub fn cancelled(&self) -> bool {
        self.phase() == QueryPhase::Cancel
    }

    /// Log entry.
    pub fn log(&self) {
        info!(
            when=self.phase().name(),
            id=%self.id,
            namespace_id=self.namespace_id.get(),
            namespace_name=self.namespace_name.as_ref(),
            query_type=self.query_type,
            query_text=%self.query_text,
            trace_id=self.trace_id.map(|id| format!("{:x}", id.get())),
            issue_time=%self.issue_time,
            plan_duration_secs=self.plan_duration().map(|d| d.as_secs_f64()),
            permit_duration_secs=self.permit_duration().map(|d| d.as_secs_f64()),
            execute_duration_secs=self.execute_duration().map(|d| d.as_secs_f64()),
            end2end_duration_secs=self.end2end_duration().map(|d| d.as_secs_f64()),
            compute_duration_secs=self.compute_duration().map(|d| d.as_secs_f64()),
            success=self.success(),
            running=self.running(),
            cancelled=self.cancelled(),
            "query",
        )
    }
}

/// Snapshot of the entries the [`QueryLog`].
#[derive(Debug)]
pub struct QueryLogEntries {
    /// Entries.
    pub entries: VecDeque<Arc<QueryLogEntry>>,

    /// Maximum number of entries
    pub max_size: usize,

    /// Number of evicted entries due to the "max size" constraint.
    pub evicted: usize,
}

/// Stores a fixed number `QueryExecutions` -- handles locking
/// internally so can be shared across multiple
pub struct QueryLog {
    log: Mutex<VecDeque<Arc<QueryLogEntry>>>,
    max_size: usize,
    evicted: AtomicUsize,
    time_provider: Arc<dyn TimeProvider>,
    id_gen: IDGen,
}

impl QueryLog {
    /// Create a new QueryLog that can hold at most `size` items.
    /// When the `size+1` item is added, item `0` is evicted.
    pub fn new(max_size: usize, time_provider: Arc<dyn TimeProvider>) -> Self {
        Self::new_with_id_gen(max_size, time_provider, Box::new(Uuid::new_v4))
    }

    pub fn new_with_id_gen(
        max_size: usize,
        time_provider: Arc<dyn TimeProvider>,
        id_gen: IDGen,
    ) -> Self {
        Self {
            log: Mutex::new(VecDeque::with_capacity(max_size)),
            max_size,
            evicted: AtomicUsize::new(0),
            time_provider,
            id_gen,
        }
    }

    pub fn push(
        &self,
        namespace_id: NamespaceId,
        namespace_name: Arc<str>,
        query_type: &'static str,
        query_text: QueryText,
        trace_id: Option<TraceId>,
    ) -> QueryCompletedToken<StateReceived> {
        let entry = Arc::new(QueryLogEntry {
            id: (self.id_gen)(),
            namespace_id,
            namespace_name,
            query_type,
            query_text,
            trace_id,
            issue_time: self.time_provider.now(),
            permit_duration: Default::default(),
            plan_duration: Default::default(),
            execute_duration: Default::default(),
            end2end_duration: Default::default(),
            compute_duration: Default::default(),
            success: atomic::AtomicBool::new(false),
            running: atomic::AtomicBool::new(true),
            phase: AtomicU8::new(QueryPhase::Received.id()),
        });
        entry.log();

        let token = QueryCompletedToken {
            entry: Some(Arc::clone(&entry)),
            time_provider: Arc::clone(&self.time_provider),
            state: Default::default(),
        };

        if self.max_size == 0 {
            return token;
        }

        let mut log = self.log.lock();

        // enforce limit
        while log.len() > self.max_size {
            log.pop_front();
            self.evicted.fetch_add(1, Ordering::SeqCst);
        }

        log.push_back(Arc::clone(&entry));
        token
    }

    pub fn entries(&self) -> QueryLogEntries {
        let log = self.log.lock();
        QueryLogEntries {
            entries: log.clone(),
            max_size: self.max_size,
            evicted: self.evicted.load(Ordering::SeqCst),
        }
    }
}

impl Debug for QueryLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryLog")
            .field("log", &self.log)
            .field("max_size", &self.max_size)
            .field("evicted", &self.evicted)
            .field("time_provider", &self.time_provider)
            .field("id_gen", &"<ID_GEN>")
            .finish()
    }
}

trait State {
    fn plan(&self) -> Option<&Arc<dyn ExecutionPlan>>;
}

/// State of [`QueryCompletedToken`], equivalent to [`QueryPhase::Received`].
#[derive(Debug, Clone, Copy, Default)]
pub struct StateReceived;

impl State for StateReceived {
    fn plan(&self) -> Option<&Arc<dyn ExecutionPlan>> {
        None
    }
}

/// State of [`QueryCompletedToken`], equivalent to [`QueryPhase::Planned`].
#[derive(Debug)]
pub struct StatePlanned {
    /// Physical execution plan.
    plan: Arc<dyn ExecutionPlan>,
}

impl State for StatePlanned {
    fn plan(&self) -> Option<&Arc<dyn ExecutionPlan>> {
        Some(&self.plan)
    }
}

/// State of [`QueryCompletedToken`], equivalent to [`QueryPhase::Permit`].
#[derive(Debug)]
pub struct StatePermit {
    /// Physical execution plan.
    plan: Arc<dyn ExecutionPlan>,
}

impl State for StatePermit {
    fn plan(&self) -> Option<&Arc<dyn ExecutionPlan>> {
        Some(&self.plan)
    }
}

/// A `QueryCompletedToken` is returned by `record_query` implementations of
/// a `QueryNamespace`. It is used to trigger side-effects (such as query timing)
/// on query completion.
#[derive(Debug)]
#[allow(private_bounds)]
pub struct QueryCompletedToken<S>
where
    S: State,
{
    /// Entry.
    ///
    /// This is optional so we can implement type state and [`Drop`] at the same time.
    entry: Option<Arc<QueryLogEntry>>,

    /// Time provider
    time_provider: Arc<dyn TimeProvider>,

    /// Current state.
    state: S,
}

#[allow(private_bounds)]
impl<S> QueryCompletedToken<S>
where
    S: State,
{
    /// Underlying entry.
    pub fn entry(&self) -> &Arc<QueryLogEntry> {
        self.entry.as_ref().expect("valid state")
    }

    fn collect_compute_time(&self, entry: &Arc<QueryLogEntry>) {
        let Some(plan) = self.state.plan() else {
            return;
        };

        entry
            .compute_duration
            .set_absolute(collect_compute_duration(plan.as_ref()));
    }
}

impl QueryCompletedToken<StateReceived> {
    /// Record that this query got planned.
    pub fn planned(mut self, plan: Arc<dyn ExecutionPlan>) -> QueryCompletedToken<StatePlanned> {
        self.set_time();

        let entry = self.entry.take().expect("valid state");
        entry
            .phase
            .store(QueryPhase::Planned.id(), Ordering::SeqCst);
        entry.log();

        QueryCompletedToken {
            entry: Some(entry),
            time_provider: Arc::clone(&self.time_provider),
            state: StatePlanned { plan },
        }
    }

    /// Record that this query failed during planning.
    pub fn fail(self) {
        self.set_time();

        let entry = self.entry.as_ref().expect("valid state");
        entry.phase.store(QueryPhase::Fail.id(), Ordering::SeqCst);
    }

    fn set_time(&self) {
        let entry = self.entry.as_ref().expect("valid state");
        let now = self.time_provider.now();
        let origin = entry.issue_time;
        entry.plan_duration.set_relative(origin, now);
    }
}

impl QueryCompletedToken<StatePlanned> {
    /// Record that this query got a semaphore permit.
    pub fn permit(mut self) -> QueryCompletedToken<StatePermit> {
        let entry = self.entry.take().expect("valid state");

        let now = self.time_provider.now();
        let origin = entry.issue_time + entry.plan_duration().expect("valid state");
        entry.permit_duration.set_relative(origin, now);
        entry.phase.store(QueryPhase::Permit.id(), Ordering::SeqCst);
        entry.log();

        QueryCompletedToken {
            entry: Some(entry),
            time_provider: Arc::clone(&self.time_provider),
            state: StatePermit {
                plan: Arc::clone(&self.state.plan),
            },
        }
    }
}

impl QueryCompletedToken<StatePermit> {
    /// Record that this query completed successfully
    pub fn success(self) {
        let entry = self.entry.as_ref().expect("valid state");
        entry.success.store(true, Ordering::SeqCst);
        entry
            .phase
            .store(QueryPhase::Success.id(), Ordering::SeqCst);

        self.finish()
    }

    /// Record that the query finished execution with an error.
    pub fn fail(self) {
        let entry = self.entry.as_ref().expect("valid state");
        entry.phase.store(QueryPhase::Fail.id(), Ordering::SeqCst);

        self.finish()
    }

    fn finish(&self) {
        let entry = self.entry.as_ref().expect("valid state");

        let now = self.time_provider.now();
        let origin = entry.issue_time
            + entry.permit_duration().expect("valid state")
            + entry.plan_duration().expect("valid state");
        entry.execute_duration.set_relative(origin, now);

        self.collect_compute_time(entry);
    }
}

impl<S> Drop for QueryCompletedToken<S>
where
    S: State,
{
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            if entry.phase() != QueryPhase::Fail && entry.execute_duration().is_none() {
                entry.phase.store(QueryPhase::Cancel.id(), Ordering::SeqCst);

                if entry.permit_duration().is_some() {
                    // started computation, collect partial stats
                    self.collect_compute_time(&entry);
                }
            }

            let now = self.time_provider.now();
            entry.end2end_duration.set_relative(entry.issue_time, now);
            entry.running.store(false, Ordering::SeqCst);

            entry.log();
        }
    }
}

/// Boxed description of a query that knows how to render to a string
///
/// This avoids storing potentially large strings
pub type QueryText = Box<dyn std::fmt::Display + Send + Sync>;

/// Method that generated [`Uuid`]s.
pub type IDGen = Box<dyn Fn() -> Uuid + Send + Sync>;

struct AtomicDuration(AtomicI64);

impl AtomicDuration {
    fn get(&self) -> Option<Duration> {
        match self.0.load(Ordering::Relaxed) {
            UNCOMPLETED_DURATION => None,
            d => Some(Duration::from_nanos(d as u64)),
        }
    }

    fn set_relative(&self, origin: Time, now: Time) {
        match now.checked_duration_since(origin) {
            Some(dur) => {
                self.0.store(dur.as_nanos() as i64, Ordering::Relaxed);
            }
            None => {
                warn!("Clock went backwards, not query duration")
            }
        }
    }

    fn set_absolute(&self, d: Duration) {
        self.0.store(d.as_nanos() as i64, Ordering::Relaxed);
    }
}

impl Default for AtomicDuration {
    fn default() -> Self {
        Self(AtomicI64::new(UNCOMPLETED_DURATION))
    }
}

/// Collect compute duration from [`ExecutionPlan`].
fn collect_compute_duration(plan: &dyn ExecutionPlan) -> Duration {
    let mut total = Duration::ZERO;

    if let Some(metrics) = plan.metrics() {
        if let Some(nanos) = metrics.elapsed_compute() {
            total += Duration::from_nanos(nanos as u64);
        }
    }

    for child in plan.children() {
        total += collect_compute_duration(child.as_ref());
    }

    total
}

#[cfg(test)]
mod test_super {
    use datafusion::error::DataFusionError;
    use std::sync::atomic::AtomicU64;

    use datafusion::physical_plan::{
        metrics::{MetricValue, MetricsSet},
        DisplayAs, Metric,
    };
    use iox_time::MockProvider;
    use test_helpers::tracing::TracingCapture;

    use super::*;

    #[test]
    fn test_token_end2end_success() {
        let capture = TracingCapture::new();

        let Test {
            time_provider,
            token,
            entry,
        } = Test::default();

        assert_eq!(entry.phase(), QueryPhase::Received);
        assert!(!entry.success());
        assert!(entry.running());
        assert!(!entry.cancelled());
        assert_eq!(entry.permit_duration(), None,);
        assert_eq!(entry.plan_duration(), None,);
        assert_eq!(entry.execute_duration(), None,);
        assert_eq!(entry.end2end_duration(), None,);
        assert_eq!(entry.compute_duration(), None,);

        time_provider.inc(Duration::from_millis(1));
        let token = token.planned(plan());

        assert_eq!(entry.phase(), QueryPhase::Planned);
        assert!(!entry.success());
        assert!(entry.running());
        assert!(!entry.cancelled());
        assert_eq!(entry.plan_duration(), Some(Duration::from_millis(1)),);
        assert_eq!(entry.permit_duration(), None,);
        assert_eq!(entry.execute_duration(), None,);
        assert_eq!(entry.end2end_duration(), None,);
        assert_eq!(entry.compute_duration(), None,);

        time_provider.inc(Duration::from_millis(10));
        let token = token.permit();

        assert_eq!(entry.phase(), QueryPhase::Permit);
        assert!(!entry.success());
        assert!(entry.running());
        assert!(!entry.cancelled());
        assert_eq!(entry.plan_duration(), Some(Duration::from_millis(1)),);
        assert_eq!(entry.permit_duration(), Some(Duration::from_millis(10)),);
        assert_eq!(entry.execute_duration(), None,);
        assert_eq!(entry.end2end_duration(), None,);
        assert_eq!(entry.compute_duration(), None,);

        time_provider.inc(Duration::from_millis(100));
        token.success();

        assert_eq!(entry.phase(), QueryPhase::Success);
        assert!(entry.success());
        assert!(!entry.running());
        assert!(!entry.cancelled());
        assert_eq!(entry.plan_duration(), Some(Duration::from_millis(1)),);
        assert_eq!(entry.permit_duration(), Some(Duration::from_millis(10)),);
        assert_eq!(entry.execute_duration(), Some(Duration::from_millis(100)),);
        assert_eq!(entry.end2end_duration(), Some(Duration::from_millis(111)),);
        assert_eq!(entry.compute_duration(), Some(Duration::from_millis(1_337)),);

        assert_logs(
            capture,
            [
                r#"level = INFO; message = query; when = "received"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "planned"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; plan_duration_secs = 0.001; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "permit"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; plan_duration_secs = 0.001; permit_duration_secs = 0.01; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "success"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; plan_duration_secs = 0.001; permit_duration_secs = 0.01; execute_duration_secs = 0.1; end2end_duration_secs = 0.111; compute_duration_secs = 1.337; success = true; running = false; cancelled = false;"#,
            ],
        );
    }

    #[test]
    fn test_token_planning_fail() {
        let capture = TracingCapture::new();

        let Test {
            time_provider,
            token,
            entry,
        } = Test::default();

        time_provider.inc(Duration::from_millis(1));
        token.fail();

        assert_eq!(entry.phase(), QueryPhase::Fail);
        assert!(!entry.success());
        assert!(!entry.running());
        assert!(!entry.cancelled());
        assert_eq!(entry.plan_duration(), Some(Duration::from_millis(1)),);
        assert_eq!(entry.permit_duration(), None,);
        assert_eq!(entry.execute_duration(), None,);
        assert_eq!(entry.end2end_duration(), Some(Duration::from_millis(1)),);
        assert_eq!(entry.compute_duration(), None,);

        assert_logs(
            capture,
            [
                r#"level = INFO; message = query; when = "received"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "fail"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; plan_duration_secs = 0.001; end2end_duration_secs = 0.001; success = false; running = false; cancelled = false;"#,
            ],
        );
    }

    #[test]
    fn test_token_execution_fail() {
        let capture = TracingCapture::new();

        let Test {
            time_provider,
            token,
            entry,
        } = Test::default();

        time_provider.inc(Duration::from_millis(1));
        let token = token.planned(plan());
        time_provider.inc(Duration::from_millis(10));
        let token = token.permit();
        time_provider.inc(Duration::from_millis(100));
        token.fail();

        assert_eq!(entry.phase(), QueryPhase::Fail);
        assert!(!entry.success());
        assert!(!entry.running());
        assert!(!entry.cancelled());
        assert_eq!(entry.plan_duration(), Some(Duration::from_millis(1)),);
        assert_eq!(entry.permit_duration(), Some(Duration::from_millis(10)),);
        assert_eq!(entry.execute_duration(), Some(Duration::from_millis(100)),);
        assert_eq!(entry.end2end_duration(), Some(Duration::from_millis(111)),);
        assert_eq!(entry.compute_duration(), Some(Duration::from_millis(1_337)),);

        assert_logs(
            capture,
            [
                r#"level = INFO; message = query; when = "received"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "planned"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; plan_duration_secs = 0.001; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "permit"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; plan_duration_secs = 0.001; permit_duration_secs = 0.01; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "fail"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; plan_duration_secs = 0.001; permit_duration_secs = 0.01; execute_duration_secs = 0.1; end2end_duration_secs = 0.111; compute_duration_secs = 1.337; success = false; running = false; cancelled = false;"#,
            ],
        );
    }

    #[test]
    fn test_token_drop_before_planned() {
        let capture = TracingCapture::new();

        let Test {
            time_provider,
            token,
            entry,
        } = Test::default();

        time_provider.inc(Duration::from_millis(1));
        drop(token);

        assert_eq!(entry.phase(), QueryPhase::Cancel);
        assert!(!entry.success());
        assert!(!entry.running());
        assert!(entry.cancelled());
        assert_eq!(entry.permit_duration(), None,);
        assert_eq!(entry.plan_duration(), None,);
        assert_eq!(entry.execute_duration(), None,);
        assert_eq!(entry.end2end_duration(), Some(Duration::from_millis(1)),);
        assert_eq!(entry.compute_duration(), None,);

        assert_logs(
            capture,
            [
                r#"level = INFO; message = query; when = "received"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "cancel"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; end2end_duration_secs = 0.001; success = false; running = false; cancelled = true;"#,
            ],
        );
    }

    #[test]
    fn test_token_drop_before_acquire() {
        let capture = TracingCapture::new();

        let Test {
            time_provider,
            token,
            entry,
        } = Test::default();

        time_provider.inc(Duration::from_millis(1));
        let token = token.planned(plan());
        time_provider.inc(Duration::from_millis(10));
        drop(token);

        assert_eq!(entry.phase(), QueryPhase::Cancel);
        assert!(!entry.success());
        assert!(!entry.running());
        assert!(entry.cancelled());
        assert_eq!(entry.permit_duration(), None,);
        assert_eq!(entry.plan_duration(), Some(Duration::from_millis(1)),);
        assert_eq!(entry.execute_duration(), None,);
        assert_eq!(entry.end2end_duration(), Some(Duration::from_millis(11)),);
        assert_eq!(entry.compute_duration(), None,);

        assert_logs(
            capture,
            [
                r#"level = INFO; message = query; when = "received"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "planned"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; plan_duration_secs = 0.001; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "cancel"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; plan_duration_secs = 0.001; end2end_duration_secs = 0.011; success = false; running = false; cancelled = true;"#,
            ],
        );
    }

    #[test]
    fn test_token_drop_before_finish() {
        let capture = TracingCapture::new();

        let Test {
            time_provider,
            token,
            entry,
        } = Test::default();

        time_provider.inc(Duration::from_millis(1));
        let token = token.planned(plan());
        time_provider.inc(Duration::from_millis(10));
        let token = token.permit();
        time_provider.inc(Duration::from_millis(100));
        drop(token);

        assert_eq!(entry.phase(), QueryPhase::Cancel);
        assert!(!entry.success());
        assert!(!entry.running());
        assert!(entry.cancelled());
        assert_eq!(entry.permit_duration(), Some(Duration::from_millis(10)),);
        assert_eq!(entry.plan_duration(), Some(Duration::from_millis(1)),);
        assert_eq!(entry.execute_duration(), None,);
        assert_eq!(entry.end2end_duration(), Some(Duration::from_millis(111)),);
        assert_eq!(entry.compute_duration(), Some(Duration::from_millis(1_337)),); // partial stats collected

        assert_logs(
            capture,
            [
                r#"level = INFO; message = query; when = "received"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "planned"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; plan_duration_secs = 0.001; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "permit"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; plan_duration_secs = 0.001; permit_duration_secs = 0.01; success = false; running = true; cancelled = false;"#,
                r#"level = INFO; message = query; when = "cancel"; id = 00000000-0000-0000-0000-000000000001; namespace_id = 1; namespace_name = "ns"; query_type = "sql"; query_text = SELECT 1; issue_time = 1970-01-01T00:00:00.100+00:00; plan_duration_secs = 0.001; permit_duration_secs = 0.01; end2end_duration_secs = 0.111; compute_duration_secs = 1.337; success = false; running = false; cancelled = true;"#,
            ],
        );
    }

    struct Test {
        time_provider: Arc<MockProvider>,
        token: QueryCompletedToken<StateReceived>,
        entry: Arc<QueryLogEntry>,
    }

    impl Default for Test {
        fn default() -> Self {
            let time_provider =
                Arc::new(MockProvider::new(Time::from_timestamp_millis(100).unwrap()));
            let id_counter = AtomicU64::new(1);
            let log = QueryLog::new_with_id_gen(
                1_000,
                Arc::clone(&time_provider) as _,
                Box::new(move || Uuid::from_u128(id_counter.fetch_add(1, Ordering::SeqCst) as _)),
            );

            let token = log.push(
                NamespaceId::new(1),
                Arc::from("ns"),
                "sql",
                Box::new("SELECT 1"),
                None,
            );

            let entry = Arc::clone(token.entry());

            Self {
                time_provider,
                token,
                entry,
            }
        }
    }

    fn plan() -> Arc<dyn ExecutionPlan> {
        Arc::new(TestExec)
    }

    #[derive(Debug)]
    struct TestExec;

    impl DisplayAs for TestExec {
        fn fmt_as(
            &self,
            _t: datafusion::physical_plan::DisplayFormatType,
            _f: &mut std::fmt::Formatter<'_>,
        ) -> std::fmt::Result {
            unimplemented!()
        }
    }

    impl ExecutionPlan for TestExec {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn schema(&self) -> arrow::datatypes::SchemaRef {
            unimplemented!()
        }

        fn output_partitioning(&self) -> datafusion::physical_plan::Partitioning {
            unimplemented!()
        }

        fn output_ordering(&self) -> Option<&[datafusion::physical_expr::PhysicalSortExpr]> {
            unimplemented!()
        }

        fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
            vec![]
        }

        fn with_new_children(
            self: Arc<Self>,
            _children: Vec<Arc<dyn ExecutionPlan>>,
        ) -> datafusion::error::Result<Arc<dyn ExecutionPlan>> {
            unimplemented!()
        }

        fn execute(
            &self,
            _partition: usize,
            _context: Arc<datafusion::execution::TaskContext>,
        ) -> datafusion::error::Result<datafusion::physical_plan::SendableRecordBatchStream>
        {
            unimplemented!()
        }

        fn statistics(&self) -> Result<datafusion::physical_plan::Statistics, DataFusionError> {
            unimplemented!()
        }

        fn metrics(&self) -> Option<MetricsSet> {
            let mut metrics = MetricsSet::default();

            let t = datafusion::physical_plan::metrics::Time::default();
            t.add_duration(Duration::from_millis(1_337));
            metrics.push(Arc::new(Metric::new(MetricValue::ElapsedCompute(t), None)));

            Some(metrics)
        }
    }

    #[track_caller]
    fn assert_logs<const N: usize>(capture: TracingCapture, expected: [&str; N]) {
        let logs = capture.to_string();
        let actual = logs.split('\n').map(|s| s.trim()).collect::<Vec<_>>();
        assert!(
            actual == expected,
            "Actual:\n{}\n\nExpected:\n{}",
            actual.join("\n"),
            expected.join("\n")
        );
    }
}
