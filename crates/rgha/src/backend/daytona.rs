//! Daytona sandboxes over its REST API (no Rust SDK exists). Per-second
//! billing; `container` class by default, `linux-vm` via a VM-class snapshot.
//!
//! Flow: create sandbox (image built from a one-line Dockerfile, cached by
//! Daytona) → wait for `started` → start `run.sh` in a toolbox session →
//! poll that command's exit code → delete the sandbox.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{Backend, Instance, JIT_ENV, Network, RUNNER_ENTRYPOINT, RunnerSpec};

const SESSION: &str = "rgha";
const LABEL_OWNER: &str = "rgha";
const LABEL_CLASS: &str = "rgha-class";
const LABEL_RUNNER: &str = "rgha-runner";

pub struct DaytonaBackend {
    http: reqwest::Client,
    api_url: String,
    api_key: String,
    image: String,
    snapshot: Option<String>,
    target: Option<String>,
    disk_gib: u32,
    /// sandbox id → (toolbox base URL, runner command id)
    commands: Mutex<HashMap<String, (String, String)>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SandboxDto {
    id: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    error_reason: Option<String>,
    #[serde(default)]
    toolbox_proxy_url: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    labels: HashMap<String, String>,
}

impl DaytonaBackend {
    pub fn new(
        api_url: &str,
        api_key: String,
        image: &str,
        snapshot: Option<String>,
        target: Option<String>,
        disk_gib: u32,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            api_url: api_url.trim_end_matches('/').to_string(),
            api_key,
            image: image.to_string(),
            snapshot,
            target,
            disk_gib,
            commands: Mutex::new(HashMap::new()),
        })
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<Option<T>> {
        let mut rb = self.http.request(method.clone(), url).bearer_auth(&self.api_key);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let resp = rb.send().await.with_context(|| format!("{method} {url}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Daytona {method} {url}: HTTP {status}: {}", &text[..text.len().min(300)]);
        }
        if text.is_empty() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&text).with_context(|| format!("decoding {url}"))?))
    }

    pub(crate) fn create_body(&self, spec: &RunnerSpec) -> serde_json::Value {
        let minutes = spec.timeout.as_secs().div_ceil(60).max(1);
        let mut body = json!({
            "name": spec.name,
            // Daytona allocates whole cores and GB. Size to the burst cap.
            "cpu": spec.cpu_limit.ceil().max(1.0) as u32,
            "memory": spec.memory_limit_mib.div_ceil(1024).max(1),
            "disk": self.disk_gib,
            // The JIT config is single-use and bound to this runner.
            "env": { JIT_ENV: spec.jit_config, "RUNNER_ALLOW_RUNASROOT": "1" },
            "labels": { LABEL_OWNER: "1", LABEL_CLASS: spec.class, LABEL_RUNNER: spec.name },
            // Hard wall-clock cap, plus delete-on-stop so nothing lingers.
            "ttlMinutes": minutes,
            "autoStopInterval": minutes,
            "autoDeleteInterval": 0,
        });
        let obj = body.as_object_mut().expect("object");
        match &self.snapshot {
            Some(s) => {
                obj.insert("snapshot".into(), json!(s));
            }
            None => {
                obj.insert(
                    "buildInfo".into(),
                    json!({ "dockerfileContent": format!("FROM {}\n", self.image), "contextHashes": [] }),
                );
            }
        }
        if let Some(t) = &self.target {
            obj.insert("target".into(), json!(t));
        }
        if let Network::Allowlist { domains, cidrs } = &spec.network {
            if !domains.is_empty() {
                obj.insert("domainAllowList".into(), json!(domains.join(",")));
            }
            if !cidrs.is_empty() {
                obj.insert("networkAllowList".into(), json!(cidrs.join(",")));
            }
        }
        body
    }

    async fn wait_started(&self, id: &str) -> anyhow::Result<SandboxDto> {
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            let sb: SandboxDto = self
                .request(reqwest::Method::GET, &format!("{}/sandbox/{id}", self.api_url), None)
                .await?
                .context("sandbox disappeared while starting")?;
            match sb.state.as_str() {
                "started" => return Ok(sb),
                "error" | "build_failed" | "destroyed" | "destroying" => {
                    bail!("sandbox {id} entered {}: {}", sb.state, sb.error_reason.unwrap_or_default())
                }
                _ if Instant::now() > deadline => bail!("sandbox {id} not started after 10 minutes ({})", sb.state),
                _ => tokio::time::sleep(Duration::from_millis(400)).await,
            }
        }
    }

    async fn launch_runner(&self, id: &str, sb: &SandboxDto) -> anyhow::Result<()> {
        let proxy = sb.toolbox_proxy_url.as_deref().context("sandbox has no toolboxProxyUrl")?;
        let toolbox = format!("{}/{id}", proxy.trim_end_matches('/'));
        let _: Option<serde_json::Value> = self
            .request(
                reqwest::Method::POST,
                &format!("{toolbox}/process/session"),
                Some(json!({ "sessionId": SESSION })),
            )
            .await?;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Exec {
            cmd_id: String,
        }
        let exec: Exec = self
            .request(
                reqwest::Method::POST,
                &format!("{toolbox}/process/session/{SESSION}/exec"),
                Some(json!({ "command": RUNNER_ENTRYPOINT, "runAsync": true })),
            )
            .await?
            .context("exec returned no command id")?;
        self.commands.lock().expect("commands lock").insert(id.to_string(), (toolbox, exec.cmd_id));
        Ok(())
    }
}

#[async_trait]
impl Backend for DaytonaBackend {
    fn kind(&self) -> &'static str {
        "daytona"
    }

    async fn prepare(&self) -> anyhow::Result<()> {
        // Validates the key; image builds happen (and are cached) on first create.
        let _: Option<serde_json::Value> =
            self.request(reqwest::Method::GET, &format!("{}/sandbox?limit=1", self.api_url), None).await?;
        Ok(())
    }

    async fn start(&self, spec: &RunnerSpec) -> anyhow::Result<String> {
        let sb: SandboxDto = self
            .request(reqwest::Method::POST, &format!("{}/sandbox", self.api_url), Some(self.create_body(spec)))
            .await?
            .context("create returned no sandbox")?;
        let id = sb.id.clone();
        let res = async {
            let started = self.wait_started(&id).await?;
            self.launch_runner(&id, &started).await
        }
        .await;
        if let Err(e) = res {
            let _ = self.stop(&id).await;
            return Err(e);
        }
        Ok(id)
    }

    async fn stop(&self, id: &str) -> anyhow::Result<()> {
        self.commands.lock().expect("commands lock").remove(id);
        let _: Option<serde_json::Value> =
            self.request(reqwest::Method::DELETE, &format!("{}/sandbox/{id}", self.api_url), None).await?;
        Ok(())
    }

    /// The sandbox outlives the runner process, so wait on the command.
    async fn wait(&self, id: &str) -> anyhow::Result<Option<i32>> {
        let Some((toolbox, cmd)) = self.commands.lock().expect("commands lock").get(id).cloned() else {
            return Ok(None);
        };
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Command {
            exit_code: Option<i32>,
        }
        loop {
            let url = format!("{toolbox}/process/session/{SESSION}/command/{cmd}");
            match self.request::<Command>(reqwest::Method::GET, &url, None).await {
                Ok(None) => return Ok(None), // sandbox gone
                Ok(Some(Command { exit_code: Some(code) })) => return Ok(Some(code)),
                Ok(Some(_)) => {}
                Err(e) => tracing::debug!(sandbox = id, error = %e, "polling runner command"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn list(&self, class: &str) -> anyhow::Result<Vec<Instance>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            items: Vec<SandboxDto>,
            next_cursor: Option<String>,
        }
        let labels = json!({ LABEL_OWNER: "1", LABEL_CLASS: class }).to_string();
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut url = reqwest::Url::parse(&format!("{}/sandbox", self.api_url))?;
            url.query_pairs_mut().append_pair("labels", &labels).append_pair("limit", "100");
            if let Some(c) = &cursor {
                url.query_pairs_mut().append_pair("cursor", c);
            }
            let Some(page): Option<Page> = self.request(reqwest::Method::GET, url.as_str(), None).await? else { break };
            for sb in page.items.into_iter().filter(|s| !matches!(s.state.as_str(), "destroyed" | "destroying")) {
                let runner_name = sb.labels.get(LABEL_RUNNER).cloned().unwrap_or(sb.name);
                out.push(Instance { id: sb.id, runner_name });
            }
            match page.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(network: Network) -> RunnerSpec {
        RunnerSpec {
            name: "c-1".into(),
            class: "c".into(),
            jit_config: "JIT".into(),
            cpu: 0.25,
            cpu_limit: 2.0,
            memory_mib: 1024,
            memory_limit_mib: 1536,
            timeout: Duration::from_secs(61 * 60),
            network,
            job_started_hook: None,
        }
    }

    #[test]
    fn create_body_rounds_resources_and_caps_lifetime() {
        let b = DaytonaBackend::new("https://x/api/", "k".into(), "img:1", None, Some("us".into()), 5).unwrap();
        let body = b.create_body(&spec(Network::Open));
        assert_eq!(body["cpu"], 2);
        assert_eq!(body["memory"], 2);
        assert_eq!(body["ttlMinutes"], 61);
        assert_eq!(body["autoDeleteInterval"], 0);
        assert_eq!(body["buildInfo"]["dockerfileContent"], "FROM img:1\n");
        assert_eq!(body["target"], "us");
        assert_eq!(body["env"][JIT_ENV], "JIT");
        assert_eq!(body["labels"][LABEL_CLASS], "c");
        assert!(body.get("domainAllowList").is_none());
    }

    #[test]
    fn create_body_uses_snapshot_and_allowlists() {
        let b = DaytonaBackend::new("https://x/api", "k".into(), "img", Some("rgha-vm".into()), None, 5).unwrap();
        let body = b.create_body(&spec(Network::Allowlist {
            domains: vec!["github.com".into(), "api.github.com".into()],
            cidrs: vec!["10.0.0.0/8".into()],
        }));
        assert_eq!(body["snapshot"], "rgha-vm");
        assert!(body.get("buildInfo").is_none());
        assert_eq!(body["domainAllowList"], "github.com,api.github.com");
        assert_eq!(body["networkAllowList"], "10.0.0.0/8");
    }
}
