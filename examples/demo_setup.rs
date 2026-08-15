use anyhow::{Context, Result};
use std::{path::PathBuf, time::Duration};
use vector_sdk::VectorBot;

#[tokio::main]
async fn main() -> Result<()> {
    let data_dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .context("usage: demo_setup <isolated-owner-data-dir>")?;
    let bot = VectorBot::builder()
        .data_dir(data_dir)
        .build()
        .await
        .context("failed to create disposable owner identity")?;
    let summary = bot
        .core()
        .create_community_v2("Porchlight Disposable Demo")
        .await
        .context("failed to create Concord v2 community")?;
    let community_id = summary
        .get("community_id")
        .and_then(|value| value.as_str())
        .context("community summary did not include community_id")?;
    let invite = bot
        .community(community_id.to_string())
        .create_invite()
        .await
        .context("failed to create shareable invite")?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    println!("DEMO_OWNER_NPUB={}", bot.npub());
    println!("DEMO_COMMUNITY_ID={community_id}");
    println!("DEMO_INVITE={invite}");
    Ok(())
}
