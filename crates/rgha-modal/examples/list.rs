//! Lists running rgha-owned Sandboxes: `cargo run -p rgha-modal --example list [app]`.

use std::collections::HashMap;

use rgha_modal::{Client, Profile};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = std::env::args().nth(1).unwrap_or_else(|| "rgha".into());
    let client = Client::connect(Profile::load(None)?).await?;
    let app_id = client.app_get_or_create(&app).await?;
    let running = client.sandbox_list(&app_id, &HashMap::from([("rgha".into(), "1".into())])).await?;
    println!("{} running sandbox(es) in app {app}", running.len());
    for s in running {
        println!("  {} {} {:?}", s.id, s.name, s.tags.get("rgha-class"));
    }
    Ok(())
}
