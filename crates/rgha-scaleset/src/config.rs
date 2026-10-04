//! Parses the "GitHub config URL" (org, repo, or enterprise) the same way
//! actions/scaleset does, and derives REST API URLs from it.

use url::Url;

use crate::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Enterprise { enterprise: String },
    Organization { org: String },
    Repository { owner: String, repo: String },
}

#[derive(Debug, Clone)]
pub struct GitHubConfig {
    pub config_url: Url,
    pub scope: Scope,
    pub is_hosted: bool,
}

impl GitHubConfig {
    pub fn parse(input: &str) -> Result<Self, Error> {
        let config_url = Url::parse(input.trim_matches('/'))
            .map_err(|e| Error::Config(format!("invalid GitHub config URL {input:?}: {e}")))?;
        let host = config_url.host_str().unwrap_or_default().to_ascii_lowercase();
        let is_hosted = std::env::var_os("GITHUB_ACTIONS_FORCE_GHES").is_none()
            && (host == "github.com"
                || host == "www.github.com"
                || host == "github.localhost"
                || host.ends_with(".ghe.com"));

        let parts: Vec<&str> = config_url.path().trim_matches('/').split('/').filter(|p| !p.is_empty()).collect();
        let scope = match parts.as_slice() {
            [org] => Scope::Organization { org: (*org).to_string() },
            [first, ent] if first.eq_ignore_ascii_case("enterprises") => {
                Scope::Enterprise { enterprise: (*ent).to_string() }
            }
            [owner, repo] => Scope::Repository { owner: (*owner).to_string(), repo: (*repo).to_string() },
            _ => {
                return Err(Error::Config(format!(
                    "{input:?}: config URL should point to an enterprise, org, or repository"
                )));
            }
        };
        Ok(Self { config_url, scope, is_hosted })
    }

    /// REST API URL for `path` (which must start with `/`).
    pub fn api_url(&self, path: &str) -> String {
        let scheme = self.config_url.scheme();
        let host = self.config_url.host_str().unwrap_or_default();
        let port = self.config_url.port().map(|p| format!(":{p}")).unwrap_or_default();
        if self.is_hosted {
            let api_host = if host.eq_ignore_ascii_case("www.github.com") {
                "api.github.com".to_string()
            } else {
                format!("api.{host}")
            };
            format!("{scheme}://{api_host}{port}{path}")
        } else {
            format!("{scheme}://{host}{port}/api/v3{path}")
        }
    }

    pub fn registration_token_path(&self) -> String {
        match &self.scope {
            Scope::Organization { org } => format!("/orgs/{org}/actions/runners/registration-token"),
            Scope::Enterprise { enterprise } => format!("/enterprises/{enterprise}/actions/runners/registration-token"),
            Scope::Repository { owner, repo } => format!("/repos/{owner}/{repo}/actions/runners/registration-token"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scopes() {
        let r = GitHubConfig::parse("https://github.com/strawgate/rgha/").unwrap();
        assert_eq!(r.scope, Scope::Repository { owner: "strawgate".into(), repo: "rgha".into() });
        assert_eq!(r.api_url("/x"), "https://api.github.com/x");
        assert_eq!(r.registration_token_path(), "/repos/strawgate/rgha/actions/runners/registration-token");

        let o = GitHubConfig::parse("https://github.com/strawgate").unwrap();
        assert_eq!(o.scope, Scope::Organization { org: "strawgate".into() });

        let e = GitHubConfig::parse("https://github.com/enterprises/acme").unwrap();
        assert_eq!(e.scope, Scope::Enterprise { enterprise: "acme".into() });

        assert!(GitHubConfig::parse("https://github.com/").is_err());
        assert!(GitHubConfig::parse("https://github.com/a/b/c").is_err());
    }

    #[test]
    fn ghes_and_ghe_com_api_urls() {
        let ghes = GitHubConfig::parse("https://git.example.com/org").unwrap();
        assert!(!ghes.is_hosted);
        assert_eq!(ghes.api_url("/x"), "https://git.example.com/api/v3/x");

        let ghe = GitHubConfig::parse("https://acme.ghe.com/org").unwrap();
        assert_eq!(ghe.api_url("/x"), "https://api.acme.ghe.com/x");

        let www = GitHubConfig::parse("https://www.github.com/org").unwrap();
        assert_eq!(www.api_url("/x"), "https://api.github.com/x");
    }
}
