use buzz_sdk::{DeleteMessageOptions, DiffMeta, ThreadRef, VoteDirection};
use nostr::{EventBuilder, PublicKey, Tag};
use std::path::Path;
use uuid::Uuid;

use crate::client::{normalize_events, normalize_write_response, BuzzClient};
use crate::error::CliError;
use crate::validate::{
    infer_language, parse_event_id, parse_uuid, read_or_stdin, truncate_diff,
    validate_content_size, validate_hex64, validate_uuid, MAX_DIFF_BYTES,
};
use buzz_sdk::mentions::{
    extract_at_mentions_with_known, extract_nostr_uris, strip_code_regions, MENTION_CAP,
};

/// Extract the thread root event ID from a Nostr tag array.
///
/// Parses `"e"` tags with NIP-10 markers:
/// - If a `"root"` marker exists, returns that event ID.
/// - Otherwise, if only a `"reply"` marker exists, returns the reply target
///   (a direct reply's parent IS the root, and nested replies need that root
///   to thread correctly).
/// - If no thread markers exist, returns `None` (parent is a top-level message,
///   so it is itself the root).
fn find_root_from_tags(tags: &serde_json::Value) -> Option<String> {
    fn valid_event_id(s: &str) -> bool {
        s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
    }
    let arr = tags.as_array()?;
    let mut root = None;
    let mut reply = None;
    for tag in arr {
        let Some(parts) = tag.as_array() else {
            continue;
        };
        if parts.len() >= 4 && parts[0].as_str() == Some("e") {
            // Defensively ignore malformed marker values so a bad tag on the
            // parent event can't block the reply â€” fall back to root == parent.
            let id = parts[1].as_str().filter(|s| valid_event_id(s));
            match (parts[3].as_str(), id) {
                (Some("root"), Some(id)) => root = Some(id.to_string()),
                (Some("reply"), Some(id)) => reply = Some(id.to_string()),
                _ => {}
            }
        }
    }
    root.or(reply)
}

/// Build a `ThreadRef` for a reply, given the immediate parent's event ID.
///
/// Fetches the parent event from the relay and inspects its NIP-10 `e` tags to
/// determine the thread root:
/// - Direct reply (parent is top-level): `root == parent`.
/// - Nested reply: `root` is the parent's own root marker; `parent` is unchanged.
///
/// Ensures CLI-sent replies thread correctly using the same NIP-10 logic.
async fn resolve_thread_ref(
    client: &BuzzClient,
    parent_event_id: &str,
) -> Result<ThreadRef, CliError> {
    let parent_eid = parse_event_id(parent_event_id)?;
    let filter = serde_json::json!({ "ids": [parent_event_id], "limit": 1 });
    let raw = client.query(&filter).await?;
    let events: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| CliError::Other(format!("failed to parse query response: {e}")))?;
    let event = events
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| CliError::Other(format!("parent event {parent_event_id} not found")))?;
    let tags = event
        .get("tags")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let root_eid = match find_root_from_tags(&tags) {
        Some(root_hex) if root_hex != parent_event_id => parse_event_id(&root_hex)?,
        _ => parent_eid,
    };

    Ok(ThreadRef {
        root_event_id: root_eid,
        parent_event_id: parent_eid,
    })
}

/// Resolve the channel UUID for an event by querying for it via POST /query.
/// Extracts the `h` tag value from the returned event's tags.
async fn resolve_channel_id(client: &BuzzClient, event_id: &str) -> Result<Uuid, CliError> {
    let filter = serde_json::json!({
        "ids": [event_id]
    });
    let raw = client.query(&filter).await?;
    let events: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| CliError::Other(format!("failed to parse query response: {e}")))?;
    let arr = events
        .as_array()
        .ok_or_else(|| CliError::Other("query response is not an array".into()))?;
    let event = arr
        .first()
        .ok_or_else(|| CliError::Other(format!("event {event_id} not found")))?;
    let tags = event
        .get("tags")
        .and_then(|t| t.as_array())
        .ok_or_else(|| CliError::Other("event missing 'tags' field".into()))?;
    for tag in tags {
        if let Some(arr) = tag.as_array() {
            if arr.first().and_then(|v| v.as_str()) == Some("h") {
                if let Some(uuid_str) = arr.get(1).and_then(|v| v.as_str()) {
                    return Uuid::parse_str(uuid_str).map_err(|_| {
                        CliError::Other(format!("event h-tag is not a valid UUID: {uuid_str}"))
                    });
                }
            }
        }
    }
    Err(CliError::Other(format!(
        "event {event_id} has no h-tag â€” cannot determine channel"
    )))
}

fn resolve_names_to_pubkeys(
    names: &[String],
    name_to_pubkeys: &std::collections::HashMap<String, Vec<String>>,
    has_explicit_mentions: bool,
) -> Result<Vec<String>, CliError> {
    let mut resolved = Vec::new();
    for name in names {
        match name_to_pubkeys
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            [pubkey] => resolved.push(pubkey.clone()),
            [] if has_explicit_mentions => {}
            [] => {
                return Err(CliError::Usage(format!(
                    "mention '@{name}' does not match a current channel member; retry with --mention <pubkey>"
                )))
            }
            _ if has_explicit_mentions => {}
            candidates => {
                return Err(CliError::Usage(format!(
                    "mention '@{name}' is ambiguous; candidates: {}. Retry with --mention <pubkey>",
                    candidates.join(", ")
                )))
            }
        }
    }
    Ok(resolved)
}

/// Resolve mention text against the channel membership snapshot.
///
/// Returns both the current member set and uniquely name-resolved pubkeys.
/// Lookup failures are fatal when mention processing is requested: publishing
/// visible mention text without its intended `p` tag is worse than not sending.
async fn resolve_content_mentions(
    client: &BuzzClient,
    channel_id: &str,
    content: &str,
    has_explicit_mentions: bool,
) -> Result<(Vec<String>, Vec<String>), CliError> {
    let stripped = strip_code_regions(content);
    if !stripped.contains('@') && !has_explicit_mentions {
        return Ok((vec![], vec![]));
    }

    let members_filter = serde_json::json!({
        "kinds": [39002],
        "#d": [channel_id],
        "limit": 1,
    });
    let member_pubkeys = fetch_member_pubkeys(client, &members_filter)
        .await
        .ok_or_else(|| {
            CliError::Other("could not load channel membership for mention preflight".into())
        })?;

    if !stripped.contains('@') {
        return Ok((member_pubkeys, vec![]));
    }

    let profiles_filter = serde_json::json!({
        "kinds": [0],
        "authors": member_pubkeys,
        "limit": member_pubkeys.len(),
    });
    let profile_events = fetch_events(client, &profiles_filter)
        .await
        .ok_or_else(|| {
            CliError::Other("could not load member profiles for mention resolution".into())
        })?;

    let mut name_to_pubkeys: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut display_names = Vec::new();
    for e in &profile_events {
        let Some(pubkey) = e.get("pubkey").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(content_json) = e.get("content").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(content_json) else {
            continue;
        };
        let Some(name) = v
            .get("display_name")
            .or_else(|| v.get("name"))
            .and_then(|n| n.as_str())
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        name_to_pubkeys
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(pubkey.to_string());
        display_names.push(name.to_string());
    }

    let known_refs: Vec<&str> = display_names.iter().map(String::as_str).collect();
    let names = extract_at_mentions_with_known(&stripped, &known_refs);
    let resolved = resolve_names_to_pubkeys(&names, &name_to_pubkeys, has_explicit_mentions)?;
    Ok((member_pubkeys, resolved))
}

fn normalize_explicit_mentions(values: &[String]) -> Result<Vec<String>, CliError> {
    let mut normalized = Vec::new();
    for value in values {
        let pubkey = PublicKey::parse(value.trim())
            .map_err(|_| CliError::Usage(format!("invalid --mention pubkey: {value}")))?;
        let hex = pubkey.to_hex();
        if !normalized.contains(&hex) {
            normalized.push(hex);
        }
    }
    if normalized.len() > MENTION_CAP {
        return Err(CliError::Usage(format!(
            "too many --mention values (max {MENTION_CAP})"
        )));
    }
    Ok(normalized)
}

fn merge_message_mentions(
    explicit: &[String],
    uri_pubkeys: &[String],
    auto_resolved: &[String],
) -> Result<Vec<String>, CliError> {
    let mut mentions = Vec::new();
    for pubkey in explicit
        .iter()
        .chain(uri_pubkeys.iter())
        .chain(auto_resolved.iter())
    {
        if !mentions.contains(pubkey) {
            mentions.push(pubkey.clone());
        }
    }
    if mentions.len() > MENTION_CAP {
        return Err(CliError::Usage(format!(
            "too many unique message mentions (max {MENTION_CAP})"
        )));
    }
    Ok(mentions)
}

fn missing_members(mentions: &[String], members: &[String]) -> Vec<String> {
    let members: std::collections::HashSet<&str> = members.iter().map(String::as_str).collect();
    mentions
        .iter()
        .filter(|pk| !members.contains(pk.as_str()))
        .cloned()
        .collect()
}

fn event_mention_pubkeys(event: &nostr::Event) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let parts = tag.as_slice();
            (parts.first().map(String::as_str) == Some("p"))
                .then(|| parts.get(1).cloned())
                .flatten()
        })
        .collect()
}

/// Fetch raw events for `filter` via the relay's `/query` endpoint.
/// Returns `None` on any I/O or parse failure.
async fn fetch_events(
    client: &BuzzClient,
    filter: &serde_json::Value,
) -> Option<Vec<serde_json::Value>> {
    let raw = client.query(filter).await.ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    parsed.as_array().cloned()
}

/// Extract member pubkeys (the `p` tag values) from a single 39002 event.
async fn fetch_member_pubkeys(
    client: &BuzzClient,
    filter: &serde_json::Value,
) -> Option<Vec<String>> {
    let events = fetch_events(client, filter).await?;
    Some(parse_member_pubkeys(events.first()?))
}

/// Parse member pubkeys from a kind 39002 event JSON value.
///
/// Filters and canonicalizes via `nostr::PublicKey::from_hex` â€” matching
/// MCP's typed-Nostr behavior so both surfaces accept exactly the same
/// pubkeys. Pure helper, split out for testing.
fn parse_member_pubkeys(event: &serde_json::Value) -> Vec<String> {
    let Some(tags) = event.get("tags").and_then(|t| t.as_array()) else {
        return vec![];
    };
    tags.iter()
        .filter_map(|t| {
            let arr = t.as_array()?;
            if arr.first()?.as_str()? != "p" {
                return None;
            }
            let pk = arr.get(1)?.as_str()?;
            PublicKey::from_hex(pk).ok().map(|k| k.to_hex())
        })
        .collect()
}

fn format_events(normalized: &str, format: &crate::OutputFormat) -> String {
    match format {
        crate::OutputFormat::Compact => {
            let events: Vec<serde_json::Value> =
                serde_json::from_str(normalized).unwrap_or_default();
            let compact: Vec<serde_json::Value> = events
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "id": e.get("id").cloned().unwrap_or_default(),
                        "content": e.get("content").cloned().unwrap_or_default(),
                        "created_at": e.get("created_at").cloned().unwrap_or_default(),
                    })
                })
                .collect();
            serde_json::to_string(&compact).unwrap_or_default()
        }
        crate::OutputFormat::Json => normalized.to_string(),
    }
}

pub async fn cmd_get_messages(
    client: &BuzzClient,
    channel_id: &str,
    limit: Option<u32>,
    before: Option<i64>,
    since: Option<i64>,
    kinds: Option<&str>,
    format: &crate::OutputFormat,
) -> Result<(), CliError> {
    validate_uuid(channel_id)?;
    let limit = limit.unwrap_or(50).min(200);

    let mut filter = serde_json::json!({
        "kinds": [9, 40002, 40008, 45001, 45003],
        "#h": [channel_id],
        "limit": limit
    });

    // If specific kinds requested, override
    if let Some(k) = kinds {
        let kind_list: Vec<u64> = k.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        if !kind_list.is_empty() {
            filter["kinds"] = serde_json::json!(kind_list);
        }
    }

    if let Some(b) = before {
        filter["until"] = serde_json::json!(b);
    }
    if let Some(s) = since {
        filter["since"] = serde_json::json!(s);
    }

    let resp = client.query(&filter).await?;
    let mut events: Vec<serde_json::Value> = serde_json::from_str(&resp).unwrap_or_default();
    events.sort_by_key(|e| e.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0));
    let normalized = normalize_events(&events);
    println!("{}", format_events(&normalized, format));
    Ok(())
}

pub async fn cmd_get_thread(
    client: &BuzzClient,
    channel_id: &str,
    event_id: &str,
    limit: Option<u32>,
    depth_limit: Option<u32>,
    format: &crate::OutputFormat,
) -> Result<(), CliError> {
    validate_uuid(channel_id)?;
    validate_hex64(event_id)?;
    let limit = limit.unwrap_or(100).min(500);

    // Two filters ORed in a single HTTP call:
    // 1. Replies referencing this event via e-tag (no kind restriction)
    // 2. The root event itself by ID
    let mut reply_filter = serde_json::json!({
        "kinds": [9, 40002, 40003, 40008, 45003],
        "#h": [channel_id],
        "#e": [event_id],
        "limit": limit
    });
    if let Some(d) = depth_limit {
        reply_filter["depth_limit"] = serde_json::json!(d);
    }
    let root_filter = serde_json::json!({
        "ids": [event_id],
        "limit": 1
    });
    let resp = client.query_multi(&[reply_filter, root_filter]).await?;
    let mut events: Vec<serde_json::Value> = serde_json::from_str(&resp).unwrap_or_default();
    events.sort_by_key(|e| e.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0));
   óNw¶‰Ëkºwµç@€€€€€¤(€€€€€€€€¹Õ¹İÉ…À ¤(€€€€€€€€¹Í¥¹}İ¥Ñ¡}­•åÌ ™-•åÌèé•¹•É…Ñ” ¤¤(€€€€€€€€¹Õ¹İÉ…À ¤ì(€€€€€€€±•ĞÑ…Ì€ô•Ù•¹Ğ(€€€€€€€€€€€€¹Ñ…Ì(€€€€€€€€€€€€¹¥Ñ•È ¤(€€€€€€€€€€€€¹µ…À¡ñÑ…ğÑ…œ¹…Í}Í±¥” ¤¹Ñ½}Ù•Œ ¤¤(€€€€€€€€€€€€¹½±±•ĞèèñY•Œñ|øø ¤ì((€€€€€€€…ÍÍ•ÉĞ„¡Ñ…Ì¹½¹Ñ…¥¹Ì ™Ù•Œ…l(€€€€€€€€€€€=5A-}aUQ%=9}Q¹¥¹Ñ¼ ¤°(€€€€€€€€€€€=5A-}aUQ%=9}=9QIQ}YIM%=8¹¥¹Ñ¼ ¤°(€€€€€€€t¤¤ì(€€€€€€€…ÍÍ•ÉĞ„¡Ñ…Ì¹½¹Ñ…¥¹Ì ™Ù•Œ…m=5A-}]}Q¹¥¹Ñ¼ ¤°	M=1UQ}]=I-MA¹¥¹Ñ¼ ¥t¤¤ì(€€€€€€€…ÍÍ•ÉĞ„ (€€€€€€€€€€€•Ù•¹Ğ¹Ù•É¥™ä ¤¹¥Í}½¬ ¤°(€€€€€€€€€€€€‰µ•Ñ…‘…Ñ„µÕÍĞ‰”½Ù•É•‰äÑ¡”Í¥¹…ÑÕÉ”ˆ(€€€€€€€€¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸½µÁ­}İ‘}Ù…±¥‘…Ñ¥½¹}É•©•ÑÍ}Õ¹‰½Õ¹‘}½É}Õ¹Í…™•}Ù…±Õ•Í}İ¥Ñ¡½ÕÑ}•¡½¥¹}Ñ¡•´ ¤ì(€€€€€€€±•ĞÍ•É•Ğ€ô€‰É•±…Ñ¥Ù”½Q=-9}ÍÕÁ•É}Í•É•Ğˆì(€€€€€€€±•ĞÕ¹‰½Õ¹€ôÙ…±¥‘…Ñ•}½µÁ­}•á•ÕÑ¥½¹}µ•Ñ…‘…Ñ„¡™…±Í”°M½µ”¡	M=1UQ}]=I-MA¤¤(€€€€€€€€€€€€¹Õ¹İÉ…Á}•ÉÈ ¤(€€€€€€€€€€€€¹Ñ½}ÍÑÉ¥¹œ ¤ì(€€€€€€€…ÍÍ•ÉĞ„¡Õ¹‰½Õ¹¹½¹Ñ…¥¹Ì ‰É•ÅÕ¥É•Ì€´µ½µÁ¬µ•á•ÕÑ¥½¸ˆ¤¤ì((€€€€€€€™½È¥¹Ù…±¥¥¸lˆˆ°Í•É•Ğ°€‰…‰Í½±ÕÑ•q¹¹•İ±¥¹”‰tì(€€€€€€€€€€€±•Ğ•ÉÉ½È€ôÙ…±¥‘…Ñ•}½µÁ­}•á•ÕÑ¥½¹}µ•Ñ…‘…Ñ„¡ÑÉÕ”°M½µ”¡¥¹Ù…±¥¤¤(€€€€€€€€€€€€€€€€¹Õ¹İÉ…Á}•ÉÈ ¤(€€€€€€€€€€€€€€€€¹Ñ½}ÍÑÉ¥¹œ ¤ì(€€€€€€€€€€€¥˜€…¥¹Ù…±¥¹¥Í}•µÁÑä ¤ì(€€€€€€€€€€€€€€€…ÍÍ•ÉĞ„ …•ÉÉ½È¹½¹Ñ…¥¹Ì¡¥¹Ù…±¥¤¤ì(€€€€€€€€€€€ô(€€€€€€€€€€€…ÍÍ•ÉĞ„ …•ÉÉ½È¹½¹Ñ…¥¹Ì ‰Q=-9}ÍÕÁ•É}Í•É•Ğˆ¤¤ì(€€€€€€€ô((€€€€€€€±•Ğ½Ù•ÉÍ¥é•€ô™½Éµ…Ğ„ ‰íõíôˆ°	M=1UQ}]=I-MA°€‰àˆ¹É•Á•…Ğ ĞÀäØ¤¤ì(€€€€€€€±•Ğ•ÉÉ½È€ôÙ…±¥‘…Ñ•}½µÁ­}•á•ÕÑ¥½¹}µ•Ñ…‘…Ñ„¡ÑÉÕ”°M½µ” ™½Ù•ÉÍ¥é•¤¤(€€€€€€€€€€€€¹Õ¹İÉ…Á}•ÉÈ ¤(€€€€€€€€€€€€¹Ñ½}ÍÑÉ¥¹œ ¤ì(€€€€€€€…ÍÍ•ÉĞ„ …•ÉÉ½È¹½¹Ñ…¥¹Ì ™½Ù•ÉÍ¥é•¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸É½½Ñ}µ…É­•É}İ¥¹Í}½Ù•É}É•Á±å}µ…É­•È ¤ì(€€€€€€€±•ĞÑ…Ì€ô©Í½¸„¡l(€€€€€€€€€€€l‰”ˆ°%}°€ˆˆ°€‰É½½Ğ‰t°(€€€€€€€€€€€l‰”ˆ°%}°€ˆˆ°€‰É•Á±ä‰t°(€€€€€€€€€€€l‰Àˆ°AU	-et°(€€€€€€€t¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡™¥¹‘}É½½Ñ}™É½µ}Ñ…Ì ™Ñ…Ì¤¹…Í}‘•É•˜ ¤°M½µ”¡%}¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸É•Á±å}½¹±å}™…±±Í}‰…­}Ñ½}É•Á±å}Ñ…É•Ğ ¤ì(€€€€€€€€¼¼¥É•ĞÉ•Á±äÑ¼„Ñ½Àµ±•Ù•°µ•ÍÍ…”ƒŠPÑ¡”Á…É•¹ĞÌ½¹±ä”µÑ…œ¥Ì„(€€€€€€€€¼¼€‰É•Á±äˆµ…É­•ÈÁ½¥¹Ñ¥¹œ…Ğ¥ĞìÑÉ•…ĞÑ¡”É•Á±äÑ…É•Ğ…ÌÑ¡”É½½Ğ¸(€€€€€€€±•ĞÑ…Ì€ô©Í½¸„¡ml‰”ˆ°%}°€ˆˆ°€‰É•Á±ä‰t°l‰Àˆ°AU	-et±t¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡™¥¹‘}É½½Ñ}™É½µ}Ñ…Ì ™Ñ…Ì¤¹…Í}‘•É•˜ ¤°M½µ”¡%}¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸¹½}Ñ¡É•…‘}µ…É­•ÉÍ}É•ÑÕÉ¹Í}¹½¹” ¤ì(€€€€€€€±•ĞÑ…Ì€ô©Í½¸„¡ml‰Àˆ°AU	-et°l‰ ˆ°€‰¡…¹¹•°µÕÕ¥‰t±t¤ì(€€€€€€€…ÍÍ•ÉĞ„¡™¥¹‘}É½½Ñ}™É½µ}Ñ…Ì ™Ñ…Ì¤¹¥Í}¹½¹” ¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸Õ¹µ…É­•‘}•}Ñ…}¥¹½É• ¤ì(€€€€€€€€¼¼9%@´ÄÀ‘•ÁÉ•…Ñ•Á½Í¥Ñ¥½¹…°µ…É­•ÉÌì¥¹½É””µÑ…Ì±…­¥¹œ…¸(€€€€€€€€¼¼•áÁ±¥¥Ğ€‰É½½Ğˆ¼‰É•Á±äˆµ…É­•ÈÉ…Ñ¡•ÈÑ¡…¸Õ•ÍÍ¥¹œ¸(€€€€€€€±•ĞÑ…Ì€ô©Í½¸„¡ml‰”ˆ°%}t°l‰”ˆ°%}°€ˆ‰t±t¤ì(€€€€€€€…ÍÍ•ÉĞ„¡™¥¹‘}É½½Ñ}™É½µ}Ñ…Ì ™Ñ…Ì¤¹¥Í}¹½¹” ¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸µ…±™½Éµ•‘}Ñ…Í}…É•}Í­¥ÁÁ• ¤ì(€€€€€€€±•ĞÑ…Ì€ô©Í½¸„¡l(€€€€€€€€€€€€‰¹½Ğµ…¸µ…ÉÉ…äˆ°(€€€€€€€€€€€l‰”‰t°(€€€€€€€€€€€l‰”ˆ°€‰Í¡½ÉĞ‰t°(€€€€€€€€€€€l‰”ˆ°%}°€ˆˆ°€‰É½½Ğ‰t°(€€€€€€€t¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡™¥¹‘}É½½Ñ}™É½µ}Ñ…Ì ™Ñ…Ì¤¹…Í}‘•É•˜ ¤°M½µ”¡%}¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸µ…±™½Éµ•‘}µ…É­•É}¥‘}¥Í}¥¹½É• ¤ì(€€€€€€€€¼¼A…É•¹Ğ•Ù•¹Ğ¡…Ì„€‰É½½Ğˆµ…É­•Èİ¡½Í”Ù…±Õ”¥Í¸Ğ„Ù…±¥€ØĞµ¡•à(€€€€€€€€¼¼•Ù•¹Ğ¥€¡½Ñ¡•Èµ±¥•¹Ğ‰Õœ°É•±…äµ…•ÁÑ•¤¸QÉ•…ĞÑ¡”µ…É­•È…Ì(€€€€€€€€¼¼…‰Í•¹ĞÍ¼Ñ¡”…±±•È™…±±Ì‰…¬Ñ¼É½½Ğ€ôôÁ…É•¹ĞÉ…Ñ¡•ÈÑ¡…¸(€€€€€€€€¼¼™…¥±¥¹œÑ¼Í•¹Ñ¡”É•Á±ä¸(€€€€€€€±•ĞÑ…Ì€ô©Í½¸„¡ml‰”ˆ°€‰¹½Ğµ„µÙ…±¥µ¥ˆ°€ˆˆ°€‰É½½Ğ‰t°l‰Àˆ°AU	-et±t¤ì(€€€€€€€…ÍÍ•ÉĞ„¡™¥¹‘}É½½Ñ}™É½µ}Ñ…Ì ™Ñ…Ì¤¹¥Í}¹½¹” ¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸µ…±™½Éµ•‘}É½½Ñ}‘½•Í}¹½Ñ}Í¡…‘½İ}Ù…±¥‘}É•Á±ä ¤ì(€€€€€€€€¼¼%˜€‰É½½Ğˆ¥Ìµ…±™½Éµ•‰ÕĞ€‰É•Á±äˆ¥ÌÙ…±¥°™…±°‰…¬Ñ¼€‰É•Á±äˆ¸(€€€€€€€±•ĞÑ…Ì€ô©Í½¸„¡ml‰”ˆ°€‰…É‰…”ˆ°€ˆˆ°€‰É½½Ğ‰t°l‰”ˆ°%}°€ˆˆ°€‰É•Á±ä‰t±t¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡™¥¹‘}É½½Ñ}™É½µ}Ñ…Ì ™Ñ…Ì¤¹…Í}‘•É•˜ ¤°M½µ”¡%}¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸¹½¹}…ÉÉ…å}¥¹ÁÕÑ}É•ÑÕÉ¹Í}¹½¹” ¤ì(€€€€€€€…ÍÍ•ÉĞ„¡™¥¹‘}É½½Ñ}™É½µ}Ñ…Ì ™©Í½¸„¡íô¤¤¹¥Í}¹½¹” ¤¤ì(€€€€€€€…ÍÍ•ÉĞ„¡™¥¹‘}É½½Ñ}™É½µ}Ñ…Ì ™©Í½¸„¡¹Õ±°¤¤¹¥Í}¹½¹” ¤¤ì(€€€ô((€€€€¼¼(€€€€¼¼Q¡•Í”Ñ•ÍÑÌ‘½¸Ğ¡¥ĞÑ¡”¹•Ñİ½É¬ƒŠPÑ¡•äÁÉ½Ù”Ñ¡…Ğ€©¥Ù•¸¨Ñ¡”(€€€€¼¼•Ù•¹ÑÌÑ¡”É•±…äÉ•ÑÕÉ¹Ì°Ñ¡”1$ÌÁ…ÉÍ”€¬µ…Ñ İ¥É¥¹œÁÉ½‘Õ•Ì(€€€€¼¼Ñ¡”É¥¡ĞÁÕ‰­•åÌ¸Q¡”…Íå¹Œ$½<İÉ…ÁÁ•È…É½Õ¹Ñ¡•´¥Ì½¹”(€€€€¼¼ÍÑÉ…¥¡Ğ±¥¹”ìÑ¡”ÁÕÉ”ÍÑ…•Ì¥Ğ½µÁ½Í•Ì…É”•á•É¥Í•¡•É”…¹(€€€€¼¼¥¸‰ÕéèµÍ‘¬¸((€€€€¼¼¼¹µÑ¼µ•¹€¡Í…¹Ì$½<¤è‰½‘äÑ•áĞƒŠH•áÑÉ…Ñ•¹…µ•ÌƒŠHµ…Ñ¡•(€€€€¼¼¼µ•µ‰•ÈÁÕ‰­•åÌ°ÕÍ¥¹œÉ•…±¥ÍÑ¥Œ€ÌäÀÀÈ€¬­¥¹èÀ•Ù•¹Ğ)M=8¸(€€€€¼¼¼Q¡¥Ì¥ÌÑ¡”É•É•ÍÍ¥½¸Õ…É™½ÈÑ¡”ÁÉ•Ù¥½ÕÌÍÑÕˆÑ¡…Ğ…±İ…åÌ(€€€€¼¼¼É•ÑÕÉ¹•Ù•Œ…mu€¸(€€€€mÑ•ÍÑt(€€€™¸±¥}Á¥Á•±¥¹•}É•Í½±Ù•Í}‰½‘å}…Ñ}¹…µ•Í}Ñ½}µ•µ‰•É}ÁÕ‰­•åÌ ¤ì(€€€€€€€€¼¼­¥¹€ÌäÀÀÈ¡…¹¹•°µµ•µ‰•ÉÌ•Ù•¹Ğİ¥Ñ Ñ¡É•”µ•µ‰•ÉÌ¸(€€€€€€€±•Ğµ•µ‰•ÉÍ}•Ù•¹Ğ€ô©Í½¸„¡ì(€€€€€€€€€€€€‰­¥¹ˆè€ÌäÀÀÈ°(€€€€€€€€€€€€‰Ñ…Ìˆèl(€€€€€€€€€€€€€€€l‰ˆ°€ˆÀÀÀÀÀÀÀÀ´ÀÀÀÀ´ÀÀÀÀ´ÀÀÀÀ´ÀÀÀÀÀÀÀÀÀÀÀÀ‰t°(€€€€€€€€€€€€€€€l‰Àˆ°A-}Y1%}°€ˆˆ°€‰µ•µ‰•È‰t°(€€€€€€€€€€€€€€€l‰Àˆ°A-}Y1%}°€ˆˆ°€‰µ•µ‰•È‰t°(€€€€€€€€€€€€€€€l‰Àˆ°A-}Y1%}°€ˆˆ°€‰µ•µ‰•È‰t°(€€€€€€€€€€€t°(€€€€€€€€€€€€‰½¹Ñ•¹Ğˆè€ˆˆ°(€€€€€€€ô¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„ (€€€€€€€€€€€Á…ÉÍ•}µ•µ‰•É}ÁÕ‰­•åÌ ™µ•µ‰•ÉÍ}•Ù•¹Ğ¤°(€€€€€€€€€€€Ù•Œ…mA-}Y1%}°A-}Y1%}°A-}Y1%}t(€€€€€€€€¤ì((€€€€€€€€¼¼Q¡É•”­¥¹èÀÁÉ½™¥±”•Ù•¹ÑÌ¸(€€€€€€€±•Ğ•¹ÑÉ¥•Ì€ôÙ•Œ…l(€€€€€€€€€€€5•¹Ñ¥½¹AÉ½™¥±”ì(€€€€€€€€€€€€€€€ÁÕ‰­•äèA-}Y1%}°(€€€€€€€€€€€€€€€½¹Ñ•¹Ñ}©Í½¸èÈŒ‰ì‰‘¥ÍÁ±…å}¹…µ”ˆè‰±¥”‰ôˆŒ°(€€€€€€€€€€€ô°(€€€€€€€€€€€5•¹Ñ¥½¹AÉ½™¥±”ì(€€€€€€€€€€€€€€€ÁÕ‰­•äèA-}Y1%}°(€€€€€€€€€€€€€€€½¹Ñ•¹Ñ}©Í½¸èÈŒ‰ì‰‘¥ÍÁ±…å}¹…µ”ˆè‰	½ˆ‰ôˆŒ°(€€€€€€€€€€€ô°(€€€€€€€€€€€5•¹Ñ¥½¹AÉ½™¥±”ì(€€€€€€€€€€€€€€€ÁÕ‰­•äèA-}Y1%}°(€€€€€€€€€€€€€€€½¹Ñ•¹Ñ}©Í½¸èÈŒ‰ì‰¹…µ”ˆè‰…É½°‰ôˆŒ°(€€€€€€€€€€€ô°(€€€€€€€tì((€€€€€€€€¼¼	½‘äµ•¹Ñ¥½¹Ì±¥”…¹…É½°€¡‘¥ÍÁ±…å}¹…µ”™…±±‰…¬Ñ¼¹…µ•€¤¸(€€€€€€€±•Ğ¹…µ•Ì€ô•áÑÉ…Ñ}…Ñ}¹…µ•Ì ‰¡•±±¼…±¥”…¹I=0ˆ¤ì(€€€€€€€±•ĞÉ•Í½±Ù•€ôµ…Ñ¡}¹…µ•Í}Ñ½}ÁÉ½™¥±•Ì ™¹…µ•Ì°€™•¹ÑÉ¥•Ì¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡É•Í½±Ù•°Ù•Œ…mA-}Y1%}°A-}Y1%}t¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸±¥}Á¥Á•±¥¹•}É•Í½±Ù•Í}µÕ±Ñ¥İ½É‘}‘¥ÍÁ±…å}¹…µ•Ì ¤ì(€€€€€€€±•ĞÁÉ½™¥±•}•Ù•¹ÑÌèY•ŒñÍ•É‘•}©Í½¸èéY…±Õ”ø€ôÙ•Œ…l(€€€€€€€€€€€©Í½¸„¡ì(€€€€€€€€€€€€€€€€‰ÁÕ‰­•äˆèA-}Y1%}°(€€€€€€€€€€€€€€€€‰½¹Ñ•¹ĞˆèÈŒ‰ì‰‘¥ÍÁ±…å}¹…µ”ˆè‰]¥±°A™±••È‰ôˆŒ°(€€€€€€€€€€€ô¤°(€€€€€€€€€€€©Í½¸„¡ì(€€€€€€€€€€€€€€€€‰ÁÕ‰­•äˆèA-}Y1%}°(€€€€€€€€€€€€€€€€‰½¹Ñ•¹ĞˆèÈŒ‰ì‰‘¥ÍÁ±…å}¹…µ”ˆè‰±¥”‰ôˆŒ°(€€€€€€€€€€€ô¤°(€€€€€€€tì((€€€€€€€€¼¼M¥µÕ±…Ñ”Ñ¡”Í¥¹±”µÁ…ÉÍ”Á¥Á•±¥¹”™É½´É•Í½±Ù•}½¹Ñ•¹Ñ}µ•¹Ñ¥½¹Ì¸(€€€€€€€±•ĞµÕĞ¹…µ•}Ñ½}ÁÕ‰­•åÌèÍÑèé½±±•Ñ¥½¹Ìèé!…Í¡5…ÀñMÑÉ¥¹œ°Y•ŒñMÑÉ¥¹œøø€ô(€€€€€€€€€€€ÍÑèé½±±•Ñ¥½¹Ìèé!…Í¡5…Àèé¹•Ü ¤ì(€€€€€€€±•ĞµÕĞ‘¥ÍÁ±…å}¹…µ•ÌèY•ŒñMÑÉ¥¹œø€ôY•Œèé¹•Ü ¤ì(€€€€€€€™½È”¥¸€™ÁÉ½™¥±•}•Ù•¹ÑÌì(€€€€€€€€€€€±•ĞÁÕ‰­•ä€ô”¹•Ğ ‰ÁÕ‰­•äˆ¤¹Õ¹İÉ…À ¤¹…Í}ÍÑÈ ¤¹Õ¹İÉ…À ¤ì(€€€€€€€€€€€±•Ğ½¹Ñ•¹Ñ}©Í½¸€ô”¹•Ğ ‰½¹Ñ•¹Ğˆ¤¹Õ¹İÉ…À ¤¹…Í}ÍÑÈ ¤¹Õ¹İÉ…À ¤ì(€€€€€€€€€€€±•ĞØèÍ•É‘•}©Í½¸èéY…±Õ”€ôÍ•É‘•}©Í½¸èé™É½µ}ÍÑÈ¡½¹Ñ•¹Ñ}©Í½¸¤¹Õ¹İÉ…À ¤ì(€€€€€€€€€€€±•Ğ¹…µ”€ôØ(€€€€€€€€€€€€€€€€¹•Ğ ‰‘¥ÍÁ±…å}¹…µ”ˆ¤(€€€€€€€€€€€€€€€€¹½É}•±Í”¡ñğØ¹•Ğ ‰¹…µ”ˆ¤¤(€€€€€€€€€€€€€€€€¹…¹‘}Ñ¡•¸¡ñ¹ğ¸¹…Í}ÍÑÈ ¤¤(€€€€€€€€€€€€€€€€¹™¥±Ñ•È¡ñ¹ğ€…¸¹¥Í}•µÁÑä ¤¤(€€€€€€€€€€€€€€€€¹Õ¹İÉ…À ¤ì(€€€€€€€€€€€±•Ğ±½İ•È€ô¹…µ”¹Ñ½}…Í¥¥}±½İ•É…Í” ¤ì(€€€€€€€€€€€¹…µ•}Ñ½}ÁÕ‰­•åÌ(€€€€€€€€€€€€€€€€¹•¹ÑÉä¡±½İ•È¤(€€€€€€€€€€€€€€€€¹½É}‘•™…Õ±Ğ ¤(€€€€€€€€€€€€€€€€¹ÁÕÍ ¡ÁÕ‰­•ä¹Ñ½}ÍÑÉ¥¹œ ¤¤ì(€€€€€€€€€€€‘¥ÍÁ±…å}¹…µ•Ì¹ÁÕÍ ¡¹…µ”¹Ñ½}ÍÑÉ¥¹œ ¤¤ì(€€€€€€€ô((€€€€€€€±•Ğ­¹½İ¹}É•™ÌèY•Œğ™ÍÑÈø€ô‘¥ÍÁ±…å}¹…µ•Ì¹¥Ñ•È ¤¹µ…À¡ñÍğÌ¹…Í}ÍÑÈ ¤¤¹½±±•Ğ ¤ì(€€€€€€€±•Ğ¹…µ•Ì€ô•áÑÉ…Ñ}…Ñ}µ•¹Ñ¥½¹Í}İ¥Ñ¡}­¹½İ¸ ‰¡•ä]¥±°A™±••È…¹…±¥”„ˆ°€™­¹½İ¹}É•™Ì¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡¹…µ•Ì°Ù•Œ…l‰İ¥±°Á™±••Èˆ°€‰…±¥”‰t¤ì((€€€€€€€±•ĞÉ•Í½±Ù•èY•ŒñMÑÉ¥¹œø€ô¹…µ•Ì(€€€€€€€€€€€€¹¥Ñ•È ¤(€€€€€€€€€€€€¹™±…Ñ}µ…À¡ñ¹ğ¹…µ•}Ñ½}ÁÕ‰­•åÌ¹•Ğ¡¸¤¹¥¹Ñ½}¥Ñ•È ¤¹™±…ÑÑ•¸ ¤¤(€€€€€€€€€€€€¹±½¹• ¤(€€€€€€€€€€€€¹½±±•Ğ ¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡É•Í½±Ù•°Ù•Œ…mA-}Y1%}°A-}Y1%}	t¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸±¥}Á¥Á•±¥¹•}É•ÑÕÉ¹Í}•µÁÑå}İ¡•¹}¹½}…Ñ}¹…µ•Ì ¤ì(€€€€€€€€¼¼M…¹¥Ñäè¹¼¹…µ•Í€¥¸‰½‘äƒŠH¹¼ÁÉ½™¥±”µ…Ñ …ÑÑ•µÁĞ¹••‘•¸(€€€€€€€±•Ğ¹…µ•Ì€ô•áÑÉ…Ñ}…Ñ}¹…µ•Ì ‰Á±…¥¸µ•ÍÍ…”°¹¼µ•¹Ñ¥½¹Ìˆ¤ì(€€€€€€€…ÍÍ•ÉĞ„¡¹…µ•Ì¹¥Í}•µÁÑä ¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸Á…ÉÍ•}µ•µ‰•É}ÁÕ‰­•åÍ}¥¹½É•Í}¹½¹}Á}Ñ…Ì ¤ì(€€€€€€€±•Ğ•Ù•¹Ğ€ô©Í½¸„¡ì(€€€€€€€€€€€€‰Ñ…Ìˆèl(€€€€€€€€€€€€€€€l‰ˆ°€‰¡…¹¹•°µ¥‰t°(€€€€€€€€€€€€€€€l‰Àˆ°A-}Y1%}t°(€€€€€€€€€€€€€€€l‰ ˆ°€‰¡…¹¹•°µ¥‰t°(€€€€€€€€€€€€€€€l‰”ˆ°€‰Í½µ”µ•Ù•¹Ğ‰t°(€€€€€€€€€€€€€€€l‰Àˆ°A-}Y1%}°€‰İÍÌè¼½É•±…äˆ°€‰µ•µ‰•È‰t°(€€€€€€€€€€€t°(€€€€€€€ô¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡Á…ÉÍ•}µ•µ‰•É}ÁÕ‰­•åÌ ™•Ù•¹Ğ¤°Ù•Œ…mA-}Y1%}°A-}Y1%}	t¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸Á…ÉÍ•}µ•µ‰•É}ÁÕ‰­•åÍ}¡…¹‘±•Í}µ…±™½Éµ•‘}•Ù•¹Ğ ¤ì(€€€€€€€…ÍÍ•ÉĞ„¡Á…ÉÍ•}µ•µ‰•É}ÁÕ‰­•åÌ ™©Í½¸„¡íô¤¤¹¥Í}•µÁÑä ¤¤ì(€€€€€€€…ÍÍ•ÉĞ„¡Á…ÉÍ•}µ•µ‰•É}ÁÕ‰­•åÌ ™©Í½¸„¡ì‰Ñ…Ìˆè€‰¹½Ğ…¸…ÉÉ…ä‰ô¤¤¹¥Í}•µÁÑä ¤¤ì(€€€€€€€…ÍÍ•ÉĞ„¡Á…ÉÍ•}µ•µ‰•É}ÁÕ‰­•åÌ ™©Í½¸„¡ì‰Ñ…Ìˆèml‰À‰uuô¤¤¹¥Í}•µÁÑä ¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸Á…ÉÍ•}µ•µ‰•É}ÁÕ‰­•åÍ}™¥±Ñ•ÉÍ}¥¹Ù…±¥‘}¡•à ¤ì(€€€€€€€€¼¼AÕ‰±¥-•äèé™É½µ}¡•á€É•©•ÑÌ¹½¸µ¡•à…¹İÉ½¹œµ±•¹Ñ ¥¹ÁÕÑÌ…¹(€€€€€€€€¼¼…¹½¹¥…±¥é•Ì¡•à…Í”¸€¡9½Ñ”è¥Ğ…•ÁÑÌ…¹ä€ØĞµ¡…Èàµ½¹±ä¡•à(€€€€€€€€¼¼İ¡½Í”¥¹Ñ••ÈÙ…±Õ”¥Ì¥¸™¥•±ì¥Ğ‘½•Ì¹½ĞÙ•É¥™äÑ¡”Á½¥¹Ğ¥Ì(€€€€€€€€¼¼…ÑÕ…±±ä½¸Ñ¡”ÕÉÙ”ƒŠPÍ…µ”…Ì5@Ì‰•¡…Ù¥½È¸¤(€€€€€€€±•ĞÁ­}ÕÁÁ•É…Í”èMÑÉ¥¹œ€ôA-}Y1%}¹Ñ½}…Í¥¥}ÕÁÁ•É…Í” ¤ì(€€€€€€€±•Ğ•Ù•¹Ğ€ô©Í½¸„¡ì(€€€€€€€€€€€€‰Ñ…Ìˆèl(€€€€€€€€€€€€€€€l‰Àˆ°A-}Y1%}t°€€€€€€€¼¼Ù…±¥°±½İ•É…Í”(€€€€€€€€€€€€€€€l‰Àˆ°Á­}ÕÁÁ•É…Í•t°€€€€€¼¼Ù…±¥¡•à°…¹½¹¥…±¥é•Ñ¼±½İ•É…Í”(€€€€€€€€€€€€€€€l‰Àˆ°€‰Ñ½¼µÍ¡½ÉĞ‰t°€€€€€€¼¼±•¹Ñ ™…¥°(€€€€€€€€€€€€€€€l‰Àˆ°€‰èˆ¹É•Á•…Ğ ØĞ¥t°€€€¼¼¹½¸µ¡•à¡…ÉÌ(€€€€€€€€€€€€€€€l‰Àˆ°€‰„ˆ¹É•Á•…Ğ ØÌ¥t°€€€¼¼½™˜µ‰äµ½¹”±•¹Ñ (€€€€€€€€€€€t°(€€€€€€€ô¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡Á…ÉÍ•}µ•µ‰•É}ÁÕ‰­•åÌ ™•Ù•¹Ğ¤°Ù•Œ…mA-}Y1%}°A-}Y1%}t¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸•áÁ±¥¥Ñ}µ•¹Ñ¥½¹Í}…•ÁÑ}¡•á}…¹‘}¹ÁÕ‰}…¹‘}‘•‘ÕÁ±¥…Ñ” ¤ì(€€€€€€€ÕÍ”¹½ÍÑÈèéQ½	• ÌÈì(€€€€€€€±•Ğ¹ÁÕˆ€ô¹½ÍÑÈèéAÕ‰±¥-•äèé™É½µ}¡•à¡A-}Y1%}¤(€€€€€€€€€€€€¹Õ¹İÉ…À ¤(€€€€€€€€€€€€¹Ñ½}‰• ÌÈ ¤(€€€€€€€€€€€€¹Õ¹İÉ…À ¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„ (€€€€€€€€€€€¹½Éµ…±¥é•}•áÁ±¥¥Ñ}µ•¹Ñ¥½¹Ì ™mA-}Y1%}¹¥¹Ñ¼ ¤°¹ÁÕ‰t¤¹Õ¹İÉ…À ¤°(€€€€€€€€€€€Ù•Œ…mA-}Y1%}t(€€€€€€€€¤ì(€€€€€€€…ÍÍ•ÉĞ„¡¹½Éµ…±¥é•}•áÁ±¥¥Ñ}µ•¹Ñ¥½¹Ì ™l‰¹½Ğµ„µ­•äˆ¹¥¹Ñ¼ ¥t¤¹¥Í}•ÉÈ ¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸•áÁ±¥¥Ñ}µ•¹Ñ¥½¹Í}…ÕÑ¡½É¥é•}ÁÉ•Í•¹Ñ…Ñ¥½¹}Ñ•áÑ}İ¥Ñ¡½ÕÑ}¹…µ•}É•Í½±ÕÑ¥½¸ ¤ì(€€€€€€€±•Ğ¹…µ•Ì€ôÙ•Œ…l‰É•¹…µ•ÕÍ•Èˆ¹¥¹Ñ¼ ¥tì(€€€€€€€±•ĞÁÉ½™¥±•Ì€ôÍÑèé½±±•Ñ¥½¹Ìèé!…Í¡5…Àèé¹•Ü ¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„ (€€€€€€€€€€€É•Í½±Ù•}¹…µ•Í}Ñ½}ÁÕ‰­•åÌ ™¹…µ•Ì°€™ÁÉ½™¥±•Ì°ÑÉÕ”¤¹Õ¹İÉ…À ¤°(€€€€€€€€€€€Y•ŒèèñMÑÉ¥¹œøèé¹•Ü ¤(€€€€€€€€¤ì(€€€€€€€…ÍÍ•ÉĞ„¡É•Í½±Ù•}¹…µ•Í}Ñ½}ÁÕ‰­•åÌ ™¹…µ•Ì°€™ÁÉ½™¥±•Ì°™…±Í”¤¹¥Í}•ÉÈ ¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸•áÁ±¥¥Ñ}µ•¹Ñ¥½¹Í}…ÕÑ¡½É¥é•}…µ‰¥Õ½ÕÍ}ÁÉ•Í•¹Ñ…Ñ¥½¹}Ñ•áĞ ¤ì(€€€€€€€±•Ğ¹…µ•Ì€ôÙ•Œ…l‰…±¥”ˆ¹¥¹Ñ¼ ¥tì(€€€€€€€±•ĞÁÉ½™¥±•Ì€ôÍÑèé½±±•Ñ¥½¹Ìèé!…Í¡5…Àèé™É½´¡l (€€€€€€€€€€€€‰…±¥”ˆ¹¥¹Ñ¼ ¤°(€€€€€€€€€€€Ù•Œ…mA-}Y1%}¹¥¹Ñ¼ ¤°A-}Y1%}¹¥¹Ñ¼ ¥t°(€€€€€€€€¥t¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„ (€€€€€€€€€€€É•Í½±Ù•}¹…µ•Í}Ñ½}ÁÕ‰­•åÌ ™¹…µ•Ì°€™ÁÉ½™¥±•Ì°ÑÉÕ”¤¹Õ¹İÉ…À ¤°(€€€€€€€€€€€Y•ŒèèñMÑÉ¥¹œøèé¹•Ü ¤(€€€€€€€€¤ì(€€€€€€€±•Ğ•ÉÉ½È€ôÉ•Í½±Ù•}¹…µ•Í}Ñ½}ÁÕ‰­•åÌ ™¹…µ•Ì°€™ÁÉ½™¥±•Ì°™…±Í”¤¹Õ¹İÉ…Á}•ÉÈ ¤ì(€€€€€€€…ÍÍ•ÉĞ„¡•ÉÉ½È¹Ñ½}ÍÑÉ¥¹œ ¤¹½¹Ñ…¥¹Ì¡A-}Y1%}¤¤ì(€€€€€€€…ÍÍ•ÉĞ„¡•ÉÉ½È¹Ñ½}ÍÑÉ¥¹œ ¤¹½¹Ñ…¥¹Ì¡A-}Y1%}¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸•áÁ±¥¥Ñ}µ•¹Ñ¥½¹Í}µ…­•}…±±}…Ñ}¹…µ•Í}ÁÉ•Í•¹Ñ…Ñ¥½¹}½¹±ä ¤ì(€€€€€€€±•Ğ¹…µ•Ì€ôÙ•Œ…l‰…±¥”ˆ¹¥¹Ñ¼ ¤°€‰‰½ˆˆ¹¥¹Ñ¼ ¥tì(€€€€€€€±•ĞÁÉ½™¥±•Ì€ôÍÑèé½±±•Ñ¥½¹Ìèé!…Í¡5…Àèé™É½´¡l ‰…±¥”ˆ¹¥¹Ñ¼ ¤°Ù•Œ…mA-}Y1%}¹¥¹Ñ¼ ¥t¥t¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„ (€€€€€€€€€€€É•Í½±Ù•}¹…µ•Í}Ñ½}ÁÕ‰­•åÌ ™¹…µ•Ì°€™ÁÉ½™¥±•Ì°ÑÉÕ”¤¹Õ¹İÉ…À ¤°(€€€€€€€€€€€Ù•Œ…mA-}Y1%}t(€€€€€€€€¤ì(€€€€€€€…ÍÍ•ÉĞ„¡É•Í½±Ù•}¹…µ•Í}Ñ½}ÁÕ‰­•åÌ ™¹…µ•Ì°€™ÁÉ½™¥±•Ì°™…±Í”¤¹¥Í}•ÉÈ ¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸½µ‰¥¹•‘}µ•¹Ñ¥½¹}Õ¹¥½¹}•ÉÉ½ÉÍ}¥¹ÍÑ•…‘}½™}ÑÉÕ¹…Ñ¥¹œ ¤ì(€€€€€€€±•Ğ•áÁ±¥¥ĞèY•ŒñMÑÉ¥¹œø€ô€ À¸¸ÔÀ¤¹µ…À¡ñ¥ğ™½Éµ…Ğ„ ‰•áÁ±¥¥Ğµí¥ôˆ¤¤¹½±±•Ğ ¤ì(€€€€€€€…ÍÍ•ÉĞ„¡µ•É•}µ•ÍÍ…•}µ•¹Ñ¥½¹Ì ™•áÁ±¥¥Ğ°€™mt°€™l‰É•Í½±Ù•µ‰½ˆˆ¹¥¹Ñ¼ ¥t¤¹¥Í}•ÉÈ ¤¤ì((€€€€€€€±•ĞµÕĞİ¥Ñ¡}‘ÕÁ±¥…Ñ”€ô•áÁ±¥¥Ğ¹±½¹” ¤ì(€€€€€€€İ¥Ñ¡}‘ÕÁ±¥…Ñ”¹ÁÕÍ ¡•áÁ±¥¥ÑlÁt¹±½¹” ¤¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„ (€€€€€€€€€€€µ•É•}µ•ÍÍ…•}µ•¹Ñ¥½¹Ì ™İ¥Ñ¡}‘ÕÁ±¥…Ñ”°€™m•áÁ±¥¥ÑlÅt¹±½¹” ¥t°€™mt¤(€€€€€€€€€€€€€€€€¹Õ¹İÉ…À ¤(€€€€€€€€€€€€€€€€¹±•¸ ¤°(€€€€€€€€€€€€ÔÀ(€€€€€€€€¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸µ•µ‰•ÉÍ¡¥Á}ÁÉ•™±¥¡Ñ}±¥ÍÑÍ}½¹±å}µ¥ÍÍ¥¹}µ•¹Ñ¥½¹Ì ¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„ (€€€€€€€€€€€µ¥ÍÍ¥¹}µ•µ‰•ÉÌ (€€€€€€€€€€€€€€€€™mA-}Y1%}¹¥¹Ñ¼ ¤°A-}Y1%}¹¥¹Ñ¼ ¥t°(€€€€€€€€€€€€€€€€™mA-}Y1%}¹¥¹Ñ¼ ¥t(€€€€€€€€€€€€¤°(€€€€€€€€€€€Ù•Œ…mA-}Y1%}	t(€€€€€€€€¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸µ•¹Ñ¥½¹}•Ù¥‘•¹•}½µ•Í}™É½µ}Í¥¹•‘}•Ù•¹Ñ}Ñ…Ì ¤ì(€€€€€€€ÕÍ”¹½ÍÑÈèéíÙ•¹Ñ	Õ¥±‘•È°-•åÌ°Q…ôì(€€€€€€€±•Ğ•Ù•¹Ğ€ôÙ•¹Ñ	Õ¥±‘•ÈèéÑ•áÑ}¹½Ñ” ‰¡•±±¼ˆ¤(€€€€€€€€€€€€¹Ñ…Ì¡Ù•Œ…mQ…œèéÁ…ÉÍ”¡l‰Àˆ°A-}Y1%}t¤¹Õ¹İÉ…À ¥t¤(€€€€€€€€€€€€¹Í¥¹}İ¥Ñ¡}­•åÌ ™-•åÌèé•¹•É…Ñ” ¤¤(€€€€€€€€€€€€¹Õ¹İÉ…À ¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡•Ù•¹Ñ}µ•¹Ñ¥½¹}ÁÕ‰­•åÌ ™•Ù•¹Ğ¤°Ù•Œ…mA-}Y1%}t¤ì(€€€ô((€€€€¼¼€´´´´µ…Ñ¡}ÁÉ½™¥±•Í}‰å}¹…µ”€¡…ÕÑ¡½ÈÉ•Í½±ÕÑ¥½¸™½Èµ•ÍÍ…•ÌÍ•…É €´µ…ÕÑ¡½É€¤€´´´´((€€€™¸ÁÉ½™¥±•}•Ù•¹Ğ (€€€€€€€ÁÕ‰­•äè€™ÍÑÈ°(€€€€€€€‘¥ÍÁ±…å}¹…µ”è=ÁÑ¥½¸ğ™ÍÑÈø°(€€€€€€€¹…µ”è=ÁÑ¥½¸ğ™ÍÑÈø°(€€€€¤€´øÍ•É‘•}©Í½¸èéY…±Õ”ì(€€€€€€€±•ĞµÕĞ½¹Ñ•¹Ğ€ôÍ•É‘•}©Í½¸èé5…Àèé¹•Ü ¤ì(€€€€€€€¥˜±•ĞM½µ”¡¤€ô‘¥ÍÁ±…å}¹…µ”ì(€€€€€€€€€€€½¹Ñ•¹Ğ¹¥¹Í•ÉĞ ‰‘¥ÍÁ±…å}¹…µ”ˆ¹¥¹Ñ¼ ¤°©Í½¸„¡¤¤ì(€€€€€€€ô(€€€€€€€¥˜±•ĞM½µ”¡¸¤€ô¹…µ”ì(€€€€€€€€€€€½¹Ñ•¹Ğ¹¥¹Í•ÉĞ ‰¹…µ”ˆ¹¥¹Ñ¼ ¤°©Í½¸„¡¸¤¤ì(€€€€€€€ô(€€€€€€€©Í½¸„¡ì(€€€€€€€€€€€€‰ÁÕ‰­•äˆèÁÕ‰­•ä°(€€€€€€€€€€€€‰½¹Ñ•¹ĞˆèÍ•É‘•}©Í½¸èéY…±Õ”èé=‰©•Ğ¡½¹Ñ•¹Ğ¤¹Ñ½}ÍÑÉ¥¹œ ¤°(€€€€€€€ô¤(€€€ô((€€€€mÑ•ÍÑt(€€€™¸…ÕÑ¡½É}¹…µ•}µ…Ñ¡}¥Í}•á…Ñ}…Í•}¥¹Í•¹Í¥Ñ¥Ù” ¤ì(€€€€€€€±•Ğ•Ù•¹ÑÌ€ôÙ•Œ…l(€€€€€€€€€€€ÁÉ½™¥±•}•Ù•¹Ğ¡A-}Y1%}°M½µ” ‰…É½¸ˆ¤°M½µ” ‰……É½¸ˆ¤¤°(€€€€€€€€€€€€¼¼MÕ‰ÍÑÉ¥¹œ½¹±äƒŠP9%@´ÔÀµ…äÉ•ÑÕÉ¸¥Ğ°‰ÕĞ¥ĞµÕÍĞ¹½Ğµ…Ñ ¸(€€€€€€€€€€€ÁÉ½™¥±•}•Ù•¹Ğ¡A-}Y1%}°M½µ” ‰…É½¹Í½¸ˆ¤°9½¹”¤°(€€€€€€€tì(€€€€€€€±•Ğµ…Ñ¡•Ì€ôµ…Ñ¡}ÁÉ½™¥±•Í}‰å}¹…µ” ™•Ù•¹ÑÌ°€‰…É=¸ˆ¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡µ…Ñ¡•Ì°Ù•Œ…l¡A-}Y1%}¹Ñ½}ÍÑÉ¥¹œ ¤°€‰…É½¸ˆ¹Ñ½}ÍÑÉ¥¹œ ¤¥t¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸…ÕÑ¡½É}¹…µ•}…µ‰¥Õ¥Ñå}É•ÑÕÉ¹Í}…±±}…¹‘¥‘…Ñ•Ì ¤ì(€€€€€€€±•Ğ•Ù•¹ÑÌ€ôÙ•Œ…l(€€€€€€€€€€€ÁÉ½™¥±•}•Ù•¹Ğ¡A-}Y1%}°M½µ” ‰M…´ˆ¤°9½¹”¤°(€€€€€€€€€€€ÁÉ½™¥±•}•Ù•¹Ğ¡A-}Y1%}°9½¹”°M½µ” ‰Í…´ˆ¤¤°(€€€€€€€tì(€€€€€€€±•Ğµ…Ñ¡•Ì€ôµ…Ñ¡}ÁÉ½™¥±•Í}‰å}¹…µ” ™•Ù•¹ÑÌ°€‰Í…´ˆ¤ì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡µ…Ñ¡•Ì¹±•¸ ¤°€È¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸…ÕÑ¡½É}¹…µ•}¹½}µ…Ñ¡}…¹‘}µ…±™½Éµ•‘}½¹Ñ•¹Ğ ¤ì(€€€€€€€±•Ğ•Ù•¹ÑÌ€ôÙ•Œ…l(€€€€€€€€€€€ÁÉ½™¥±•}•Ù•¹Ğ¡A-}Y1%}°M½µ” ‰…É½¸ˆ¤°9½¹”¤°(€€€€€€€€€€€©Í½¸„¡ì‰ÁÕ‰­•äˆèA-}Y1%}°€‰½¹Ñ•¹Ğˆè€‰¹½Ğµ©Í½¸‰ô¤°(€€€€€€€€€€€©Í½¸„¡ì‰½¹Ñ•¹Ğˆè€‰íô‰ô¤°€¼¼µ¥ÍÍ¥¹œÁÕ‰­•ä(€€€€€€€tì(€€€€€€€…ÍÍ•ÉĞ„¡µ…Ñ¡}ÁÉ½™¥±•Í}‰å}¹…µ” ™•Ù•¹ÑÌ°€‰i½”ˆ¤¹¥Í}•µÁÑä ¤¤ì(€€€ô((€€€€mÑ•ÍÑt(€€€™¸…ÕÑ¡½É}¹…µ•}‘•‘ÕÁÍ}É•Á±…•…‰±•}•Ù•¹Ñ}½Á¥•Ì ¤ì(€€€€€€€€¼¼M…µ”€¡ÁÕ‰­•ä°¹…µ”¤…ÁÁ•…É¥¹œÑİ¥”€¡”¹œ¸‘ÕÁ±¥…Ñ”­¥¹èÀÉ½İÌ¤(€€€€€€€€¼¼µÕÍĞÉ•Í½±Ù”Õ¹…µ‰¥Õ½ÕÍ±ä¸(€€€€€€€±•Ğ•Ù•¹ÑÌ€ôÙ•Œ…l(€€€€€€€€€€€ÁÉ½™¥±•}•Ù•¹Ğ¡A-}Y1%}°M½µ” ‰…É½¸ˆ¤°9½¹”¤°(€€€€€€€€€€€ÁÉ½™¥±•}•Ù•¹Ğ¡A-}Y1%}°M½µ” ‰…É½¸ˆ¤°9½¹”¤°(€€€€€€€tì(€€€€€€€…ÍÍ•ÉÑ}•Ä„¡µ…Ñ¡}ÁÉ½™¥±•Í}‰å}¹…µ” ™•Ù•¹ÑÌ°€‰…É½¸ˆ¤¹±•¸ ¤°€Ä¤ì(€€€ô)ô(