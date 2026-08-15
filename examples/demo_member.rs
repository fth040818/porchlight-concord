use anyhow::{bail, Context, Result};
use std::{collections::BTreeSet, path::PathBuf, time::Duration};
use tokio::sync::oneshot;
use vector_sdk::{BotEvent, VectorBot};

const STEP_TIMEOUT: Duration = Duration::from_secs(75);
const MATCH_TIMEOUT: Duration = Duration::from_secs(240);

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let invite = args
        .next()
        .map(|value| value.to_string_lossy().into_owned())
        .context("arg 1: invite URL")?;
    let bot_npub = args
        .next()
        .map(|value| value.to_string_lossy().into_owned())
        .context("arg 2: Porchlight npub")?;
    let data_dir = args.next().map(PathBuf::from).context("arg 3: data dir")?;
    let label = args
        .next()
        .map(|value| value.to_string_lossy().into_owned())
        .context("arg 4: member label")?;

    let bot = VectorBot::builder()
        .data_dir(data_dir)
        .build()
        .await
        .context("failed to create disposable member")?;
    bot.core()
        .join_community(&invite)
        .await
        .context("failed to join disposable community")?;

    let (ready_tx, ready_rx) = oneshot::channel();
    let ready_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(ready_tx)));
    let listener = bot.clone();
    let listener_task = tokio::spawn(async move {
        listener
            .on_event(move |_bot, event| {
                let ready_tx = ready_tx.clone();
                async move {
                    if matches!(event, BotEvent::Ready { .. }) {
                        if let Some(sender) = ready_tx.lock().ok().and_then(|mut slot| slot.take())
                        {
                            let _ = sender.send(());
                        }
                    }
                }
            })
            .await
    });
    tokio::time::timeout(STEP_TIMEOUT, ready_rx)
        .await
        .context("member listener did not become ready")?
        .context("member ready channel closed")?;

    let channel_id = wait_for_channel(&bot, "general").await?;
    let mut seen = seen_dm_ids(&bot, &bot_npub).await;
    bot.channel(channel_id)
        .send("/intro")
        .await
        .context("failed to send /intro")?;
    wait_for_dm(&bot, &bot_npub, &mut seen, STEP_TIMEOUT, |text| {
        text.contains("15-minute private onboarding session")
    })
    .await
    .context("did not receive private onboarding DM")?;

    let token = loop {
        bot.dm(bot_npub.clone())
            .send("/profile")
            .await
            .context("failed to send /profile")?;
        let reply = wait_for_dm(&bot, &bot_npub, &mut seen, STEP_TIMEOUT, |text| {
            text.contains("PLP-") || text.contains("could not confirm current membership")
        })
        .await
        .context("did not receive a profile prompt")?;
        if let Some(token) = extract_token(&reply) {
            break token;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };

    loop {
        let profile = format!("{token} | UTC+8 | nostr, music, community");
        bot.dm(bot_npub.clone())
            .send(&profile)
            .await
            .context("failed to send token-bound profile")?;
        let reply = wait_for_dm(&bot, &bot_npub, &mut seen, STEP_TIMEOUT, |text| {
            text.contains("minimal profile was saved")
                || text.contains("could not confirm current membership")
        })
        .await
        .context("did not receive profile result")?;
        if reply.contains("minimal profile was saved") {
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    loop {
        bot.dm(bot_npub.clone())
            .send("/buddy")
            .await
            .context("failed to send /buddy")?;
        let reply = wait_for_dm(&bot, &bot_npub, &mut seen, STEP_TIMEOUT, |text| {
            text.contains("opt-in queue")
                || text.contains("Match PLM-")
                || text.contains("could not confirm current membership")
        })
        .await
        .context("did not receive buddy result")?;
        if !reply.contains("could not confirm current membership") {
            println!("DEMO_MEMBER_READY={label}:{}", bot.npub());
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    let introduction = wait_for_dm(&bot, &bot_npub, &mut seen, MATCH_TIMEOUT, |text| {
        text.contains("Porchlight introduction PLM-")
    })
    .await
    .context("did not receive the buddy introduction")?;
    let match_id = extract_prefixed(&introduction, "PLM-").unwrap_or("PLM-UNKNOWN");
    println!("DEMO_MATCH_RECEIVED={label}:{match_id}");
    listener_task.abort();
    Ok(())
}

async fn wait_for_channel(bot: &VectorBot, name: &str) -> Result<String> {
    for _ in 0..30 {
        for community in bot.core().list_communities().await {
            let Some(channels) = community.get("channels").and_then(|value| value.as_array())
            else {
                continue;
            };
            for channel in channels {
                if channel.get("name").and_then(|value| value.as_str()) == Some(name) {
                    if let Some(id) = channel.get("channel_id").and_then(|value| value.as_str()) {
                        return Ok(id.to_string());
                    }
                }
            }
        }
        let _ = bot.core().sync_communities().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("channel {name:?} did not appear")
}

async fn seen_dm_ids(bot: &VectorBot, peer: &str) -> BTreeSet<String> {
    bot.dm(peer.to_string())
        .history(100)
        .await
        .into_iter()
        .map(|message| message.id)
        .collect()
}

async fn wait_for_dm(
    bot: &VectorBot,
    peer: &str,
    seen: &mut BTreeSet<String>,
    timeout: Duration,
    predicate: impl Fn(&str) -> bool,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let mut messages = bot.dm(peer.to_string()).history(100).await;
        messages.sort_by(|left, right| left.at.cmp(&right.at).then_with(|| left.id.cmp(&right.id)));
        for message in messages.into_iter().filter(|message| !message.mine) {
            if seen.contains(&message.id) {
                continue;
            }
            if predicate(&message.content) {
                seen.insert(message.id);
                return Ok(message.content);
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("timed out waiting for expected DM")
}

fn extract_token(text: &str) -> Option<String> {
    extract_prefixed(text, "PLP-").map(str::to_string)
}

fn extract_prefixed<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.split_whitespace()
        .map(|word| {
            word.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '-'
            })
        })
        .find(|word| word.starts_with(prefix))
}
