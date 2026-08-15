use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncWriteExt, sync::Mutex};

const CURRENT_VERSION: u8 = 3;
const ONBOARDING_TTL_SECS: u64 = 15 * 60;
const BUDDY_QUEUE_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const DELIVERY_LEASE_SECS: u64 = 60;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct StateStore {
    path: PathBuf,
    inner: Arc<Mutex<PersistedState>>,
    _lock: Arc<File>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedState {
    version: u8,
    #[serde(default)]
    next_session_id: u64,
    #[serde(default)]
    next_match_id: u64,
    #[serde(default)]
    next_delivery_id: u64,
    #[serde(default)]
    pending: BTreeMap<String, PendingOnboarding>,
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    waiting: BTreeMap<String, BTreeMap<String, u64>>,
    #[serde(default)]
    matches: BTreeMap<String, MatchRecord>,
    #[serde(default)]
    deliveries: BTreeMap<String, DeliveryRecord>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            next_session_id: 0,
            next_match_id: 0,
            next_delivery_id: 0,
            pending: BTreeMap::new(),
            profiles: BTreeMap::new(),
            waiting: BTreeMap::new(),
            matches: BTreeMap::new(),
            deliveries: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingOnboarding {
    community_id: String,
    channel_id: String,
    token: String,
    source_message_id: String,
    created_at: u64,
    expires_at: u64,
    awaiting_input: bool,
    #[serde(default)]
    processed_message_ids: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Profile {
    npub: String,
    community_id: String,
    channel_id: String,
    interests: Vec<String>,
    timezone: String,
    updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MatchRecord {
    partner: String,
    scope: String,
    match_id: String,
    created_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeliveryRecord {
    id: String,
    dedupe_key: String,
    participants: Vec<String>,
    target: DeliveryTarget,
    body: Option<String>,
    created_at: u64,
    lease_until: Option<u64>,
    sent_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum DeliveryTarget {
    Direct(String),
    Channel(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimedDelivery {
    pub id: String,
    pub participants: Vec<String>,
    pub target: DeliveryTarget,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipScope {
    pub community_id: String,
    pub channel_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BeginOnboarding {
    Started,
    Refreshed,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfilePrompt {
    Ready { token: String, expires_at: u64 },
    Missing,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileCompletion {
    Saved,
    Missing,
    Expired,
    NotAwaiting,
    TokenMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuddyMatch {
    pub match_id: String,
    pub first: String,
    pub second: String,
    pub shared_interests: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinOutcome {
    Waiting,
    NewlyMatched(BuddyMatch),
    AlreadyMatched(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuddyStatus {
    Matched(String),
    Waiting,
    NotQueued,
    NoProfile,
}

impl StateStore {
    pub async fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = parent_dir(&path) {
            prepare_private_directory(parent).await?;
        }

        reject_insecure_existing_state(&path).await?;

        let lock_path = sidecar_path(&path, ".lock");
        reject_symlink_if_present(&lock_path).await?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open state lock {}", lock_path.display()))?;
        secure_open_file(&lock, &lock_path)?;
        FileExt::try_lock_exclusive(&lock).with_context(|| {
            format!(
                "state file is already in use by another Porchlight process: {}",
                path.display()
            )
        })?;

        let state = match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("failed to parse state file {}", path.display()))?;
                let version = value
                    .get("version")
                    .and_then(serde_json::Value::as_u64)
                    .context("state file is missing a numeric version")?;
                if version != u64::from(CURRENT_VERSION) {
                    bail!("unsupported state schema version {version}; expected {CURRENT_VERSION}");
                }
                serde_json::from_value(value)
                    .with_context(|| format!("failed to decode state file {}", path.display()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PersistedState::default(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read state file {}", path.display()))
            }
        };

        Ok(Self {
            path,
            inner: Arc::new(Mutex::new(state)),
            _lock: Arc::new(lock),
        })
    }

    pub async fn begin_onboarding(
        &self,
        npub: &str,
        community_id: &str,
        channel_id: &str,
        source_message_id: &str,
    ) -> Result<BeginOnboarding> {
        self.begin_onboarding_at(
            npub,
            community_id,
            channel_id,
            source_message_id,
            now_secs(),
        )
        .await
    }

    async fn begin_onboarding_at(
        &self,
        npub: &str,
        community_id: &str,
        channel_id: &str,
        source_message_id: &str,
        now: u64,
    ) -> Result<BeginOnboarding> {
        let npub = npub.to_string();
        let community_id = community_id.to_string();
        let channel_id = channel_id.to_string();
        let source_message_id = source_message_id.to_string();
        self.update(move |state| {
            let existing = state.pending.get(&npub).cloned();
            if let Some(pending) = existing.as_ref() {
                if pending.expires_at > now
                    && (pending.community_id != community_id || pending.channel_id != channel_id)
                {
                    return Ok(BeginOnboarding::Conflict);
                }
                if pending.expires_at > now && pending.source_message_id == source_message_id {
                    return Ok(BeginOnboarding::Refreshed);
                }
            }

            let refreshed = existing.as_ref().is_some_and(|pending| {
                pending.expires_at > now
                    && pending.community_id == community_id
                    && pending.channel_id == channel_id
            });
            state.next_session_id = next_counter(state.next_session_id, "onboarding session")?;
            let token = format!("PLP-{:08X}", state.next_session_id);
            state.pending.insert(
                npub,
                PendingOnboarding {
                    community_id,
                    channel_id,
                    token,
                    source_message_id,
                    created_at: now,
                    expires_at: now.saturating_add(ONBOARDING_TTL_SECS),
                    awaiting_input: false,
                    processed_message_ids: BTreeSet::new(),
                },
            );
            Ok(if refreshed {
                BeginOnboarding::Refreshed
            } else {
                BeginOnboarding::Started
            })
        })
        .await
    }

    pub async fn prepare_profile_prompt(&self, npub: &str) -> Result<ProfilePrompt> {
        self.prepare_profile_prompt_at(npub, now_secs()).await
    }

    async fn prepare_profile_prompt_at(&self, npub: &str, now: u64) -> Result<ProfilePrompt> {
        let npub = npub.to_string();
        self.update(move |state| {
            let Some(pending) = state.pending.get_mut(&npub) else {
                return Ok(ProfilePrompt::Missing);
            };
            if pending.expires_at <= now {
                state.pending.remove(&npub);
                return Ok(ProfilePrompt::Expired);
            }
            pending.awaiting_input = true;
            Ok(ProfilePrompt::Ready {
                token: pending.token.clone(),
                expires_at: pending.expires_at,
            })
        })
        .await
    }

    pub async fn active_profile_token(&self, npub: &str) -> Option<String> {
        let now = now_secs();
        self.inner
            .lock()
            .await
            .pending
            .get(npub)
            .filter(|pending| pending.awaiting_input && pending.expires_at > now)
            .map(|pending| pending.token.clone())
    }

    pub async fn pending_profile_members(&self) -> Vec<String> {
        let now = now_secs();
        self.inner
            .lock()
            .await
            .pending
            .iter()
            .filter(|(_, pending)| pending.expires_at > now)
            .map(|(npub, _)| npub.clone())
            .collect()
    }

    pub async fn claim_pending_message(
        &self,
        npub: &str,
        message_id: &str,
        message_at_secs: u64,
    ) -> Result<bool> {
        let now = now_secs();
        let npub = npub.to_string();
        let message_id = message_id.to_string();
        self.update(move |state| {
            let Some(pending) = state.pending.get_mut(&npub) else {
                return Ok(false);
            };
            if pending.expires_at <= now || message_at_secs < pending.created_at {
                return Ok(false);
            }
            if pending.processed_message_ids.contains(&message_id) {
                return Ok(false);
            }
            while pending.processed_message_ids.len() >= 64 {
                let Some(oldest) = pending.processed_message_ids.iter().next().cloned() else {
                    break;
                };
                pending.processed_message_ids.remove(&oldest);
            }
            pending.processed_message_ids.insert(message_id);
            Ok(true)
        })
        .await
    }

    pub async fn pending_membership(&self, npub: &str) -> Option<MembershipScope> {
        let now = now_secs();
        self.inner
            .lock()
            .await
            .pending
            .get(npub)
            .filter(|pending| pending.expires_at > now)
            .map(|pending| MembershipScope {
                community_id: pending.community_id.clone(),
                channel_id: pending.channel_id.clone(),
            })
    }

    pub async fn profile_membership(&self, npub: &str) -> Option<MembershipScope> {
        self.inner
            .lock()
            .await
            .profiles
            .get(npub)
            .map(|profile| MembershipScope {
                community_id: profile.community_id.clone(),
                channel_id: profile.channel_id.clone(),
            })
    }

    pub async fn complete_profile(
        &self,
        npub: &str,
        token: &str,
        timezone: String,
        interests: Vec<String>,
    ) -> Result<ProfileCompletion> {
        let npub = npub.to_string();
        let token = token.to_string();
        let now = now_secs();
        self.update(move |state| {
            let Some(pending) = state.pending.get(&npub).cloned() else {
                return Ok(ProfileCompletion::Missing);
            };
            if pending.expires_at <= now {
                state.pending.remove(&npub);
                return Ok(ProfileCompletion::Expired);
            }
            if !pending.awaiting_input {
                return Ok(ProfileCompletion::NotAwaiting);
            }
            if pending.token != token {
                return Ok(ProfileCompletion::TokenMismatch);
            }

            let scope_changed = state.profiles.get(&npub).is_some_and(|profile| {
                profile.community_id != pending.community_id
                    || profile.channel_id != pending.channel_id
            });
            if scope_changed {
                cleanup_member_relations(state, &npub);
            }
            state.profiles.insert(
                npub.clone(),
                Profile {
                    npub: npub.clone(),
                    community_id: pending.community_id,
                    channel_id: pending.channel_id,
                    interests,
                    timezone,
                    updated_at: now,
                },
            );
            state.pending.remove(&npub);
            Ok(ProfileCompletion::Saved)
        })
        .await
    }

    pub async fn join_buddy_queue(&self, npub: &str) -> Result<JoinOutcome> {
        let npub = npub.to_string();
        let now = now_secs();
        self.update(move |state| {
            prune_waiting(state, now);
            let Some(profile) = state.profiles.get(&npub).cloned() else {
                bail!("a private profile is required before joining the buddy queue");
            };

            if let Some(existing) = state.matches.get(&npub) {
                if existing.scope == profile.community_id {
                    return Ok(JoinOutcome::AlreadyMatched(existing.partner.clone()));
                }
                cleanup_member_relations(state, &npub);
            }

            let mut queue = state
                .waiting
                .remove(&profile.community_id)
                .unwrap_or_default();
            queue.retain(|candidate, expires_at| {
                candidate != &npub
                    && *expires_at > now
                    && !state.matches.contains_key(candidate)
                    && state
                        .profiles
                        .get(candidate)
                        .is_some_and(|other| other.community_id == profile.community_id)
            });

            let candidate = best_candidate(&state.profiles, queue.keys(), &npub);
            let outcome = if let Some(candidate) = candidate {
                queue.remove(&candidate);
                state.next_match_id = next_counter(state.next_match_id, "buddy match")?;
                let match_id = format!("PLM-{:08X}", state.next_match_id);
                let shared = shared_interests(&state.profiles, &npub, &candidate);
                state.matches.insert(
                    npub.clone(),
                    MatchRecord {
                        partner: candidate.clone(),
                        scope: profile.community_id.clone(),
                        match_id: match_id.clone(),
                        created_at: now,
                    },
                );
                state.matches.insert(
                    candidate.clone(),
                    MatchRecord {
                        partner: npub.clone(),
                        scope: profile.community_id.clone(),
                        match_id: match_id.clone(),
                        created_at: now,
                    },
                );

                let pair = BuddyMatch {
                    match_id,
                    first: npub.clone(),
                    second: candidate,
                    shared_interests: shared,
                };
                queue_buddy_deliveries(state, &pair, now)?;
                JoinOutcome::NewlyMatched(pair)
            } else {
                queue.insert(npub.clone(), now.saturating_add(BUDDY_QUEUE_TTL_SECS));
                JoinOutcome::Waiting
            };

            state.waiting.insert(profile.community_id, queue);
            Ok(outcome)
        })
        .await
    }

    pub async fn leave_buddy_queue(&self, npub: &str) -> Result<bool> {
        let npub = npub.to_string();
        self.update(move |state| {
            let Some(scope) = state
                .profiles
                .get(&npub)
                .map(|profile| profile.community_id.clone())
            else {
                return Ok(false);
            };
            Ok(state
                .waiting
                .get_mut(&scope)
                .is_some_and(|queue| queue.remove(&npub).is_some()))
        })
        .await
    }

    pub async fn buddy_status(&self, npub: &str) -> BuddyStatus {
        let now = now_secs();
        let state = self.inner.lock().await;
        let Some(profile) = state.profiles.get(npub) else {
            return BuddyStatus::NoProfile;
        };
        if let Some(record) = state.matches.get(npub) {
            if record.scope == profile.community_id {
                return BuddyStatus::Matched(record.partner.clone());
            }
        }
        if state
            .waiting
            .get(&profile.community_id)
            .and_then(|queue| queue.get(npub))
            .is_some_and(|expires_at| *expires_at > now)
        {
            BuddyStatus::Waiting
        } else {
            BuddyStatus::NotQueued
        }
    }

    pub async fn forget(&self, npub: &str) -> Result<bool> {
        let npub = npub.to_string();
        self.update(move |state| {
            let mut existed = state.pending.remove(&npub).is_some();
            existed |= state.profiles.remove(&npub).is_some();
            existed |= cleanup_member_relations(state, &npub);
            Ok(existed)
        })
        .await
    }

    pub async fn queue_welcome(
        &self,
        scope: &str,
        npub: &str,
        public_body: String,
        private_body: String,
    ) -> Result<()> {
        let scope = scope.to_string();
        let npub = npub.to_string();
        let now = now_secs();
        self.update(move |state| {
            queue_delivery(
                state,
                format!("welcome:public:{scope}:{npub}"),
                vec![npub.clone()],
                DeliveryTarget::Channel(scope.clone()),
                public_body,
                now,
            )?;
            queue_delivery(
                state,
                format!("welcome:private:{scope}:{npub}"),
                vec![npub.clone()],
                DeliveryTarget::Direct(npub),
                private_body,
                now,
            )?;
            Ok(())
        })
        .await
    }

    pub async fn claim_deliveries(&self, limit: usize) -> Result<Vec<ClaimedDelivery>> {
        let now = now_secs();
        let mut current = self.inner.lock().await;
        let mut next = current.clone();
        let mut claimed = Vec::new();
        for record in next.deliveries.values_mut() {
            if claimed.len() >= limit {
                break;
            }
            if record.sent_at.is_some()
                || record.body.is_none()
                || record.lease_until.is_some_and(|lease| lease > now)
            {
                continue;
            }
            record.lease_until = Some(now.saturating_add(DELIVERY_LEASE_SECS));
            claimed.push(ClaimedDelivery {
                id: record.id.clone(),
                participants: record.participants.clone(),
                target: record.target.clone(),
                body: record.body.clone().unwrap_or_default(),
            });
        }
        if claimed.is_empty() {
            return Ok(claimed);
        }
        self.save_atomic(&next).await?;
        *current = next;
        Ok(claimed)
    }

    pub async fn delivery_is_pending(&self, id: &str) -> bool {
        self.inner
            .lock()
            .await
            .deliveries
            .get(id)
            .is_some_and(|record| record.sent_at.is_none() && record.body.is_some())
    }

    pub async fn acknowledge_delivery(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        let now = now_secs();
        self.update(move |state| {
            let Some(record) = state.deliveries.get_mut(&id) else {
                return Ok(false);
            };
            record.sent_at = Some(now);
            record.lease_until = None;
            record.body = None;
            Ok(true)
        })
        .await
    }

    pub async fn release_delivery(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.update(move |state| {
            let Some(record) = state.deliveries.get_mut(&id) else {
                return Ok(false);
            };
            if record.sent_at.is_none() {
                record.lease_until = None;
            }
            Ok(true)
        })
        .await
    }

    pub async fn pending_delivery_count(&self) -> usize {
        self.inner
            .lock()
            .await
            .deliveries
            .values()
            .filter(|record| record.sent_at.is_none() && record.body.is_some())
            .count()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn update<T>(&self, apply: impl FnOnce(&mut PersistedState) -> Result<T>) -> Result<T> {
        let mut current = self.inner.lock().await;
        let mut next = current.clone();
        let value = apply(&mut next)?;
        self.save_atomic(&next).await?;
        *current = next;
        Ok(value)
    }

    async fn save_atomic(&self, state: &PersistedState) -> Result<()> {
        if let Some(parent) = parent_dir(&self.path) {
            prepare_private_directory(parent).await?;
        }

        let bytes = serde_json::to_vec_pretty(state)?;
        let temp = unique_temp_path(&self.path);
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .await
            .with_context(|| format!("failed to create temporary state file {}", temp.display()))?;
        secure_async_file(&file, &temp).await?;

        let write_result = async {
            file.write_all(&bytes).await?;
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            let source = temp.clone();
            let destination = self.path.clone();
            tokio::task::spawn_blocking(move || replace_atomically(&source, &destination))
                .await
                .context("atomic state replacement task failed")??;
            Result::<(), anyhow::Error>::Ok(())
        }
        .await;

        if write_result.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
        }
        write_result.with_context(|| format!("failed to persist state {}", self.path.display()))
    }
}

fn queue_buddy_deliveries(state: &mut PersistedState, pair: &BuddyMatch, now: u64) -> Result<()> {
    let shared = pair
        .shared_interests
        .iter()
        .map(|interest| crate::escape_markdown(interest))
        .collect::<Vec<_>>()
        .join(", ");
    let participants = vec![pair.first.clone(), pair.second.clone()];
    let first_body = format!(
        "Porchlight introduction {}\nBuddy: {}\nShared interests: {}\nBoth people opted in. Say hello only if you want; use /forget at any time.",
        pair.match_id, pair.second, shared
    );
    let second_body = format!(
        "Porchlight introduction {}\nBuddy: {}\nShared interests: {}\nBoth people opted in. Say hello only if you want; use /forget at any time.",
        pair.match_id, pair.first, shared
    );
    queue_delivery(
        state,
        format!("buddy:{}:{}", pair.match_id, pair.first),
        participants.clone(),
        DeliveryTarget::Direct(pair.first.clone()),
        first_body,
        now,
    )?;
    queue_delivery(
        state,
        format!("buddy:{}:{}", pair.match_id, pair.second),
        participants,
        DeliveryTarget::Direct(pair.second.clone()),
        second_body,
        now,
    )?;
    Ok(())
}

fn queue_delivery(
    state: &mut PersistedState,
    dedupe_key: String,
    participants: Vec<String>,
    target: DeliveryTarget,
    body: String,
    now: u64,
) -> Result<()> {
    if state
        .deliveries
        .values()
        .any(|record| record.dedupe_key == dedupe_key)
    {
        return Ok(());
    }
    state.next_delivery_id = next_counter(state.next_delivery_id, "delivery")?;
    let id = format!("PLD-{:08X}", state.next_delivery_id);
    state.deliveries.insert(
        id.clone(),
        DeliveryRecord {
            id,
            dedupe_key,
            participants,
            target,
            body: Some(body),
            created_at: now,
            lease_until: None,
            sent_at: None,
        },
    );
    Ok(())
}

fn cleanup_member_relations(state: &mut PersistedState, npub: &str) -> bool {
    let mut existed = false;
    for queue in state.waiting.values_mut() {
        existed |= queue.remove(npub).is_some();
    }
    if let Some(record) = state.matches.remove(npub) {
        state.matches.remove(&record.partner);
        existed = true;
    }
    let before = state.deliveries.len();
    state
        .deliveries
        .retain(|_, delivery| !delivery.participants.iter().any(|member| member == npub));
    existed |= before != state.deliveries.len();
    existed
}

fn prune_waiting(state: &mut PersistedState, now: u64) {
    let profiles = &state.profiles;
    let matches = &state.matches;
    state.waiting.retain(|scope, queue| {
        queue.retain(|npub, expires_at| {
            *expires_at > now
                && !matches.contains_key(npub)
                && profiles
                    .get(npub)
                    .is_some_and(|profile| &profile.community_id == scope)
        });
        !queue.is_empty()
    });
}

fn best_candidate<'a>(
    profiles: &BTreeMap<String, Profile>,
    queue: impl Iterator<Item = &'a String>,
    npub: &str,
) -> Option<String> {
    let profile = profiles.get(npub)?;
    queue
        .filter_map(|candidate| {
            let other = profiles.get(candidate)?;
            if other.community_id != profile.community_id {
                return None;
            }
            let shared = overlap_count(&profile.interests, &other.interests);
            if shared == 0 {
                return None;
            }
            let timezone_bonus =
                usize::from(!profile.timezone.is_empty() && profile.timezone == other.timezone);
            Some((candidate.clone(), shared * 10 + timezone_bonus))
        })
        .max_by(|(left_npub, left_score), (right_npub, right_score)| {
            left_score
                .cmp(right_score)
                .then_with(|| right_npub.cmp(left_npub))
        })
        .map(|(candidate, _)| candidate)
}

fn overlap_count(left: &[String], right: &[String]) -> usize {
    let right = right.iter().collect::<BTreeSet<_>>();
    left.iter().filter(|item| right.contains(item)).count()
}

fn shared_interests(
    profiles: &BTreeMap<String, Profile>,
    first: &str,
    second: &str,
) -> Vec<String> {
    let Some(first) = profiles.get(first) else {
        return vec![];
    };
    let Some(second) = profiles.get(second) else {
        return vec![];
    };
    first
        .interests
        .iter()
        .filter(|item| second.interests.contains(item))
        .cloned()
        .collect()
}

fn next_counter(current: u64, label: &str) -> Result<u64> {
    current
        .checked_add(1)
        .with_context(|| format!("{label} counter exhausted"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

async fn prepare_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    let existed = tokio::fs::try_exists(path)
        .await
        .with_context(|| format!("failed to inspect private directory {}", path.display()))?;

    tokio::fs::create_dir_all(path)
        .await
        .with_context(|| format!("failed to create private directory {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = tokio::fs::symlink_metadata(path)
            .await
            .with_context(|| format!("failed to inspect private directory {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "private directory must be a real directory, not a symlink: {}",
                path.display()
            );
        }
        if existed {
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                bail!(
                    "private directory {} has mode {mode:03o}; restrict it to 700 before running Porchlight",
                    path.display()
                );
            }
        } else {
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .await
                .with_context(|| {
                    format!("failed to set private directory mode on {}", path.display())
                })?;
        }
    }

    Ok(())
}

async fn reject_insecure_existing_state(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    bail!(
                        "state path must be a regular file, not a symlink: {}",
                        path.display()
                    );
                }
                let mode = metadata.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    bail!(
                        "state file {} has mode {mode:03o}; restrict it to 600 before running Porchlight",
                        path.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect state file {}", path.display()))
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

async fn reject_symlink_if_present(path: &Path) -> Result<()> {
    #[cfg(unix)]
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing to use a symlinked lock file: {}", path.display())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect lock file {}", path.display()))
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn secure_open_file(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set private file mode on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (file, path);
    Ok(())
}

async fn secure_async_file(file: &tokio::fs::File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .await
            .with_context(|| format!("failed to set private file mode on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (file, path);
    Ok(())
}

fn parent_dir(path: &Path) -> Option<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

fn unique_temp_path(path: &Path) -> PathBuf {
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let suffix = format!(".{}.{}.tmp", std::process::id(), counter);
    sidecar_path(path, &suffix)
}

#[cfg(not(windows))]
fn replace_atomically(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_atomically(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    if !destination.exists() {
        return std::fs::rename(source, destination);
    }

    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            source.as_ptr(),
            ptr::null(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn store() -> (TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateStore::load(dir.path().join("state.json"))
            .await
            .unwrap();
        (dir, state)
    }

    async fn profile(store: &StateStore, npub: &str, scope: &str, interests: &[&str]) {
        let channel = format!("{scope}-general");
        store
            .begin_onboarding(npub, scope, &channel, &format!("intro-{npub}-{scope}"))
            .await
            .unwrap();
        let token = match store.prepare_profile_prompt(npub).await.unwrap() {
            ProfilePrompt::Ready { token, .. } => token,
            other => panic!("unexpected prompt: {other:?}"),
        };
        assert_eq!(
            store
                .complete_profile(
                    npub,
                    &token,
                    "UTC+8".into(),
                    interests.iter().map(|value| value.to_string()).collect(),
                )
                .await
                .unwrap(),
            ProfileCompletion::Saved
        );
    }

    #[tokio::test]
    async fn profile_session_is_token_bound_and_consumed() {
        let (_dir, store) = store().await;
        store
            .begin_onboarding("alice", "community", "lounge", "intro-1")
            .await
            .unwrap();
        let token = match store.prepare_profile_prompt("alice").await.unwrap() {
            ProfilePrompt::Ready { token, .. } => token,
            other => panic!("unexpected prompt: {other:?}"),
        };
        assert_eq!(
            store
                .complete_profile("alice", "PLP-DELAYED", "UTC+8".into(), vec!["nostr".into()],)
                .await
                .unwrap(),
            ProfileCompletion::TokenMismatch
        );
        assert_eq!(
            store
                .complete_profile("alice", &token, "UTC+8".into(), vec!["nostr".into()],)
                .await
                .unwrap(),
            ProfileCompletion::Saved
        );
        assert_eq!(store.active_profile_token("alice").await, None);
    }

    #[tokio::test]
    async fn active_session_cannot_be_overwritten_by_another_scope() {
        let (_dir, store) = store().await;
        assert_eq!(
            store
                .begin_onboarding("alice", "community", "lounge", "intro-1")
                .await
                .unwrap(),
            BeginOnboarding::Started
        );
        assert_eq!(
            store
                .begin_onboarding("alice", "community", "makers", "intro-2")
                .await
                .unwrap(),
            BeginOnboarding::Conflict
        );
    }

    #[tokio::test]
    async fn expired_session_is_rejected() {
        let (_dir, store) = store().await;
        store
            .begin_onboarding_at("alice", "community", "lounge", "intro-1", 100)
            .await
            .unwrap();
        assert_eq!(
            store
                .prepare_profile_prompt_at("alice", 100 + ONBOARDING_TTL_SECS)
                .await
                .unwrap(),
            ProfilePrompt::Expired
        );
    }

    #[tokio::test]
    async fn pending_messages_are_time_bound_and_deduplicated() {
        let (_dir, store) = store().await;
        let now = now_secs();
        store
            .begin_onboarding_at("alice", "community", "lounge", "intro-1", now)
            .await
            .unwrap();
        assert!(!store
            .claim_pending_message("alice", "old-message", now.saturating_sub(1))
            .await
            .unwrap());
        assert!(store
            .claim_pending_message("alice", "new-message", now.saturating_add(1))
            .await
            .unwrap());
        assert!(!store
            .claim_pending_message("alice", "new-message", now.saturating_add(1))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn matches_the_candidate_with_most_shared_interests() {
        let (_dir, store) = store().await;
        profile(&store, "alice", "lounge", &["nostr", "music"]).await;
        profile(&store, "bob", "lounge", &["gardening"]).await;
        profile(&store, "carol", "lounge", &["music", "nostr"]).await;
        assert_eq!(
            store.join_buddy_queue("bob").await.unwrap(),
            JoinOutcome::Waiting
        );
        assert_eq!(
            store.join_buddy_queue("carol").await.unwrap(),
            JoinOutcome::Waiting
        );

        let JoinOutcome::NewlyMatched(matched) = store.join_buddy_queue("alice").await.unwrap()
        else {
            panic!("expected a new match")
        };
        assert_eq!(matched.second, "carol");
        assert_eq!(matched.shared_interests, vec!["nostr", "music"]);
        assert_eq!(store.buddy_status("bob").await, BuddyStatus::Waiting);
    }

    #[tokio::test]
    async fn timezone_alone_does_not_create_a_match() {
        let (_dir, store) = store().await;
        profile(&store, "alice", "lounge", &["music"]).await;
        profile(&store, "bob", "lounge", &["gardening"]).await;
        store.join_buddy_queue("bob").await.unwrap();
        assert_eq!(
            store.join_buddy_queue("alice").await.unwrap(),
            JoinOutcome::Waiting
        );
    }

    #[tokio::test]
    async fn duplicate_join_only_creates_one_match() {
        let (_dir, store) = store().await;
        profile(&store, "alice", "lounge", &["nostr"]).await;
        profile(&store, "bob", "lounge", &["nostr"]).await;
        store.join_buddy_queue("bob").await.unwrap();

        let (first, second) = tokio::join!(
            store.join_buddy_queue("alice"),
            store.join_buddy_queue("alice")
        );
        let outcomes = [first.unwrap(), second.unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, JoinOutcome::NewlyMatched(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, JoinOutcome::AlreadyMatched(_)))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn changing_scope_clears_an_old_match_and_delivery() {
        let (_dir, store) = store().await;
        profile(&store, "alice", "lounge", &["nostr"]).await;
        profile(&store, "bob", "lounge", &["nostr"]).await;
        store.join_buddy_queue("bob").await.unwrap();
        store.join_buddy_queue("alice").await.unwrap();
        assert_eq!(store.pending_delivery_count().await, 2);

        profile(&store, "alice", "makers", &["rust"]).await;
        assert_eq!(store.buddy_status("bob").await, BuddyStatus::NotQueued);
        assert_eq!(store.buddy_status("alice").await, BuddyStatus::NotQueued);
        assert_eq!(store.pending_delivery_count().await, 0);
    }

    #[tokio::test]
    async fn delivery_outbox_claims_acks_and_retries() {
        let (_dir, store) = store().await;
        profile(&store, "alice", "lounge", &["nostr"]).await;
        profile(&store, "bob", "lounge", &["nostr"]).await;
        store.join_buddy_queue("bob").await.unwrap();
        store.join_buddy_queue("alice").await.unwrap();

        let claimed = store.claim_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 2);
        store.acknowledge_delivery(&claimed[0].id).await.unwrap();
        store.release_delivery(&claimed[1].id).await.unwrap();
        let retry = store.claim_deliveries(10).await.unwrap();
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].id, claimed[1].id);
    }

    #[tokio::test]
    async fn an_expired_delivery_lease_can_be_claimed_again() {
        let (_dir, store) = store().await;
        profile(&store, "alice", "lounge", &["nostr"]).await;
        profile(&store, "bob", "lounge", &["nostr"]).await;
        store.join_buddy_queue("bob").await.unwrap();
        store.join_buddy_queue("alice").await.unwrap();

        let claimed = store.claim_deliveries(1).await.unwrap();
        assert_eq!(claimed.len(), 1);
        store
            .update(|state| {
                state
                    .deliveries
                    .get_mut(&claimed[0].id)
                    .unwrap()
                    .lease_until = Some(0);
                Ok(())
            })
            .await
            .unwrap();

        let retried = store.claim_deliveries(1).await.unwrap();
        assert_eq!(retried.len(), 1);
        assert_eq!(retried[0].id, claimed[0].id);
    }

    #[tokio::test]
    async fn forget_removes_a_delivery_even_after_it_was_claimed() {
        let (_dir, store) = store().await;
        profile(&store, "alice", "lounge", &["nostr"]).await;
        profile(&store, "bob", "lounge", &["nostr"]).await;
        store.join_buddy_queue("bob").await.unwrap();
        store.join_buddy_queue("alice").await.unwrap();

        let claimed = store.claim_deliveries(2).await.unwrap();
        assert_eq!(claimed.len(), 2);
        assert!(claimed
            .iter()
            .all(|delivery| delivery.participants.iter().any(|member| member == "alice")));

        assert!(store.forget("alice").await.unwrap());
        for delivery in claimed {
            assert!(!store.delivery_is_pending(&delivery.id).await);
        }
    }

    #[tokio::test]
    async fn welcome_delivery_is_idempotent() {
        let (_dir, store) = store().await;
        for _ in 0..2 {
            store
                .queue_welcome("lounge", "alice", "public".into(), "private".into())
                .await
                .unwrap();
        }
        assert_eq!(store.pending_delivery_count().await, 2);
    }

    #[tokio::test]
    async fn second_process_cannot_open_the_same_state() {
        let (dir, store) = store().await;
        let second = StateStore::load(dir.path().join("state.json")).await;
        assert!(second.is_err());
        drop(store);
        assert!(StateStore::load(dir.path().join("state.json"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn state_survives_drop_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let store = StateStore::load(path.clone()).await.unwrap();
            profile(&store, "alice", "lounge", &["nostr"]).await;
        }
        let reloaded = StateStore::load(path).await.unwrap();
        assert_eq!(reloaded.buddy_status("alice").await, BuddyStatus::NotQueued);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn state_and_lock_files_are_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, store) = store().await;
        profile(&store, "alice", "lounge", &["nostr"]).await;

        let state_mode = std::fs::metadata(dir.path().join("state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let lock_mode = std::fs::metadata(dir.path().join("state.json.lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(state_mode, 0o600);
        assert_eq!(lock_mode, 0o600);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn insecure_existing_state_file_fails_closed_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, br#"{"version":3}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let error = StateStore::load(&path).await.unwrap_err();
        assert!(error.to_string().contains("restrict it to 600"));
    }

    #[tokio::test]
    async fn failed_persistence_does_not_advance_memory_state() {
        let (dir, mut store) = store().await;
        store.path = dir.path().to_path_buf();
        assert!(store
            .begin_onboarding("alice", "community", "lounge", "intro-1")
            .await
            .is_err());
        assert!(store.pending_membership("alice").await.is_none());
    }

    #[tokio::test]
    async fn unknown_schema_version_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        tokio::fs::write(&path, br#"{"version":99}"#).await.unwrap();
        let error = StateStore::load(path).await.unwrap_err();
        assert!(error.to_string().contains("unsupported state schema"));
    }
}
