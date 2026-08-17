# Security and privacy notes

Porchlight is an experimental community concierge, not a production moderation, custody or safety service.

## Dedicated bot identity

- Use a new, low-value Nostr identity dedicated to one Porchlight process.
- Never reuse a personal identity, Lightning/payment identity, or key that controls funds.
- Never commit `identity.nsec`, `VECTOR_NSEC`, `.porchlight-data`, `porchlight.toml`, populated state, lock or temporary files.
- The program obtains exclusive cooperative locks for `bot.data_dir` and `bot.state_file`. Do not bypass those locks or open the same Vector directory with another process.

## Windows ACL requirement

`vector_sdk 0.8.2` writes a generated `identity.nsec` to the data directory, and its Unix owner-only permission helper is a no-op on Windows. Before a real run, put the directory on an operator-controlled local disk and restrict it to the running account, SYSTEM and local administrators:

```powershell
.\scripts\Initialize-PorchlightDataDir.ps1 -Path .\.porchlight-data
```

The script replaces inherited access rules on that exact directory. Inspect the resolved path before accepting the change, especially if configuration points outside the repository. Full-disk encryption such as BitLocker is also recommended. Porchlight emits a warning on every Windows start because it does not yet perform a complete native DACL audit.

The script refuses broad/protected paths, reparse points and non-empty targets by default. `-AllowNonEmpty` is only for an already inspected, dedicated Porchlight directory; it does not override the protected-path checks.

## Unix file modes

On Unix, new Porchlight state/data directories are created as owner-only (`0700`), and state, temporary and lock files are set to `0600`. Existing data/state directories or state files with group/other permissions cause startup to fail closed instead of silently changing a potentially shared directory. Symlinked data, state or lock paths are rejected. Operators must still protect backups and the surrounding filesystem.

## Vector database boundary

Encrypted Nostr/Concord transport does not imply complete local encryption:

- `VectorBotBuilder::password()` unlocks an already provisioned encrypted account; it does not enable encryption for a fresh SDK account.
- Vector uses ordinary SQLite plus selective field encryption, not SQLCipher whole-database encryption.
- Synced event metadata remains local, and the pinned core writes retained NIP-17 wrap secrets to its SQLite table without covering them in the available field-encryption migration.
- The SDK has no supported high-level operation to delete an inbound DM or securely erase an entire conversation from local state and every relay.

Treat the whole Vector data directory as sensitive even when the bot uses only disposable keys.

## Porchlight JSON and deletion

Porchlight's separate JSON state stores membership scope, session tokens, processed event IDs, voluntarily declared timezone/interests, queue/match records and the generated text of an unsent delivery. Session authorization expires after 15 minutes and queue eligibility after seven days, but their records are not guaranteed to be physically pruned at those times. Matches and sent-delivery receipts have no automatic retention expiry. After a delivery acknowledgement, its body is cleared and a receipt remains; `/forget` or operator cleanup removes retained application records.

`/forget` removes the requester's Porchlight records, both sides of a saved match and related queued delivery records. It does **not** remove:

- inbound or outbound events in Vector's SQLite database;
- events or ciphertext retained by relays;
- messages already delivered to another member;
- screenshots, exports or filesystem backups.

Operators remain responsible for backup expiry and deletion of filesystem copies.

## Runtime and protocol limits

- Membership validation uses Vector's eventually consistent local fold and fails closed when evidence is missing.
- Waiting candidates are checked before selection and both participants are checked again under ordered per-member locks before a buddy delivery. Every welcome is likewise bound to and rechecked against its membership scope. Failure cancels the unsent delivery; an online `MemberLeave` removes member state at community scope, while `Removed` clears state after the bot itself leaves or is kicked.
- Vector has no atomic membership-check-and-send primitive. Membership can change after Porchlight's final local check but before the network send completes; this narrow race cannot be eliminated by the application.
- Porchlight attempts to reconcile active onboarding DMs from the most recent 100 messages only when the SDK exposes that local history after restart. One disposable restart test returned an empty history, so this is best-effort rather than guaranteed. Offline group commands and `MemberJoin` handlers are not replayed by the SDK.
- Delivery is at-least-once. A crash after network send but before local acknowledgement can create a recognizable duplicate.
- The pinned Vector core is affected by [issue #84](https://github.com/VectorPrivacy/Vector/issues/84), where a channel can stop advancing after a community base rekey. Use a fresh disposable evaluation community and avoid ban/kick/rekey.
- Porchlight is text-only and does not implement moderation, attachments, custody or payments.

Report a suspected vulnerability through [GitHub private vulnerability reporting](https://github.com/fth040818/porchlight-concord/security/advisories/new). Never publish private keys, private messages or member data in a public issue.
