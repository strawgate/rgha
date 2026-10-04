//! Modal Sandboxes: per-second billing, gVisor isolation by default, optional
//! VM runtime, egress allowlists enforced by Modal outside the sandbox.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use rgha_modal::{Client, Profile, SandboxSpec};
use tokio::sync::OnceCell;

use super::{Backend, Instance, JIT_ENV, Network, RUNNER_ENTRYPOINT, RunnerSpec};

const TAG_OWNER: &str = "rgha";
const TAG_CLASS: &str = "rgha-class";
const TAG_RUNNER: &str = "rgha-runner";

pub struct ModalBackend {
    name: String,
    client: Client,
    app_name: String,
    image: String,
    image_commands: Vec<String>,
    runtime: Option<String>,
    regions: Vec<String>,
    ids: OnceCell<(String, String)>,
}

impl ModalBackend {
    pub async fn connect(
        name: &str,
        app: &str,
        image: &str,
        image_commands: &[String],
        profile: Option<&str>,
        runtime: Option<String>,
        regions: Vec<String>,
    ) -> anyhow::Result<Self> {
        let profile = Profile::load(profile).context("loading Modal credentials")?;
        let client = Client::connect(profile).await.context("connecting to Modal")?;
        Ok(Self {
            name: name.to_string(),
            client,
            app_name: app.to_string(),
            image: image.to_string(),
            image_commands: image_commands.to_vec(),
            runtime,
            regions,
            ids: OnceCell::new(),
        })
    }

    async fn ids(&self) -> anyhow::Result<&(String, String)> {
        self.ids
            .get_or_try_init(|| async {
                let app_id = self.client.app_get_or_create(&self.app_name).await?;
                tracing::info!(backend = %self.name, image = %self.image, "building/reusing Modal image");
                let image_id = self.client.image_from_registry(&app_id, &self.image, &self.image_commands).await?;
                tracing::info!(backend = %self.name, %app_id, %image_id, "Modal backend ready");
                anyhow::Ok((app_id, image_id))
            })
            .await
    }
}

pub(crate) fn sandbox_spec(
    spec: &RunnerSpec,
    image_id: &str,
    runtime: Option<String>,
    regions: Vec<String>,
) -> SandboxSpec {
    let network = match &spec.network {
        Network::Open => rgha_modal::Network::Open,
        Network::Allowlist { domains, cidrs } => {
            rgha_modal::Network::Allowlist { cidrs: cidrs.clone(), domains: domains.clone() }
        }
    };
    SandboxSpec {
        name: spec.name.clone(),
        image_id: image_id.to_string(),
        command: vec![RUNNER_ENTRYPOINT.to_string()],
        workdir: Some("/home/runner".into()),
        // The JIT config travels in an ephemeral Secret, not in the Sandbox
        // definition. It is single-use and bound to this one runner.
        secret_env: HashMap::from([
            (JIT_ENV.to_string(), spec.jit_config.clone()),
            // Modal may run the entrypoint as root; the sandbox is the boundary.
            ("RUNNER_ALLOW_RUNASROOT".to_string(), "1".to_string()),
        ]),
        cpu: spec.cpu,
        cpu_limit: Some(spec.cpu_limit),
        memory_mib: spec.memory_mib,
        memory_limit_mib: Some(spec.memory_mib),
        timeout: spec.timeout,
        network,
        runtime,
        regions,
        tags: HashMap::from([
            (TAG_OWNER.to_string(), "1".to_string()),
            (TAG_CLASS.to_string(), spec.class.clone()),
            (TAG_RUNNER.to_string(), spec.name.clone()),
        ]),
    }
}

#[async_trait]
impl Backend for ModalBackend {
    fn kind(&self) -> &'static str {
        "modal"
    }

    async fn prepare(&self) -> anyhow::Result<()> {
        self.ids().await.map(|_| ())
    }

    async fn start(&self, spec: &RunnerSpec) -> anyhow::Result<String> {
        let (app_id, image_id) = self.ids().await?;
        let sb = sandbox_spec(spec, image_id, self.runtime.clone(), self.regions.clone());
        Ok(self.client.sandbox_create(app_id, &sb).await?)
    }

    async fn stop(&self, id: &str) -> anyhow::Result<()> {
        Ok(self.client.sandbox_terminate(id).await?)
    }

    async fn wait(&self, id: &str) -> anyhow::Result<Option<i32>> {
        loop {
            if let Some(code) = self.client.sandbox_wait(id, Duration::from_secs(50)).await? {
                return Ok(Some(code));
            }
        }
    }

    async fn list(&self, class: &str) -> anyhow::Result<Vec<Instance>> {
        let (app_id, _) = self.ids().await?;
        let tags =
            HashMap::from([(TAG_OWNER.to_string(), "1".to_string()), (TAG_CLASS.to_string(), class.to_string())]);
        Ok(self
            .client
            .sandbox_list(app_id, &tags)
            .await?
            .into_iter()
            .map(|s| Instance { runner_name: s.tags.get(TAG_RUNNER).cloned().unwrap_or(s.name), id: s.id })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_config_only_in_secret_env() {
        let spec = RunnerSpec {
            name: "c-1".into(),
            class: "c".into(),
            jit_config: "SECRET".into(),
            cpu: 0.25,
            cpu_limit: 0.25,
            memory_mib: 512,
            timeout: Duration::from_secs(600),
            network: Network::Allowlist { domains: vec!["github.com".into()], cidrs: vec![] },
        };
        let sb = sandbox_spec(&spec, "im-1", None, vec![]);
        assert_eq!(sb.secret_env.get(JIT_ENV).map(String::as_str), Some("SECRET"));
        assert!(!sb.command.iter().any(|a| a.contains("SECRET")));
        assert!(!sb.tags.values().any(|v| v.contains("SECRET")));
        assert_eq!(sb.cpu_limit, Some(0.25));
        assert!(matches!(sb.network, rgha_modal::Network::Allowlist { .. }));
        assert_eq!(sb.tags.get(TAG_CLASS).map(String::as_str), Some("c"));
    }
}
