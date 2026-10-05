//! Calibrates Modal's usage metering against requests:
//! `cargo run -p rgha-modal --example calibrate`.
//! Runs an idle and a CPU-burning Sandbox (0.125 core / 128 MiB requested,
//! 2 cores / 1 GiB limit) for ~30 s each and prints what Modal metered.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rgha_modal::{Client, Network, Profile, SandboxSpec};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::connect(Profile::load(None)?).await?;
    let app_id = client.app_get_or_create("rgha-smoke").await?;
    let image_id = client.image_from_registry(&app_id, "alpine:3.20", &[]).await?;
    let cases = [
        ("idle", "sleep 30"),
        ("burn2", "timeout 30 sh -c 'yes >/dev/null & yes >/dev/null & yes >/dev/null & yes >/dev/null & wait'"),
        ("mem", "head -c 600m /dev/zero | tail >/dev/null; sleep 25"),
    ];
    let mut running = vec![];
    for (name, cmd) in cases {
        let spec = SandboxSpec {
            name: format!("cal-{name}-{}", std::process::id()),
            image_id: image_id.clone(),
            command: vec!["sh".into(), "-c".into(), cmd.into()],
            workdir: None,
            secret_env: HashMap::new(),
            cpu: 0.125,
            cpu_limit: Some(2.0),
            memory_mib: 128,
            memory_limit_mib: Some(1024),
            timeout: Duration::from_secs(120),
            network: Network::Blocked,
            runtime: None,
            regions: vec![],
            tags: HashMap::from([("rgha".into(), "calibrate".into()), ("case".into(), name.into())]),
            enable_snapshot: false,
        };
        running.push((name, client.sandbox_create(&app_id, &spec).await?, Instant::now()));
    }
    for (name, id, t0) in running {
        while client.sandbox_wait(&id, Duration::from_secs(10)).await?.is_none() {}
        let wall = t0.elapsed().as_secs_f64();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let u = client.sandbox_resource_usage(&id).await?;
        println!(
            "{name:>6} {id} wall={wall:.1}s cpu_core_s={:.2} (req floor {:.2}) mem_gib_s={:.2} (req floor {:.2})",
            u.cpu_core_secs,
            0.125 * wall,
            u.mem_gib_secs,
            0.125 * wall
        );
        client.sandbox_terminate(&id).await?;
    }
    Ok(())
}
