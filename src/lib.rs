pub mod config;
pub mod state;

pub use config::{AppConfig, InvitePolicy};
pub use state::{
    BeginOnboarding, BuddyMatch, BuddyStatus, ClaimedDelivery, DeliveryTarget, JoinOutcome,
    MembershipScope, ProfileCompletion, ProfilePrompt, StateStore,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedProfile {
    pub token: String,
    pub timezone: String,
    pub interests: Vec<String>,
}

/// Return a compact, non-identifying label suitable for a public welcome.
pub fn short_npub(npub: &str) -> String {
    if npub.len() <= 16 {
        return npub.to_string();
    }
    format!("{}…{}", &npub[..10], &npub[npub.len() - 4..])
}

/// Parse a token-bound private profile line: `TOKEN | timezone | interest, interest`.
pub fn parse_profile_input(raw: &str) -> Result<ParsedProfile, String> {
    if raw.chars().count() > 640 {
        return Err("That profile is too long (maximum 640 characters).".into());
    }
    let mut fields = raw.splitn(3, '|').map(str::trim);
    let token = fields.next().unwrap_or_default();
    let timezone = fields.next().unwrap_or_default();
    let interests = fields.next().unwrap_or_default();
    if token.is_empty() || timezone.is_empty() || interests.is_empty() {
        return Err("Use: TOKEN | UTC+8 | nostr, music".into());
    }
    if token.len() > 24 || !token.starts_with("PLP-") || !token.is_ascii() {
        return Err("The onboarding token is invalid. Run /profile again.".into());
    }
    if timezone.chars().count() > 32 || timezone.chars().any(char::is_control) {
        return Err("Timezone must be 1–32 printable characters.".into());
    }

    let raw_interests = interests.split(',').collect::<Vec<_>>();
    if raw_interests.len() > 12 {
        return Err("Please use at most 12 interests.".into());
    }
    let mut values = Vec::with_capacity(raw_interests.len());
    for value in raw_interests {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if value.chars().count() > 48 || value.chars().any(char::is_control) {
            return Err("Each interest must be at most 48 printable characters.".into());
        }
        values.push(value.to_lowercase());
    }
    values.sort();
    values.dedup();
    if values.is_empty() {
        return Err("Please include at least one interest.".into());
    }
    Ok(ParsedProfile {
        token: token.to_string(),
        timezone: timezone.to_string(),
        interests: values,
    })
}

/// Escape user-supplied profile fields before placing them in generated Markdown-like messages.
pub fn escape_markdown(raw: &str) -> String {
    raw.chars()
        .flat_map(|character| {
            if matches!(
                character,
                '\\' | '`'
                    | '*'
                    | '_'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '#'
                    | '+'
                    | '!'
                    | '|'
            ) {
                vec!['\\', character]
            } else {
                vec![character]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_profile_is_parsed_and_deduplicated() {
        let parsed =
            parse_profile_input("PLP-00000001 | UTC+8 | Rust, Music, rust, Nostr").unwrap();
        assert_eq!(parsed.interests, vec!["music", "nostr", "rust"]);
        assert_eq!(parsed.timezone, "UTC+8");
    }

    #[test]
    fn npub_labels_do_not_expose_the_whole_identifier() {
        assert_eq!(
            short_npub("npub1abcdefghijklmnopqrstuvwxyz"),
            "npub1abcde…wxyz"
        );
    }

    #[test]
    fn oversized_interest_is_rejected_instead_of_truncated() {
        let long = "a".repeat(80);
        assert!(parse_profile_input(&format!("PLP-00000001 | UTC | {long}")).is_err());
    }

    #[test]
    fn delayed_or_malformed_profile_lines_are_rejected() {
        assert!(parse_profile_input("UTC+8, nostr").is_err());
        assert!(parse_profile_input("wrong | UTC+8 | nostr").is_err());
    }

    #[test]
    fn markdown_metacharacters_are_escaped() {
        assert_eq!(escape_markdown("[nostr]*"), "\\[nostr\\]\\*");
    }
}
