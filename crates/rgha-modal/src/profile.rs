//! Modal credentials, resolved the same way as the official SDKs:
//! `MODAL_*` environment variables override the active profile in
//! `~/.modal.toml` (or `$MODAL_CONFIG_PATH`).

use std::collections::HashMap;

use serde::Deserialize;

use crate::{Error, Result};

const DEFAULT_SERVER_URL: &str = "https://api.modal.com:443";

#[derive(Clone, Default)]
pub struct Profile {
    pub server_url: String,
    pub token_id: String,
    pub token_secret: String,
    pub environment: String,
    pub image_builder_version: Option<String>,
}

impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile")
            .field("server_url", &self.server_url)
            .field("token_id", &self.token_id)
            .field("token_secret", &"<redacted>")
            .field("environment", &self.environment)
            .finish()
    }
}

#[derive(Deserialize, Default)]
struct RawProfile {
    server_url: Option<String>,
    token_id: Option<String>,
    token_secret: Option<String>,
    environment: Option<String>,
    image_builder_version: Option<String>,
    #[serde(default)]
    active: bool,
}

impl Profile {
    /// Loads `name` (or the active profile) with env overrides applied.
    pub fn load(name: Option<&str>) -> Result<Self> {
        let path = std::env::var("MODAL_CONFIG_PATH")
            .ok()
            .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.modal.toml")));
        let file = match path {
            Some(p) => match std::fs::read_to_string(&p) {
                Ok(s) => Some(s),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(Error::Config(format!("reading {p}: {e}"))),
            },
            None => None,
        };
        Self::resolve(name, file.as_deref(), |k| std::env::var(k).ok())
    }

    fn resolve(name: Option<&str>, file: Option<&str>, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let profiles: HashMap<String, RawProfile> = match file {
            Some(s) => toml::from_str(s).map_err(|e| Error::Config(format!("parsing .modal.toml: {e}")))?,
            None => HashMap::new(),
        };
        let raw = match name {
            Some(n) => profiles.get(n),
            None => profiles.values().find(|p| p.active),
        };
        let pick =
            |key: &str, from_file: Option<&String>| env(key).filter(|v| !v.is_empty()).or_else(|| from_file.cloned());

        let profile = Profile {
            server_url: pick("MODAL_SERVER_URL", raw.and_then(|r| r.server_url.as_ref()))
                .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string()),
            token_id: pick("MODAL_TOKEN_ID", raw.and_then(|r| r.token_id.as_ref())).unwrap_or_default(),
            token_secret: pick("MODAL_TOKEN_SECRET", raw.and_then(|r| r.token_secret.as_ref())).unwrap_or_default(),
            environment: pick("MODAL_ENVIRONMENT", raw.and_then(|r| r.environment.as_ref())).unwrap_or_default(),
            image_builder_version: pick(
                "MODAL_IMAGE_BUILDER_VERSION",
                raw.and_then(|r| r.image_builder_version.as_ref()),
            ),
        };
        if profile.token_id.is_empty() || profile.token_secret.is_empty() {
            return Err(Error::Config(
                "missing Modal credentials: set MODAL_TOKEN_ID/MODAL_TOKEN_SECRET or run `modal token new`".into(),
            ));
        }
        Ok(profile)
    }

    /// Returns `(host, https-endpoint)` for the gRPC channel.
    pub(crate) fn endpoint(&self) -> Result<(String, String)> {
        let rest = self
            .server_url
            .strip_prefix("https://")
            .ok_or_else(|| Error::Config(format!("server_url must be https: {}", self.server_url)))?;
        let host = rest.split([':', '/']).next().unwrap_or_default().to_string();
        Ok((host, self.server_url.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"
[a]
token_id = "ak-a"
token_secret = "as-a"

[b]
token_id = "ak-b"
token_secret = "as-b"
environment = "dev"
active = true
"#;

    #[test]
    fn picks_active_profile() {
        let p = Profile::resolve(None, Some(FILE), |_| None).unwrap();
        assert_eq!(p.token_id, "ak-b");
        assert_eq!(p.environment, "dev");
        assert_eq!(p.server_url, DEFAULT_SERVER_URL);
    }

    #[test]
    fn env_overrides_file() {
        let p = Profile::resolve(Some("a"), Some(FILE), |k| (k == "MODAL_TOKEN_ID").then(|| "env-id".into())).unwrap();
        assert_eq!(p.token_id, "env-id");
        assert_eq!(p.token_secret, "as-a");
    }

    #[test]
    fn missing_credentials_is_an_error() {
        assert!(Profile::resolve(None, None, |_| None).is_err());
    }

    #[test]
    fn debug_redacts_secret() {
        let p = Profile::resolve(None, Some(FILE), |_| None).unwrap();
        assert!(!format!("{p:?}").contains("as-b"));
    }

    #[test]
    fn endpoint_host() {
        let p = Profile::resolve(None, Some(FILE), |_| None).unwrap();
        assert_eq!(p.endpoint().unwrap().0, "api.modal.com");
    }
}
