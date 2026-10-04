//! Sandbox backends. A backend starts one isolated environment per runner,
//! running the official `actions/runner` with a single-use JIT config.

mod docker;
mod modal;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::{BackendConfig, ClassConfig, GITHUB_RUNNER_DOMAINS, NetworkMode};

pub use docker::DockerBackend;
pub use modal::ModalBackend;

/// Env var the official runner image reads its JIT config from.
pub const JIT_ENV: &str = "ACTIONS_RUNNER_INPUT_JITCONFIG";
pub const RUNNER_ENTRYPOINT: &str = "/home/runner/run.sh";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Network {
    Open,
    Allowlist { domains: Vec<String>, cidrs: Vec<String> },
}

impl Network {
    pub fn for_class(c: &ClassConfig) -> Self {
        match c.network {
            NetworkMode::Open => Network::Open,
            NetworkMode::GithubOnly => Network::Allowlist {
                domains: GITHUB_RUNNER_DOMAINS.iter().map(|s| s.to_string()).collect(),
                cidrs: vec![],
            },
            NetworkMode::Allowlist => {
                let mut domains: Vec<String> = GITHUB_RUNNER_DOMAINS.iter().map(|s| s.to_string()).collect();
                domains.extend(c.allow_domains.iter().cloned());
                Network::Allowlist { domains, cidrs: c.allow_cidrs.clone() }
            }
        }
    }
}

/// Everything a backend needs to start one runner.
#[derive(Clone)]
pub struct RunnerSpec {
    pub name: String,
    pub class: String,
    pub jit_config: String,
    pub cpu: f64,
    pub cpu_limit: f64,
    pub memory_mib: u32,
    pub timeout: Duration,
    pub network: Network,
}

impl std::fmt::Debug for RunnerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerSpec")
            .field("name", &self.name)
            .field("class", &self.class)
            .field("jit_config", &"<redacted>")
            .field("cpu", &self.cpu)
            .field("cpu_limit", &self.cpu_limit)
            .field("memory_mib", &self.memory_mib)
            .field("timeout", &self.timeout)
            .field("network", &self.network)
            .finish()
    }
}

/// A running (or recently running) instance owned by this controller.
#[derive(Debug, Clone)]
pub struct Instance {
    pub id: String,
    pub runner_name: String,
}

#[async_trait]
pub trait Backend: Send + Sync {
    fn kind(&self) -> &'static str;
    /// One-time setup (connect, build/pull image). Called before any start.
    async fn prepare(&self) -> anyhow::Result<()>;
    /// Starts a runner and returns the backend instance id.
    async fn start(&self, spec: &RunnerSpec) -> anyhow::Result<String>;
    /// Stops an instance. Must be idempotent.
    async fn stop(&self, id: &str) -> anyhow::Result<()>;
    /// Blocks until the instance exits; returns its exit code if known.
    async fn wait(&self, id: &str) -> anyhow::Result<Option<i32>>;
    /// Live instances previously started for `class` (for orphan cleanup).
    async fn list(&self, class: &str) -> anyhow::Result<Vec<Instance>>;
}

pub async fn build(name: &str, cfg: &BackendConfig) -> anyhow::Result<Arc<dyn Backend>> {
    let backend: Arc<dyn Backend> = match cfg {
        BackendConfig::Modal { app, image, image_commands, profile, runtime, regions, .. } => Arc::new(
            ModalBackend::connect(
                name,
                app,
                image,
                image_commands,
                profile.as_deref(),
                runtime.clone(),
                regions.clone(),
            )
            .await?,
        ),
        BackendConfig::Docker { image, runtime, bin, .. } => Arc::new(DockerBackend::new(bin, image, runtime.clone())),
    };
    backend.prepare().await?;
    Ok(backend)
}
