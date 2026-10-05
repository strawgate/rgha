//! Prints Modal's metered usage for a Sandbox and the billed cost per
//! object for an App over the last N hours:
//! `cargo run -p rgha-modal --example usage -- <app> [hours] [sandbox-id]`.

use rgha_modal::{Client, Profile};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let app = args.next().unwrap_or_else(|| "rgha-testbed".into());
    let hours: i64 = args.next().and_then(|h| h.parse().ok()).unwrap_or(24);
    let sandbox = args.next();
    let client = Client::connect(Profile::load(None)?).await?;
    if let Some(id) = sandbox {
        println!("{id}: {:?}", client.sandbox_resource_usage(&id).await?);
    }
    let app_id = client.app_get_or_create(&app).await?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    let items = client.billing_report(&app_id, now - hours * 3600, now).await?;
    let total: f64 = items.iter().map(|i| i.cost_usd).sum();
    println!("app {app}: {} billing rows over {hours}h, total ${total:.4}", items.len());
    for i in &items {
        println!("  t={} ${:.6} {:?}", i.interval_unix, i.cost_usd, i.cost_by_resource);
    }
    Ok(())
}
