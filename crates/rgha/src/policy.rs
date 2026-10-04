//! Which jobs a runner class is allowed to take.
//!
//! The `runs-on` label is chosen by whoever edits the workflow file — and in a
//! fork PR that is the PR author. So a label can never grant trust. Instead
//! every class decides, from fields GitHub fills in server-side (event name,
//! repository, and the workflow ref the job came from), whether to acquire a
//! job. A job that is not acquired is never assigned to this class's runners.

use rgha_scaleset::JobMessageBase;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Trust {
    /// Runs anything, including fork PRs. Use small, network-restricted,
    /// short-lived sandboxes for these classes.
    #[default]
    Untrusted,
    /// Only code that is already on a branch or tag of the repo: never PR
    /// heads. Suitable for classes with bigger resources or open egress.
    Trusted,
}

/// Events whose code comes from a branch/tag of the base repository.
pub const TRUSTED_EVENTS: &[&str] =
    &["push", "schedule", "workflow_dispatch", "merge_group", "release", "repository_dispatch", "workflow_run"];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    pub trust: Trust,
    /// Overrides the default event allowlist (all events for untrusted,
    /// [`TRUSTED_EVENTS`] for trusted).
    pub allowed_events: Option<Vec<String>>,
    #[serde(default)]
    pub denied_events: Vec<String>,
    /// `owner/repo` globs (`*` matches any run of characters). Empty = any.
    #[serde(default)]
    pub allowed_repos: Vec<String>,
    /// Globs over `job_workflow_ref`, e.g. `strawgate/*/.github/workflows/*@refs/heads/main`.
    #[serde(default)]
    pub allowed_workflow_refs: Vec<String>,
    /// Accept pull requests whose head is in a fork. Default: true for
    /// untrusted classes, false for trusted ones. When false, rgha looks the
    /// run up (Actions: read) and rejects forks; if the lookup fails, the job
    /// is rejected (fail closed).
    pub allow_fork_prs: Option<bool>,
    /// Trusted classes only: also accept pull requests from branches of the
    /// same repository (their authors already have write access). Fork PRs
    /// are still rejected.
    #[serde(default)]
    pub allow_same_repo_prs: bool,
}

/// What rgha learned about a job beyond the scale set message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobContext {
    /// `Some(true)` if the PR head is in a fork; `None` if unknown/not looked up.
    pub fork: Option<bool>,
}

pub fn is_pull_request_event(event: &str) -> bool {
    event.starts_with("pull_request")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Acquire,
    Reject(String),
}

impl Policy {
    fn fork_prs_allowed(&self) -> bool {
        self.allow_fork_prs.unwrap_or(self.trust == Trust::Untrusted)
    }

    /// True if the decision for this job depends on whether its PR is from a
    /// fork, i.e. only then is the REST lookup worth making.
    pub fn needs_fork_info(&self, job: &JobMessageBase) -> bool {
        let accept = |fork| self.evaluate(job, JobContext { fork: Some(fork) }) == Decision::Acquire;
        is_pull_request_event(&job.event_name) && accept(false) != accept(true)
    }

    pub fn evaluate(&self, job: &JobMessageBase, ctx: JobContext) -> Decision {
        let event = job.event_name.as_str();
        let is_pr = is_pull_request_event(event);
        let same_repo_pr = is_pr && ctx.fork == Some(false);
        if self.denied_events.iter().any(|e| e == event) {
            return Decision::Reject(format!("event {event:?} is denied"));
        }
        let allowed = match (&self.allowed_events, self.trust) {
            (Some(list), _) => list.iter().any(|e| e == event),
            (None, Trust::Untrusted) => true,
            (None, Trust::Trusted) => TRUSTED_EVENTS.contains(&event) || (self.allow_same_repo_prs && same_repo_pr),
        };
        if !allowed {
            return Decision::Reject(format!("event {event:?} is not allowed for this class"));
        }
        if self.trust == Trust::Trusted {
            // A trusted class never runs code from a PR ref, even if someone adds
            // `pull_request` to allowed_events by mistake, unless it is a
            // verified same-repo PR and allow_same_repo_prs is set.
            let git_ref = job.job_workflow_ref.rsplit_once('@').map(|(_, r)| r).unwrap_or("");
            if git_ref.starts_with("refs/pull/") && !(self.allow_same_repo_prs && same_repo_pr) {
                return Decision::Reject(format!("workflow ref {:?} is a pull request ref", job.job_workflow_ref));
            }
        }
        if is_pr && !self.fork_prs_allowed() {
            match ctx.fork {
                Some(false) => {}
                Some(true) => return Decision::Reject("pull request from a fork".into()),
                None => return Decision::Reject("could not verify the pull request is not from a fork".into()),
            }
        }
        let repo = format!("{}/{}", job.owner_name, job.repository_name);
        if !self.allowed_repos.is_empty() && !self.allowed_repos.iter().any(|p| glob_match(p, &repo)) {
            return Decision::Reject(format!("repository {repo:?} is not allowed"));
        }
        if !self.allowed_workflow_refs.is_empty()
            && !self.allowed_workflow_refs.iter().any(|p| glob_match(p, &job.job_workflow_ref))
        {
            return Decision::Reject(format!("workflow ref {:?} is not allowed", job.job_workflow_ref));
        }
        Decision::Acquire
    }
}

/// Case-sensitive glob where `*` matches any (possibly empty) run of characters.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] != '*' && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn job(event: &str, wf_ref: &str) -> JobMessageBase {
        JobMessageBase {
            event_name: event.into(),
            owner_name: "strawgate".into(),
            repository_name: "rgha".into(),
            job_workflow_ref: wf_ref.into(),
            ..Default::default()
        }
    }

    impl Policy {
        fn evaluate_plain(&self, job: &JobMessageBase) -> Decision {
            self.evaluate(job, JobContext::default())
        }
    }

    const MAIN: &str = "strawgate/rgha/.github/workflows/ci.yml@refs/heads/main";
    const PR: &str = "strawgate/rgha/.github/workflows/ci.yml@refs/pull/12/merge";

    #[test]
    fn untrusted_accepts_fork_prs() {
        assert_eq!(Policy::default().evaluate_plain(&job("pull_request", PR)), Decision::Acquire);
    }

    #[test]
    fn trusted_rejects_pr_events_and_pr_refs() {
        let p = Policy { trust: Trust::Trusted, ..Default::default() };
        assert_eq!(p.evaluate_plain(&job("push", MAIN)), Decision::Acquire);
        assert!(matches!(p.evaluate_plain(&job("pull_request", PR)), Decision::Reject(_)));
        assert!(matches!(p.evaluate_plain(&job("pull_request_target", MAIN)), Decision::Reject(_)));
        // Even an explicit allowlist cannot let PR refs into a trusted class.
        let p =
            Policy { trust: Trust::Trusted, allowed_events: Some(vec!["pull_request".into()]), ..Default::default() };
        assert!(matches!(p.evaluate_plain(&job("pull_request", PR)), Decision::Reject(_)));
    }

    #[test]
    fn repo_and_ref_globs() {
        let p = Policy {
            allowed_repos: vec!["strawgate/*".into()],
            allowed_workflow_refs: vec!["*@refs/heads/main".into()],
            ..Default::default()
        };
        assert_eq!(p.evaluate_plain(&job("push", MAIN)), Decision::Acquire);
        assert!(matches!(p.evaluate_plain(&job("push", PR)), Decision::Reject(_)));
        let mut other = job("push", MAIN);
        other.owner_name = "evil".into();
        assert!(matches!(p.evaluate_plain(&other), Decision::Reject(_)));
    }

    fn ctx(fork: Option<bool>) -> JobContext {
        JobContext { fork }
    }

    #[test]
    fn fork_decision_table() {
        let untrusted = Policy::default();
        let no_forks = Policy { allow_fork_prs: Some(false), ..Default::default() };
        let trusted = Policy { trust: Trust::Trusted, ..Default::default() };
        let trusted_same_repo = Policy { trust: Trust::Trusted, allow_same_repo_prs: true, ..Default::default() };
        let pr = job("pull_request", PR);
        let push = job("push", MAIN);
        let ok = |d: Decision| d == Decision::Acquire;

        // untrusted: anything, no lookup needed
        assert!(!untrusted.needs_fork_info(&pr));
        assert!(ok(untrusted.evaluate(&pr, ctx(Some(true)))));
        // untrusted without forks: same-repo only, unknown fails closed
        assert!(no_forks.needs_fork_info(&pr));
        assert!(!no_forks.needs_fork_info(&push));
        assert!(ok(no_forks.evaluate(&pr, ctx(Some(false)))));
        assert!(!ok(no_forks.evaluate(&pr, ctx(Some(true)))));
        assert!(!ok(no_forks.evaluate(&pr, ctx(None))));
        assert!(ok(no_forks.evaluate(&push, ctx(None))));
        // trusted: never PRs by default, no lookup needed
        assert!(!trusted.needs_fork_info(&pr));
        assert!(!ok(trusted.evaluate(&pr, ctx(Some(false)))));
        // trusted + allow_same_repo_prs: verified same-repo PRs only
        assert!(trusted_same_repo.needs_fork_info(&pr));
        assert!(ok(trusted_same_repo.evaluate(&pr, ctx(Some(false)))));
        assert!(!ok(trusted_same_repo.evaluate(&pr, ctx(Some(true)))));
        assert!(!ok(trusted_same_repo.evaluate(&pr, ctx(None))));
        assert!(ok(trusted_same_repo.evaluate(&push, ctx(None))));
        // explicitly allowing forks on a trusted class still can't bypass the PR-ref rule
        let trusted_forks = Policy { trust: Trust::Trusted, allow_fork_prs: Some(true), ..Default::default() };
        assert!(!ok(trusted_forks.evaluate(&pr, ctx(Some(true)))));
    }

    #[test]
    fn denied_events_win() {
        let p = Policy { denied_events: vec!["pull_request_target".into()], ..Default::default() };
        assert!(matches!(p.evaluate_plain(&job("pull_request_target", MAIN)), Decision::Reject(_)));
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("*", ""));
        assert!(glob_match("a*c", "abbbc"));
        assert!(glob_match("strawgate/*", "strawgate/rgha"));
        assert!(!glob_match("strawgate/*", "strawgatex/rgha"));
        assert!(!glob_match("abc", "abcd"));
        assert!(glob_match("*@refs/heads/*", "o/r/.github/workflows/x.yml@refs/heads/main"));
    }

    proptest! {
        #[test]
        fn glob_literal_matches_only_itself(s in "[a-z/@.]{0,12}", t in "[a-z/@.]{0,12}") {
            prop_assert_eq!(glob_match(&s, &t), s == t);
        }

        #[test]
        fn glob_prefix_star_matches_any_suffix(prefix in "[a-z]{0,6}", suffix in "[a-z*]{0,6}") {
            let pattern = format!("{prefix}*");
            let text = format!("{prefix}{suffix}");
            prop_assert!(glob_match(&pattern, &text));
        }
    }
}
