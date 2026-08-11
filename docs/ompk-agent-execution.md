# OMPK agent execution from Pkzz

This document describes the additive Pkzz-to-OMPK execution path. The boundary
is intentionally narrow:

```text
signed Pkzz room event
        |
        v
buzz-acp OMPK execution adapter
        |  ACP session/new + session/prompt
        v
      OMPK
        |  agent and worker execution
        v
status and result in the originating Pkzz conversation
```

Pkzz remains the conversation and control surface. OMPK remains the execution
and orchestration runtime. The task, correlation, and return flow stay
backend-agnostic above one isolated OMPK adapter; backend-specific execution
does not leak across Pkzz's room/event machinery. Pkzz does not SSH to another
machine, start an OMPK worker directly, or translate a chat message into a
shell command.

## Architecture before this integration

Pkzz already had most of the return path:

- `buzz-acp` consumes signed Nostr events, applies the existing author and
  engagement gates, queues accepted work per channel, and maintains one ACP
  session per channel.
- Pkzz Desktop already exposes OMPK as the `ompk acp` runtime and prefers it
  when that preset is available.
- The Pkzz ACP client already verifies OMPK's owner-permission bridge during
  `initialize`. OMPK can also acknowledge the Pkzz host-final extension, which
  lets the harness use the semantic final reply from the matching
  `session/prompt` response instead of reconstructing an answer from streamed
  process output.
- Accepted work, typing/working signals, observer frames, retry handling, and
  durable host-final replies already flow through the originating Pkzz
  channel/thread. The signed trigger event contains the routing data used for
  the reply.

OMPK's live ACP server already requires an absolute `cwd` on `session/new` and
creates the root agent session in that directory. OMPK's task system then owns
subagent creation, execution profiles, isolation, and optional per-worker cwd.
It does not currently expose an ACP machine/host placement field or an agent
spawn API that accepts a machine constraint.

The integration therefore extends Pkzz's existing ACP session creation rather
than adding another service or execution framework.

## Execution request contract

An execution request is ordinary message content plus small, signed structural
tags. The message content remains untrusted conversational input.

| Logical field | Representation | Notes |
|---|---|---|
| Task | Pkzz event content | Passed through the existing bounded prompt builder; never interpolated into a shell command. |
| Contract version | `["ompk-execution", "1"]` | Exactly one tag is required for a structured OMPK execution request. |
| Optional cwd placement | `["ompk-cwd", "<absolute-directory>"]` | At most one. Valid only for an OMPK runtime and an allowed workspace. |
| Execution/correlation ID | Signed trigger event ID | Stable across relay redelivery and retries; no second identifier is invented. |
| Return destination | Trigger event's channel/thread/reply routing | The existing reply builder and durable outbox preserve it. |

Unknown versions, duplicate tags, a cwd tag without the execution tag, and
placement tags sent to a non-OMPK harness are rejected. Reserved v1 placement
fields such as host, machine, runner, repo, and workspace are rejected rather
than ignored; version 1 accepts only the cwd field documented above.

The command-line surface is additive to `buzz messages send`:

```bash
# Structured execution using OMPK's normal/default session directory.
buzz messages send --channel <channel-uuid> \
  --mention <ompk-agent-pubkey> \
  --ompk-execution \
  --content "Run the focused test suite and report the evidence"

# The same request with an explicit, pre-authorized session directory.
buzz messages send --channel <channel-uuid> \
  --mention <ompk-agent-pubkey> \
  --ompk-execution \
  --ompk-cwd /srv/workspaces/project-a \
  --content "Implement the assigned change and validate it"
```

`--ompk-cwd` requires `--ompk-execution`. The message must still target an
OMPK-backed Pkzz agent through the normal mention/subscription rules; the flag
does not bypass authorization or select an arbitrary process.

### Default execution

With `--ompk-execution` and no `--ompk-cwd`, `buzz-acp` sends the session cwd it
was already configured to use. OMPK's existing local/default execution behavior
remains authoritative. Ordinary messages without either tag retain the
pre-existing behavior.

### Explicit cwd placement

With `--ompk-cwd`, `buzz-acp` validates and canonicalizes the requested
directory, then passes that directory only as ACP `session/new.cwd`. OMPK owns
everything that executes in the session.

ACP sessions are cwd-scoped. If a channel has a cached session for a different
cwd, the harness creates a new session before prompting. A later default request
switches back to the harness default instead of inheriting the previous
request's explicit cwd. Placement is therefore per request, not contagious
channel state.

## Lifecycle and result correlation

The integration reuses lifecycle signals Pkzz and OMPK can actually support:

1. The signed event is admitted by the normal Pkzz author/engagement gates and
   queued. Pkzz's existing acknowledgement reaction represents acceptance.
2. `buzz-acp` validates the structured contract and selects default or explicit
   cwd placement. Observer telemetry carries the trigger event ID, without
   copying credentials or message content.
3. The harness creates or reuses a cwd-compatible OMPK ACP session and submits
   the bounded prompt. Existing typing/working signals and turn observer frames
   represent active work.
4. OMPK chooses the agent backend and controls any internal workers. Agents can
   publish meaningful progress or questions with the existing Pkzz CLI.
5. On completion, the acknowledged host-final result is delivered through the
   durable Pkzz outbox to the channel/thread derived from the trigger event.
6. Protocol, process, timeout, validation, and publication failures follow the
   existing retry/dead-letter and user-visible failure paths instead of being
   treated as a successful agent result.

The trigger event ID is the execution identity throughout this path. Relay
redelivery and queue retries do not mint a new request identity. This does not
claim that an agent's textual assertion of completion proves validation passed;
validation evidence remains part of the returned result and observer evidence.

## Context boundary

The placement tags do not serialize the room history, process environment, or
root orchestrator state. `buzz-acp` continues to construct its deterministic,
bounded channel/thread prompt using the configured context-message limit. OMPK
continues to construct worker assignments and worker context through its own
task/orchestration system.

An orchestrating agent should put the minimum sufficient worker contract in its
OMPK assignment: objective, scope, relevant files, constraints, dependencies,
required output, and acceptance criteria. Pkzz does not automatically copy the
full project conversation into every OMPK worker.

## Configuration

Explicit cwd placement is opt-in at the harness:

| Flag | Environment variable | Default |
|---|---|---|
| `--no-ompk-execution` | `BUZZ_ACP_NO_OMPK_EXECUTION` | `false`; tagged requests are enabled |
| `--ompk-allowed-workspaces <path>` | `BUZZ_ACP_OMPK_ALLOWED_WORKSPACES` | Empty; every explicit cwd is denied |

The workspace flag is repeatable and both forms accept comma-delimited values.
Configure one or more absolute, existing workspace roots to enable explicit cwd
placement. Default OMPK execution does not require an allowed root. Each request
is canonicalized before containment is checked, so `..` and filesystem-link
traversal cannot escape an allowed root.

The integration can also be disabled at the harness without disabling ordinary
ACP conversation handling. See the OMPK execution rows in
[`crates/buzz-acp/README.md`](../crates/buzz-acp/README.md) and the commented
examples in [`.env.example`](../.env.example) for the final CLI and environment
variable names.

## Security and trust boundary

- Existing Pkzz author, subscription, channel, and mention rules run before an
  execution request reaches OMPK. The integration adds no independent identity
  or permission system.
- Only the fixed tag names and version are parsed as control data. Message text
  remains prompt content and is never treated as an executable, argument list,
  hostname, or environment assignment.
- Explicit cwd must be absolute, must resolve to an existing directory, and
  must be contained by a configured canonical allowed root. Validation errors
  do not echo rejected paths into normal error text.
- OMPK credentials, provider tokens, SSH material, and environment secrets stay
  in OMPK's existing configuration. They are not copied into Pkzz events,
  observer frames, or result messages.
- A requested cwd is itself signed event metadata and may be persisted by the
  relay. Use non-sensitive workspace paths; never encode a credential in a
  path or message.

## Supported and deferred placement

| Placement | Status | Owner |
|---|---|---|
| OMPK normal/default execution | Supported | OMPK |
| Explicit cwd, which may be a repo/workspace directory on the same OMPK execution host | Supported through ACP `session/new.cwd` | Pkzz validates the constraint; OMPK executes it |
| Explicit machine/host | Unsupported by the current OMPK ACP contract | Deferred until OMPK exposes a brokered placement API |
| PKZZ-to-host SSH or direct remote process launch | Intentionally not implemented | Out of scope; would violate the execution boundary |
| Backend/model selection | Unchanged | Existing OMPK configuration and orchestration |

Pkzz's separate managed-agent provider code can deploy a whole ACP harness on
a remote backend. That is not an OMPK per-request machine-placement API and is
not presented as one here.

Future cross-machine execution requires an OMPK runner registration and
placement API. Pkzz may carry an authenticated runner or placement constraint
as structural request data, but OMPK must determine candidate runners, select
one, resolve its workspace, and perform the execution. A machine running or
installing the Pkzz client is **not** thereby an execution target. It becomes
eligible only when an OMPK runner on that machine is explicitly enabled,
registered, and authenticated under OMPK policy.

When OMPK exposes that stable API, this versioned request contract can add the
structurally validated constraint and pass it through the same isolated adapter.
Pkzz should not implement runner discovery, host authentication, SSH,
scheduling, or workspace provisioning itself. Version 1 accepts cwd placement
only.

## Verification

The focused checks for this integration are:

```bash
cargo test -p buzz-acp ompk_execution -- --nocapture
cargo test -p buzz-cli ompk_execution -- --nocapture
cargo check -p buzz-acp -p buzz-cli
cargo clippy -p buzz-acp -p buzz-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

Run commands after activating the repository's Hermit environment, as required
by [`AGENTS.md`](../AGENTS.md). The broader repository gate remains `just ci`.

The existing env-gated live boundary probe launches a real `ompk acp` process:

```powershell
$env:OMPK_BIN = 'C:\path\to\ompk.exe'
cargo test -p buzz-acp initialize_live_ompk_bridge_probe_when_binary_available -- --nocapture
```

That probe proves process launch, the OMPK `initialize` handshake, and a real
`session/new` call with a temporary absolute cwd. It requires OMPK to return a
non-empty session ID, so it exercises the live cwd-placement transport boundary
without making a model call by default. To exercise actual agent execution and
the acknowledged host-final result as an explicit, potentially billable check,
also set `OMPK_LIVE_AGENT_PROMPT=1`; the test then requires the exact terminal
reply `PKZZ_OMPK_LIVE_OK`.
