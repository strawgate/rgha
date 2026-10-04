//! Live smoke test: `cargo run -p rgha-modal --example smoke`.
//! Creates one tiny Sandbox in the `rgha-smoke` App, waits for it, and terminates it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rgha_modal::{Client, Network, Profile, SandboxSpec};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    let client = Client::connect(Profile::load(None)?).await?;
    let app_id = client.app_get_or_create("rgha-smoke").await?;
    let image_id = client.image_from_registry(&app_id, "alpine:3.20", &[]).await?;
    println!("app={app_id} image={image_id} setup={:?}", t0.elapsed());

    let t1 = Instant::now();
    let spec = SandboxSpec {
        name: format!("smoke-{}", std::process::id()),
        image_id,
        command: vec!["sh".into(), "-c".into(), "echo hello from $GREETING; exit 7".into()],
        workdir: None,
        secret_env: HashMap::from([("GREETING".into(), "rgha".into())]),
        cpu: 0.125,
        cpu_limit: None,
        memory_mib: 128,
        memory_limit_mib: None,
        timeout: Duration::from_secs(60),
        network: Network::Blocked,
        runtime: None,
        regions: vec![],
        tags: HashMap::from([("rgha".into(), "smoke".into())]),
    };
    let id = client.sandbox_create(&app_id, &spec).await?;
    println!("sandbox={id} create_rpc={:?}", t1.elapsed());
    let listed = client.sandbox_list(&app_id, &spec.tags).await?;
    println!("listed_by_tag={}", listed.iter().any(|s| s.id == id));
    let mut code = None;
    for _ in 0..12 {
        code = client.sandbox_wait(&id, Duration::from_secs(10)).await?;
        if code.is_some() {
            break;
        }
    }
    println!("exit={code:?} create_to_exit={:?}", t1.elapsed());
    client.sandbox_terminate(&id).await?;
    Ok(())
}
