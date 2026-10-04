//! Scale set admin client: GitHub App / PAT auth, Actions service admin token
//! exchange, scale set CRUD, JIT runner configs and runner removal.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, RwLock};

use crate::config::GitHubConfig;
use crate::session::MessageSession;
use crate::types::*;
use crate::{Error, Result};

pub(crate) const SCALE_SET_ENDPOINT: &str = "_apis/runtime/runnerscalesets";
const RUNNER_ENDPOINT: &str = "_apis/distributedtask/pools/0/agents";
pub(crate) const API_VERSION: &str = "6.0-preview";

/// How the controller authenticates to GitHub.
#[derive(Clone)]
pub enum Credentials {
    /// Recommended: short-lived installation tokens, scoped to the installation.
    App { client_id: String, installation_id: i64, private_key_pem: String },
    /// Classic PAT with `repo`/`admin:org`, or fine-grained with Administration: write.
    Token(String),
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credentials::App { client_id, installation_id, .. } => f
                .debug_struct("App")
                .field("client_id", client_id)
                .field("installation_id", installation_id)
                .finish_non_exhaustive(),
            Credentials::Token(_) => f.write_str("Token(<redacted>)"),
        }
    }
}

/// Identifies the controller to GitHub in the User-Agent (as ARC does).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SystemInfo {
    pub system: String,
    pub version: String,
    pub commit_sha: String,
    pub scale_set_id: i64,
    pub subsystem: String,
}

#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub system: SystemInfo,
    /// Overrides the derived REST API base URL (tests, proxies).
    pub api_base_url: Option<String>,
    pub max_retries: u32,
    pub max_retry_wait: Duration,
    pub request_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            system: SystemInfo {
                system: "rgha".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            api_base_url: None,
            max_retries: 4,
            max_retry_wait: Duration::from_secs(30),
            request_timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone)]
struct AdminToken {
    url: String,
    token: String,
    expires_at: DateTime<Utc>,
}

pub(crate) struct Inner {
    pub(crate) http: reqwest::Client,
    config: GitHubConfig,
    creds: Credentials,
    opts: ClientOptions,
    pub(crate) user_agent: String,
    admin: RwLock<Option<AdminToken>>,
    refresh: Mutex<()>,
}

/// Cheap to clone; all clones share the admin token cache.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<Inner>,
}

impl Client {
    pub fn new(config_url: &str, creds: Credentials, opts: ClientOptions) -> Result<Self> {
        let config = GitHubConfig::parse(config_url)?;
        if let Credentials::App { client_id, installation_id, private_key_pem } = &creds {
            if client_id.is_empty() || *installation_id == 0 || private_key_pem.is_empty() {
                return Err(Error::Config("GitHub App client_id, installation_id and private key are required".into()));
            }
            jsonwebtoken::EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
                .map_err(|e| Error::Config(format!("invalid GitHub App private key: {e}")))?;
        }
        let user_agent = serde_json::json!({
            "system": opts.system.system, "version": opts.system.version, "commit_sha": opts.system.commit_sha,
            "scale_set_id": opts.system.scale_set_id, "subsystem": opts.system.subsystem,
            "build_version": env!("CARGO_PKG_VERSION"), "kind": "scaleset",
        })
        .to_string();
        let http = reqwest::Client::builder()
            .timeout(opts.request_timeout)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Config(format!("building HTTP client: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                config,
                creds,
                opts,
                user_agent,
                admin: RwLock::new(None),
                refresh: Mutex::new(()),
            }),
        })
    }

    fn api_url(&self, path: &str) -> String {
        match &self.inner.opts.api_base_url {
            Some(base) => format!("{}{path}", base.trim_end_matches('/')),
            None => self.inner.config.api_url(path),
        }
    }

    // ---- retrying transport -------------------------------------------------

    /// Sends with retries on connection errors, 429 and 5xx (except 501),
    /// matching hashicorp/go-retryablehttp's default policy used upstream.
    pub(crate) async fn send(&self, req: reqwest::Request) -> Result<reqwest::Response> {
        self.send_retrying(req, |_| false).await
    }

    pub(crate) async fn send_retrying(
        &self,
        req: reqwest::Request,
        extra_retry: impl Fn(reqwest::StatusCode) -> bool,
    ) -> Result<reqwest::Response> {
        let mut attempt = 0u32;
        loop {
            let this = req.try_clone().expect("requests have buffered bodies");
            let method = this.method().clone();
            let url = this.url().clone();
            let res = self.inner.http.execute(this).await;
            let retryable = match &res {
                Ok(r) => {
                    let s = r.status();
                    s == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || (s.is_server_error() && s != reqwest::StatusCode::NOT_IMPLEMENTED)
                        || extra_retry(s)
                }
                Err(e) => e.is_connect() || e.is_timeout() || e.is_request(),
            };
            if !retryable || attempt >= self.inner.opts.max_retries {
                return res.map_err(|e| Error::Transport { method: method.to_string(), url: redact(&url), source: e });
            }
            let wait = Duration::from_secs(1 << attempt.min(5)).min(self.inner.opts.max_retry_wait);
            tracing::debug!(%method, url = %redact(&url), attempt, ?wait, "retrying request");
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    // ---- GitHub REST auth -----------------------------------------------------

    async fn github_bearer(&self) -> Result<String> {
        match &self.inner.creds {
            Credentials::Token(t) => Ok(format!("Bearer {t}")),
            Credentials::App { client_id, installation_id, private_key_pem } => {
                let jwt = app_jwt(client_id, private_key_pem)?;
                let req = self
                    .inner
                    .http
                    .post(self.api_url(&format!("/app/installations/{installation_id}/access_tokens")))
                    .header("Accept", "application/vnd.github+json")
                    .header("Authorization", format!("Bearer {jwt}"))
                    .header("User-Agent", &self.inner.user_agent)
                    .build()
                    .map_err(Error::build)?;
                #[derive(Deserialize)]
                struct AccessToken {
                    token: String,
                }
                let t: AccessToken = expect_json(self.send(req).await?, 201).await?;
                Ok(format!("Bearer {}", t.token))
            }
        }
    }

    async fn registration_token(&self) -> Result<String> {
        let req = self
            .inner
            .http
            .post(self.api_url(&self.inner.config.registration_token_path()))
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", self.github_bearer().await?)
            .header("User-Agent", &self.inner.user_agent)
            .build()
            .map_err(Error::build)?;
        #[derive(Deserialize)]
        struct RegToken {
            token: String,
        }
        Ok(expect_json::<RegToken>(self.send(req).await?, 201).await?.token)
    }

    /// Returns a valid Actions service admin token, refreshing it when it
    /// expires within 60 seconds.
    async fn admin_token(&self) -> Result<AdminToken> {
        let fresh =
            |t: &Option<AdminToken>| t.clone().filter(|t| t.expires_at > Utc::now() + chrono::Duration::seconds(60));
        if let Some(t) = fresh(&*self.inner.admin.read().await) {
            return Ok(t);
        }
        let _guard = self.inner.refresh.lock().await;
        if let Some(t) = fresh(&*self.inner.admin.read().await) {
            return Ok(t);
        }
        tracing::info!(config_url = %self.inner.config.config_url, "refreshing actions service admin token");
        let reg = self.registration_token().await?;
        let body = serde_json::json!({ "url": self.inner.config.config_url.as_str(), "runner_event": "register" });
        let req = self
            .inner
            .http
            .post(self.api_url("/actions/runner-registration"))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("RemoteAuth {reg}"))
            .header("User-Agent", &self.inner.user_agent)
            .body(body.to_string())
            .build()
            .map_err(Error::build)?;
        #[derive(Deserialize)]
        struct AdminConn {
            url: Option<String>,
            token: Option<String>,
        }
        // Upstream retries 401/403 here: the fresh registration token can take a
        // moment to become valid.
        let resp = self
            .send_retrying(req, |s| s == reqwest::StatusCode::UNAUTHORIZED || s == reqwest::StatusCode::FORBIDDEN)
            .await?;
        let conn: AdminConn = expect_json_range(resp).await?;
        let (url, token) = match (conn.url.filter(|s| !s.is_empty()), conn.token.filter(|s| !s.is_empty())) {
            (Some(u), Some(t)) => (u, t),
            _ => return Err(Error::Protocol("runner-registration response missing url or token".into())),
        };
        let expires_at = jwt_expiry(&token).ok_or_else(|| Error::Protocol("admin token has no exp claim".into()))?;
        let t = AdminToken { url, token, expires_at };
        *self.inner.admin.write().await = Some(t.clone());
        Ok(t)
    }

    /// Builds a request against the Actions service using the admin token.
    pub(crate) async fn actions_request(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<reqwest::RequestBuilder> {
        let admin = self.admin_token().await?;
        let mut url = url::Url::parse(&format!("{}/{}", admin.url.trim_end_matches('/'), path.trim_start_matches('/')))
            .map_err(|e| Error::Protocol(format!("bad actions service url: {e}")))?;
        {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
            q.append_pair("api-version", API_VERSION);
        }
        let mut rb = self
            .inner
            .http
            .request(method, url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", admin.token))
            .header("User-Agent", &self.inner.user_agent);
        if let Some(b) = body {
            rb = rb.body(b.to_string());
        }
        Ok(rb)
    }

    async fn actions_json<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
        expect: u16,
    ) -> Result<T> {
        let req = self.actions_request(method, path, query, body).await?.build().map_err(Error::build)?;
        expect_json(self.send(req).await?, expect).await
    }

    async fn actions_empty(&self, method: reqwest::Method, path: &str, expect: u16) -> Result<()> {
        let req = self.actions_request(method, path, &[], None).await?.build().map_err(Error::build)?;
        expect_status(self.send(req).await?, expect).await
    }

    // ---- public API -----------------------------------------------------------

    pub async fn get_runner_group_by_name(&self, name: &str) -> Result<RunnerGroup> {
        let list: ListResponse<RunnerGroup> = self
            .actions_json(reqwest::Method::GET, "_apis/runtime/runnergroups/", &[("groupName", name.into())], None, 200)
            .await?;
        match list.count {
            1 => Ok(list.value.into_iter().next().expect("count==1")),
            0 => Err(Error::NotFound(format!("runner group {name:?}"))),
            _ => Err(Error::Protocol(format!("multiple runner groups named {name:?}"))),
        }
    }

    pub async fn get_scale_set(&self, runner_group_id: i64, name: &str) -> Result<Option<RunnerScaleSet>> {
        let list: ListResponse<RunnerScaleSet> = self
            .actions_json(
                reqwest::Method::GET,
                SCALE_SET_ENDPOINT,
                &[("runnerGroupId", runner_group_id.to_string()), ("name", name.into())],
                None,
                200,
            )
            .await?;
        match list.count {
            0 => Ok(None),
            1 => Ok(list.value.into_iter().next()),
            _ => Err(Error::Protocol(format!("multiple scale sets named {name:?}"))),
        }
    }

    pub async fn create_scale_set(&self, mut scale_set: RunnerScaleSet) -> Result<RunnerScaleSet> {
        if scale_set.labels.is_empty() {
            if scale_set.name.is_empty() {
                return Err(Error::Config("scale set needs a name or at least one label".into()));
            }
            scale_set.labels = vec![Label::system(scale_set.name.clone())];
        }
        default_label_types(&mut scale_set);
        self.actions_json(reqwest::Method::POST, SCALE_SET_ENDPOINT, &[], Some(serde_json::to_value(&scale_set)?), 200)
            .await
    }

    pub async fn update_scale_set(&self, id: i64, mut scale_set: RunnerScaleSet) -> Result<RunnerScaleSet> {
        default_label_types(&mut scale_set);
        self.actions_json(
            reqwest::Method::PATCH,
            &format!("{SCALE_SET_ENDPOINT}/{id}"),
            &[],
            Some(serde_json::to_value(&scale_set)?),
            200,
        )
        .await
    }

    pub async fn delete_scale_set(&self, id: i64) -> Result<()> {
        self.actions_empty(reqwest::Method::DELETE, &format!("{SCALE_SET_ENDPOINT}/{id}"), 204).await
    }

    /// Returns a single-use runner registration: the runner takes exactly one job.
    pub async fn generate_jit_config(
        &self,
        scale_set_id: i64,
        runner_name: &str,
        work_folder: &str,
    ) -> Result<JitRunnerConfig> {
        let body = serde_json::to_value(JitRunnerSetting { name: runner_name, work_folder })?;
        self.actions_json(
            reqwest::Method::POST,
            &format!("{SCALE_SET_ENDPOINT}/{scale_set_id}/generatejitconfig"),
            &[],
            Some(body),
            200,
        )
        .await
    }

    pub async fn get_runner_by_name(&self, name: &str) -> Result<Option<RunnerReference>> {
        let list: ListResponse<RunnerReference> =
            self.actions_json(reqwest::Method::GET, RUNNER_ENDPOINT, &[("agentName", name.into())], None, 200).await?;
        match list.count {
            0 => Ok(None),
            1 => Ok(list.value.into_iter().next()),
            _ => Err(Error::Protocol(format!("multiple runners named {name:?}"))),
        }
    }

    /// Cancels a workflow run via the REST API (needs Actions: write). Used to
    /// enforce policy on jobs the service assigned to a scale set directly.
    pub async fn cancel_workflow_run(&self, owner: &str, repo: &str, run_id: i64) -> Result<()> {
        let req = self
            .inner
            .http
            .post(self.api_url(&format!("/repos/{owner}/{repo}/actions/runs/{run_id}/cancel")))
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", self.github_bearer().await?)
            .header("User-Agent", &self.inner.user_agent)
            .build()
            .map_err(Error::build)?;
        let resp = self.send(req).await?;
        // 409: the run already finished or is already being cancelled.
        if resp.status().as_u16() == 409 {
            return Ok(());
        }
        expect_status(resp, 202).await
    }

    /// Deregisters a runner. The service refuses to remove a runner that is
    /// running a job, which makes this a safe "reap only if idle" primitive.
    pub async fn remove_runner(&self, runner_id: i64) -> Result<()> {
        self.actions_empty(reqwest::Method::DELETE, &format!("{RUNNER_ENDPOINT}/{runner_id}"), 204).await
    }

    /// Opens a message session (long-poll queue) for a scale set.
    pub async fn message_session(&self, scale_set_id: i64, owner: &str) -> Result<MessageSession> {
        MessageSession::create(self.clone(), scale_set_id, owner).await
    }
}

fn default_label_types(s: &mut RunnerScaleSet) {
    for l in &mut s.labels {
        if l.kind.is_empty() {
            l.kind = "System".into();
        }
    }
}

fn app_jwt(client_id: &str, pem: &str) -> Result<String> {
    #[derive(serde::Serialize)]
    struct Claims<'a> {
        iat: i64,
        exp: i64,
        iss: &'a str,
    }
    let iat = Utc::now().timestamp() - 60;
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|e| Error::Config(format!("invalid GitHub App private key: {e}")))?;
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &Claims { iat, exp: iat + 9 * 60, iss: client_id },
        &key,
    )
    .map_err(|e| Error::Config(format!("signing GitHub App JWT: {e}")))
}

/// Reads `exp` from a JWT without verifying the signature.
pub(crate) fn jwt_expiry(token: &str) -> Option<DateTime<Utc>> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    DateTime::from_timestamp(v.get("exp")?.as_i64()?, 0)
}

/// Drops the query string so tokens never land in logs.
pub(crate) fn redact(url: &url::Url) -> String {
    let mut u = url.clone();
    u.set_query(None);
    u.to_string()
}

pub(crate) async fn read_body(resp: reqwest::Response) -> (reqwest::StatusCode, String, Vec<u8>) {
    let status = resp.status();
    let activity = resp
        .headers()
        .get("x-github-request-id")
        .or_else(|| resp.headers().get("activityid"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").map(<[u8]>::to_vec).unwrap_or(bytes);
    (status, activity, bytes)
}

pub(crate) fn http_error(status: reqwest::StatusCode, activity: String, body: &[u8], url: &str) -> Error {
    let message = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(&body[..body.len().min(512)]).into_owned());
    Error::Http { status: status.as_u16(), url: url.to_string(), activity_id: activity, message }
}

pub(crate) async fn expect_json<T: DeserializeOwned>(resp: reqwest::Response, expect: u16) -> Result<T> {
    let url = redact(resp.url());
    let (status, activity, body) = read_body(resp).await;
    if status.as_u16() != expect {
        return Err(http_error(status, activity, &body, &url));
    }
    serde_json::from_slice(&body).map_err(|e| Error::Protocol(format!("decoding response from {url}: {e}")))
}

async fn expect_json_range<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
    let url = redact(resp.url());
    let (status, activity, body) = read_body(resp).await;
    if !status.is_success() {
        return Err(http_error(status, activity, &body, &url));
    }
    serde_json::from_slice(&body).map_err(|e| Error::Protocol(format!("decoding response from {url}: {e}")))
}

pub(crate) async fn expect_status(resp: reqwest::Response, expect: u16) -> Result<()> {
    let url = redact(resp.url());
    let (status, activity, body) = read_body(resp).await;
    if status.as_u16() != expect {
        return Err(http_error(status, activity, &body, &url));
    }
    Ok(())
}
