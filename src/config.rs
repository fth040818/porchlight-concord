use anyhow::{bail, Context, Result};
use nostr_sdk::prelude::PublicKey;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Deserialize)]
pub struct AppConfig {
    pub bot: BotConfig,
    pub welcome: WelcomeConfig,
    #[serde(default)]
    pub resources: Vec<Resource>,
    #[serde(default)]
    pub events: Vec<CommunityEvent>,
    pub privacy: PrivacyConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BotConfig {
    pub name: String,
    pub about: String,
    pub data_dir: PathBuf,
    pub state_file: PathBuf,
    #[serde(default)]
    pub invite_policy: InvitePolicy,
    #[serde(default)]
    pub owner_npubs: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InvitePolicy {
    #[default]
    Manual,
    Public,
    Whitelist,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WelcomeConfig {
    pub public_message: String,
    pub dm_intro: String,
    #[serde(default)]
    pub checklist: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Resource {
    pub topic: String,
    pub label: String,
    pub url: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CommunityEvent {
    pub title: String,
    pub when: String,
    #[serde(rename = "where")]
    pub location: String,
    #[serde(default)]
    pub url: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PrivacyConfig {
    pub retention_note: String,
}

impl AppConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.bot.name.trim().is_empty() {
            bail!("bot.name cannot be empty");
        }
        if self.bot.about.trim().is_empty() {
            bail!("bot.about cannot be empty");
        }
        if self.welcome.public_message.trim().is_empty() || self.welcome.dm_intro.trim().is_empty()
        {
            bail!("welcome messages cannot be empty");
        }
        if self.welcome.checklist.is_empty() {
            bail!("welcome.checklist must contain at least one step");
        }
        if self.privacy.retention_note.trim().is_empty() {
            bail!("privacy.retention_note cannot be empty");
        }
        if self.bot.invite_policy == InvitePolicy::Whitelist && self.bot.owner_npubs.is_empty() {
            bail!("whitelist invite policy requires at least one owner_npubs entry");
        }
        for npub in &self.bot.owner_npubs {
            if PublicKey::parse(npub).is_err() || !npub.starts_with("npub1") {
                bail!("owner_npubs contains an invalid npub");
            }
        }
        for resource in &self.resources {
            if resource.topic.trim().is_empty() || resource.label.trim().is_empty() {
                bail!("resource topic and label cannot be empty");
            }
            if !resource.url.starts_with("https://") {
                bail!("resource URLs must use https: {}", resource.url);
            }
        }
        let topics = self.resource_topics();
        if topics.len() > 31 {
            bail!("at most 31 resource topics are supported by the command manifest");
        }
        Ok(())
    }

    pub fn resource_topics(&self) -> Vec<String> {
        self.resources
            .iter()
            .map(|resource| resource.topic.to_lowercase())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn checklist_text(&self) -> String {
        self.welcome
            .checklist
            .iter()
            .enumerate()
            .map(|(index, item)| format!("{}. {}", index + 1, item))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> AppConfig {
        AppConfig {
            bot: BotConfig {
                name: "Porchlight".into(),
                about: "Welcome helper".into(),
                data_dir: ".data".into(),
                state_file: ".data/state.json".into(),
                invite_policy: InvitePolicy::Manual,
                owner_npubs: vec![],
            },
            welcome: WelcomeConfig {
                public_message: "hello".into(),
                dm_intro: "hello privately".into(),
                checklist: vec!["say hello".into()],
            },
            resources: vec![],
            events: vec![],
            privacy: PrivacyConfig {
                retention_note: "nothing sensitive".into(),
            },
        }
    }

    #[test]
    fn whitelist_requires_an_owner() {
        let mut config = valid_config();
        config.bot.invite_policy = InvitePolicy::Whitelist;
        assert!(config.validate().is_err());
    }

    #[test]
    fn whitelist_rejects_an_invalid_npub() {
        let mut config = valid_config();
        config.bot.invite_policy = InvitePolicy::Whitelist;
        config.bot.owner_npubs = vec!["npub1not-a-real-key".into()];
        assert!(config.validate().is_err());
    }

    #[test]
    fn example_config_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("porchlight.example.toml");
        AppConfig::load(path).unwrap();
    }
}
