# Porchlight

[![CI](https://github.com/fth040818/porchlight-concord/actions/workflows/ci.yml/badge.svg)](https://github.com/fth040818/porchlight-concord/actions/workflows/ci.yml)

Porchlight is a privacy-conscious onboarding companion for small [Concord](https://soapbox.pub/blog/building-an-armada-voyage-to-encrypted-communities) communities on Nostr. It welcomes new members, serves an operator-configured checklist, shares resources and events, and offers a strictly opt-in one-person buddy introduction over encrypted DMs.

This is a new project created on 2026-08-15 for the **Community Building** category of Derek Ross's “Two Weeks — Build something that grows Nostr” challenge.

## Why it exists

Encrypted communities solve an important protocol problem, but a newcomer can still arrive without knowing what to read, where to talk, or whom to ask. Porchlight adds a reusable welcome layer with an inspectable local state, explicit consent, bounded inputs and an honest deletion boundary.

## What it does

- Queues an idempotent public welcome and encrypted checklist DM on `MemberJoin`.
- Publishes ten native slash commands through Vector's bot manifest.
- `/resources [topic]` and `/events` serve operator-curated community information.
- `/intro` has no arguments. It binds a 15-minute onboarding session to the member, community and origin channel, then moves the flow to DM.
- `/profile` also has no arguments. In DM it issues a one-time session token; the next ordinary DM carries `TOKEN | timezone | interests` so sensitive fields can never be typed-command arguments in a community channel.
- `/buddy`, `/buddy_leave` and `/buddy_status` have no arguments and work only in DM.
- Matching requires at least one shared declared interest; equal-interest candidates use timezone as a tie-breaker.
- Waiting candidates are membership-checked before selection, and both participants are checked again under per-member locks immediately before either introduction is sent. A failed check cancels the match without sending shared interests.
- Every queued welcome is bound to its community/channel and membership-checked again before send. `MemberLeave` removes that community's pending/profile state, related matches and unsent deliveries; `Removed` clears the community after the bot itself leaves or is kicked.
- `/privacy` explains storage, while `/forget` removes Porchlight's own application state.
- A persistent delivery outbox retries partial welcome and buddy-notification failures. Stable delivery and match IDs make possible duplicates recognizable.
- JSON mutations use clone → same-directory temporary file → `sync_all` → atomic replacement → in-memory commit. The state and Vector data directories are locked against a second Porchlight process.
- On startup, active DM onboarding sessions reconcile up to 100 locally synced messages by message ID. Per-sender locks and token checks serialize the sensitive path.
- Before saving a profile, joining the queue, selecting a waiting candidate or sending any welcome or buddy introduction, Porchlight rechecks the bot's community presence plus member and origin-channel access from Vector's local folded Concord view and fails closed if it cannot confirm them.

## Storage and privacy boundary

Porchlight's own JSON state stores:

- the member npub, community ID and origin channel ID;
- a token-bound onboarding session and processed-message IDs; its authorization expires after 15 minutes, but its record can remain until `/forget` or operator cleanup;
- the timezone and interests the member explicitly supplied;
- queue, match and delivery metadata; queue eligibility expires after seven days, while match and sent-delivery receipts remain until `/forget` or operator cleanup;
- generated outbound delivery text until it is acknowledged as sent. The body is then cleared while a receipt remains.

Porchlight does not copy arbitrary community chat or raw inbound DM bodies into that JSON. Its application handler ignores ordinary community messages.

However, the pinned `vector_sdk 0.8.2` maintains its own local SQLite account database for synced events. A fresh SDK bot does not have a supported encryption-provisioning API; on Windows the generated `identity.nsec` is plaintext, and retained NIP-17 wrap secrets are not covered by Vector's field-encryption migration. Encrypted transport is therefore **not** the same thing as complete encryption at rest.

`/forget` removes Porchlight JSON records for the requester and cancels related unsent deliveries. It cannot remove inbound messages from Vector's database, relays, recipients, screenshots or backups, and it cannot retract an introduction already delivered. See [SECURITY.md](SECURITY.md).

## Quick start

Requirements: Rust 1.91+ and a Concord-compatible community/client. The pinned `vector-core 0.7.2` uses `str::floor_char_boundary`, which became stable in Rust 1.91 despite the crate advertising a lower compiler requirement.

```sh
cp porchlight.example.toml porchlight.toml
cargo run --locked -- --config porchlight.toml check
cargo run --locked -- --config porchlight.toml run
```

On Windows, protect the data directory before the first real run:

```powershell
.\scripts\Initialize-PorchlightDataDir.ps1 -Path .\.porchlight-data
```

The script refuses volume roots, the repository/current/user/Windows directory, reparse points and non-empty targets by default. Inspect an existing dedicated Porchlight directory before using `-AllowNonEmpty`.

On Unix, Porchlight creates new data/state directories as `0700` and files as `0600`. It fails closed if an existing data/state directory or state file grants group/other access; fix that explicitly before starting, for example:

```sh
chmod 700 .porchlight-data .porchlight-data/vector
chmod 600 .porchlight-data/porchlight-state.json 2>/dev/null || true
```

On first run, `vector_sdk` creates a dedicated Nostr identity under `bot.data_dir`. To use an existing **bot-only** identity, set `VECTOR_NSEC` in the process environment. Never reuse a payment or personal identity, and never place an nsec in TOML or Git.

To join from a shareable Concord invite:

```sh
cargo run --locked -- --config porchlight.toml run --invite '<invite-url>'
```

Invite policies:

- `manual` (default): invites remain pending for the operator;
- `public`: automatically accept every community invitation;
- `whitelist`: automatically accept only invitations from validated `owner_npubs`.

Prefer `manual` or `whitelist` outside a disposable demo.

## Test and review

```sh
cargo fmt --all --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

Unit tests cover token/session races, message deduplication, input bounds, concurrent duplicate joins, scope changes, candidate validation, community-scoped leave/removal cleanup, outbox retry, welcome idempotency, process locking, atomic reload, schema rejection and rollback after persistence failure. A full encrypted network demo additionally requires a fresh Concord community, the bot identity and two disposable test members. The original 2026-08-15 run and a final-code 2026-08-17 confirmation bound to commit `d7b2020` are recorded in [docs/e2e-evidence.md](docs/e2e-evidence.md).

## Known limits

- Vector's membership APIs are eventually consistent local folds, not a fresh authoritative network query. Porchlight rejects when the required evidence is absent, which can produce temporary false negatives or cancel a valid match that can be retried later.
- Online `MemberLeave` events remove state at community scope. Leave events missed while the bot is offline are not replayed by the SDK, so the final pre-send membership check remains the privacy gate.
- Event handlers run concurrently. Porchlight deliberately treats an observed leave as authoritative for privacy; a stale leave arriving after a quick rejoin can clear application state, requiring the member to run `/intro` again.
- Vector has no atomic "membership check and send" operation. A member can leave during the narrow interval after the final local check but before the network send completes; an observed leave still cancels anything that remains queued.
- A `MemberJoin` or slash command received only while the bot is offline is not replayed by the SDK. Porchlight attempts to reconcile active token-bound DM answers only when the SDK exposes them in local history; that history was empty after reopening one disposable test identity, so recovery is not guaranteed. Keep the bot and clients online during onboarding.
- The outbox is at-least-once and wakes every 30 seconds. If a Nostr send succeeds and the local acknowledgement write then fails, a retry can duplicate a message; its stable `PLD-*`/`PLM-*` ID identifies the duplicate.
- Matching requires a shared interest. Timezone only breaks ties; it never creates a match by itself.
- Queue eligibility expires after seven days, though the saved record may remain until later activity or `/forget`. A match has no automatic retention expiry and is an introduction, not consent to further contact.
- The pinned Vector core is affected by [Vector issue #84](https://github.com/VectorPrivacy/Vector/issues/84): a public channel can stop advancing after a community base rekey. Use a fresh disposable test community, do not ban/kick/rekey during evaluation, and do not treat this MVP as production-ready.
- Porchlight deliberately does not implement moderation, attachment processing, custody or payments. Relevant upstream limitations include [#85](https://github.com/VectorPrivacy/Vector/issues/85) and [#77](https://github.com/VectorPrivacy/Vector/issues/77).

## Development disclosure

The project was designed and implemented with OpenAI Codex as a development collaborator. The architecture, privacy boundaries, tests and release evidence remain reviewable; no claim is made that the source was typed without AI assistance.

## License

MIT
