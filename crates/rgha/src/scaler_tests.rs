//! Integration tests for `ClassScaler`: the real scaler, driven by
//! synthetic scale set messages, against a mock GitHub/Actions service
//! (wiremock) and an in-memory fake backend.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use rgha_scaleset::{
    Client, ClientOptions, Credentials, JobAssigned, JobCompleted, JobMessageBase, JobStarted, MessageSession,
    ScaleSetMessage, Scaler, Statistics,
};
use serde_json::json;
use tokio::sync::watch;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::ClassScaler;
use crate::backend::{Backend, Instance, RunnerSpec};
use crate::config::ClassConfig;
use crate::cost::Pricing;

// ---------------------------------------------------------------- fake backend

#[derive(Default)]
struct FakeInstance {
    runner_name: String,
    class: String,
    running: bool,
    exit: Option<watch::Sender<Option<i32>>>,
}

#[derive(Default)]
struct FakeBackend {
    instances: Mutex<HashMap<String, FakeInstance>>,
    next: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
    fail_starts: std::sync::atomic::AtomicBool,
}

impl FakeBackend {
    fn running(&self) -> Vec<String> {
        let m = self.instances.lock().unwrap();
        let mut v: Vec<String> = m.values().filter(|i| i.running).map(|i| i.runner_name.clone()).collect();
        v.sort();
        v
    }

    fn id_of(&self, runner: &str) -> String {
        self.instances.lock().unwrap().iter().find(|(_, i)| i.runner_name == runner).map(|(id, _)| id.clone()).unwrap()
    }

    /// Simulates the runner process exiting (as after its one job).
    fn exit(&self, runner: &str, code: i32) {
        let id = self.id_of(runner);
        let m = self.instances.lock().unwrap();
        m[&id].exit.as_ref().unwrap().send_replace(Some(code));
    }

    /// Adds an instance this scaler never started (left by a previous process).
    fn add_untracked(&self, runner: &str, class: &str) -> String {
        let id = format!("orphan-{runner}");
        let (tx, _) = watch::channel(None);
        self.instances.lock().unwrap().insert(
            id.clone(),
            FakeInstance { runner_name: runner.into(), class: class.into(), running: true, exit: Some(tx) },
        );
        id
    }
}

#[async_trait]
impl Backend for FakeBackend {
    fn kind(&self) -> &'static str {
        "fake"
    }
    async fn prepare(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn start(&self, spec: &RunnerSpec) -> anyhow::Result<String> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if self.fail_starts.load(Ordering::SeqCst) {
            anyhow::bail!("backend unavailable");
        }
        let id = format!("i-{}", self.next.fetch_add(1, Ordering::SeqCst));
        let (tx, _) = watch::channel(None);
        self.instances.lock().unwrap().insert(
            id.clone(),
            FakeInstance { runner_name: spec.name.clone(), class: spec.class.clone(), running: true, exit: Some(tx) },
        );
        Ok(id)
    }
    async fn stop(&self, id: &str) -> anyhow::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if let Some(i) = self.instances.lock().unwrap().get_mut(id) {
            i.running = false;
            i.exit.as_ref().unwrap().send_replace(Some(137));
        }
        Ok(())
    }
    async fn wait(&self, id: &str) -> anyhow::Result<Option<i32>> {
        let mut rx = self.instances.lock().unwrap()[id].exit.as_ref().unwrap().subscribe();
        loop {
            if let Some(code) = *rx.borrow() {
                if let Some(i) = self.instances.lock().unwrap().get_mut(id) {
                    i.running = false;
                }
                return Ok(Some(code));
            }
            rx.changed().await?;
        }
    }
    async fn list(&self, class: &str) -> anyhow::Result<Vec<Instance>> {
        Ok(self
            .instances
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, i)| i.running && i.class == class)
            .map(|(id, i)| Instance { id: id.clone(), runner_name: i.runner_name.clone() })
            .collect())
    }
}

// ---------------------------------------------------------------- mock GitHub

fn fake_jwt() -> String {
    let enc = |s: String| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
    let exp = chrono::Utc::now().timestamp() + 3600;
    format!("{}.{}.sig", enc(r#"{"alg":"none"}"#.into()), enc(format!(r#"{{"exp":{exp}}}"#)))
}

struct Harness {
    server: MockServer,
    client: Client,
    session: MessageSession,
    backend: Arc<FakeBackend>,
}

impl Harness {
    async fn new() -> Self {
        let server = MockServer::start().await;
        let next_runner = Arc::new(AtomicI64::new(100));
        Mock::given(method("POST"))
            .and(path("/repos/o/r/actions/runners/registration-token"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"token": "reg"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/actions/runner-registration"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"url": format!("{}/t/", server.uri()), "token": fake_jwt()})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/t/_apis/runtime/runnerscalesets/1/sessions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sessionId": "8a7c4ad0-7c8e-4a1b-9f3e-5b1b2c3d4e5f",
                "messageQueueUrl": format!("{}/q", server.uri()),
                "messageQueueAccessToken": "q", "statistics": {}
            })))
            .mount(&server)
            .await;
        let counter = next_runner.clone();
        Mock::given(method("POST"))
            .and(path("/t/_apis/runtime/runnerscalesets/1/generatejitconfig"))
            .respond_with(move |req: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                let id = counter.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({
                    "runner": {"id": id, "name": body["name"], "runnerScaleSetId": 1, "status": 0},
                    "encodedJITConfig": "jit"
                }))
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/repos/o/r/actions/runs/\d+/cancel$"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        let opts = ClientOptions { api_base_url: Some(server.uri()), max_retries: 0, ..Default::default() };
        let client = Client::new("https://github.com/o/r", Credentials::Token("t".into()), opts).unwrap();
        let session = client.message_session(1, "test").await.unwrap();
        Self { server, client, session, backend: Arc::new(FakeBackend::default()) }
    }

    /// Runner deregistration succeeds (idle) unless `busy`. Later calls take
    /// precedence over earlier ones.
    async fn remove_runner(&self, busy: bool) {
        static PRIORITY: AtomicUsize = AtomicUsize::new(100);
        let priority = PRIORITY.fetch_sub(1, Ordering::SeqCst).clamp(1, 255) as u8;
        let resp = if busy {
            ResponseTemplate::new(400).set_body_json(json!({"message": "runner is busy"}))
        } else {
            ResponseTemplate::new(204)
        };
        Mock::given(method("DELETE"))
            .and(path_regex(r"^/t/_apis/distributedtask/pools/0/agents/\d+$"))
            .respond_with(resp)
            .with_priority(priority)
            .mount(&self.server)
            .await;
    }

    fn scaler(&self, class: ClassConfig) -> ClassScaler {
        ClassScaler::new(class, 1, self.client.clone(), self.backend.clone(), Pricing::MODAL_SANDBOX)
    }

    async fn requests_to(&self, suffix: &str) -> usize {
        self.server.received_requests().await.unwrap().iter().filter(|r| r.url.path().ends_with(suffix)).count()
    }
}

fn class(toml_extra: &str) -> ClassConfig {
    toml::from_str(&format!("name = \"c\"\nbackend = \"b\"\nmax_runners = 5\nidle_ttl_secs = 0\n{toml_extra}")).unwrap()
}

fn base(job: &str, event: &str, wf_ref: &str) -> JobMessageBase {
    JobMessageBase {
        job_id: job.into(),
        owner_name: "o".into(),
        repository_name: "r".into(),
        event_name: event.into(),
        job_workflow_ref: wf_ref.into(),
        workflow_run_id: 4242,
        ..Default::default()
    }
}

fn stats(assigned: i64) -> Option<Statistics> {
    Some(Statistics { total_assigned_jobs: assigned, ..Default::default() })
}

const MAIN: &str = "o/r/.github/workflows/ci.yml@refs/heads/main";
const PR: &str = "o/r/.github/workflows/ci.yml@refs/pull/7/merge";

/// Lets spawned watcher/stop tasks run.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

// ---------------------------------------------------------------- scenarios

#[tokio::test]
async fn assigned_jobs_start_runners_and_completed_jobs_stop_them() {
    let h = Harness::new().await;
    h.remove_runner(false).await;
    let mut s = h.scaler(class(""));

    let assigned = ScaleSetMessage {
        message_id: 1,
        statistics: stats(2),
        job_assigned: vec![
            JobAssigned { base: base("j1", "push", MAIN) },
            JobAssigned { base: base("j2", "push", MAIN) },
        ],
        ..Default::default()
    };
    s.scale(&h.session, Some(&assigned)).await.unwrap();
    let running = h.backend.running();
    assert_eq!(running.len(), 2, "one runner per assigned job");

    let mut done = base("j1", "push", MAIN);
    done.runner_assign_time = Some("2026-10-04T00:00:00Z".parse().unwrap());
    done.finish_time = Some("2026-10-04T00:00:10Z".parse().unwrap());
    let msg = ScaleSetMessage {
        message_id: 2,
        statistics: stats(1),
        job_started: vec![JobStarted { runner_name: running[0].clone(), base: done.clone(), ..Default::default() }],
        job_completed: vec![JobCompleted {
            runner_name: running[0].clone(),
            result: "succeeded".into(),
            base: done,
            ..Default::default()
        }],
        ..Default::default()
    };
    s.scale(&h.session, Some(&msg)).await.unwrap();
    settle().await;
    assert_eq!(h.backend.running(), vec![running[1].clone()], "completed job's sandbox stopped");
    assert_eq!(s.ledger.jobs, 1);
    assert!((s.ledger.job_secs - 10.0).abs() < 1e-9, "duration from GitHub timestamps");
}

#[tokio::test]
async fn exit_before_job_completed_keeps_the_real_result() {
    let h = Harness::new().await;
    h.remove_runner(false).await;
    let mut s = h.scaler(class(""));
    s.scale(
        &h.session,
        Some(&ScaleSetMessage {
            message_id: 1,
            statistics: stats(1),
            job_assigned: vec![JobAssigned { base: base("j1", "push", MAIN) }],
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    let runner = h.backend.running()[0].clone();
    let started = ScaleSetMessage {
        message_id: 2,
        statistics: stats(1),
        job_started: vec![JobStarted {
            runner_name: runner.clone(),
            base: base("j1", "push", MAIN),
            ..Default::default()
        }],
        ..Default::default()
    };
    s.scale(&h.session, Some(&started)).await.unwrap();

    // The runner exits right after its job, before GitHub's JobCompleted.
    h.backend.exit(&runner, 0);
    settle().await;
    s.scale(&h.session, None).await.unwrap();
    assert!(s.pool.get(&runner).is_some_and(|r| r.exited_at.is_some()), "exit was observed");
    assert_eq!(s.ledger.jobs, 0, "not recorded yet: waiting for JobCompleted");

    let mut done = base("j1", "push", MAIN);
    done.runner_assign_time = Some("2026-10-04T00:00:00Z".parse().unwrap());
    done.finish_time = Some("2026-10-04T00:00:07Z".parse().unwrap());
    let completed = ScaleSetMessage {
        message_id: 3,
        statistics: stats(0),
        job_completed: vec![JobCompleted {
            runner_name: runner,
            result: "succeeded".into(),
            base: done,
            ..Default::default()
        }],
        ..Default::default()
    };
    s.scale(&h.session, Some(&completed)).await.unwrap();
    assert_eq!(s.ledger.jobs, 1);
    assert!((s.ledger.job_secs - 7.0).abs() < 1e-9, "recorded from JobCompleted, not the exit");
}

#[tokio::test]
async fn policy_rejected_job_is_cancelled_and_gets_no_runner() {
    let h = Harness::new().await;
    let mut s = h.scaler(class("[policy]\ntrust = \"trusted\""));
    let msg = ScaleSetMessage {
        message_id: 1,
        statistics: stats(1),
        job_assigned: vec![JobAssigned { base: base("j1", "pull_request", PR) }],
        ..Default::default()
    };
    s.scale(&h.session, Some(&msg)).await.unwrap();
    settle().await;
    assert!(h.backend.running().is_empty(), "no runner for a blocked job");
    assert_eq!(h.requests_to("/actions/runs/4242/cancel").await, 1);

    // Redelivery must not cancel twice.
    s.scale(&h.session, Some(&msg)).await.unwrap();
    settle().await;
    assert_eq!(h.requests_to("/actions/runs/4242/cancel").await, 1);
}

#[tokio::test]
async fn surplus_idle_runner_is_reaped_but_busy_one_is_kept() {
    let h = Harness::new().await;
    let mut s = h.scaler(class(""));
    s.scale(&h.session, Some(&ScaleSetMessage { message_id: 1, statistics: stats(1), ..Default::default() }))
        .await
        .unwrap();
    assert_eq!(h.backend.running().len(), 1);

    // The job was cancelled before pickup: assigned drops to 0, runner is surplus.
    // GitHub refuses to deregister it (it just took a job): keep it.
    h.remove_runner(true).await;
    s.scale(&h.session, Some(&ScaleSetMessage { message_id: 2, statistics: stats(0), ..Default::default() }))
        .await
        .unwrap();
    assert_eq!(h.backend.running().len(), 1, "busy runner not reaped");

    h.remove_runner(false).await;
    s.scale(&h.session, None).await.unwrap();
    settle().await;
    assert!(h.backend.running().is_empty(), "idle surplus runner reaped");
}

#[tokio::test]
async fn warm_pool_follows_activity_window() {
    let h = Harness::new().await;
    h.remove_runner(false).await;
    let mut s = h.scaler(class("min_idle = 1\nwarm_for_secs = 1"));
    s.scale(&h.session, Some(&ScaleSetMessage { message_id: -1, statistics: stats(0), ..Default::default() }))
        .await
        .unwrap();
    assert!(h.backend.running().is_empty(), "cold until the first job");

    let activity = ScaleSetMessage {
        message_id: 1,
        statistics: stats(0),
        job_assigned: vec![JobAssigned { base: base("j1", "push", MAIN) }],
        ..Default::default()
    };
    s.scale(&h.session, Some(&activity)).await.unwrap();
    assert_eq!(h.backend.running().len(), 1, "warm runner after activity");

    tokio::time::sleep(Duration::from_millis(1100)).await;
    s.scale(&h.session, None).await.unwrap();
    settle().await;
    assert!(h.backend.running().is_empty(), "warm runner reaped after the window");
}

#[tokio::test]
async fn reconcile_stops_untracked_instances_unless_busy() {
    let h = Harness::new().await;
    let orphan = h.backend.add_untracked("c-orphan", "c");
    let busy = h.backend.add_untracked("c-busy", "c");
    let _other_class = h.backend.add_untracked("x-other", "x");
    Mock::given(method("GET"))
        .and(path("/t/_apis/distributedtask/pools/0/agents"))
        .and(query_param("agentName", "c-orphan"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"count": 0, "value": []})))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/t/_apis/distributedtask/pools/0/agents"))
        .and(query_param("agentName", "c-busy"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"count": 1, "value": [{"id": 9, "name": "c-busy"}]})),
        )
        .mount(&h.server)
        .await;
    h.remove_runner(true).await; // agent 9 is mid-job

    let mut s = h.scaler(class(""));
    s.reconcile().await;
    let m = h.backend.instances.lock().unwrap();
    assert!(!m[&orphan].running, "unregistered orphan stopped");
    assert!(m[&busy].running, "busy orphan left to finish");
    assert_eq!(m.values().filter(|i| i.class == "x" && i.running).count(), 1, "other classes untouched");
}

#[tokio::test]
async fn failing_backend_backs_off_instead_of_hot_looping() {
    let h = Harness::new().await;
    h.remove_runner(false).await;
    h.backend.fail_starts.store(true, Ordering::SeqCst);
    let mut s = h.scaler(class(""));
    let msg = ScaleSetMessage { message_id: 1, statistics: stats(1), ..Default::default() };
    s.scale(&h.session, Some(&msg)).await.unwrap();
    let attempts = h.backend.starts.load(Ordering::SeqCst);
    assert_eq!(attempts, super::START_ATTEMPTS as usize, "retried within the poll");

    // Next poll arrives during the backoff window: no new attempts.
    s.scale(&h.session, None).await.unwrap();
    assert_eq!(h.backend.starts.load(Ordering::SeqCst), attempts);
    // Every failed attempt deregistered its JIT runner.
    assert_eq!(h.requests_to("/generatejitconfig").await, attempts);
    assert_eq!(
        h.server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "DELETE").count(),
        attempts
    );
}

async fn mock_run(h: &Harness, status: u16, head: &str) {
    Mock::given(method("GET"))
        .and(path("/repos/o/r/actions/runs/4242"))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "id": 4242, "event": "pull_request",
            "repository": {"full_name": "o/r"}, "head_repository": {"full_name": head}
        })))
        .mount(&h.server)
        .await;
}

fn pr_assigned(id: i64, job: &str) -> ScaleSetMessage {
    ScaleSetMessage {
        message_id: id,
        statistics: stats(1),
        job_assigned: vec![JobAssigned { base: base(job, "pull_request", PR) }],
        ..Default::default()
    }
}

#[tokio::test]
async fn fork_pr_rejected_when_forks_disallowed_and_lookup_is_cached() {
    let h = Harness::new().await;
    mock_run(&h, 200, "someone/r").await;
    let mut s = h.scaler(class("[policy]\nallow_fork_prs = false"));
    s.scale(&h.session, Some(&pr_assigned(1, "j1"))).await.unwrap();
    settle().await;
    assert!(h.backend.running().is_empty());
    assert_eq!(h.requests_to("/actions/runs/4242/cancel").await, 1);
    // A second job from the same run reuses the cached lookup and the cancel.
    s.scale(&h.session, Some(&pr_assigned(2, "j2"))).await.unwrap();
    settle().await;
    assert_eq!(h.requests_to("/actions/runs/4242").await, 1, "one REST lookup per run");
    assert_eq!(h.requests_to("/actions/runs/4242/cancel").await, 1, "one cancel per run");
}

#[tokio::test]
async fn same_repo_pr_accepted_when_forks_disallowed() {
    let h = Harness::new().await;
    mock_run(&h, 200, "o/r").await;
    let mut s = h.scaler(class("[policy]\nallow_fork_prs = false"));
    s.scale(&h.session, Some(&pr_assigned(1, "j1"))).await.unwrap();
    settle().await;
    assert_eq!(h.backend.running().len(), 1);
    assert_eq!(h.requests_to("/cancel").await, 0);
}

#[tokio::test]
async fn trusted_class_accepts_verified_same_repo_pr_only_when_enabled() {
    let h = Harness::new().await;
    mock_run(&h, 200, "o/r").await;
    let mut s = h.scaler(class("[policy]\ntrust = \"trusted\"\nallow_same_repo_prs = true"));
    s.scale(&h.session, Some(&pr_assigned(1, "j1"))).await.unwrap();
    assert_eq!(h.backend.running().len(), 1);
}

#[tokio::test]
async fn failed_lookup_fails_closed() {
    let h = Harness::new().await;
    mock_run(&h, 500, "o/r").await;
    let mut s = h.scaler(class("[policy]\nallow_fork_prs = false"));
    s.scale(&h.session, Some(&pr_assigned(1, "j1"))).await.unwrap();
    settle().await;
    assert!(h.backend.running().is_empty(), "unverified PR not run");
    assert_eq!(h.requests_to("/actions/runs/4242/cancel").await, 1);
    assert_eq!(h.requests_to("/actions/runs/4242").await, 3, "retried before failing closed");
}
