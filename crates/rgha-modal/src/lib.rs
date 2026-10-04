//! Minimal Modal client for creating and terminating Sandboxes.
//!
//! Modal publishes official SDKs for Python, JS and Go only. This crate talks to
//! the same public gRPC API (vendored `proto/api.proto`, Apache-2.0) and covers
//! only what rgha needs: build a registry image, create a Sandbox with an
//! ephemeral Secret, list it by tag, and terminate it.
//!
//! Modal states that direct gRPC usage carries no compatibility guarantee, so
//! the surface here is intentionally tiny and mirrors the Go SDK request shapes.

pub mod pb {
    #![allow(clippy::all)]
    tonic::include_proto!("modal.client");
}

mod profile;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pb::modal_client_client::ModalClientClient;
use tokio::sync::Mutex;
use tonic::metadata::{AsciiMetadataValue, MetadataMap};
use tonic::service::Interceptor;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

pub use profile::Profile;

/// Mirrors the Python SDK version the Go SDK claims compatibility with.
const CLIENT_VERSION: &str = "1.0.0";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("modal config: {0}")]
    Config(String),
    #[error("modal transport: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("modal rpc: {0}")]
    Rpc(#[from] tonic::Status),
    #[error("modal image build failed: {0}")]
    ImageBuild(String),
    #[error("modal sandbox: {0}")]
    Sandbox(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
struct HeaderInterceptor {
    static_headers: Arc<Vec<(&'static str, AsciiMetadataValue)>>,
    auth_token: Arc<std::sync::RwLock<Option<AsciiMetadataValue>>>,
}

impl Interceptor for HeaderInterceptor {
    fn call(&mut self, mut req: tonic::Request<()>) -> std::result::Result<tonic::Request<()>, tonic::Status> {
        let md: &mut MetadataMap = req.metadata_mut();
        for (k, v) in self.static_headers.iter() {
            md.insert(*k, v.clone());
        }
        if let Some(token) = self.auth_token.read().expect("auth token lock").clone() {
            md.insert("x-modal-auth-token", token);
        }
        Ok(req)
    }
}

type Grpc = ModalClientClient<tonic::service::interceptor::InterceptedService<Channel, HeaderInterceptor>>;

/// Resources and isolation settings for one Sandbox.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub name: String,
    pub image_id: String,
    pub command: Vec<String>,
    pub workdir: Option<String>,
    /// Injected through an ephemeral Modal Secret, never in the definition itself.
    pub secret_env: HashMap<String, String>,
    /// Physical cores (Modal bills per physical core; 1 core = 2 vCPU).
    pub cpu: f64,
    pub cpu_limit: Option<f64>,
    pub memory_mib: u32,
    pub memory_limit_mib: Option<u32>,
    pub timeout: Duration,
    pub network: Network,
    /// `None` lets Modal pick (gVisor today); `Some("vm")` requests the alpha VM runtime.
    pub runtime: Option<String>,
    pub regions: Vec<String>,
    pub tags: HashMap<String, String>,
    /// Allow memory snapshots of this Sandbox (alpha).
    pub enable_snapshot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Network {
    Open,
    Blocked,
    Allowlist { cidrs: Vec<String>, domains: Vec<String> },
}

impl Network {
    fn to_proto(&self) -> pb::NetworkAccess {
        use pb::network_access::NetworkAccessType as T;
        match self {
            Network::Open => pb::NetworkAccess { network_access_type: T::Open as i32, ..Default::default() },
            Network::Blocked => pb::NetworkAccess { network_access_type: T::Blocked as i32, ..Default::default() },
            Network::Allowlist { cidrs, domains } => pb::NetworkAccess {
                network_access_type: T::Allowlist as i32,
                allowed_cidrs: cidrs.clone(),
                allowed_domains: domains.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ResourceUsage {
    pub cpu_core_secs: f64,
    pub mem_gib_secs: f64,
}

#[derive(Debug, Clone)]
pub struct BillingItem {
    pub object_id: String,
    pub description: String,
    pub cost_usd: f64,
    pub cost_by_resource: HashMap<String, f64>,
    pub tags: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct SandboxSummary {
    pub id: String,
    pub name: String,
    pub created_at: f64,
    pub tags: HashMap<String, String>,
}

/// A connected Modal client scoped to one App and environment.
#[derive(Clone)]
pub struct Client {
    grpc: Grpc,
    auth_token: Arc<std::sync::RwLock<Option<AsciiMetadataValue>>>,
    auth_expiry: Arc<Mutex<u64>>,
    environment: String,
    image_builder_version: Option<String>,
}

impl Client {
    pub async fn connect(profile: Profile) -> Result<Self> {
        // Both ring (tonic) and aws-lc (reqwest, in the rgha binary) may be linked;
        // pick one explicitly. Errors only mean a provider is already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (host, endpoint) = profile.endpoint()?;
        let channel = Endpoint::from_shared(endpoint)?
            .tls_config(ClientTlsConfig::new().with_webpki_roots())?
            .connect_timeout(Duration::from_secs(10))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .connect()
            .await?;

        let mut headers: Vec<(&'static str, AsciiMetadataValue)> = vec![
            ("x-modal-client-type", ascii(&(pb::ClientType::LibmodalGo as i32).to_string())?),
            ("x-modal-client-version", ascii(CLIENT_VERSION)?),
            ("x-modal-libmodal-version", ascii(concat!("rgha-modal/", env!("CARGO_PKG_VERSION")))?),
            ("x-modal-host", ascii(&host)?),
        ];
        headers.push(("x-modal-token-id", ascii(&profile.token_id)?));
        headers.push(("x-modal-token-secret", ascii(&profile.token_secret)?));

        let auth_token = Arc::new(std::sync::RwLock::new(None));
        let interceptor = HeaderInterceptor { static_headers: Arc::new(headers), auth_token: auth_token.clone() };
        let grpc =
            ModalClientClient::with_interceptor(channel, interceptor).max_decoding_message_size(64 * 1024 * 1024);

        let client = Self {
            grpc,
            auth_token,
            auth_expiry: Arc::new(Mutex::new(0)),
            environment: profile.environment.clone(),
            image_builder_version: profile.image_builder_version.clone(),
        };
        client.refresh_auth_token().await?;
        Ok(client)
    }

    /// Fetches a short-lived auth token, refreshing ~5 minutes before expiry.
    async fn refresh_auth_token(&self) -> Result<()> {
        let mut expiry = self.auth_expiry.lock().await;
        if *expiry > now_secs() + 300 {
            return Ok(());
        }
        let resp = self.grpc.clone().auth_token_get(pb::AuthTokenGetRequest {}).await?.into_inner();
        if resp.token.is_empty() {
            return Err(Error::Config("AuthTokenGet returned an empty token".into()));
        }
        *expiry = jwt_exp(&resp.token).unwrap_or(now_secs() + 1200);
        *self.auth_token.write().expect("auth token lock") = Some(ascii(&resp.token)?);
        Ok(())
    }

    async fn rpc(&self) -> Result<Grpc> {
        self.refresh_auth_token().await?;
        Ok(self.grpc.clone())
    }

    pub async fn app_get_or_create(&self, name: &str) -> Result<String> {
        let resp = self
            .rpc()
            .await?
            .app_get_or_create(pb::AppGetOrCreateRequest {
                app_name: name.to_string(),
                environment_name: self.environment.clone(),
                object_creation_type: pb::ObjectCreationType::CreateIfMissing as i32,
            })
            .await?
            .into_inner();
        Ok(resp.app_id)
    }

    async fn builder_version(&self) -> Result<String> {
        if let Some(v) = &self.image_builder_version {
            return Ok(v.clone());
        }
        let resp = self
            .rpc()
            .await?
            .environment_get_or_create(pb::EnvironmentGetOrCreateRequest {
                deployment_name: self.environment.clone(),
                object_creation_type: pb::ObjectCreationType::Unspecified as i32,
            })
            .await?
            .into_inner();
        Ok(resp.metadata.and_then(|m| m.settings).map(|s| s.image_builder_version).unwrap_or_default())
    }

    /// Builds (or reuses Modal's cache of) an image from a registry tag plus
    /// optional Dockerfile commands, returning the image id.
    pub async fn image_from_registry(&self, app_id: &str, tag: &str, extra_commands: &[String]) -> Result<String> {
        let mut dockerfile_commands = vec![format!("FROM {tag}")];
        dockerfile_commands.extend(extra_commands.iter().cloned());
        let resp = self
            .rpc()
            .await?
            .image_get_or_create(pb::ImageGetOrCreateRequest {
                app_id: app_id.to_string(),
                image: Some(pb::Image { dockerfile_commands, ..Default::default() }),
                builder_version: self.builder_version().await?,
                ..Default::default()
            })
            .await?
            .into_inner();

        let mut result = resp.result.filter(|r| r.status != pb::generic_result::GenericStatus::Unspecified as i32);
        let mut last_entry_id = String::new();
        // Tail of the build output, surfaced if the build fails.
        let mut log_tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        while result.is_none() {
            let mut stream = self
                .rpc()
                .await?
                .image_join_streaming(pb::ImageJoinStreamingRequest {
                    image_id: resp.image_id.clone(),
                    timeout: 55.0,
                    last_entry_id: last_entry_id.clone(),
                    include_logs_for_finished: true,
                })
                .await?
                .into_inner();
            while let Some(item) = stream.message().await? {
                if !item.entry_id.is_empty() {
                    last_entry_id = item.entry_id.clone();
                }
                for log in &item.task_logs {
                    for line in log.data.lines().filter(|l| !l.trim().is_empty()) {
                        tracing::debug!(target: "rgha_modal::image_build", "{line}");
                        log_tail.push_back(line.to_string());
                        if log_tail.len() > 40 {
                            log_tail.pop_front();
                        }
                    }
                }
                if let Some(r) = item.result
                    && r.status != pb::generic_result::GenericStatus::Unspecified as i32
                {
                    result = Some(r);
                    break;
                }
            }
        }
        let result = result.expect("loop exits only with a result");
        if result.status != pb::generic_result::GenericStatus::Success as i32 {
            let tail: Vec<String> = log_tail.into_iter().collect();
            return Err(Error::ImageBuild(format!(
                "image {} status {}: {}\n--- build log tail ---\n{}",
                resp.image_id,
                result.status,
                result.exception,
                tail.join("\n")
            )));
        }
        Ok(resp.image_id)
    }

    async fn ephemeral_secret(&self, env: &HashMap<String, String>) -> Result<String> {
        let resp = self
            .rpc()
            .await?
            .secret_get_or_create(pb::SecretGetOrCreateRequest {
                environment_name: self.environment.clone(),
                object_creation_type: pb::ObjectCreationType::Ephemeral as i32,
                env_dict: env.clone(),
                ..Default::default()
            })
            .await?
            .into_inner();
        Ok(resp.secret_id)
    }

    /// Creates a Sandbox and returns its id. Billing starts once it is scheduled
    /// and stops when its entrypoint exits, it times out, or it is terminated.
    pub async fn sandbox_create(&self, app_id: &str, spec: &SandboxSpec) -> Result<String> {
        let secret_ids =
            if spec.secret_env.is_empty() { vec![] } else { vec![self.ephemeral_secret(&spec.secret_env).await?] };
        let resources = pb::Resources {
            milli_cpu: (spec.cpu * 1000.0).round() as u32,
            milli_cpu_max: spec.cpu_limit.map(|c| (c * 1000.0).round() as u32).unwrap_or(0),
            memory_mb: spec.memory_mib,
            memory_mb_max: spec.memory_limit_mib.unwrap_or(0),
            ..Default::default()
        };
        let definition = pb::Sandbox {
            entrypoint_args: spec.command.clone(),
            image_id: spec.image_id.clone(),
            secret_ids,
            resources: Some(resources),
            timeout_secs: spec.timeout.as_secs().max(1) as u32,
            workdir: spec.workdir.clone(),
            network_access: Some(spec.network.to_proto()),
            runtime: spec.runtime.clone(),
            name: Some(spec.name.clone()),
            enable_snapshot: spec.enable_snapshot,
            scheduler_placement: (!spec.regions.is_empty())
                .then(|| pb::SchedulerPlacement { regions: spec.regions.clone(), ..Default::default() }),
            ..Default::default()
        };
        let resp = self
            .rpc()
            .await?
            .sandbox_create(pb::SandboxCreateRequest {
                app_id: app_id.to_string(),
                definition: Some(definition),
                environment_name: self.environment.clone(),
                tags: to_tags(&spec.tags),
            })
            .await?
            .into_inner();
        Ok(resp.sandbox_id)
    }

    /// Waits until the Sandbox is scheduled and running (or `timeout`).
    pub async fn sandbox_wait_running(&self, sandbox_id: &str, timeout: Duration) -> Result<()> {
        let resp = self
            .rpc()
            .await?
            .sandbox_get_task_id(pb::SandboxGetTaskIdRequest {
                sandbox_id: sandbox_id.to_string(),
                timeout: Some(timeout.as_secs_f32()),
                wait_until_ready: true,
            })
            .await?
            .into_inner();
        if let Some(r) = resp.task_result
            && r.status != pb::generic_result::GenericStatus::Unspecified as i32
            && r.status != pb::generic_result::GenericStatus::Success as i32
        {
            return Err(Error::Sandbox(format!("sandbox {sandbox_id} failed to start: {}", r.exception)));
        }
        Ok(())
    }

    /// Takes a memory snapshot (RAM + filesystem; open TCP connections are
    /// closed). The Sandbox must have been created with `enable_snapshot`.
    pub async fn sandbox_snapshot(&self, sandbox_id: &str) -> Result<String> {
        let snapshot_id = self
            .rpc()
            .await?
            .sandbox_snapshot(pb::SandboxSnapshotRequest { sandbox_id: sandbox_id.to_string() })
            .await?
            .into_inner()
            .snapshot_id;
        loop {
            let r = self
                .rpc()
                .await?
                .sandbox_snapshot_wait(pb::SandboxSnapshotWaitRequest {
                    snapshot_id: snapshot_id.clone(),
                    timeout: 55.0,
                })
                .await?
                .into_inner()
                .result;
            match r {
                Some(r) if r.status == pb::generic_result::GenericStatus::Success as i32 => return Ok(snapshot_id),
                Some(r) if r.status != pb::generic_result::GenericStatus::Unspecified as i32 => {
                    return Err(Error::Sandbox(format!("snapshot {snapshot_id} failed: {}", r.exception)));
                }
                _ => {}
            }
        }
    }

    /// Restores a memory snapshot into a new running Sandbox; returns its id.
    pub async fn sandbox_restore(&self, snapshot_id: &str) -> Result<String> {
        let sandbox_id = self
            .rpc()
            .await?
            .sandbox_restore(pb::SandboxRestoreRequest {
                snapshot_id: snapshot_id.to_string(),
                sandbox_name_override_type: pb::sandbox_restore_request::SandboxNameOverrideType::None as i32,
                ..Default::default()
            })
            .await?
            .into_inner()
            .sandbox_id;
        self.sandbox_wait_running(&sandbox_id, Duration::from_secs(55)).await?;
        Ok(sandbox_id)
    }

    /// Actual resource usage of a Sandbox so far (CPU core-seconds and
    /// memory GiB-seconds), as metered by Modal.
    pub async fn sandbox_resource_usage(&self, sandbox_id: &str) -> Result<ResourceUsage> {
        let r = self
            .rpc()
            .await?
            .sandbox_get_resource_usage(pb::SandboxGetResourceUsageRequest { sandbox_id: sandbox_id.to_string() })
            .await?
            .into_inner();
        Ok(ResourceUsage {
            cpu_core_secs: r.cpu_core_nanosecs as f64 / 1e9,
            mem_gib_secs: r.mem_gib_nanosecs as f64 / 1e9,
        })
    }

    /// Billed cost per object (e.g. Sandbox) for `app_id` between two Unix
    /// times, at hourly resolution, as reported by Modal's billing system.
    pub async fn billing_report(&self, app_id: &str, start_unix: i64, end_unix: i64) -> Result<Vec<BillingItem>> {
        let ts = |s: i64| prost_types::Timestamp { seconds: s, nanos: 0 };
        let mut stream = self
            .rpc()
            .await?
            .workspace_billing_report(pb::WorkspaceBillingReportRequest {
                start_timestamp: Some(ts(start_unix)),
                end_timestamp: Some(ts(end_unix)),
                resolution: "h".into(),
                app_ids: vec![app_id.to_string()],
                ..Default::default()
            })
            .await?
            .into_inner();
        let mut out = Vec::new();
        while let Some(item) = stream.message().await? {
            out.push(BillingItem {
                object_id: item.object_id,
                description: item.description,
                cost_usd: item.cost.parse().unwrap_or(0.0),
                cost_by_resource: item
                    .cost_by_resource
                    .into_iter()
                    .map(|(k, v)| (k, v.parse().unwrap_or(0.0)))
                    .collect(),
                tags: item.tags.into_iter().collect(),
            });
        }
        Ok(out)
    }

    pub async fn sandbox_terminate(&self, sandbox_id: &str) -> Result<()> {
        self.rpc().await?.sandbox_terminate(pb::SandboxTerminateRequest { sandbox_id: sandbox_id.to_string() }).await?;
        Ok(())
    }

    /// Waits up to `timeout` for the Sandbox to finish; returns its exit code if it did.
    pub async fn sandbox_wait(&self, sandbox_id: &str, timeout: Duration) -> Result<Option<i32>> {
        let resp = self
            .rpc()
            .await?
            .sandbox_wait(pb::SandboxWaitRequest { sandbox_id: sandbox_id.to_string(), timeout: timeout.as_secs_f32() })
            .await?
            .into_inner();
        Ok(resp.result.map(|r| r.exitcode))
    }

    /// Lists running Sandboxes in the App that carry all of `tags`.
    pub async fn sandbox_list(&self, app_id: &str, tags: &HashMap<String, String>) -> Result<Vec<SandboxSummary>> {
        let mut out = Vec::new();
        let mut before = 0.0;
        loop {
            let resp = self
                .rpc()
                .await?
                .sandbox_list(pb::SandboxListRequest {
                    app_id: app_id.to_string(),
                    before_timestamp: before,
                    environment_name: self.environment.clone(),
                    include_finished: false,
                    tags: to_tags(tags),
                })
                .await?
                .into_inner();
            if resp.sandboxes.is_empty() {
                break;
            }
            for sb in resp.sandboxes {
                before = sb.created_at;
                out.push(SandboxSummary {
                    id: sb.id,
                    name: sb.name,
                    created_at: sb.created_at,
                    tags: sb.tags.into_iter().map(|t| (t.tag_name, t.tag_value)).collect(),
                });
            }
        }
        Ok(out)
    }
}

fn to_tags(tags: &HashMap<String, String>) -> Vec<pb::SandboxTag> {
    tags.iter().map(|(k, v)| pb::SandboxTag { tag_name: k.clone(), tag_value: v.clone() }).collect()
}

fn ascii(s: &str) -> Result<AsciiMetadataValue> {
    s.parse().map_err(|_| Error::Config("header value is not ASCII".into()))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Reads the `exp` claim from a JWT without verifying it (the server is the
/// authority; we only need it to schedule a refresh).
fn jwt_exp(token: &str) -> Option<u64> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp")?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_exp_reads_claim() {
        use base64::Engine;
        let enc = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        let token = format!("{}.{}.sig", enc(r#"{"alg":"none"}"#), enc(r#"{"exp":1234}"#));
        assert_eq!(jwt_exp(&token), Some(1234));
        assert_eq!(jwt_exp("garbage"), None);
    }

    #[test]
    fn network_allowlist_maps_to_proto() {
        let n = Network::Allowlist { cidrs: vec!["10.0.0.0/8".into()], domains: vec!["github.com".into()] };
        let p = n.to_proto();
        assert_eq!(p.network_access_type, pb::network_access::NetworkAccessType::Allowlist as i32);
        assert_eq!(p.allowed_domains, vec!["github.com".to_string()]);
        assert_eq!(
            Network::Blocked.to_proto().network_access_type,
            pb::network_access::NetworkAccessType::Blocked as i32
        );
    }
}
