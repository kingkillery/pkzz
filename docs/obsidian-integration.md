# Obsidian Integration

There is no dedicated Obsidian crate/plugin in this repo. The lowest-effort,
fully-supported integration uses the existing `buzz-workflow` webhook trigger
(`POST /hooks/{id}`, `crates/buzz-workflow`, `crates/buzz-relay/src/api/bridge.rs`) —
no relay or client code changes required. This doc is the recipe for "post an
Obsidian note into a Pkzz channel."

For fuller bidirectional message↔note sync (edits, deletes, threads), build a
dedicated bridge daemon following the pattern in
[external-collaboration-bridges.md](external-collaboration-bridges.md) instead —
that is a new crate-sized effort, not covered here.

## How it works

1. A workflow with a `webhook` trigger is created in a channel via `buzz-cli`.
   Creating it returns a webhook URL and secret (`POST /hooks/{id}`,
   `X-Webhook-Secret` header).
2. Any JSON object POSTed to that URL is flattened into
   `buzz_workflow::executor::TriggerContext::webhook_fields` and exposed to
   step templates as `{{trigger.<field>}}` (see
   `crates/buzz-workflow/src/executor.rs`).
3. An Obsidian automation (community "Shell commands" / "Local REST API"
   plugin, a Templater script, or a plain `curl` bound to a hotkey) POSTs the
   active note's title/path/content on save or command.

## Workflow definition

```yaml
name: Obsidian Note Sync
description: Posts an Obsidian note to this channel via webhook trigger
trigger:
  on: webhook
steps:
  - id: post_note
    action: send_message
    text: '**{{trigger.title}}** ({{trigger.vault}}/{{trigger.path}})

      {{trigger.content | truncate(2000)}}'
```

This exact definition is pinned by
`parse_obsidian_note_webhook_example` in `crates/buzz-workflow/src/schema.rs`.

## Setup

```bash
# Create the workflow in a channel; capture the returned webhook_secret.
buzz workflows create --channel <channel-uuid> --yaml obsidian-sync.yaml

# Inspect it to get the webhook id (== workflow_id) and confirm it's active.
buzz workflows get <workflow-id>
```

The webhook URL is `https://<relay-host>/hooks/<workflow-id>`.

## Triggering from Obsidian

Any client that can fire an HTTP POST on save/command works. Example via the
"Shell commands" plugin (or a Templater `user function`) bound to a hotkey:

```bash
curl -sS -X POST "https://<relay-host>/hooks/<workflow-id>" \
  -H "X-Webhook-Secret: <secret-from-create>" \
  -H "Content-Type: application/json" \
  -d "{
    \"title\": \"{{title}}\",
    \"vault\": \"{{vault_name}}\",
    \"path\": \"{{file_path}}\",
    \"content\": $(jq -Rs . < "{{file_path}}")
  }"
```

Substitute `{{title}}`, `{{vault_name}}`, `{{file_path}}` with whatever
placeholder syntax your chosen Obsidian plugin exposes for the active file.

## Notes / caveats

- One-directional only (note → channel message). Editing or deleting the note
  does not update or retract the posted message.
- `send_message` text is capped by the `truncate(2000)` filter above; adjust
  to taste — full vault dumps are not a good fit for chat messages.
- Treat the webhook secret like a credential: it authenticates the *caller*,
  but the run executes with the workflow owner's standing channel authority
  (see `SEC-006` comment in `crates/buzz-relay/src/api/bridge.rs`).
- For scoping a note to a specific channel, either template `channel:` in the
  `send_message` action or create one workflow per target channel.
