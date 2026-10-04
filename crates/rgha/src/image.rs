//! Runner image preloading: Dockerfile layers that bake actions and
//! toolchains into the image so jobs skip downloads at run time.
//!
//! - Actions go into the runner's action archive cache
//!   (`ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE`), unpacked in the
//!   `{owner}_{repo}/{sha}/<archive dir>` layout the runner symlinks from when
//!   `ACTIONS_RUNNER_SYMLINK_CACHED_ACTIONS=true` (see actions/runner
//!   `ActionManager.cs`). A tag that moves after the image was built is just
//!   a cache miss: the runner falls back to downloading.
//! - Toolchains go into the tool cache (`RUNNER_TOOL_CACHE`) in the
//!   `<tool>/<version>/x64` + `x64.complete` layout that `setup-node` and
//!   `setup-python` check before downloading.

use serde::Deserialize;

pub const TOOL_CACHE: &str = "/opt/hostedtoolcache";
pub const ACTION_CACHE: &str = "/opt/actionarchivecache";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preload {
    /// `owner/repo@ref` (tag, branch or full commit SHA).
    #[serde(default)]
    pub actions: Vec<String>,
    /// Node versions as `setup-node` would request them, e.g. `"22"`, `"20.18"`.
    #[serde(default)]
    pub node: Vec<String>,
    /// Python versions as `setup-python` would request them, e.g. `"3.12"`.
    #[serde(default)]
    pub python: Vec<String>,
}

fn is_safe(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-/@".contains(c))
}

impl Preload {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.node.is_empty() && self.python.is_empty()
    }

    pub fn validate(&self) -> Result<(), String> {
        for a in &self.actions {
            let Some((repo, git_ref)) = a.split_once('@') else {
                return Err(format!("preload action {a:?} must look like owner/repo@ref"));
            };
            if repo.split('/').count() != 2 || !is_safe(repo) || !is_safe(git_ref) {
                return Err(format!("preload action {a:?} must look like owner/repo@ref"));
            }
        }
        for v in self.node.iter().chain(&self.python) {
            if !v.chars().all(|c| c.is_ascii_digit() || c == '.') || v.is_empty() {
                return Err(format!("preload version {v:?} must be numeric, like 22 or 3.12"));
            }
        }
        Ok(())
    }

    /// Dockerfile instructions, to be layered on the runner image. Leaves the
    /// image's user as `runner`, matching the official image.
    pub fn dockerfile_commands(&self) -> Vec<String> {
        if self.is_empty() {
            return vec![];
        }
        let mut cmds = vec![
            "USER root".to_string(),
            format!(
                "ENV RUNNER_TOOL_CACHE={TOOL_CACHE} AGENT_TOOLSDIRECTORY={TOOL_CACHE} \
                 ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE={ACTION_CACHE} ACTIONS_RUNNER_SYMLINK_CACHED_ACTIONS=true"
            ),
            format!("RUN mkdir -p {TOOL_CACHE} {ACTION_CACHE}"),
        ];
        for a in &self.actions {
            let (repo, git_ref) = a.split_once('@').expect("validated");
            let dir = repo.replace('/', "_");
            // Prefer the peeled commit of an annotated tag; accept full SHAs as-is.
            cmds.push(format!(
                "RUN set -eu; repo='{repo}'; ref='{git_ref}'; \
                 if echo \"$ref\" | grep -Eq '^[0-9a-f]{{40}}$'; then sha=\"$ref\"; else \
                 out=$(git ls-remote \"https://github.com/$repo\" \"refs/tags/$ref\" \"refs/tags/$ref^{{}}\" \"refs/heads/$ref\"); \
                 sha=$(echo \"$out\" | awk '$2 ~ /\\^\\{{\\}}$/ {{print $1; exit}}'); \
                 [ -n \"$sha\" ] || sha=$(echo \"$out\" | head -1 | cut -f1); fi; \
                 [ -n \"$sha\" ] || {{ echo \"cannot resolve $repo@$ref\" >&2; exit 1; }}; \
                 mkdir -p {ACTION_CACHE}/{dir}/$sha; \
                 curl -fsSL \"https://codeload.github.com/$repo/tar.gz/$sha\" | tar xz -C {ACTION_CACHE}/{dir}/$sha; \
                 echo \"preloaded $repo@$ref -> $sha\""
            ));
        }
        for v in &self.node {
            cmds.push(format!(
                "RUN set -eu; want='{v}'; \
                 ver=$(curl -fsSL https://nodejs.org/dist/index.json | jq -r --arg v \"$want\" \
                 '[.[] | select(.version == (\"v\" + $v) or (.version | startswith(\"v\" + $v + \".\")))][0].version' | sed 's/^v//'); \
                 [ -n \"$ver\" ] && [ \"$ver\" != null ] || {{ echo \"no node $want\" >&2; exit 1; }}; \
                 d={TOOL_CACHE}/node/$ver/x64; mkdir -p $d; \
                 curl -fsSL \"https://nodejs.org/dist/v$ver/node-v$ver-linux-x64.tar.gz\" | tar xz --strip-components=1 -C $d; \
                 touch {TOOL_CACHE}/node/$ver/x64.complete; echo \"preloaded node $ver\""
            ));
        }
        for v in &self.python {
            cmds.push(format!(
                "RUN set -eu; want='{v}'; \
                 url=$(curl -fsSL https://raw.githubusercontent.com/actions/python-versions/main/versions-manifest.json | jq -r --arg v \"$want\" \
                 '[.[] | select(.stable and (.version == $v or (.version | startswith($v + \".\"))))][0].files[] \
                 | select(.platform == \"linux\" and .platform_version == \"24.04\" and .arch == \"x64\") | .download_url' | head -1); \
                 [ -n \"$url\" ] || {{ echo \"no python $want\" >&2; exit 1; }}; \
                 tmp=$(mktemp -d); curl -fsSL \"$url\" | tar xz -C $tmp; (cd $tmp && bash ./setup.sh); rm -rf $tmp; \
                 echo \"preloaded python from $url\""
            ));
        }
        cmds.push(format!("RUN chown -R runner:docker {TOOL_CACHE} {ACTION_CACHE}"));
        cmds.push("USER runner".to_string());
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_preload_adds_nothing() {
        assert!(Preload::default().dockerfile_commands().is_empty());
    }

    #[test]
    fn commands_cover_each_item_and_restore_user() {
        let p = Preload {
            actions: vec![
                "actions/checkout@v5".into(),
                "actions/setup-node@0123456789abcdef0123456789abcdef01234567".into(),
            ],
            node: vec!["22".into()],
            python: vec!["3.12".into()],
        };
        p.validate().unwrap();
        let cmds = p.dockerfile_commands();
        assert_eq!(cmds.first().map(String::as_str), Some("USER root"));
        assert_eq!(cmds.last().map(String::as_str), Some("USER runner"));
        assert!(cmds.iter().any(|c| c.contains("ACTIONS_RUNNER_SYMLINK_CACHED_ACTIONS=true")));
        assert!(cmds.iter().any(|c| c.contains("repo='actions/checkout'") && c.contains("actions_checkout")));
        assert!(cmds.iter().any(|c| c.contains("node/$ver/x64.complete")));
        assert!(cmds.iter().any(|c| c.contains("python-versions") && c.contains("24.04")));
    }

    #[test]
    fn rejects_injection_and_malformed_specs() {
        for bad in ["actions/checkout", "actions/checkout@v5;rm -rf /", "a/b/c@v1", "x@'"] {
            assert!(Preload { actions: vec![bad.into()], ..Default::default() }.validate().is_err(), "{bad}");
        }
        assert!(Preload { node: vec!["22; curl evil".into()], ..Default::default() }.validate().is_err());
    }
}
