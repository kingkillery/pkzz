# Engagement Discernment — Field Workbook

> **Ethos: from "agents you can talk to" to "agents that know when to talk."**
>
> We shipped *reachability*: conversations flow without @-ceremony, guarded by
> deterministic brakes. The next layer — the `open` should-respond scheduler and
> a structured "needs human" escalation kind — must be built from **observed
> failures of the deterministic rules**, not from vibes. This workbook is the
> instrument: dogfood, log what felt wrong, and when the pick-up gates at the
> bottom are met, we build.

Companion docs: `crates/buzz-acp/README.md` § Engagement (mechanics),
repo `AGENTS.md` (harness conventions).

---

## 1. Where we left off (shipped state)

| Layer | Behavior | Where |
|---|---|---|
| Client reply addressing | Replying to an agent-authored message auto-p-tags that agent (known-agent registry only; humans never auto-tagged). Works in strict `mentions` mode. | `desktop/src/features/messages/lib/threading.ts` (`withAgentParentRecipient`), send mutation in `features/messages/hooks.ts` |
| `engagement = "thread"` | Mention OR reply into a thread the agent has posted in fires a turn. Deterministic; no model decides. | `crates/buzz-acp/src/filter.rs` (`EngagementMode`), `participation.rs` |
| Guardrails | Chain cap: max consecutive non-owner-triggered turns per thread (default **3**), reset by owner post or explicit mention. Cooldown between non-mention turns per channel (default **15s**). Mentions always bypass. | `participation.rs` (`EngagementGuard`) |
| Telemetry | Every engaged/suppressed decision → `engagement_decision` observer frame `{eventId, channelId, author, decision, chainDepth, reason}` (published as owner-encrypted kind-24200 relay frames) + tracing lines. **Viewing:** session viewer → toggle **"Raw ACP activity"** (the default Activity transcript has no branch for this frame type and silently drops it), or the **Harness Log** panel. | `crates/buzz-acp/src/lib.rs` (`emit_engagement_decision`) |
| Participation memory | Agent's own published events (any client, same keypair) recorded from the wide subscription; bounded; rehydrated from last **24h** at startup. Forgetting degrades to "needs a fresh @mention," never over-engagement. | `participation.rs`, `rehydrate_participation` |

Knobs: `BUZZ_ACP_ENGAGEMENT=thread` · `BUZZ_ACP_MAX_AGENT_CHAIN` ·
`BUZZ_ACP_THREAD_ENGAGE_COOLDOWN` — or per-rule `engagement` in a rules file.

Deliberately NOT built yet: `open` scheduler mode, escalation event kind,
mobile/inbox client-side parent tagging (covered by thread mode anyway).

> §1 audited against the code 2026-08-11 via map-reduce swarm (selectors:
> `.ompk/mapreduce/engagement-workbook-audit.selectors.md`): 28/30 claims
> verified verbatim; 2 adjudicated (WS send path carries augmented recipients
> directly — no `buildReplyTags` detour; known-agent baseline is managed ∪
> relay-registered, which widens reply-addressing coverage). Telemetry
> viewing instructions corrected per audit finding.

---

## 2. Setup checklist (once per agent)

- [ ] Pick a **lab channel**; keep every other channel on `mentions`.
- [ ] Enable thread mode for one agent:
  - Managed agent: `BUZZ_ACP_ENGAGEMENT=thread` in its env, or
  - Rules file: `mentions`-everywhere rule + `thread` rule scoped to the lab channel UUID (example in `crates/buzz-acp/README.md`).
- [ ] Smoke-test the ladder in the lab channel:
  - [ ] `@agent` root message → responds (mention path).
  - [ ] Plain reply in that thread → responds (thread path, no mention).
  - [ ] Unrelated thread it never joined → silent.
  - [ ] Rapid-fire replies → cooldown suppressions appear.
- [ ] Confirm `engagement_decision` frames show up — open the agent's session panel and switch to **"Raw ACP activity"** (default view drops this frame type), or watch the **Harness Log** panel for `thread engagement — firing turn` / `thread engagement suppressed`.
- [ ] Second agent in the same lab channel (for agent-to-agent bounce data), same setup.

Agents in lab as of __________ : ______________________________________

---

## 3. Field log

Log an entry **at the moment it feels wrong** — the feeling is the data.
Severity: 1 = shrug, 2 = annoying, 3 = broke the flow of work.

### A. False silence — it should have spoken

| Date | Channel/thread ref | What happened (one line) | Why did the rules stay silent? (no mention? never joined thread? chain cap? cooldown?) | Sev |
|---|---|---|---|---|
| | | | | |
| | | | | |
| | | | | |

### B. False speech — it should have stayed quiet

| Date | Channel/thread ref | What it said / did | Why did the rules fire? (reply landed in its thread but wasn't *for* it? mention out of habit?) | Sev |
|---|---|---|---|---|
| | | | | |
| | | | | |
| | | | | |

### C. Guardrail verdicts — cap/cooldown hits

When a suppression happens, was the brake **right** (loop/noise averted) or
**wrong** (killed a productive exchange)?

| Date | Thread ref | Reason (`chain_cap` / `cooldown`) | Right or wrong call? | If wrong: what should the rule have known? |
|---|---|---|---|---|
| | | | | |
| | | | | |

### D. Escalation moments — the human-contact boundary

The raw material for the escalation kind. Two failure directions:

| Date | Direction (`should-have-pinged-me` / `pinged-me-needlessly`) | Situation | What info did the agent have/lack at that moment? |
|---|---|---|---|
| | | | |
| | | | |

### E. Agent-to-agent bounce sessions

When two agents work a thread together, note how it *ended*:

| Date | Thread ref | Ended by (natural finish / chain cap / owner stepped in / degenerated into loop) | Useful exchange? | Notes |
|---|---|---|---|---|
| | | | | |
| | | | | |

---

## 4. Knob-tweak journal

Change one knob at a time; note what the change did to the feel.

| Date | Knob | Old → New | Motivating entries (§3 refs) | Verdict after a few days |
|---|---|---|---|---|
| | | | | |
| | | | | |

---

## 5. Synthesis (fill when patterns emerge)

Top 3 recurring failure shapes (name them — e.g. "addressed-by-name-not-tag",
"question aimed at other agent", "thread drifted off-topic"):

1. ____________________________________________________________________
2. ____________________________________________________________________
3. ____________________________________________________________________

Candidate **scheduler signals** implied by the data (cheap, per-event,
model-free first — name match, question mark aimed at agent, recency of
agent's last post in thread, "addressed elsewhere" detection…):

- ____________________________________________________________________
- ____________________________________________________________________
- ____________________________________________________________________

Candidate **escalation triggers** implied by §3D (blocked > N min, failed
attempt count, decision with irreversible effect, explicit uncertainty…):

- ____________________________________________________________________
- ____________________________________________________________________

---

## 6. Pick-up gates

Resume the build when ALL of:

- [ ] ≥ 2 weeks of dogfooding with ≥ 2 agents sharing the lab channel
- [ ] ≥ 15 entries across §3A–B (the scheduler's training ground)
- [ ] ≥ 5 entries in §3D (the escalation kind's requirements)
- [ ] §5 failure shapes named — we can say *in one sentence each* what the deterministic layer cannot express
- [ ] At least one knob-tweak cycle logged in §4 (proves the brakes are tuned, not defaulted)

## 7. Pick-up protocol (for the next session)

1. Read this workbook filled-in; treat §5 as the spec seed. Optional UX
   warm-up: render `engagement_decision` frames in the default Activity
   transcript (currently raw-feed only — see §1 Telemetry row).
2. Build `open` engagement mode: `should_respond` decider trait in
   `buzz-acp`, first implementation = **heuristics from §5 only** (no model
   call). Wire behind the existing `EngagementMode` enum — additive, per-rule,
   lab-scoped like thread mode was.
3. Design the escalation event kind from §3D: new kind in
   `buzz-core/src/kind.rs`, p-tags the owner, structured payload
   (reason, blocking ref, requested action) — an inbox view, not a mention.
4. Keep the invariant that got us here: **deterministic and boring by
   default; experimentation opt-in per channel; every new behavior emits a
   decision frame before we trust it.**
