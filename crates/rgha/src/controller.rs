//! Per-class lifecycle: ensure the scale set exists, clean up orphans from a
//! previous run, then listen (re-establishing the session on errors) until
//! shutdown.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use rgha_scaleset::{Client, Label, Listener, RunnerScaleSet};
use tokio::sync::watch;

use crate::backend::Backend;
use crate::config::ClassConfig;
use crate::cost::Pricing;
use crate::scaler::ClassScaler;

pub struct ClassController {
    pub class: ClassConfig,
    pub client: Client,
    pub backend: Arc<dyn Backend>,
    pub pricing: Pricing,
    pub runner_group_id: i64,
    pub owner: String,
}

impl ClassController {
    async fn ensure_scale_set(&self) -> anyhow::Result<RunnerScaleSet> {
        if let Some(ss) = self.client.get_scale_set(self.runner_group_id, &self.class.name).await? {
            return Ok(ss);
        }
        tracing::info!(class = %self.class.name, "creating runner scale set");
        let ss = self
            .client
            .create_scale_set(RunnerScaleSet {
                name: self.class.name.clone(),
                runner_group_id: self.runner_group_id,
                labels: vec![Label::system(self.class.name.clone())],
                ..Default::default()
            })
            .await?;
        Ok(ss)
    }

    /// Stops instances left by a previous controller process, unless their
    /// runner is mid-job (deregistration fails), in which case they finish.
    async fn cleanup_orphans(&self) {
        let instances = match self.backend.list(&self.class.name).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(class = %self.class.name, error = %format!("{e:#}"), "could not list instances for orphan cleanup");
                return;
            }
        };
        for inst in instances {
            let removable = match self.client.get_runner_by_name(&inst.runner_name).await {
                Ok(None) => true,
                Ok(Some(r)) => self.client.remove_runner(r.id).await.is_ok(),
                Err(_) => false,
            };
            if removable {
                tracing::info!(class = %self.class.name, runner = %inst.runner_name, "stopping orphaned instance");
                let _ = self.backend.stop(&inst.id).await;
            } else {
                tracing::info!(class = %self.class.name, runner = %inst.runner_name, "orphan is busy; letting it finish");
            }
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        let ss = self.ensure_scale_set().await.with_context(|| format!("scale set for class {}", self.class.name))?;
        tracing::info!(class = %self.class.name, scale_set_id = ss.id, backend = self.backend.kind(), "class ready; runs-on: {}", self.class.name);
        self.cleanup_orphans().await;

        let mut scaler =
            ClassScaler::new(self.class.clone(), ss.id, self.client.clone(), self.backend.clone(), self.pricing);
        let mut backoff = Duration::from_secs(2);
        while !*shutdown.borrow() {
            let session = match self.client.message_session(ss.id, &self.owner).await {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    // 409: a previous session for this scale set is still alive.
                    tracing::warn!(class = %self.class.name, error = %e, ?backoff, "could not open message session; retrying");
                    if wait_or_shutdown(&mut shutdown, backoff).await {
                        break;
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    continue;
                }
            };
            backoff = Duration::from_secs(2);
            let listener = Listener::new(session.clone(), self.class.max_runners);
            let mut rx = shutdown.clone();
            let stop = async move {
                let _ = rx.wait_for(|v| *v).await;
            };
            let result = listener.run(&mut scaler, stop).await;
            if let Err(e) = session.close().await {
                tracing::debug!(class = %self.class.name, error = %e, "closing session");
            }
            match result {
                Ok(()) => break,
                Err(e) => {
                    tracing::error!(class = %self.class.name, error = %e, "listener stopped; reconnecting");
                    if wait_or_shutdown(&mut shutdown, Duration::from_secs(5)).await {
                        break;
                    }
                }
            }
        }
        scaler.shutdown().await;
        if let Some(r) = scaler.ledger.savings_ratio() {
            tracing::info!(class = %self.class.name, jobs = scaler.ledger.jobs, cost_usd = scaler.ledger.sandbox_usd,
                github_equiv_usd = scaler.ledger.github_usd, "session savings: {r:.1}x cheaper than GitHub-hosted");
        }
        Ok(())
    }
}

/// Sleeps for `d`; returns true if shutdown was requested meanwhile.
async fn wait_or_shutdown(rx: &mut watch::Receiver<bool>, d: Duration) -> bool {
    let stopped = tokio::select! {
        _ = tokio::time::sleep(d) => None,
        _ = rx.wait_for(|v| *v) => Some(true),
    };
    stopped.unwrap_or_else(|| *rx.borrow())
}
