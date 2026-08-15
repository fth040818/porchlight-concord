# Encrypted two-member demo evidence

This records the disposable live-network acceptance run performed on 2026-08-15 from 12:10 to 12:13 (UTC+08:00). It contains no private keys, payment credentials or real-user profile data.

## Environment

- Porchlight bot: `npub14s0639zkp7sje6epwya97jvd4scx6c99j0yejwvcf0lff2m399rsffu9ll`
- Concord v2 community: `40a8ead4640d0e800371ca09eb2e41a5363c6ae2ba69a325ab9fb0b19561dae3`
- Channel: `general` (`0a0001bcc85a8fe68665ee69a93b072004629916b183da02b8f9f9de0b420688`)
- Member C: `npub1fgpwl2dv87jt8pv66ypyzse9c97p7r96wmaa4vtg375xtx6h8vhqcpe302`
- Member D: `npub1tpgdjt3k73rdspl9djjmfrk8zmu2wysxrk982pwjnr6gdu0p3qtspcr7z4`
- Match: `PLM-00000002`
- SDK/core: `vector_sdk 0.8.2` / `vector-core 0.7.2`, locked by `Cargo.lock`

The community and all three identities were created solely for this demo. No money moved. The community stayed at its initial epoch; no ban, kick or rekey was performed because of the documented Vector issue #84.

## Acceptance path

Both members independently joined through the same public Concord invite while Porchlight remained online. Each member then completed:

1. `/intro` in `general` with no public profile arguments;
2. receipt of the private onboarding message;
3. `/profile` in the bot DM;
4. an ordinary token-bound profile DM;
5. `/buddy` in the bot DM.

The first member waited. The second compatible opt-in created one match and two durable outbox deliveries. Both independent clients then observed the same introduction ID:

```text
DEMO_MEMBER_READY=C:npub1fgpwl2dv87jt8pv66ypyzse9c97p7r96wmaa4vtg375xtx6h8vhqcpe302
DEMO_MEMBER_READY=D:npub1tpgdjt3k73rdspl9djjmfrk8zmu2wysxrk982pwjnr6gdu0p3qtspcr7z4
DEMO_MATCH_RECEIVED=D:PLM-00000002
DEMO_MATCH_RECEIVED=C:PLM-00000002
```

The persisted Porchlight state independently contained reciprocal match records for C and D and acknowledged both introduction deliveries with their bodies cleared.

## What this proves

- Concord join, `MemberJoin`, public zero-argument command and NIP-17 DM interoperate on the live network.
- Profile capture stays in DM after the public `/intro` transition.
- Matching is opt-in and creates a single reciprocal match.
- Both simultaneously online recipients received an introduction bearing the same stable match ID.
- The outbox acknowledged both sends and cleared the retained bodies.

It does **not** prove guaranteed relay delivery, authoritative real-time membership, deletion from Nostr, complete encryption at rest or safe behavior after a Concord base rekey. A separate restart diagnostic found that the SDK returned an empty DM history for one reopened disposable member, so this evidence deliberately relies on two independent online receivers rather than a post-hoc history reconstruction.
