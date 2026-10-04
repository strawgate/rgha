//! Drives one runner class: enforces the class policy, starts one sandbox
//! per needed runner, tears sandboxes down when jobs finish, reaps surplus
//! idle runners, reconciles leaked instances, and keeps a cost ledger.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Utc};
use futures::future::join_all;
use rgha_scaleset::{Client, JobMessageBase, MessageSession, ScaleSetMessage, Statistics};
use tokio::sync::mpsc;

use crate::backend::{Backend, Network, RunnerSpec};
use crate::config::ClassConfig;
use crate::cost::{Ledger, Pricing, github_hosted_cost};
use crate::metrics;
use crate::policy::Decision;
use crate::pool::{Departed, Pool, State};

/// How often to look for instances the pool doesn't know about.
const RECONCILE_EVERY: Duration = Duration::from_secs(5 * 60);
/// How long to wait for JobCompleted after a busy runner's instance exits.
const EXIT_GRACE: Duration = Duration::from_secs(120);
/// Attempts per runner within one poll before deferring to backoff.
const START_ATTEMPTS: u32 = 3;
/// Base delay between start attempts (doubled each retry).
const RETRY_BASE_MS: u64 = if cfg!(test) { 1 } else { 500 };

#[derive(Debug)]
enum Event {
    Exited { name: String, code: Option<i32>, timed_out: bool },
}

/// Exponential backoff after consecutive start failures, so a broken
/// backend doesn't get hammered on every poll.
#[derive(Debug, Default)]
struct StartBackoff {
    failures: u32,
    until: Option<Instant>,
}

impl StartBackoff {
    fn ready(&self, now: Instant) -> bool {
        self.until.is_none_or(|u| now >= u)
    }
    fn failed(&mut self, now: Instant) -> Duration {
        self.failures += 1;
        let wait = Duration::from_secs(1u64 << self.failures.min(6)).min(Duration::from_secs(60));
        self.until = Some(now + wait);
        wait
    }
    fn succeeded(&mut self) {
        *self = Self::default();
    }
}

pub struct ClassScaler {
    class: ClassConfig,
    scale_set_id: i64,
    client: Client,
    backend: Arc<dyn Backend>,
    pricing: Pricing,
    pool: Pool,
    last_stats: Option<Statistics>,
    events_tx: mpsc::UnboundedSender<Event>,
    events_rx: mpsc::UnboundedReceiver<Event>,
    /// Jobs assigned to this scale set that policy rejected and that we have
    /// asked GitHub to cancel, keyed by job id. Excluded from the desired count
    /// so no runner is started that could pick them up.
    blocked: HashMap<String, Instant>,
    backoff: StartBackoff,
    last_reconcile: Option<Instant>,
    /// Last time a job was offered, assigned or started (drives `warm_for_secs`).
    last_activity: Option<Instant>,
    warm: bool,
    pub ledger: Ledger,
}

impl ClassScaler {
    pub fn new(
        class: ClassConfig,
        scale_set_id: i64,
        client: Client,
        backend: Arc<dyn Backend>,
        pricing: Pricing,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let pool = Pool::new(class.min_idle, class.max_runners);
        Self {
            class,
            scale_set_id,
            client,
            backend,
            pricing,
            pool,
            last_stats: None,
            events_tx,
            events_rx,
            blocked: HashMap::new(),
            backoff: StartBackoff::default(),
            last_reconcile: None,
            last_activity: None,
            warm: false,
            ledger: Ledger::default(),
        }
    }

    /// Assigned jobs we intend to run (assigned minus policy-blocked).
    fn assigned(&self) -> i64 {
        let assigned = self.last_stats.map(|s| s.total_assigned_jobs).unwrap_or(0);
        (assigned - self.blocked.len() as i64).max(0)
    }

    fn log_reject(&self, job: &JobMessageBase, reason: &str, action: &str) {
        metrics::policy_rejected(&self.class.name);
        tracing::warn!(
            class = %self.class.name,
            repo = %format!("{}/{}", job.owner_name, job.repository_name),
            workflow_ref = %job.job_workflow_ref,
            event = %job.event_name,
            run_id = job.workflow_run_id,
            %reason,
            "policy rejected job: {action}"
        );
    }

    /// Legacy flow: jobs offered as `JobAvailable` are only acquired if allowed.
    async fn acquire(&mut self, session: &MessageSession, msg: &ScaleSetMessage) -> rgha_scaleset::Result<()> {
        let mut ids = Vec::new();
        for job in &msg.job_available {
            match self.class.policy.evaluate(&job.base) {
                Decision::Acquire => ids.push(job.base.runner_request_id),
                Decision::Reject(reason) => self.log_reject(&job.base, &reason, "not acquiring"),
            }
        }
        if !ids.is_empty() {
            let got = session.acquire_jobs(&ids).await?;
            tracing::info!(class = %self.class.name, requested = ids.len(), acquired = got.len(), "acquired jobs");
        }
        Ok(())
    }

    /// Current flow: the service assigns jobs to the scale set directly
    /// (`JobAssigned`, no acquire step), so a disallowed job can only be
    /// stopped by cancelling its workflow run.
    fn enforce_assigned(&mut self, msg: &ScaleSetMessage) {
        let now = Instant::now();
        self.blocked.retain(|_, at| now.saturating_duration_since(*at) < Duration::from_secs(15 * 60));
        for job in &msg.job_assigned {
            let Decision::Reject(reason) = self.class.policy.evaluate(&job.base) else { continue };
            if self.blocked.insert(job.base.job_id.clone(), now).is_some() {
                continue; // redelivered; already cancelling
            }
            self.log_reject(&job.base, &reason, "cancelling workflow run");
            let (client, b) = (self.client.clone(), job.base.clone());
            tokio::spawn(async move {
                if let Err(e) = client.cancel_workflow_run(&b.owner_name, &b.repository_name, b.workflow_run_id).await {
                    tracing::error!(run_id = b.workflow_run_id, error = %e, "failed to cancel rejected workflow run");
                }
            });
        }
    }

    /// `job_secs` from GitHub's timestamps wins over our own observation,
    /// which is coarse because start/complete often arrive in one batch.
    fn record(&mut self, d: &Departed, why: &str, job_secs: Option<f64>) {
        let sandbox_secs = d.lifetime.as_secs_f64();
        let job_secs = job_secs.or(d.job.map(|j| j.as_secs_f64()));
        let usd = sandbox_cost(&self.pricing, &self.class, sandbox_secs, job_secs);
        let github_usd = job_secs.map(|j| github_hosted_cost(j, self.class.github_equivalent_per_min)).unwrap_or(0.0);
        self.ledger.record(sandbox_secs, job_secs, usd, self.class.github_equivalent_per_min);
        metrics::job_finished(&self.class.name, why, job_secs, sandbox_secs, usd, github_usd);
        tracing::info!(
            class = %self.class.name,
            runner = %d.runner.name,
            why,
            job_secs = %fmt_secs(job_secs),
            sandbox_secs = format!("{sandbox_secs:.1}"),
            cost_usd = format!("{usd:.6}"),
            total_jobs = self.ledger.jobs,
            total_cost_usd = format!("{:.4}", self.ledger.sandbox_usd),
            github_equiv_usd = format!("{:.4}", self.ledger.github_usd),
            "runner finished"
        );
    }

    fn stop_in_background(&self, instance_id: String) {
        let backend = self.backend.clone();
        tokio::spawn(async move {
            if let Err(e) = backend.stop(&instance_id).await {
                tracing::warn!(%instance_id, error = %e, "failed to stop instance");
            }
        });
    }

    fn drain_events(&mut self) {
        let now = Instant::now();
        while let Ok(Event::Exited { name, code, timed_out }) = self.events_rx.try_recv() {
            // A runner exits right after finishing its job, often before the
            // JobCompleted message arrives. Let that message record the job.
            if !timed_out && self.pool.mark_exited(&name, now) {
                tracing::debug!(class = %self.class.name, runner = %name, ?code, "busy runner exited; awaiting JobCompleted");
                continue;
            }
            self.depart_exited(&name, code, timed_out);
        }
        for name in self.pool.stale_exited(EXIT_GRACE, now) {
            self.depart_exited(&name, None, false);
        }
    }

    /// Removes a runner whose instance ended without a JobCompleted.
    fn depart_exited(&mut self, name: &str, code: Option<i32>, timed_out: bool) {
        let Some(d) = self.pool.remove(name, Instant::now()) else { return };
        let why = if timed_out { "timed out" } else { "instance exited" };
        tracing::info!(class = %self.class.name, runner = %name, ?code, why, "runner instance ended");
        self.record(&d, why, None);
        // A runner that died before taking a job is still registered.
        let client = self.client.clone();
        let id = d.runner.runner_id;
        tokio::spawn(async move {
            if let Err(e) = client.remove_runner(id).await
                && e.status() != Some(404)
            {
                tracing::debug!(runner_id = id, error = %e, "remove_runner after exit");
            }
        });
    }

    async fn start_runners(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        if !self.backoff.ready(Instant::now()) {
            tracing::debug!(class = %self.class.name, wanted = n, "runner starts backing off");
            return;
        }
        tracing::info!(class = %self.class.name, count = n, pool = self.pool.len(), "starting runners");
        let starts = (0..n).map(|_| {
            let name = format!("{}-{}", self.class.name, &uuid::Uuid::new_v4().simple().to_string()[..10]);
            start_with_retry(self.client.clone(), self.backend.clone(), self.scale_set_id, &self.class, name)
        });
        let mut any_failed = false;
        for res in join_all(starts).await {
            match res {
                Ok((spec, runner_id, instance_id, boot)) => {
                    metrics::runner_started(&self.class.name, boot.as_secs_f64());
                    tracing::info!(class = %self.class.name, runner = %spec.name, %instance_id, boot_ms = boot.as_millis() as u64, "runner started");
                    self.pool.insert(spec.name.clone(), runner_id, instance_id.clone(), Instant::now());
                    self.watch(spec, instance_id);
                }
                Err(e) => {
                    any_failed = true;
                    metrics::runner_start_failed(&self.class.name);
                    tracing::error!(class = %self.class.name, error = %format!("{e:#}"), "failed to start runner");
                }
            }
        }
        if any_failed {
            let wait = self.backoff.failed(Instant::now());
            tracing::warn!(class = %self.class.name, ?wait, "backing off runner starts");
        } else {
            self.backoff.succeeded();
        }
    }

    /// Enforces the max lifetime and reports when the instance ends.
    fn watch(&self, spec: RunnerSpec, instance_id: String) {
        let backend = self.backend.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let (code, timed_out) = match tokio::time::timeout(spec.timeout, backend.wait(&instance_id)).await {
                Ok(Ok(code)) => {
                    // Some backends (Daytona) keep the sandbox alive after the
                    // runner exits; stop is idempotent everywhere.
                    let _ = backend.stop(&instance_id).await;
                    (code, false)
                }
                Ok(Err(e)) => {
                    tracing::debug!(%instance_id, error = %e, "wait failed; stopping");
                    let _ = backend.stop(&instance_id).await;
                    (None, false)
                }
                Err(_) => {
                    let _ = backend.stop(&instance_id).await;
                    (None, true)
                }
            };
            let _ = tx.send(Event::Exited { name: spec.name, code, timed_out });
        });
    }

    async fn reap(&mut self) {
        let ttl = Duration::from_secs(self.class.idle_ttl_secs);
        for name in self.pool.reap_candidates(self.assigned(), ttl, Instant::now()) {
            let Some(r) = self.pool.get(&name).cloned() else { continue };
            // Deregister first: the service refuses if the runner just took a
            // job, which closes the race between "idle" and "assigned".
            match self.client.remove_runner(r.runner_id).await {
                Ok(()) => {}
                Err(e) if e.status() == Some(404) => {}
                Err(e) => {
                    tracing::debug!(runner = %name, error = %e, "not reaping (likely busy)");
                    continue;
                }
            }
            if let Some(d) = self.pool.remove(&name, Instant::now()) {
                self.record(&d, "reaped idle", None);
                self.stop_in_background(d.runner.instance_id);
            }
        }
    }

    /// Stops backend instances for this class that the pool doesn't track
    /// (left by a crash or a previous process), unless their runner is
    /// mid-job: deregistration fails for busy runners, and those finish.
    pub async fn reconcile(&mut self) {
        self.last_reconcile = Some(Instant::now());
        let instances = match self.backend.list(&self.class.name).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(class = %self.class.name, error = %format!("{e:#}"), "could not list instances to reconcile");
                return;
            }
        };
        for inst in instances.into_iter().filter(|i| self.pool.get(&i.runner_name).is_none()) {
            let removable = match self.client.get_runner_by_name(&inst.runner_name).await {
                Ok(None) => true,
                Ok(Some(r)) => self.client.remove_runner(r.id).await.is_ok(),
                Err(_) => false,
            };
            if removable {
                tracing::info!(class = %self.class.name, runner = %inst.runner_name, instance = %inst.id, "stopping untracked instance");
                metrics::orphan_stopped(&self.class.name);
                let _ = self.backend.stop(&inst.id).await;
            } else {
                tracing::info!(class = %self.class.name, runner = %inst.runner_name, "untracked instance is busy; letting it finish");
            }
        }
    }

    /// Deregisters and stops idle runners; leaves busy ones to finish (they
    /// are bounded by the sandbox timeout).
    pub async fn shutdown(&mut self) {
        let idle: Vec<_> = self.pool.runners().filter(|r| r.state == State::Idle).cloned().collect();
        let busy = self.pool.len() - idle.len();
        for r in idle {
            let _ = self.client.remove_runner(r.runner_id).await;
            let _ = self.backend.stop(&r.instance_id).await;
            if let Some(d) = self.pool.remove(&r.name, Instant::now()) {
                self.record(&d, "shutdown", None);
            }
        }
        if busy > 0 {
            tracing::warn!(class = %self.class.name, busy, "leaving busy runners to finish their jobs");
        }
    }

    /// Applies `warm_for_secs`: the warm pool only exists for a while after
    /// the last job activity. Surplus warm runners are then reaped normally.
    fn update_warm_pool(&mut self, now: Instant) {
        let warm = warm_active(self.class.warm_for_secs, self.last_activity, now);
        if warm != self.warm {
            tracing::info!(class = %self.class.name, warm, min_idle = self.class.min_idle, "warm pool {}", if warm { "on" } else { "off" });
            self.warm = warm;
        }
        self.pool.min_idle = if warm { self.class.min_idle } else { 0 };
    }

    fn publish_gauges(&self) {
        let busy = self.pool.runners().filter(|r| r.state == State::Busy).count();
        metrics::pool(&self.class.name, self.pool.len() - busy, busy, self.assigned());
    }
}

#[async_trait::async_trait]
impl rgha_scaleset::Scaler for ClassScaler {
    async fn scale(&mut self, session: &MessageSession, msg: Option<&ScaleSetMessage>) -> rgha_scaleset::Result<()> {
        self.drain_events();
        if let Some(msg) = msg {
            tracing::debug!(class = %self.class.name, ?msg, "message");
            // Acquire first so jobs are assigned as early as possible.
            self.acquire(session, msg).await?;
            self.enforce_assigned(msg);
            let now = Instant::now();
            for s in &msg.job_started {
                if self.pool.job_started(&s.runner_name, now) {
                    let pickup = secs_between(s.base.scale_set_assign_time, s.base.runner_assign_time);
                    if let Some(p) = pickup {
                        metrics::pickup(&self.class.name, p);
                    }
                    tracing::info!(class = %self.class.name, runner = %s.runner_name, job = %s.base.job_display_name,
                        repo = %s.base.repository_name, event = %s.base.event_name,
                        pickup_secs = %fmt_secs(pickup), "job started");
                }
            }
            for c in &msg.job_completed {
                self.blocked.remove(&c.base.job_id);
                if let Some(d) = self.pool.remove(&c.runner_name, now) {
                    let job_secs = secs_between(c.base.runner_assign_time, c.base.finish_time);
                    self.record(&d, &c.result, job_secs);
                    self.stop_in_background(d.runner.instance_id);
                }
            }
            if msg.statistics.is_some() {
                self.last_stats = msg.statistics;
            }
            if !(msg.job_available.is_empty() && msg.job_assigned.is_empty() && msg.job_started.is_empty()) {
                self.last_activity = Some(now);
            }
        }
        self.update_warm_pool(Instant::now());
        // Converge on every poll, including empty ones, using cached stats.
        let deficit = self.pool.deficit(self.assigned());
        self.start_runners(deficit).await;
        self.reap().await;
        if self.last_reconcile.is_none_or(|t| t.elapsed() >= RECONCILE_EVERY) {
            self.reconcile().await;
        }
        self.publish_gauges();
        Ok(())
    }
}

/// Busy time is priced at the CPU and memory caps (an upper bound for
/// bursting jobs); idle/boot time at the requests, which is what Modal bills
/// when the sandbox is mostly waiting.
fn sandbox_cost(pricing: &Pricing, class: &ClassConfig, sandbox_secs: f64, job_secs: Option<f64>) -> f64 {
    let busy = job_secs.unwrap_or(0.0).clamp(0.0, sandbox_secs);
    pricing.cost(class.cpu_cap(), class.memory_cap_mib(), busy)
        + pricing.cost(class.cpu, class.memory_mib, sandbox_secs - busy)
}

fn warm_active(warm_for_secs: Option<u64>, last_activity: Option<Instant>, now: Instant) -> bool {
    match warm_for_secs {
        None => true,
        Some(w) => last_activity.is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(w)),
    }
}

/// The service sends `0001-01-01T00:00:00Z` for unset timestamps.
fn real(t: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    t.filter(|t| t.year() >= 2000)
}

fn secs_between(from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>) -> Option<f64> {
    match (real(from), real(to)) {
        (Some(a), Some(b)) if b >= a => Some((b - a).num_milliseconds() as f64 / 1000.0),
        _ => None,
    }
}

fn fmt_secs(s: Option<f64>) -> String {
    s.map(|s| format!("{s:.1}")).unwrap_or_else(|| "-".into())
}

/// Retries transient start failures (API blips, scheduling hiccups) a few
/// times with short waits; each attempt uses a fresh runner registration.
async fn start_with_retry(
    client: Client,
    backend: Arc<dyn Backend>,
    scale_set_id: i64,
    class: &ClassConfig,
    name: String,
) -> anyhow::Result<(RunnerSpec, i64, String, Duration)> {
    let mut last = None;
    for attempt in 0..START_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(RETRY_BASE_MS * (1 << attempt))).await;
        }
        let name = if attempt == 0 { name.clone() } else { format!("{name}-{attempt}") };
        match start_one(client.clone(), backend.clone(), scale_set_id, class, name).await {
            Ok(ok) => return Ok(ok),
            Err(e) => {
                tracing::debug!(class = %class.name, attempt, error = %format!("{e:#}"), "runner start attempt failed");
                last = Some(e);
            }
        }
    }
    Err(last.expect("at least one attempt"))
}

async fn start_one(
    client: Client,
    backend: Arc<dyn Backend>,
    scale_set_id: i64,
    class: &ClassConfig,
    name: String,
) -> anyhow::Result<(RunnerSpec, i64, String, Duration)> {
    let t0 = Instant::now();
    let jit = client.generate_jit_config(scale_set_id, &name, "_work").await?;
    let spec = RunnerSpec {
        name,
        class: class.name.clone(),
        jit_config: jit.encoded_jit_config,
        cpu: class.cpu,
        cpu_limit: class.cpu_limit.unwrap_or(class.cpu),
        memory_mib: class.memory_mib,
        memory_limit_mib: class.memory_cap_mib(),
        timeout: class.sandbox_timeout(),
        network: Network::for_class(class),
    };
    match backend.start(&spec).await {
        Ok(instance_id) => Ok((spec, jit.runner.id, instance_id, t0.elapsed())),
        Err(e) => {
            // Don't leave a registered runner with no sandbox behind it.
            let _ = client.remove_runner(jit.runner.id).await;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Option<DateTime<Utc>> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn zero_timestamps_are_ignored() {
        assert_eq!(secs_between(ts("0001-01-01T00:00:00Z"), ts("2026-10-04T02:28:13.452Z")), None);
        assert_eq!(secs_between(ts("2026-10-04T02:28:08.055Z"), ts("2026-10-04T02:28:13.452Z")), Some(5.397));
        assert_eq!(secs_between(ts("2026-10-04T02:28:13Z"), ts("2026-10-04T02:28:08Z")), None);
        assert_eq!(secs_between(None, ts("2026-10-04T02:28:08Z")), None);
    }

    #[test]
    fn idle_time_priced_at_request_busy_at_cap() {
        let mut class: ClassConfig =
            toml::from_str("name = \"c\"\nbackend = \"b\"\ncpu = 0.25\ncpu_limit = 2.0\nmemory_mib = 1024").unwrap();
        let p = Pricing { cpu_per_sec: 1.0, gib_per_sec: 0.0, min_billed_secs: 0.0 };
        // 30s sandbox, 10s busy: 10*2.0 + 20*0.25
        assert!((sandbox_cost(&p, &class, 30.0, Some(10.0)) - 25.0).abs() < 1e-9);
        // job longer than sandbox lifetime (clock skew) is clamped
        assert!((sandbox_cost(&p, &class, 5.0, Some(9.0)) - 10.0).abs() < 1e-9);
        class.cpu_limit = None;
        assert!((sandbox_cost(&p, &class, 30.0, Some(10.0)) - 7.5).abs() < 1e-9);
    }

    #[test]
    fn warm_window() {
        let t0 = Instant::now();
        assert!(warm_active(None, None, t0), "no window = always warm");
        assert!(!warm_active(Some(300), None, t0), "cold until the first job");
        assert!(warm_active(Some(300), Some(t0), t0 + Duration::from_secs(299)));
        assert!(!warm_active(Some(300), Some(t0), t0 + Duration::from_secs(300)));
    }

    #[test]
    fn memory_cap_priced_while_busy() {
        let class: ClassConfig =
            toml::from_str("name = \"c\"\nbackend = \"b\"\ncpu = 0.125\nmemory_mib = 256\nmemory_limit_mib = 4096")
                .unwrap();
        let p = Pricing { cpu_per_sec: 0.0, gib_per_sec: 1.0, min_billed_secs: 0.0 };
        // 10s busy at 4 GiB + 20s idle at 0.25 GiB
        assert!((sandbox_cost(&p, &class, 30.0, Some(10.0)) - 45.0).abs() < 1e-9);
    }

    #[test]
    fn backoff_grows_and_resets() {
        let t0 = Instant::now();
        let mut b = StartBackoff::default();
        assert!(b.ready(t0));
        assert_eq!(b.failed(t0), Duration::from_secs(2));
        assert!(!b.ready(t0 + Duration::from_secs(1)));
        assert!(b.ready(t0 + Duration::from_secs(2)));
        assert_eq!(b.failed(t0), Duration::from_secs(4));
        for _ in 0..10 {
            b.failed(t0);
        }
        assert_eq!(b.failed(t0), Duration::from_secs(60));
        b.succeeded();
        assert!(b.ready(t0));
    }
}

#[cfg(test)]
#[path = "scaler_tests.rs"]
mod integration_tests;
