//! rgha: a GitHub Actions runner controller that runs every job in its own
//! per-second-billed sandbox, so short and low-CPU jobs cost a fraction of a
//! per-minute hosted runner and queue time stays low.

mod backend;
mod config;
mod controller;
mod cost;
mod hook;
mod image;
mod lab;
mod metrics;
mod policy;
mod pool;
mod scaler;
mod schedule;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use rgha_scaleset::{Client, ClientOptions, Credentials, SystemInfo};
use tokio::sync::watch;

use crate::config::Config;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the controller.
    Run {
        #[arg(short, long, env = "RGHA_CONFIG", default_value = "rgha.toml")]
        config: PathBuf,
        /// Serve Prometheus metrics on this address, e.g. 0.0.0.0:9464.
        #[arg(long, env = "RGHA_METRICS_ADDR")]
        metrics_addr: Option<std::net::SocketAddr>,
        /// Emit JSON logs (for log shippers).
        #[arg(long, env = "RGHA_LOG_JSON")]
        log_json: bool,
    },
    /// Validate config and connectivity (GitHub auth, runner group, backends).
    Check {
        #[arg(short, long, env = "RGHA_CONFIG", default_value = "rgha.toml")]
        config: PathBuf,
        /// Only validate the file; don't contact GitHub or backends.
        #[arg(long)]
        offline: bool,
    },
    /// Experiments (see lab.rs).
    #[command(hide = true)]
    Lab {
        #[arg(short, long, env = "RGHA_CONFIG", default_value = "rgha.toml")]
        config: PathBuf,
        #[command(subcommand)]
        experiment: LabCmd,
    },
    /// Compare per-second sandbox cost with a per-minute GitHub-hosted runner.
    Estimate {
        /// Job duration in seconds.
        #[arg(long)]
        seconds: f64,
        /// Sandbox overhead (boot + runner registration) in seconds.
        #[arg(long, default_value_t = 10.0)]
        overhead: f64,
        /// Modal cores.
        #[arg(long, default_value_t = 0.25)]
        cpu: f64,
        #[arg(long, default_value_t = 1024)]
        memory_mib: u32,
        #[arg(long, default_value_t = cost::GITHUB_LINUX_2CORE_PER_MIN)]
        github_per_min: f64,
    },
}

#[derive(Subcommand)]
enum LabCmd {
    /// Register a runner, wait until it's online, memory-snapshot it, stop it.
    HibernatePrepare { class: String },
    /// Restore a runner snapshot into a running sandbox.
    Restore { backend: String, snapshot_id: String },
    /// Terminate a sandbox and delete a class's scale set (experiment cleanup).
    Cleanup { backend: String, class: String, sandbox_id: Option<String> },
}

fn credentials(cfg: &config::GitHub) -> anyhow::Result<Credentials> {
    if let (Some(client_id), Some(installation_id)) = (&cfg.app_client_id, cfg.app_installation_id) {
        let pem = match std::env::var("RGHA_GITHUB_APP_PRIVATE_KEY") {
            Ok(p) if !p.is_empty() => p,
            _ => {
                let path = cfg.app_private_key_path.as_ref().context(
                    "GitHub App configured but neither RGHA_GITHUB_APP_PRIVATE_KEY nor github.app_private_key_path is set",
                )?;
                std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?
            }
        };
        return Ok(Credentials::App { client_id: client_id.clone(), installation_id, private_key_pem: pem });
    }
    match std::env::var(&cfg.token_env) {
        Ok(t) if !t.is_empty() => Ok(Credentials::Token(t)),
        _ => bail!("no GitHub credentials: configure a GitHub App or set {}", cfg.token_env),
    }
}

fn github_client(cfg: &Config) -> anyhow::Result<Client> {
    let opts = ClientOptions {
        system: SystemInfo {
            system: "rgha".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            subsystem: "controller".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    Ok(Client::new(&cfg.github.url, credentials(&cfg.github)?, opts)?)
}

async fn build_backends(cfg: &Config) -> anyhow::Result<HashMap<String, Arc<dyn backend::Backend>>> {
    let mut out = HashMap::new();
    for (name, b) in &cfg.backends {
        if cfg.classes.iter().any(|c| &c.backend == name) {
            let built = backend::build(name, b).await.with_context(|| format!("backend {name}"))?;
            out.insert(name.clone(), built);
        }
    }
    Ok(out)
}

/// Resolves on SIGINT (Ctrl-C) or SIGTERM (`docker stop`, Kubernetes,
/// systemd), returning which one arrived.
async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = term.recv() => "SIGTERM",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let filter =
        tracing_subscriber::EnvFilter::try_from_env("RGHA_LOG").unwrap_or_else(|_| "info,h2=warn,tonic=warn".into());
    if matches!(cli.command, Cmd::Run { log_json: true, .. }) {
        tracing_subscriber::fmt().json().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    match cli.command {
        Cmd::Estimate { seconds, overhead, cpu, memory_mib, github_per_min } => {
            let sandbox = cost::Pricing::MODAL_SANDBOX.cost(cpu, memory_mib, seconds + overhead);
            let gh = cost::github_hosted_cost(seconds, github_per_min);
            println!("job: {seconds}s (+{overhead}s overhead) at {cpu} cores / {memory_mib} MiB");
            println!("  Modal sandbox : ${sandbox:.6}");
            println!("  GitHub-hosted : ${gh:.6}  (rounded up to {} min)", (seconds / 60.0).ceil().max(1.0));
            if sandbox > 0.0 {
                println!("  ratio         : {:.1}x", gh / sandbox);
            }
            Ok(())
        }
        Cmd::Lab { config, experiment } => {
            let cfg = Config::load(&config)?;
            match experiment {
                LabCmd::HibernatePrepare { class } => lab::hibernate_prepare(&cfg, &github_client(&cfg)?, &class).await,
                LabCmd::Restore { backend, snapshot_id } => lab::restore(&cfg, &backend, &snapshot_id).await,
                LabCmd::Cleanup { backend, class, sandbox_id } => {
                    lab::cleanup(&cfg, &github_client(&cfg)?, &backend, &class, sandbox_id.as_deref()).await
                }
            }
        }
        Cmd::Check { config, offline } => {
            let cfg = Config::load(&config)?;
            println!("config OK: {} class(es), {} backend(s)", cfg.classes.len(), cfg.backends.len());
            if offline {
                return Ok(());
            }
            let client = github_client(&cfg)?;
            let group = client.get_runner_group_by_name(&cfg.github.runner_group).await?;
            println!("github OK: runner group {:?} (id {})", group.name, group.id);
            for c in &cfg.classes {
                match client.get_scale_set(group.id, &c.name).await? {
                    Some(ss) => println!("  class {}: scale set exists (id {})", c.name, ss.id),
                    None => println!("  class {}: scale set will be created", c.name),
                }
            }
            let backends = build_backends(&cfg).await?;
            for (name, b) in &backends {
                println!("backend OK: {name} ({})", b.kind());
            }
            Ok(())
        }
        Cmd::Run { config, metrics_addr, .. } => {
            let cfg = Config::load(&config)?;
            if let Some(addr) = metrics_addr {
                metrics::install(addr)?;
            }
            let client = github_client(&cfg)?;
            let group = client.get_runner_group_by_name(&cfg.github.runner_group).await?;
            let backends = build_backends(&cfg).await?;
            let owner = format!(
                "rgha-{}",
                std::env::var("HOSTNAME")
                    .ok()
                    .filter(|h| !h.is_empty())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()[..8].to_string())
            );

            let (tx, rx) = watch::channel(false);
            let mut tasks = tokio::task::JoinSet::new();
            for class in cfg.classes.clone() {
                let ctrl = controller::ClassController {
                    pricing: cfg.backends[&class.backend].pricing(),
                    backend: backends[&class.backend].clone(),
                    client: client.clone(),
                    runner_group_id: group.id,
                    owner: owner.clone(),
                    class,
                };
                tasks.spawn(ctrl.run(rx.clone()));
            }

            tokio::select! {
                sig = shutdown_signal() => tracing::info!(signal = sig, "shutting down"),
                Some(res) = tasks.join_next() => {
                    // A class failed hard (e.g. scale set creation). Stop the rest.
                    tracing::error!("class controller exited: {:?}", res);
                }
            }
            let _ = tx.send(true);
            while let Some(res) = tasks.join_next().await {
                if let Ok(Err(e)) = res {
                    tracing::error!(error = %format!("{e:#}"), "class controller error");
                }
            }
            Ok(())
        }
    }
}
