//! Measures Modal memory snapshot + restore latency for a trivial Sandbox:
//! `cargo run -p rgha-modal --example snapshot_bench`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rgha_modal::{Client, Network, Profile, SandboxSpec};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::connect(Profile::load(None)?).await?;
    let app_id = client.app_get_or_create("rgha-smoke").await?;
    let image_id = client.image_from_registry(&app_id, "alpine:3.20", &[]).await?;
    let spec = SandboxSpec {
        name: format!("snapbench-{}", std::process::id()),
        image_id,
        // A process with in-memory state: a counter that keeps ticking.
        command: vec!["sh".into(), "-c".into(), "i=0; while true; do i=$((i+1)); sleep 0.1; done".into()],
        workdir: None,
        secret_env: HashMap::new(),
        cpu: 0.125,
        cpu_limit: Some(1.0),
        memory_mib: 256,
        memory_limit_mib: None,
        timeout: Duration::from_secs(300),
        network: Network::Open,
        runtime: None,
        regions: vec![],
        tags: HashMap::from([("rgha".into(), "snapbench".into())]),
        enable_snapshot: true,
    };
    let t = Instant::now();
    let id = client.sandbox_create(&app_id, &spec).await?;
    client.sandbox_wait_running(&id, Duration::from_secs(55)).await?;
    println!("create->running: {:?}", t.elapsed());

    tokio::time::sleep(Duration::from_secs(3)).await;
    let t = Instant::now();
    let snap = client.sandbox_snapshot(&id).await?;
    println!("snapshot: {:?} ({snap})", t.elapsed());
    client.sandbox_terminate(&id).await?;

    for i in 0..3 {
        let t = Instant::now();
        let restored = client.sandbox_restore(&snap).await?;
        println!("restore #{i} -> running: {:?} ({restored})", t.elapsed());
        client.sandbox_terminate(&restored).await?;
    }
    Ok(())
}
