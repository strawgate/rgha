//! TOML configuration. See `examples/rgha.toml`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::cost::Pricing;
use crate::policy::Policy;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub github: GitHub,
    #[serde(default)]
    pub backends: BTreeMap<String, BackendConfig>,
    #[serde(rename = "class", default)]
    pub classes: Vec<ClassConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHub {
    /// Org (`https://github.com/org`), repo (`https://github.com/org/repo`) or
    /// enterprise (`https://github.com/enterprises/x`) URL.
    pub url: String,
    #[serde(default = "default_runner_group")]
    pub runner_group: String,
    pub app_client_id: Option<String>,
    pub app_installation_id: Option<i64>,
    /// Path to the App's PEM key; `RGHA_GITHUB_APP_PRIVATE_KEY` (PEM contents) wins if set.
    pub app_private_key_path: Option<String>,
    /// Env var holding a PAT, used when no App is configured.
    #[serde(default = "default_token_env")]
    pub token_env: String,
}

fn default_runner_group() -> String {
    "default".into()
}
fn default_token_env() -> String {
    "GITHUB_TOKEN".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum BackendConfig {
    Modal {
        /// Modal App that owns the Sandboxes.
        #[serde(default = "default_modal_app")]
        app: String,
        #[serde(default = "default_runner_image")]
        image: String,
        /// Extra Dockerfile commands layered on the image (cached by Modal).
        #[serde(default)]
        image_commands: Vec<String>,
        /// `.modal.toml` profile; default is the active profile / `MODAL_*` env.
        profile: Option<String>,
        /// `"vm"` requests Modal's alpha VM runtime (Docker-in-job); default gVisor.
        runtime: Option<String>,
        #[serde(default)]
        regions: Vec<String>,
        /// Start dockerd in each sandbox so jobs can use Docker. Requires runtime = "vm".
        #[serde(default)]
        docker: bool,
        /// Actions and toolchains baked into the image (see `image.rs`).
        #[serde(default)]
        preload: crate::image::Preload,
        pricing: Option<Pricing>,
    },
    /// Local containers via the docker CLI. Plain `runc` shares the host
    /// kernel and is NOT a security boundary for untrusted code; set
    /// `runtime = "runsc"` (gVisor) or a Kata runtime for isolation.
    Docker {
        #[serde(default = "default_runner_image")]
        image: String,
        runtime: Option<String>,
        #[serde(default = "default_docker_bin")]
        bin: String,
        /// Must be set to run an untrusted class on plain runc.
        #[serde(default)]
        allow_insecure_runc: bool,
        pricing: Option<Pricing>,
    },
    /// Daytona sandboxes (REST API). `container` class unless `snapshot`
    /// names a VM-class (`linux-vm`) snapshot.
    Daytona {
        #[serde(default = "default_daytona_url")]
        api_url: String,
        #[serde(default = "default_daytona_key_env")]
        api_key_env: String,
        /// File containing the API key (used if the env var is unset).
        api_key_file: Option<String>,
        #[serde(default = "default_runner_image")]
        image: String,
        snapshot: Option<String>,
        target: Option<String>,
        #[serde(default = "default_disk_gib")]
        disk_gib: u32,
        /// Must be set to run an untrusted class without a VM-class snapshot.
        #[serde(default)]
        allow_container_class: bool,
        pricing: Option<Pricing>,
    },
}

fn default_daytona_url() -> String {
    "https://app.daytona.io/api".into()
}
fn default_daytona_key_env() -> String {
    "DAYTONA_API_KEY".into()
}
fn default_disk_gib() -> u32 {
    5
}

fn default_modal_app() -> String {
    "rgha".into()
}
fn default_runner_image() -> String {
    "ghcr.io/actions/actions-runner:latest".into()
}
fn default_docker_bin() -> String {
    "docker".into()
}

impl BackendConfig {
    pub fn pricing(&self) -> Pricing {
        match self {
            BackendConfig::Modal { pricing, .. } => pricing.unwrap_or(Pricing::MODAL_SANDBOX),
            BackendConfig::Docker { pricing, .. } => pricing.unwrap_or(Pricing::FREE),
            BackendConfig::Daytona { pricing, .. } => pricing.unwrap_or(Pricing::DAYTONA),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
    #[default]
    Open,
    /// GitHub's runner endpoints only (domain allowlist, TLS/443).
    GithubOnly,
    Allowlist,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassConfig {
    /// Scale set name; workflows use `runs-on: <name>`.
    pub name: String,
    pub backend: String,
    /// CPU in the backend's unit (Modal: physical cores; Docker: CPUs).
    #[serde(default = "default_cpu")]
    pub cpu: f64,
    /// Hard CPU cap; may exceed `cpu` to let boot and bursty steps go faster.
    /// Modal bills max(request, usage), so bursts cost only what they use.
    /// Default: same as `cpu`.
    pub cpu_limit: Option<f64>,
    #[serde(default = "default_memory")]
    pub memory_mib: u32,
    /// Hard memory cap; may exceed `memory_mib`. Modal bills max(request,
    /// usage), so a small request with a high limit keeps warm runners cheap
    /// while jobs can still use more. Default: same as `memory_mib`.
    pub memory_limit_mib: Option<u32>,
    /// Keep the `min_idle` warm pool only for this long after the last job
    /// activity (bursts get instant pickup; quiet periods cost nothing).
    /// Default: always warm.
    pub warm_for_secs: Option<u64>,
    #[serde(default = "default_max_runners")]
    pub max_runners: u32,
    /// Idle runners kept warm. 0 = pure scale-to-zero (cheapest).
    #[serde(default)]
    pub min_idle: u32,
    #[serde(default = "default_max_job_minutes")]
    pub max_job_minutes: u64,
    /// Seconds an idle surplus runner may live before being reaped.
    #[serde(default = "default_idle_ttl")]
    pub idle_ttl_secs: u64,
    #[serde(default)]
    pub network: NetworkMode,
    #[serde(default)]
    pub allow_domains: Vec<String>,
    #[serde(default)]
    pub allow_cidrs: Vec<String>,
    /// GitHub-hosted per-minute price used for the savings report.
    #[serde(default = "default_github_per_min")]
    pub github_equivalent_per_min: f64,
    #[serde(default)]
    pub policy: Policy,
    /// See the min_idle check in `Config::validate`.
    #[serde(default)]
    pub allow_warm_trusted: bool,
}

fn default_cpu() -> f64 {
    0.25
}
fn default_memory() -> u32 {
    1024
}
fn default_max_runners() -> u32 {
    10
}
fn default_max_job_minutes() -> u64 {
    30
}
fn default_idle_ttl() -> u64 {
    120
}
fn default_github_per_min() -> f64 {
    crate::cost::GITHUB_LINUX_2CORE_PER_MIN
}

/// Endpoints a runner must reach to register, fetch jobs, stream logs and
/// upload artifacts/caches. From GitHub's self-hosted runner networking docs.
pub const GITHUB_RUNNER_DOMAINS: &[&str] = &[
    "github.com",
    "api.github.com",
    "*.actions.githubusercontent.com",
    "codeload.github.com",
    "pkg.actions.githubusercontent.com",
    "results-receiver.actions.githubusercontent.com",
    "*.blob.core.windows.net",
    "objects.githubusercontent.com",
    "raw.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "objects-origin.githubusercontent.com",
    "github-releases.githubusercontent.com",
    "github-registry-files.githubusercontent.com",
    "ghcr.io",
    "*.pkg.github.com",
    "pkg-containers.githubusercontent.com",
];

impl ClassConfig {
    pub fn cpu_cap(&self) -> f64 {
        self.cpu_limit.unwrap_or(self.cpu)
    }

    pub fn memory_cap_mib(&self) -> u32 {
        self.memory_limit_mib.unwrap_or(self.memory_mib)
    }

    pub fn max_job(&self) -> Duration {
        Duration::from_secs(self.max_job_minutes * 60)
    }

    /// Sandbox lifetime cap: max job time plus slack for boot and idle wait.
    pub fn sandbox_timeout(&self) -> Duration {
        self.max_job() + Duration::from_secs(self.idle_ttl_secs.max(60) + 120)
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.classes.is_empty() {
            bail!("at least one [[class]] is required");
        }
        let mut names = std::collections::HashSet::new();
        for c in &self.classes {
            if !names.insert(&c.name) {
                bail!("duplicate class name {:?}", c.name);
            }
            if c.name.is_empty() || !c.name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
                bail!("class name {:?} must be non-empty [A-Za-z0-9_-]", c.name);
            }
            let Some(backend) = self.backends.get(&c.backend) else {
                bail!("class {:?} references unknown backend {:?}", c.name, c.backend);
            };
            if c.cpu <= 0.0 || c.memory_mib == 0 {
                bail!("class {:?}: cpu and memory_mib must be positive", c.name);
            }
            if c.cpu_limit.is_some_and(|l| l < c.cpu) {
                bail!("class {:?}: cpu_limit must be >= cpu", c.name);
            }
            if c.memory_limit_mib.is_some_and(|l| l < c.memory_mib) {
                bail!("class {:?}: memory_limit_mib must be >= memory_mib", c.name);
            }
            if c.policy.trust == crate::policy::Trust::Trusted && c.min_idle > 0 && !c.allow_warm_trusted {
                bail!(
                    "class {:?}: trusted classes default to min_idle = 0. GitHub assigns jobs to a scale set before \
                     rgha can check them, so a warm runner could start a disallowed job before rgha cancels it. \
                     Set allow_warm_trusted = true to accept that race",
                    c.name
                );
            }
            if c.min_idle > c.max_runners {
                bail!("class {:?}: min_idle exceeds max_runners", c.name);
            }
            if c.network == NetworkMode::Allowlist && c.allow_domains.is_empty() && c.allow_cidrs.is_empty() {
                bail!("class {:?}: network = \"allowlist\" needs allow_domains or allow_cidrs", c.name);
            }
            if let BackendConfig::Daytona { snapshot, allow_container_class, .. } = backend
                && c.policy.trust == crate::policy::Trust::Untrusted
                && snapshot.is_none()
                && !allow_container_class
            {
                bail!(
                    "class {:?} is untrusted but backend {:?} uses Daytona's container class. Point `snapshot` \
                     at a linux-vm snapshot, or set allow_container_class = true",
                    c.name,
                    c.backend
                );
            }
            if let BackendConfig::Docker { runtime, allow_insecure_runc, .. } = backend {
                let isolated = runtime.as_deref().is_some_and(|r| r != "runc");
                if c.policy.trust == crate::policy::Trust::Untrusted && !isolated && !allow_insecure_runc {
                    bail!(
                        "class {:?} is untrusted but backend {:?} uses plain runc (shared kernel). \
                         Set runtime = \"runsc\" (gVisor) / a Kata runtime, or allow_insecure_runc = true for local testing",
                        c.name,
                        c.backend
                    );
                }
                if c.network != NetworkMode::Open {
                    bail!("class {:?}: network restrictions are not implemented for the docker backend yet", c.name);
                }
            }
        }
        for (name, b) in &self.backends {
            if let BackendConfig::Modal { preload, .. } = b {
                preload.validate().map_err(|e| anyhow::anyhow!("backend {name:?}: {e}"))?;
            }
            if let BackendConfig::Modal { docker: true, runtime, .. } = b
                && runtime.as_deref() != Some("vm")
            {
                bail!("backend {name:?}: docker = true requires runtime = \"vm\" (gVisor cannot run dockerd)");
            }
        }
        if self.github.app_client_id.is_some() != self.github.app_installation_id.is_some() {
            bail!("github.app_client_id and github.app_installation_id must be set together");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../examples/rgha.toml");

    #[test]
    fn example_config_is_valid() {
        let cfg: Config = toml::from_str(EXAMPLE).unwrap();
        cfg.validate().unwrap();
        assert!(cfg.classes.len() >= 2);
    }

    #[test]
    fn untrusted_on_runc_requires_opt_in() {
        let toml = r#"
            [github]
            url = "https://github.com/o/r"
            [backends.local]
            type = "docker"
            [[class]]
            name = "x"
            backend = "local"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.validate().unwrap_err().to_string().contains("runc"));
        let cfg: Config =
            toml::from_str(&toml.replace("type = \"docker\"", "type = \"docker\"\nruntime = \"runsc\"")).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_backend_and_fields() {
        let toml = r#"
            [github]
            url = "https://github.com/o/r"
            [[class]]
            name = "x"
            backend = "nope"
        "#;
        assert!(toml::from_str::<Config>(toml).unwrap().validate().is_err());
        assert!(toml::from_str::<Config>(&toml.replace("backend = \"nope\"", "backend = \"nope\"\ncpus = 1")).is_err());
    }
}
