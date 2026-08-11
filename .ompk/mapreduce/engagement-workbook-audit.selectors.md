# Selectors — engagement workbook audit (2026-08-11)

Task: verify every "shipped state" claim in `docs/engagement-discernment-workbook.md` §1
against the code, and the §2 checklist's feasibility. A false claim in §1 poisons the
field study that feeds the `open` scheduler build.

| # | Selector | Tool | Pattern / file | Known-positive |
|---|---|---|---|---|
| S1 | Client parent-recipient helper exists + wired into both send paths | regex | `withAgentParentRecipient` in `desktop/src/features/messages/{lib/threading.ts,hooks.ts}` | helper defined threading.ts, called twice in hooks.ts (mutationFn + onMutate) |
| S2 | Known-agent registry is the gate | regex | `useKnownAgentPubkeys` usage in `desktop/src/features/messages/hooks.ts` | called in `useSendMessageMutation` |
| S3 | EngagementMode enum + legacy precedence | regex | `enum EngagementMode`, `effective_engagement` in `crates/buzz-acp/src/filter.rs` | 3 variants; engagement wins over require_mention |
| S4 | Delivery widening | regex | `require_mention` derivation in `crates/buzz-acp/src/config.rs` (`resolve_channel_filters`, `resolve_dynamic_channel_filter`, `mentions_mode_engagement`) | thread/all → require_mention=false |
| S5 | Participation tracker semantics | regex | `crates/buzz-acp/src/participation.rs` | OWN_EVENT_LIMIT=4096, THREAD_LIMIT=1024, skip kinds 5/7/ephemeral, channel-scoped thread keys |
| S6 | Rehydration bounds | regex | `rehydrate_participation` in `crates/buzz-acp/src/lib.rs` | 86400s window, limit 200, 5s timeout, kinds [9, 45001, 45003] |
| S7 | Guardrails | regex | `EngagementGuard` in participation.rs + admit/reset call sites in lib.rs | defaults 3 / 15s; mention bypass; owner reset; suppress reasons chain_cap/cooldown |
| S8 | Telemetry frame | regex | `engagement_decision` emit in lib.rs | payload keys eventId/channelId/author/decision/chainDepth/reason; tracing lines "thread engagement — firing turn" / "suppressed" |
| S9 | Config surface + docs | regex | `BUZZ_ACP_ENGAGEMENT`, `BUZZ_ACP_MAX_AGENT_CHAIN`, `BUZZ_ACP_THREAD_ENGAGE_COOLDOWN` in config.rs; `### Engagement` in crates/buzz-acp/README.md | defaults 3 / 15; warn-and-ignore in all/config modes |
| S10 | Frame observability (checklist claim) | regex | how observer frames are viewed: session viewer / CLI subcommand surfacing `engagement_decision` | must exist somewhere reachable, else §2 checklist overpromises |
| S11 | Workbook internal coherence | read | `docs/engagement-discernment-workbook.md` §2/§3/§6/§7 cross-references | gates reference §3 tables that exist; protocol steps reference real extension points |

Completeness: §1 has 5 table rows + knobs paragraph + "deliberately not built" list;
S1–S9 cover all rows/knobs 1:1. §2 feasibility = S10. §3–§7 = S11.
