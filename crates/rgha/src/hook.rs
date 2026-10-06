//! In-sandbox defense in depth: a GitHub runner job-started hook that
//! re-checks the class policy before any step runs.
//!
//! GitHub assigns jobs to a scale set before rgha sees them, so a warm runner
//! can pick up a job that the policy rejects before rgha cancels its run. The
//! hook closes that gap: the runner runs it ahead of the job's steps (via
//! `ACTIONS_RUNNER_HOOK_JOB_STARTED`), and a non-zero exit fails the job. It
//! mirrors [`Policy::evaluate`] using the job's own context (event, workflow
//! ref, repository, actors, and the PR head from the event payload), with the
//! runner's bundled Node.js.

use serde_json::json;

use crate::policy::{Policy, TRUSTED_EVENTS, Trust};

/// The decision logic, kept line-for-line parallel to `Policy::evaluate`.
const DECIDE_FN: &str = r#"

function glob(pattern, text) {
  const p = [...pattern], t = [...text];
  let pi = 0, ti = 0, star = -1, mark = 0;
  while (ti < t.length) {
    if (pi < p.length && p[pi] !== '*' && p[pi] === t[ti]) { pi++; ti++; }
    else if (pi < p.length && p[pi] === '*') { star = pi; mark = ti; pi++; }
    else if (star >= 0) { pi = star + 1; mark++; ti = mark; }
    else return false;
  }
  return p.slice(pi).every((c) => c === '*');
}

function decide(e) {
  const isPr = e.event.startsWith('pull_request');
  const sameRepoPr = isPr && e.fork === false;
  if (P.denied_events.includes(e.event)) return `event "${e.event}" is denied`;
  const allowed = P.allowed_events !== null ? P.allowed_events.includes(e.event)
    : !P.trusted ? true
    : P.trusted_events.includes(e.event) || (P.allow_same_repo_prs && sameRepoPr);
  if (!allowed) return `event "${e.event}" is not allowed for this class`;
  if (P.trusted) {
    const at = e.ref.lastIndexOf('@');
    const gitRef = at >= 0 ? e.ref.slice(at + 1) : '';
    if (gitRef.startsWith('refs/pull/') && !(P.allow_same_repo_prs && sameRepoPr)) return `workflow ref "${e.ref}" is a pull request ref`;
  }
  if (isPr && !P.fork_prs_allowed) {
    if (e.fork === true) return 'pull request from a fork';
    if (e.fork === null) return 'could not verify the pull request is not from a fork';
  }
  if (P.allowed_actors.length || P.denied_actors.length) {
    const listed = (list, login) => list.some((l) => l.toLowerCase() === login.toLowerCase());
    for (const [who, login] of [['actor', e.actor], ['triggering actor', e.triggering]]) {
      if (!login) return 'could not verify who triggered the run';
      if (listed(P.denied_actors, login)) return `${who} "${login}" is denied`;
      if (P.allowed_actors.length && !listed(P.allowed_actors, login)) return `${who} "${login}" is not in allowed_actors`;
    }
  }
  if (P.allowed_repos.length && !P.allowed_repos.some((p) => glob(p, e.repo))) return `repository "${e.repo}" is not allowed`;
  if (P.allowed_workflow_refs.length && !P.allowed_workflow_refs.some((p) => glob(p, e.ref))) return `workflow ref "${e.ref}" is not allowed`;
  return null;
}
"#;

/// Reads the job's context from the runner environment and fails the job on rejection.
const MAIN_JS: &str = r#"
const fs = require('fs');
const env = process.env;
let fork = null;
if ((env.GITHUB_EVENT_NAME || '').startsWith('pull_request')) {
  try {
    const head = JSON.parse(fs.readFileSync(env.GITHUB_EVENT_PATH, 'utf8')).pull_request?.head?.repo?.full_name;
    if (head) fork = head.toLowerCase() !== (env.GITHUB_REPOSITORY || '').toLowerCase();
  } catch (_) { fork = null; }
}
const why = decide({
  event: env.GITHUB_EVENT_NAME || '',
  ref: env.GITHUB_WORKFLOW_REF || '',
  repo: env.GITHUB_REPOSITORY || '',
  fork,
  actor: env.GITHUB_ACTOR || '',
  triggering: env.GITHUB_TRIGGERING_ACTOR || env.GITHUB_ACTOR || '',
});
if (why) {
  console.log(`::error::rgha: this runner's class policy rejects the job: ${why}`);
  process.exit(1);
}
console.log('rgha: class policy check passed');
"#;

/// The hook script for a class policy.
pub fn job_started_script(policy: &Policy) -> String {
    let p = json!({
        "trusted": policy.trust == Trust::Trusted,
        "denied_events": policy.denied_events,
        "allowed_events": policy.allowed_events,
        "trusted_events": TRUSTED_EVENTS,
        "allow_same_repo_prs": policy.allow_same_repo_prs,
        "fork_prs_allowed": policy.fork_prs_allowed(),
        "allowed_actors": policy.allowed_actors,
        "denied_actors": policy.denied_actors,
        "allowed_repos": policy.allowed_repos,
        "allowed_workflow_refs": policy.allowed_workflow_refs,
    });
    format!(
        "#!/usr/bin/env bash\n\
         # rgha job-started hook: re-checks the class policy before any step runs.\n\
         node=\"${{RGHA_NODE:-$(ls -d /home/runner/externals/node*/bin/node 2>/dev/null | sort -V | tail -1)}}\"\n\
         if [ ! -x \"$node\" ]; then echo '::error::rgha: no Node.js runtime for the policy hook'; exit 1; fi\n\
         exec \"$node\" - <<'RGHA_JS'\nconst P = {p};\n{DECIDE_FN}\n{MAIN_JS}\nRGHA_JS\n"
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::Command;

    use rgha_scaleset::JobMessageBase;

    use super::*;
    use crate::policy::{Actors, Decision, JobContext};

    fn node() -> Option<String> {
        let out = Command::new("which").arg("node").output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Runs the hook for one job; true if it lets the job run.
    fn hook_accepts(script: &str, node: &str, job: &JobMessageBase, ctx: &JobContext) -> bool {
        let dir = std::env::temp_dir().join(format!("rgha-hook-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("hook.sh");
        std::fs::write(&script_path, script).unwrap();
        let event_path = dir.join("event.json");
        let repo = format!("{}/{}", job.owner_name, job.repository_name);
        let head = match ctx.fork {
            Some(true) => json!({"pull_request": {"head": {"repo": {"full_name": "someone/fork"}}}}),
            Some(false) => json!({"pull_request": {"head": {"repo": {"full_name": repo}}}}),
            None => json!({"pull_request": {"head": {"repo": null}}}),
        };
        std::fs::File::create(&event_path).unwrap().write_all(head.to_string().as_bytes()).unwrap();
        let (actor, triggering) =
            ctx.actors.as_ref().map(|a| (a.actor.clone(), a.triggering.clone())).unwrap_or_default();
        let status = Command::new("bash")
            .arg(&script_path)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("RGHA_NODE", node)
            .env("GITHUB_EVENT_NAME", &job.event_name)
            .env("GITHUB_WORKFLOW_REF", &job.job_workflow_ref)
            .env("GITHUB_REPOSITORY", &repo)
            .env("GITHUB_EVENT_PATH", &event_path)
            .env("GITHUB_ACTOR", actor)
            .env("GITHUB_TRIGGERING_ACTOR", triggering)
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
        status.status.success()
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::SeqCst)
    }

    fn policy_json(policy: &Policy) -> String {
        let script = job_started_script(policy);
        let start = script.find("const P = ").unwrap() + "const P = ".len();
        script[start..script[start..].find('\n').unwrap() + start].trim_end_matches(';').to_string()
    }

    fn policies() -> Vec<Policy> {
        vec![
            Policy::default(),
            Policy { trust: Trust::Trusted, ..Default::default() },
            Policy { trust: Trust::Trusted, allow_same_repo_prs: true, ..Default::default() },
            Policy {
                trust: Trust::Trusted,
                allowed_events: Some(vec!["push".into(), "pull_request".into(), "issue_comment".into()]),
                allow_same_repo_prs: true,
                allowed_repos: vec!["o/r".into()],
                ..Default::default()
            },
            Policy { allow_fork_prs: Some(false), allowed_actors: vec!["Alice".into()], ..Default::default() },
            Policy {
                denied_actors: vec!["mallory".into()],
                denied_events: vec!["schedule".into()],
                ..Default::default()
            },
            Policy {
                allowed_repos: vec!["o/*".into()],
                allowed_workflow_refs: vec!["*@refs/heads/main".into()],
                ..Default::default()
            },
        ]
    }

    /// The hook's decision function and `Policy::evaluate` agree on a matrix
    /// of policies × events × refs × repos × fork status × actors.
    #[test]
    fn decision_matches_policy_evaluate() {
        let Some(node) = node() else {
            eprintln!("node not found; skipping hook parity test");
            return;
        };
        let events = ["push", "pull_request", "pull_request_target", "issue_comment", "schedule", "workflow_dispatch"];
        let refs = ["o/r/.github/workflows/ci.yml@refs/heads/main", "o/r/.github/workflows/ci.yml@refs/pull/7/merge"];
        let repos = [("o", "r"), ("evil", "r")];
        let forks = [Some(false), Some(true), None];
        let actors = [None, Some(("alice", "alice")), Some(("alice", "mallory")), Some(("bob", "bob"))];
        let mut checked = 0;
        for policy in policies() {
            let mut cases = vec![];
            let mut want = vec![];
            for event in events {
                for git_ref in refs {
                    for (owner, name) in repos {
                        for fork in forks {
                            let fork = if event.starts_with("pull_request") { fork } else { None };
                            for actor in actors {
                                let job = JobMessageBase {
                                    event_name: event.into(),
                                    job_workflow_ref: git_ref.into(),
                                    owner_name: owner.into(),
                                    repository_name: name.into(),
                                    ..Default::default()
                                };
                                let ctx = JobContext {
                                    fork,
                                    actors: actor.map(|(a, t)| Actors { actor: a.into(), triggering: t.into() }),
                                };
                                want.push(policy.evaluate(&job, ctx.clone()) == Decision::Acquire);
                                let (a, t) = actor.unwrap_or(("", ""));
                                cases.push(json!({"event": event, "ref": git_ref, "repo": format!("{owner}/{name}"),
                                                  "fork": fork, "actor": a, "triggering": t}));
                            }
                        }
                    }
                }
            }
            let js = format!(
                "const P = {};\n{DECIDE_FN}\nconsole.log(JSON.stringify({}.map((c) => decide(c) === null)));",
                policy_json(&policy),
                serde_json::Value::from(cases)
            );
            let out = Command::new(&node).arg("-e").arg(js).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            let got: Vec<bool> = serde_json::from_slice(&out.stdout).unwrap();
            for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                assert_eq!(g, w, "policy {policy:?} case {i}");
            }
            checked += want.len();
        }
        assert!(checked > 1000, "matrix too small: {checked}");
    }

    /// End to end through the real script: environment, event payload and exit code.
    #[test]
    fn hook_script_reads_the_job_context() {
        let Some(node) = node() else { return };
        let trusted = Policy { trust: Trust::Trusted, allow_same_repo_prs: true, ..Default::default() };
        let pr = |owner: &str| JobMessageBase {
            event_name: "pull_request".into(),
            job_workflow_ref: "o/r/.github/workflows/ci.yml@refs/pull/7/merge".into(),
            owner_name: owner.into(),
            repository_name: "r".into(),
            ..Default::default()
        };
        let same = JobContext { fork: Some(false), actors: None };
        let fork = JobContext { fork: Some(true), actors: None };
        let unknown = JobContext { fork: None, actors: None };
        let script = job_started_script(&trusted);
        assert!(
            hook_accepts(&script, &node, &pr("o"), &same),
            "same-repo PR on a trusted class with allow_same_repo_prs"
        );
        assert!(!hook_accepts(&script, &node, &pr("o"), &fork), "fork PR rejected (head repo from the event payload)");
        assert!(!hook_accepts(&script, &node, &pr("o"), &unknown), "missing head repo fails closed");
        let actors = Policy { allowed_actors: vec!["alice".into()], ..Default::default() };
        let ctx = |a: &str, t: &str| JobContext {
            fork: Some(false),
            actors: Some(Actors { actor: a.into(), triggering: t.into() }),
        };
        let script = job_started_script(&actors);
        assert!(hook_accepts(&script, &node, &pr("o"), &ctx("Alice", "alice")));
        assert!(!hook_accepts(&script, &node, &pr("o"), &ctx("alice", "bob")), "re-run by someone else");
    }
}
