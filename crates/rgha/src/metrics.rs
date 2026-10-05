//! Prometheus metrics. Names are centralised here so dashboards have one
//! place to look. Served on `--metrics-addr` (or `[metrics] listen`).

use std::net::SocketAddr;

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};

pub fn install(addr: SocketAddr) -> anyhow::Result<()> {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_buckets(&[0.5, 1.0, 2.0, 3.0, 5.0, 8.0, 13.0, 21.0, 34.0, 60.0, 120.0, 300.0, 600.0, 1800.0])?
        .install()?;
    describe_histogram!(
        "rgha_pickup_seconds",
        "Time from GitHub assigning a job to the scale set until a runner took it"
    );
    describe_histogram!("rgha_job_seconds", "Job duration from GitHub timestamps");
    describe_histogram!("rgha_sandbox_seconds", "Sandbox lifetime as billed (create to stop)");
    describe_histogram!("rgha_runner_start_seconds", "JIT config + backend create latency");
    describe_counter!("rgha_jobs_total", "Jobs completed on rgha runners, by result");
    describe_counter!("rgha_runner_start_failures_total", "Failed runner starts");
    describe_counter!("rgha_policy_rejections_total", "Jobs rejected by class policy");
    describe_counter!("rgha_orphans_stopped_total", "Leaked instances stopped by reconciliation");
    describe_gauge!("rgha_cost_usd_total", "Estimated sandbox spend (upper bound at cpu_limit)");
    describe_gauge!("rgha_metered_cost_usd_total", "Sandbox spend from the platform's own usage meter");
    describe_gauge!(
        "rgha_github_equivalent_usd_total",
        "What the same jobs would cost on per-minute GitHub-hosted runners"
    );
    describe_gauge!("rgha_runners", "Runners in the pool, by state");
    describe_gauge!("rgha_assigned_jobs", "Jobs assigned to the scale set (minus policy-blocked)");
    tracing::info!(%addr, "serving Prometheus metrics");
    Ok(())
}

pub fn pickup(class: &str, secs: f64) {
    histogram!("rgha_pickup_seconds", "class" => class.to_string()).record(secs);
}

pub fn job_finished(class: &str, result: &str, job_secs: Option<f64>, sandbox_secs: f64, usd: f64, github_usd: f64) {
    let c = class.to_string();
    counter!("rgha_jobs_total", "class" => c.clone(), "result" => result.to_string()).increment(1);
    if let Some(j) = job_secs {
        histogram!("rgha_job_seconds", "class" => c.clone()).record(j);
    }
    histogram!("rgha_sandbox_seconds", "class" => c.clone()).record(sandbox_secs);
    gauge!("rgha_cost_usd_total", "class" => c.clone()).increment(usd);
    gauge!("rgha_github_equivalent_usd_total", "class" => c).increment(github_usd);
}

pub fn metered(class: &str, usd: f64) {
    gauge!("rgha_metered_cost_usd_total", "class" => class.to_string()).increment(usd);
}

pub fn runner_started(class: &str, secs: f64) {
    histogram!("rgha_runner_start_seconds", "class" => class.to_string()).record(secs);
}

pub fn runner_start_failed(class: &str) {
    counter!("rgha_runner_start_failures_total", "class" => class.to_string()).increment(1);
}

pub fn policy_rejected(class: &str) {
    counter!("rgha_policy_rejections_total", "class" => class.to_string()).increment(1);
}

pub fn orphan_stopped(class: &str) {
    counter!("rgha_orphans_stopped_total", "class" => class.to_string()).increment(1);
}

pub fn pool(class: &str, idle: usize, busy: usize, assigned: i64) {
    gauge!("rgha_runners", "class" => class.to_string(), "state" => "idle").set(idle as f64);
    gauge!("rgha_runners", "class" => class.to_string(), "state" => "busy").set(busy as f64);
    gauge!("rgha_assigned_jobs", "class" => class.to_string()).set(assigned as f64);
}
