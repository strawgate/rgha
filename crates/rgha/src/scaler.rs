//! Drives one runner class: enforces the class policy, starts one sandbox
//! per needed runner, tears sandboxes down when jobs finish, reaps surplus
//! idle runners, and keeps a per-second cost ledger.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Utc};
use futures::future::join_all;
use rgha_scaleset::{Client, JobMessageBase, MessageSession, ScaleSetMessage, Statistics};
use tokio::sync::mpsc;

use crate::backend::{Backend, Network, RunnerSpec};
use crate::config::ClassConfig;
use crate::cost::{Ledger, Pricing};
use crate::policy::Decision;
use crate::pool::{Departed, Pool};

#[derive(Debug)]
enum Event {
    Exited { name: String, code: Option<i32>, timed_out: bool },
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
            ledger: Ledger::default(),
        }
    }

    /// Assigned jobs we intend to run (assigned minus policy-blocked).
    fn assigned(&self) -> i64 {
        let assigned = self.last_stats.map(|s| s.total_assigned_jobs).unwrap_or(0);
        (assigned - self.blocked.len() as i64).max(0)
    }

    fn log_reject(&self, job: &JobMessageBase, reason: &str, action: &str) {
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
        // Priced at the CPU limit: an upper bound when bursting above the request.
        let cpu = self.class.cpu_limit.unwrap_or(self.class.cpu);
        let usd = self.pricing.cost(cpu, self.class.memory_mib, sandbox_secs);
        let job_secs = job_secs.or(d.job.map(|j| j.as_secs_f64()));
        self.ledger.record(sandbox_secs, job_secs, usd, self.class.github_equivalent_per_min);
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
        while let Ok(Event::Exited { name, code, timed_out }) = self.events_rx.try_recv() {
            if let Some(d) = self.pool.remove(&name, Instant::now()) {
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
        }
    }

    async fn start_runners(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        tracing::info!(class = %self.class.name, count = n, pool = self.pool.len(), "starting runners");
        let starts = (0..n).map(|_| {
            let name = format!("{}-{}", self.class.name, &uuid::Uuid::new_v4().simple().to_string()[..10]);
            start_one(self.client.clone(), self.backend.clone(), self.scale_set_id, &self.class, name)
        });
        for res in join_all(starts).await {
            match res {
                Ok((spec, runner_id, instance_id, boot)) => {
                    tracing::info!(class = %self.class.name, runner = %spec.name, %instance_id, boot_ms = boot.as_millis() as u64, "runner started");
                    self.pool.insert(spec.name.clone(), runner_id, instance_id.clone(), Instant::now());
                    self.watch(spec, instance_id);
                }
                Err(e) => {
                    tracing::error!(class = %self.class.name, error = %format!("{e:#}"), "failed to start runner")
                }
            }
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
        let ttl = std::time::Duration::from_secs(self.class.idle_ttl_secs);
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

    /// Deregisters and stops idle runners; leaves busy ones to finish (they
    /// are bounded by the sandbox timeout).
    pub async fn shutdown(&mut self) {
        let idle: Vec<_> = self.pool.runners().filter(|r| r.state == crate::pool::State::Idle).cloned().collect();
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
                    tracing::info!(class = %self.class.name, runner = %s.runner_name, job = %s.base.job_display_name,
                        repo = %s.base.repository_name, event = %s.base.event_name,
                        pickup_secs = %fmt_secs(pickup), "job started");
                }
            }
            for c in &msg.job_completed {
                self.blocked.remove(&c.base.job_id);
                if let Some(d) = self.pool.remove(&c.runner_name, now) {
                    let job_secs = secs_between(c.base.runner_assign_time, c.base.finish_time);
                    self.record(&d, &format!("job {}", c.result), job_secs);
                    self.stop_in_background(d.runner.instance_id);
                }
            }
            if msg.statistics.is_some() {
                self.last_stats = msg.statistics;
            }
        }
        // Converge on every poll, including empty ones, using cached stats.
        let deficit = self.pool.deficit(self.assigned());
        self.start_runners(deficit).await;
        self.reap().await;
        Ok(())
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

async fn start_one(
    client: Client,
    backend: Arc<dyn Backend>,
    scale_set_id: i64,
    class: &ClassConfig,
    name: String,
) -> anyhow::Result<(RunnerSpec, i64, String, std::time::Duration)> {
    let t0 = Instant::now();
    let jit = client.generate_jit_config(scale_set_id, &name, "_work").await?;
    let spec = RunnerSpec {
        name,
        class: class.name.clone(),
        jit_config: jit.encoded_jit_config,
        cpu: class.cpu,
        cpu_limit: class.cpu_limit.unwrap_or(class.cpu),
        memory_mib: class.memory_mib,
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
}
