use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use porchlight_concord::{
    parse_profile_input, short_npub, AppConfig, BeginOnboarding, BuddyStatus, DeliveryTarget,
    InvitePolicy, JoinOutcome, MembershipScope, ProfileCompletion, ProfilePrompt, StateStore,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use vector_sdk::{BotEvent, IncomingMessage, VectorBot};

#[derive(Debug, Parser)]
#[command(
    name = "porchlight",
    version,
    about = "A privacy-conscious Concord community concierge"
)]
struct Cli {
    #[arg(short, long, default_value = "porchlight.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate configuration without connecting to Nostr.
    Check,
    /// Run the bot. An optional invite link joins a community on startup.
    Run {
        #[arg(long)]
        invite: Option<String>,
    },
}

#[derive(Clone, Default)]
struct SenderLocks {
    inner: Arc<Mutex<BTreeMap<String, Arc<Mutex<()>>>>>,
}

impl SenderLocks {
    async fn for_sender(&self, npub: &str) -> Arc<Mutex<()>> {
        let mut locks = self.inner.lock().await;
        locks
            .entry(npub.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

struct DataDirGuard {
    _lock: File,
}

impl DataDirGuard {
    async fn acquire(data_dir: &Path) -> Result<Self> {
        #[cfg(unix)]
        let existed = tokio::fs::try_exists(data_dir)
            .await
            .with_context(|| format!("failed to inspect data directory {}", data_dir.display()))?;
        tokio::fs::create_dir_all(data_dir)
            .await
            .with_context(|| format!("failed to create data directory {}", data_dir.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let metadata = tokio::fs::symlink_metadata(data_dir)
                .await
                .with_context(|| {
                    format!("failed to inspect data directory {}", data_dir.display())
                })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                anyhow::bail!(
                    "bot data directory must be a real directory, not a symlink: {}",
                    data_dir.display()
                );
            }
            if existed {
                let mode = metadata.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    anyhow::bail!(
                        "bot data directory {} has mode {mode:03o}; restrict it to 700 before running Porchlight",
                        data_dir.display()
                    );
                }
            } else {
                tokio::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))
                    .await
                    .with_context(|| {
                        format!("failed to set private mode on {}", data_dir.display())
                    })?;
            }
        }

        let lock_path = data_dir.join(".porchlight.lock");
        #[cfg(unix)]
        match tokio::fs::symlink_metadata(&lock_path).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "refusing to use a symlinked data lock: {}",
                    lock_path.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect data lock {}", lock_path.display())
                })
            }
        }
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open data lock {}", lock_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            lock.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| {
                    format!("failed to set private mode on {}", lock_path.display())
                })?;
        }
        FileExt::try_lock_exclusive(&lock).with_context(|| {
            format!(
                "Vector data directory is already in use by another Porchlight process: {}",
                data_dir.display()
            )
        })?;
        Ok(Self { _lock: lock })
    }
}

#[derive(Default)]
struct FlushSummary {
    sent: usize,
    failed: usize,
    canceled: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("porchlight_concord=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .init();

    let cli = Cli::parse();
    let config = AppConfig::load(&cli.config)?;
    match cli.command {
        Command::Check => {
            println!("configuration OK: {}", cli.config.display());
            println!("invite policy: {:?}", config.bot.invite_policy);
            println!("resource topics: {}", config.resource_topics().join(", "));
            Ok(())
        }
        Command::Run { invite } => run(config, invite).await,
    }
}

async fn run(config: AppConfig, invite: Option<String>) -> Result<()> {
    let config = Arc::new(config);
    let state = Arc::new(StateStore::load(config.bot.state_file.clone()).await?);
    let _data_guard = DataDirGuard::acquire(&config.bot.data_dir).await?;
    let sender_locks = SenderLocks::default();

    #[cfg(windows)]
    warn!(
        data_dir = %config.bot.data_dir.display(),
        "Vector 0.8.2 does not provide complete at-rest encryption for a fresh SDK bot; protect this directory with a dedicated Windows ACL and never reuse a payment key"
    );

    let mut builder = VectorBot::builder().data_dir(config.bot.data_dir.clone());
    if let Ok(nsec) = std::env::var("VECTOR_NSEC") {
        builder = builder.nsec(nsec);
    }
    builder = match config.bot.invite_policy {
        InvitePolicy::Manual => builder,
        InvitePolicy::Public => builder.public(),
        InvitePolicy::Whitelist => builder.whitelist(config.bot.owner_npubs.clone()),
    };

    let bot = builder
        .build()
        .await
        .context("failed to initialize Vector bot")?;
    info!(npub = %bot.npub(), state = %state.path().display(), "Porchlight identity ready");

    if let Some(invite) = invite {
        let summary = bot
            .core()
            .join_community(&invite)
            .await
            .context("failed to join community invite")?;
        info!(community = ?summary, "joined community");
    }

    register_commands(&bot, config.clone(), state.clone(), sender_locks.clone());

    let worker_bot = bot.clone();
    let worker_state = state.clone();
    let worker_locks = sender_locks.clone();
    let outbox_worker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            flush_outbox(&worker_bot, &worker_state, &worker_locks).await;
        }
    });

    let handler_config = config.clone();
    let handler_state = state.clone();
    let handler_locks = sender_locks.clone();
    let events = bot.on_event(move |bot, event| {
        let config = handler_config.clone();
        let state = handler_state.clone();
        let sender_locks = handler_locks.clone();
        async move {
            match event {
                BotEvent::Ready { communities } => {
                    info!(communities, "Porchlight is listening");
                    let updated = bot
                        .core()
                        .update_bot_profile(&config.bot.name, "", "", &config.bot.about)
                        .await;
                    if !updated {
                        warn!("bot profile could not be published yet");
                    }
                    flush_outbox(&bot, &state, &sender_locks).await;
                    reconcile_pending_profiles(&bot, &state, &sender_locks).await;
                }
                BotEvent::MemberJoin { channel_id, npub } if npub != bot.npub() => {
                    let Some(community_id) = community_id_for_channel(&bot, &channel_id).await else {
                        warn!(%npub, %channel_id, "could not map MemberJoin channel to a community; skipped welcome for fail-safe privacy");
                        return;
                    };
                    let scope = MembershipScope {
                        community_id,
                        channel_id: channel_id.clone(),
                    };
                    let sender_lock = sender_locks.for_sender(&npub).await;
                    let guard = sender_lock.lock().await;
                    let queued = queue_welcome(&state, &config, scope, &npub).await;
                    drop(guard);
                    if let Err(error) = queued {
                        error!(%error, "failed to queue welcome delivery");
                    } else {
                        flush_outbox(&bot, &state, &sender_locks).await;
                    }
                }
                BotEvent::MemberLeave { channel_id, npub } if npub != bot.npub() => {
                    let Some(community_id) = community_id_for_channel(&bot, &channel_id).await else {
                        warn!(%npub, %channel_id, "could not map MemberLeave channel to a community; retained state for fail-safe review");
                        return;
                    };
                    let sender_lock = sender_locks.for_sender(&npub).await;
                    let _guard = sender_lock.lock().await;
                    match state.remove_member_from_community(&npub, &community_id).await {
                        Ok(true) => info!(%npub, %community_id, "removed departed member onboarding, match and pending delivery state"),
                        Ok(false) => {}
                        Err(error) => error!(%npub, %community_id, %error, "failed to remove departed member state"),
                    }
                }
                BotEvent::Removed { community_id } => {
                    vector_core::community::realtime::teardown_local(&community_id).await;
                    match state.remove_community(&community_id).await {
                        Ok(true) => info!(%community_id, "removed state and pending deliveries after Porchlight left the community"),
                        Ok(false) => {}
                        Err(error) => error!(%community_id, %error, "failed to remove state after Porchlight left the community"),
                    }
                }
                BotEvent::Message(message)
                    if !message.is_mine() && !message.is_group && !message.is_file =>
                {
                    process_profile_message(&bot, &state, &sender_locks, &message).await;
                }
                BotEvent::Message(message)
                    if !message.is_mine() && !message.is_group && message.is_file =>
                {
                    let _ = message
                        .channel()
                        .send(
                            "Porchlight onboarding accepts text only; no attachment was processed.",
                        )
                        .await;
                }
                _ => {}
            }
        }
    });
    tokio::pin!(events);
    tokio::select! {
        result = &mut events => result?,
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for Ctrl+C")?;
            info!("shutdown signal received");
        }
    }

    outbox_worker.abort();
    Ok(())
}

fn register_commands(
    bot: &VectorBot,
    config: Arc<AppConfig>,
    state: Arc<StateStore>,
    sender_locks: SenderLocks,
) {
    {
        let state = state.clone();
        let config = config.clone();
        let sender_locks = sender_locks.clone();
        bot.command("intro", "Open a short private onboarding session")
            .run(move |ctx| {
                let state = state.clone();
                let retention_note = config.privacy.retention_note.clone();
                let sender_locks = sender_locks.clone();
                async move {
                    if !ctx.msg.is_group {
                        let _ = ctx
                            .reply("Run /intro in the community channel where you want a buddy.")
                            .await;
                        return;
                    }
                    let Some(npub) = sender_npub(&ctx.msg) else {
                        let _ = ctx.reply("I could not identify the sender.").await;
                        return;
                    };
                    let Some(community) = ctx.msg.community() else {
                        let _ = ctx
                            .reply("I could not resolve this channel's community. Nothing was stored.")
                            .await;
                        return;
                    };
                    let sender_lock = sender_locks.for_sender(&npub).await;
                    let _guard = sender_lock.lock().await;
                    match state
                        .begin_onboarding(
                            &npub,
                            community.id(),
                            &ctx.msg.chat_id,
                            &ctx.msg.message.id,
                        )
                        .await
                    {
                        Ok(BeginOnboarding::Started | BeginOnboarding::Refreshed) => {
                            let private = format!(
                                "A 15-minute private onboarding session is open. Run /profile here; I will then ask for a token-bound text reply.\n\n{retention_note}"
                            );
                            match ctx.bot.dm(npub).send(&private).await {
                                Ok(_) => {
                                    let _ = ctx.reply("I sent an encrypted DM. No profile fields were posted in this channel.").await;
                                }
                                Err(error) => {
                                    warn!(%error, "private onboarding DM failed");
                                    let _ = ctx.reply("The private DM could not be delivered. No profile was created or queued.").await;
                                }
                            }
                        }
                        Ok(BeginOnboarding::Conflict) => {
                            let _ = ctx.reply("You already have a short onboarding session open elsewhere. Finish it in DM or use /forget there before starting another.").await;
                        }
                        Err(error) => {
                            error!(%error, "failed to begin onboarding");
                            let _ = ctx.reply("I could not start onboarding. No profile was created or queued.").await;
                        }
                    }
                }
            });
    }

    {
        let state = state.clone();
        let sender_locks = sender_locks.clone();
        bot.command("profile", "Request the private profile-entry prompt")
            .run(move |ctx| {
                let state = state.clone();
                let sender_locks = sender_locks.clone();
                async move {
                    if ctx.msg.is_group {
                        let _ = ctx.reply("For privacy, /profile has no public arguments and only works in my encrypted DM. Start with /intro here.").await;
                        return;
                    }
                    let Some(npub) = sender_npub(&ctx.msg) else {
                        return;
                    };
                    let sender_lock = sender_locks.for_sender(&npub).await;
                    let _guard = sender_lock.lock().await;
                    match state
                        .claim_pending_message(
                            &npub,
                            &ctx.msg.message.id,
                            message_time_secs(ctx.msg.message.at),
                        )
                        .await
                    {
                        Ok(false) => return,
                        Err(error) => {
                            error!(%error, "failed to claim profile command");
                            return;
                        }
                        Ok(true) => {}
                    }
                    send_profile_prompt(&ctx.bot, &state, &npub).await;
                }
            });
    }

    {
        let state = state.clone();
        let sender_locks = sender_locks.clone();
        bot.command("buddy", "Privately join the one-person buddy queue")
            .run(move |ctx| {
                let state = state.clone();
                let sender_locks = sender_locks.clone();
                async move {
                    if ctx.msg.is_group {
                        let _ = ctx.reply("For privacy, /buddy has no public arguments and only works in my encrypted DM.").await;
                        return;
                    }
                    let Some(npub) = sender_npub(&ctx.msg) else {
                        return;
                    };
                    let sender_lock = sender_locks.for_sender(&npub).await;
                    let _guard = sender_lock.lock().await;
                    let Some(scope) = state.profile_membership(&npub).await else {
                        let _ = ctx.reply("No private profile exists. Run /intro in the community, then /profile here.").await;
                        return;
                    };
                    if !member_can_use_scope(&ctx.bot, &scope, &npub).await {
                        let _ = ctx.reply("I could not confirm current membership from my local Concord view. Nothing was queued; retry after the community finishes syncing.").await;
                        return;
                    }
                    let mut eligible_candidates = BTreeSet::new();
                    for (candidate, scope) in state.buddy_candidates(&npub).await {
                        if member_can_use_scope(&ctx.bot, &scope, &candidate).await {
                            eligible_candidates.insert(candidate);
                        }
                    }
                    let outcome = state
                        .join_buddy_queue(&npub, &eligible_candidates)
                        .await;
                    drop(_guard);
                    match outcome {
                        Ok(JoinOutcome::Waiting) => {
                            let _ = ctx.reply("You are in the opt-in queue for seven days. I will privately notify both people after a compatible member joins.").await;
                        }
                        Ok(JoinOutcome::AlreadyMatched(partner)) => {
                            flush_outbox(&ctx.bot, &state, &sender_locks).await;
                            let _ = ctx.reply(format!("You already have an opt-in match with {partner}. Any unsent introduction was retried.")).await;
                        }
                        Ok(JoinOutcome::NewlyMatched(pair)) => {
                            let summary = flush_outbox(&ctx.bot, &state, &sender_locks).await;
                            let pending = state.pending_delivery_count().await;
                            let _ = ctx.reply(format!(
                                "Match {} was created. Delivery attempt: {} sent, {} failed, {} canceled after membership recheck; {} introduction message(s) remain queued.",
                                pair.match_id, summary.sent, summary.failed, summary.canceled, pending
                            )).await;
                        }
                        Err(error) => {
                            error!(%error, "buddy join failed");
                            let _ = ctx.reply("I could not update the buddy queue.").await;
                        }
                    }
                }
            });
    }

    {
        let state = state.clone();
        bot.command("buddy_leave", "Privately leave the waiting queue")
            .run(move |ctx| {
                let state = state.clone();
                async move {
                    if ctx.msg.is_group {
                        let _ = ctx.reply("Use /buddy_leave in my encrypted DM.").await;
                        return;
                    }
                    let Some(npub) = sender_npub(&ctx.msg) else {
                        return;
                    };
                    match state.leave_buddy_queue(&npub).await {
                        Ok(true) => { let _ = ctx.reply("You left the waiting queue. Your profile remains until /forget.").await; }
                        Ok(false) => { let _ = ctx.reply("You were not in the waiting queue.").await; }
                        Err(error) => error!(%error, "buddy leave failed"),
                    }
                }
            });
    }

    {
        let state = state.clone();
        bot.command("buddy_status", "Privately inspect buddy status")
            .run(move |ctx| {
                let state = state.clone();
                async move {
                    if ctx.msg.is_group {
                        let _ = ctx.reply("Use /buddy_status in my encrypted DM.").await;
                        return;
                    }
                    let Some(npub) = sender_npub(&ctx.msg) else {
                        return;
                    };
                    let text = match state.buddy_status(&npub).await {
                        BuddyStatus::Matched(partner) => format!("Matched with {partner}. Use /forget to remove Porchlight's saved match and profile state."),
                        BuddyStatus::Waiting => "You are waiting for a compatible opt-in buddy.".into(),
                        BuddyStatus::NotQueued => "You have a profile but are not in the queue. Use /buddy to join.".into(),
                        BuddyStatus::NoProfile => "No private profile exists. Start with /intro in the community.".into(),
                    };
                    let _ = ctx.reply(text).await;
                }
            });
    }

    register_information_commands(bot, config.clone());

    {
        let state = state.clone();
        let sender_locks = sender_locks.clone();
        bot.command("forget", "Delete Porchlight's saved state for you")
            .run(move |ctx| {
                let state = state.clone();
                let sender_locks = sender_locks.clone();
                async move {
                    if ctx.msg.is_group {
                        let _ = ctx.reply("For privacy, /forget only works in my encrypted DM.").await;
                        return;
                    }
                    let Some(npub) = sender_npub(&ctx.msg) else {
                        return;
                    };
                    let sender_lock = sender_locks.for_sender(&npub).await;
                    let _guard = sender_lock.lock().await;
                    match state.forget(&npub).await {
                        Ok(true) => {
                            let _ = ctx.reply("Deleted your Porchlight JSON profile, pending session, queue, match and delivery metadata. This cannot delete messages already stored by Vector clients, relays, recipients or backups.").await;
                        }
                        Ok(false) => { let _ = ctx.reply("Porchlight had no application state saved for you. Vector/Nostr message history is outside this deletion.").await; }
                        Err(error) => error!(%error, "forget failed"),
                    }
                }
            });
    }

    {
        let config = config.clone();
        bot.command("help", "Show the onboarding checklist and commands")
            .run(move |ctx| {
                let checklist = config.checklist_text();
                async move {
                    let _ = ctx.reply(format!(
                        "Welcome checklist\n{checklist}\n\nCommands: /resources, /events, /intro, /profile, /buddy, /buddy_leave, /buddy_status, /privacy, /forget. Profile entry, buddy actions and deletion stay in encrypted DMs."
                    )).await;
                }
            });
    }
}

fn register_information_commands(bot: &VectorBot, config: Arc<AppConfig>) {
    {
        let config = config.clone();
        let topics = config.resource_topics();
        let builder = bot.command("resources", "Show operator-curated links by topic");
        let builder = if topics.is_empty() {
            builder
        } else {
            builder.choice("topic", "Resource topic", topics, false)
        };
        builder.run(move |ctx| {
            let config = config.clone();
            async move {
                let selected = ctx.str("topic").map(str::to_lowercase);
                let resources = config
                    .resources
                    .iter()
                    .filter(|resource| {
                        selected
                            .as_ref()
                            .is_none_or(|topic| &resource.topic.to_lowercase() == topic)
                    })
                    .take(8)
                    .map(|resource| {
                        let detail = if resource.description.is_empty() {
                            String::new()
                        } else {
                            format!(" — {}", resource.description)
                        };
                        format!("• [{}]({}){}", resource.label, resource.url, detail)
                    })
                    .collect::<Vec<_>>();
                let text = if resources.is_empty() {
                    let topics = config.resource_topics();
                    if topics.is_empty() {
                        "No resources are configured yet.".into()
                    } else {
                        format!("Topics: {}", topics.join(", "))
                    }
                } else {
                    resources.join("\n")
                };
                let _ = ctx.reply(text).await;
            }
        });
    }

    {
        let config = config.clone();
        bot.command("events", "List configured community gatherings")
            .run(move |ctx| {
                let config = config.clone();
                async move {
                    let events = config
                        .events
                        .iter()
                        .take(6)
                        .map(|event| {
                            let link = if event.url.is_empty() {
                                String::new()
                            } else {
                                format!(" — {}", event.url)
                            };
                            format!(
                                "• **{}** — {} — {}{}",
                                event.title, event.when, event.location, link
                            )
                        })
                        .collect::<Vec<_>>();
                    let text = if events.is_empty() {
                        "No upcoming events are configured.".into()
                    } else {
                        events.join("\n")
                    };
                    let _ = ctx.reply(text).await;
                }
            });
    }

    {
        let config = config.clone();
        bot.command("privacy", "Explain the exact storage and deletion boundary")
            .run(move |ctx| {
                let note = config.privacy.retention_note.clone();
                async move {
                    let _ = ctx.reply(note).await;
                }
            });
    }
}

async fn send_profile_prompt(bot: &VectorBot, state: &StateStore, npub: &str) {
    let Some(scope) = state.pending_membership(npub).await else {
        let _ = bot
            .dm(npub.to_string())
            .send("No active onboarding session exists. Run /intro in the community first.")
            .await;
        return;
    };
    if !member_can_use_scope(bot, &scope, npub).await {
        let _ = bot
            .dm(npub.to_string())
            .send("I could not confirm current membership from my local Concord view. Nothing was stored; retry /profile after the community finishes syncing.")
            .await;
        return;
    }
    match state.prepare_profile_prompt(npub).await {
        Ok(ProfilePrompt::Ready { token, .. }) => {
            let prompt = format!(
                "Reply as one ordinary DM (do not start it with /):\n{token} | UTC+8 | nostr, music\n\nMaximum: 32 timezone characters, 12 interests, 48 characters each. The profile line is encrypted in transit, but Vector 0.8.2 also keeps local event data; protect the bot data directory."
            );
            let _ = bot.dm(npub.to_string()).send(&prompt).await;
        }
        Ok(ProfilePrompt::Missing | ProfilePrompt::Expired) => {
            let _ = bot
                .dm(npub.to_string())
                .send("The onboarding session is missing or expired. Run /intro in the community again.")
                .await;
        }
        Err(error) => error!(%error, "failed to prepare private profile prompt"),
    }
}

async fn process_profile_message(
    bot: &VectorBot,
    state: &StateStore,
    sender_locks: &SenderLocks,
    message: &IncomingMessage,
) {
    let npub = message.chat_id.clone();
    let sender_lock = sender_locks.for_sender(&npub).await;
    let _guard = sender_lock.lock().await;
    process_profile_text(
        bot,
        state,
        &npub,
        &message.message.id,
        message.message.at,
        &message.message.content,
    )
    .await;
}

async fn process_profile_text(
    bot: &VectorBot,
    state: &StateStore,
    npub: &str,
    message_id: &str,
    message_at: u64,
    content: &str,
) {
    match state
        .claim_pending_message(npub, message_id, message_time_secs(message_at))
        .await
    {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            error!(%error, "failed to claim private onboarding message");
            return;
        }
    }

    if content.trim() == "/profile" {
        send_profile_prompt(bot, state, npub).await;
        return;
    }

    let Some(expected_token) = state.active_profile_token(npub).await else {
        let _ = bot
            .dm(npub.to_string())
            .send("Run /profile in this DM before sending profile fields.")
            .await;
        return;
    };
    let parsed = match parse_profile_input(content) {
        Ok(parsed) => parsed,
        Err(message) => {
            let reply = format!("{message}\nTry: {expected_token} | UTC+8 | nostr, music");
            let _ = bot.dm(npub.to_string()).send(&reply).await;
            return;
        }
    };
    let Some(scope) = state.pending_membership(npub).await else {
        return;
    };
    if !member_can_use_scope(bot, &scope, npub).await {
        let _ = bot
            .dm(npub.to_string())
            .send("I could not confirm current membership. No profile was saved; retry after the community finishes syncing.")
            .await;
        return;
    }
    match state
        .complete_profile(npub, &parsed.token, parsed.timezone, parsed.interests)
        .await
    {
        Ok(ProfileCompletion::Saved) => {
            let _ = bot
                .dm(npub.to_string())
                .send("Your minimal profile was saved. Use /buddy to join privately, /buddy_status to inspect it, or /forget to delete Porchlight's application state.")
                .await;
        }
        Ok(ProfileCompletion::TokenMismatch) => {
            let _ = bot
                .dm(npub.to_string())
                .send("That token belongs to a different or older session. Run /profile for the current token.")
                .await;
        }
        Ok(ProfileCompletion::Missing | ProfileCompletion::Expired) => {
            let _ = bot
                .dm(npub.to_string())
                .send("The session expired. Run /intro in the community again.")
                .await;
        }
        Ok(ProfileCompletion::NotAwaiting) => {
            let _ = bot
                .dm(npub.to_string())
                .send("Run /profile before sending profile fields.")
                .await;
        }
        Err(error) => error!(%error, "failed to save private profile"),
    }
}

async fn reconcile_pending_profiles(
    bot: &VectorBot,
    state: &StateStore,
    sender_locks: &SenderLocks,
) {
    for npub in state.pending_profile_members().await {
        let mut messages = bot.dm(npub.clone()).history(100).await;
        messages.sort_by(|left, right| left.at.cmp(&right.at).then_with(|| left.id.cmp(&right.id)));
        let sender_lock = sender_locks.for_sender(&npub).await;
        let _guard = sender_lock.lock().await;
        for message in messages.into_iter().filter(|message| !message.mine) {
            process_profile_text(bot, state, &npub, &message.id, message.at, &message.content)
                .await;
        }
    }
}

async fn member_can_use_scope(bot: &VectorBot, scope: &MembershipScope, npub: &str) -> bool {
    let Some(community) = bot
        .communities()
        .await
        .into_iter()
        .find(|community| community.id() == scope.community_id)
    else {
        return false;
    };
    if community.is_dissolved().await {
        return false;
    }
    if !community
        .members()
        .await
        .iter()
        .any(|member| member.npub() == npub)
    {
        return false;
    }
    let Some(channel) = community
        .channels()
        .await
        .into_iter()
        .find(|channel| channel.id() == scope.channel_id)
    else {
        return false;
    };
    if !channel.is_private() || community.member(npub.to_string()).is_owner() {
        return true;
    }
    community
        .channel_members(&scope.channel_id)
        .iter()
        .any(|member| member.npub() == npub)
}

async fn community_id_for_channel(bot: &VectorBot, channel_id: &str) -> Option<String> {
    for community in bot.communities().await {
        if community
            .channels()
            .await
            .iter()
            .any(|channel| channel.id() == channel_id)
        {
            return Some(community.id().to_string());
        }
    }
    None
}

async fn queue_welcome(
    state: &StateStore,
    config: &AppConfig,
    scope: MembershipScope,
    npub: &str,
) -> Result<()> {
    let label = short_npub(npub);
    let public = config.welcome.public_message.replace("{user}", &label);
    let private = format!(
        "{}\n\n{}\n\n{}",
        config.welcome.dm_intro,
        config.checklist_text(),
        config.privacy.retention_note
    );
    state.queue_welcome(scope, npub, public, private).await
}

async fn flush_outbox(
    bot: &VectorBot,
    state: &StateStore,
    sender_locks: &SenderLocks,
) -> FlushSummary {
    let mut summary = FlushSummary::default();
    let deliveries = match state.claim_deliveries(32).await {
        Ok(deliveries) => deliveries,
        Err(error) => {
            error!(%error, "failed to claim delivery outbox");
            return summary;
        }
    };
    for delivery in deliveries {
        let mut participants = delivery.participants.clone();
        participants.sort();
        participants.dedup();
        let mut locks = Vec::with_capacity(participants.len());
        for participant in participants {
            locks.push(sender_locks.for_sender(&participant).await);
        }
        let mut guards = Vec::with_capacity(locks.len());
        for lock in &locks {
            guards.push(lock.lock().await);
        }
        if !state.delivery_is_pending(&delivery.id).await {
            continue;
        }
        if delivery.participants.len() <= 1 {
            let participant = delivery.participants.first();
            let membership_valid = match (participant, delivery.membership_scope.as_ref()) {
                (Some(participant), Some(scope)) => {
                    member_can_use_scope(bot, scope, participant).await
                }
                _ => false,
            };
            if !membership_valid {
                warn!(delivery = %delivery.id, "single-member delivery canceled because its current membership scope could not be confirmed");
                match state.cancel_delivery(&delivery.id).await {
                    Ok(true) => summary.canceled += 1,
                    Ok(false) => {}
                    Err(error) => {
                        summary.failed += 1;
                        error!(delivery = %delivery.id, %error, "failed to persist invalid delivery cancellation; nothing was sent and the lease remains for retry");
                    }
                }
                continue;
            }
        }
        if delivery.participants.len() > 1 {
            let mut invalid_member = None;
            for participant in &delivery.participants {
                let Some(scope) = state.profile_membership(participant).await else {
                    invalid_member = Some(participant.clone());
                    break;
                };
                if !member_can_use_scope(bot, &scope, participant).await {
                    invalid_member = Some(participant.clone());
                    break;
                }
            }
            if let Some(invalid_member) = invalid_member {
                warn!(delivery = %delivery.id, member = %invalid_member, "buddy delivery canceled because current membership could not be confirmed");
                match state.cancel_buddy_match(&invalid_member).await {
                    Ok(true) => summary.canceled += 1,
                    Ok(false) => {}
                    Err(error) => {
                        summary.failed += 1;
                        error!(delivery = %delivery.id, %error, "failed to persist invalid buddy-match cancellation; nothing was sent and the lease remains for retry");
                    }
                }
                continue;
            }
        }
        let result = match &delivery.target {
            DeliveryTarget::Direct(npub) => bot.dm(npub.clone()).send(&delivery.body).await,
            DeliveryTarget::Channel(channel_id) => {
                bot.channel(channel_id.clone()).send(&delivery.body).await
            }
        };
        match result {
            Ok(_) => {
                summary.sent += 1;
                if let Err(error) = state.acknowledge_delivery(&delivery.id).await {
                    error!(delivery = %delivery.id, %error, "delivery sent but acknowledgement was not persisted; a later retry may duplicate the message");
                }
            }
            Err(send_error) => {
                summary.failed += 1;
                warn!(delivery = %delivery.id, error = %send_error, "delivery remains queued");
                if let Err(error) = state.release_delivery(&delivery.id).await {
                    error!(delivery = %delivery.id, %error, "failed to release delivery lease");
                }
            }
        }
    }
    summary
}

fn sender_npub(message: &IncomingMessage) -> Option<String> {
    if message.is_group {
        message.message.npub.clone()
    } else {
        Some(message.chat_id.clone())
    }
}

fn message_time_secs(value: u64) -> u64 {
    if value > 10_000_000_000 {
        value / 1_000
    } else {
        value
    }
}
