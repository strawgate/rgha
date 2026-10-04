//! Sandbox backends. A backend starts one isolated environment per runner,
//! running the official `actions/runner` with a single-use JIT config.

mod daytona;
mod docker;
mod firecracker;
mod modal;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::{BackendConfig, ClassConfig, GITHUB_RUNNER_DOMAINS, NetworkMode};

pub use daytona::DaytonaBackend;
pub use docker::DockerBackend;
pub use firecracker::{FirecrackerBackend, FirecrackerSettings, JailerSettings};
pub use modal::{ModalBackend, ModalSettings};

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
    pub memory_limit_mib: u32,
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
            .field("memory_limit_mib", &self.memory_limit_mib)
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

/// Accepts a bare key or a `NAME=value` line (dotenv style); the last
/// non-empty line wins.
fn parse_key_file(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).rfind(|l| !l.is_empty() && !l.starts_with('#'))?;
    let value = line.strip_prefix("export ").unwrap_or(line);
    let value = value.split_once('=').map(|(_, v)| v).unwrap_or(value);
    let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
    (!value.is_empty()).then(|| value.to_string())
}

/// Docker layer commands, for experiments that build the image themselves.
pub fn modal_docker_commands() -> Vec<String> {
    modal::DOCKER_IMAGE_COMMANDS.iter().map(|c| c.to_string()).collect()
}

pub use modal::sandbox_spec as modal_sandbox_spec;

/// Parses `a.b.0.0/16` into the first two octets.
pub fn parse_fc_subnet(s: &str) -> Result<[u8; 2], String> {
    let (ip, len) = s.split_once('/').ok_or("subnet must look like 10.213.0.0/16")?;
    let ip: std::net::Ipv4Addr = ip.parse().map_err(|_| format!("bad subnet address {ip:?}"))?;
    let o = ip.octets();
    if len != "16" || o[2] != 0 || o[3] != 0 {
        return Err(format!("subnet {s:?} must be a /16 like 10.213.0.0/16"));
    }
    Ok([o[0], o[1]])
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}

pub async fn build(name: &str, cfg: &BackendConfig) -> anyhow::Result<Arc<dyn Backend>> {
    let backend: Arc<dyn Backend> = match cfg {
        BackendConfig::Modal { app, image, image_commands, profile, runtime, regions, docker, preload, .. } => {
            Arc::new(
                ModalBackend::connect(
                    name,
                    ModalSettings {
                        app,
                        image,
                        image_commands,
                        profile: profile.as_deref(),
                        runtime: runtime.clone(),
                        regions: regions.clone(),
                        docker: *docker,
                        preload,
                    },
                )
                .await?,
            )
        }
        BackendConfig::Docker { image, runtime, bin, .. } => Arc::new(DockerBackend::new(bin, image, runtime.clone())),
        BackendConfig::Firecracker {
            image,
            image_commands,
            preload,
            kernel,
            firecracker_bin,
            jailer_bin,
            jailer_uid,
            jailer_gid,
            state_dir,
            docker_bin,
            rootfs_size_gib,
            scratch_size_gib,
            snapshots,
            subnet,
            uplink,
            dns,
            ..
        } => Arc::new(FirecrackerBackend::new(FirecrackerSettings {
            image: image.clone(),
            image_commands: image_commands.clone(),
            preload: preload.clone(),
            firecracker_bin: firecracker_bin.clone(),
            jailer: jailer_bin.as_ref().map(|bin| JailerSettings {
                bin: bin.clone(),
                uid: *jailer_uid,
                gid: *jailer_gid,
            }),
            kernel: kernel.clone(),
            state_dir: state_dir.into(),
            docker_bin: docker_bin.clone(),
            rootfs_size_gib: *rootfs_size_gib,
            scratch_size_gib: *scratch_size_gib,
            snapshots: *snapshots,
            subnet: parse_fc_subnet(subnet).map_err(|e| anyhow::anyhow!(e))?,
            uplink: uplink.clone(),
            dns: dns.clone(),
        })),
        BackendConfig::Daytona { api_url, api_key_env, api_key_file, image, snapshot, target, disk_gib, .. } => {
            let key = match std::env::var(api_key_env) {
                Ok(k) if !k.trim().is_empty() => k.trim().to_string(),
                _ => match api_key_file {
                    Some(path) => {
                        let path = expand_home(path);
                        let text = std::fs::read_to_string(&path)
                            .map_err(|e| anyhow::anyhow!("reading Daytona API key file {path}: {e}"))?;
                        parse_key_file(&text).ok_or_else(|| anyhow::anyhow!("Daytona API key file {path} is empty"))?
                    }
                    None => anyhow::bail!("Daytona backend {name}: set {api_key_env} or api_key_file"),
                },
            };
            Arc::new(DaytonaBackend::new(api_url, key, image, snapshot.clone(), target.clone(), *disk_gib)?)
        }
    };
    backend.prepare().await?;
    Ok(backend)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_file_formats() {
        assert_eq!(parse_key_file("dtn_abc\n").as_deref(), Some("dtn_abc"));
        assert_eq!(parse_key_file("DAYTONA_API_KEY=dtn_abc").as_deref(), Some("dtn_abc"));
        assert_eq!(parse_key_file("# c\nexport DAYTONA_API_KEY=\"dtn_abc\"\n\n").as_deref(), Some("dtn_abc"));
        assert_eq!(parse_key_file("\n  \n"), None);
    }
}
