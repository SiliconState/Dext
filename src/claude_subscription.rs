// Claude subscription wire compatibility is independently implemented for Dext
// from the behavior documented by pi-black (MIT, Copyright (c) 2025 Mario Zechner).

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::session::user_home_dir;

pub(crate) const CLAUDE_CODE_VERSION: &str = "2.1.224";
pub(crate) const CLAUDE_CODE_ENTRYPOINT: &str = "sdk-cli";
pub(crate) const AGENT_SDK_SYSTEM_PROMPT: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const CCH_PLACEHOLDER: &str = "cch=00000";
const CCH_SEED: u64 = 0x4d65_9218_e32a_3268;
const PRIME64_1: u64 = 0x9e37_79b1_85eb_ca87;
const PRIME64_2: u64 = 0xc2b2_ae3d_27d4_eb4f;
const PRIME64_3: u64 = 0x1656_67b1_9e37_79f9;
const PRIME64_4: u64 = 0x85eb_ca77_c2b2_ae63;
const PRIME64_5: u64 = 0x27d4_eb2f_1656_67c5;
const CLAUDE_STATE_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeIdentity {
    pub(crate) device_id: String,
    pub(crate) account_uuid: String,
}

fn read_u32_le(bytes: &[u8], offset: usize) -> u64 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes")) as u64
}

fn read_u64_le(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("eight bytes"))
}

fn round(accumulator: u64, input: u64) -> u64 {
    accumulator
        .wrapping_add(input.wrapping_mul(PRIME64_2))
        .rotate_left(31)
        .wrapping_mul(PRIME64_1)
}

fn merge_round(accumulator: u64, value: u64) -> u64 {
    (accumulator ^ round(0, value))
        .wrapping_mul(PRIME64_1)
        .wrapping_add(PRIME64_4)
}

pub(crate) fn xxhash64(bytes: &[u8], seed: u64) -> u64 {
    let mut offset = 0usize;
    let mut hash = if bytes.len() >= 32 {
        let mut v1 = seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2);
        let mut v2 = seed.wrapping_add(PRIME64_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME64_1);
        while offset <= bytes.len() - 32 {
            v1 = round(v1, read_u64_le(bytes, offset));
            v2 = round(v2, read_u64_le(bytes, offset + 8));
            v3 = round(v3, read_u64_le(bytes, offset + 16));
            v4 = round(v4, read_u64_le(bytes, offset + 24));
            offset += 32;
        }
        let mut value = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        value = merge_round(value, v1);
        value = merge_round(value, v2);
        value = merge_round(value, v3);
        merge_round(value, v4)
    } else {
        seed.wrapping_add(PRIME64_5)
    };

    hash = hash.wrapping_add(bytes.len() as u64);
    while offset <= bytes.len().saturating_sub(8) && offset + 8 <= bytes.len() {
        let lane = round(0, read_u64_le(bytes, offset));
        hash = (hash ^ lane)
            .rotate_left(27)
            .wrapping_mul(PRIME64_1)
            .wrapping_add(PRIME64_4);
        offset += 8;
    }
    if offset + 4 <= bytes.len() {
        hash ^= read_u32_le(bytes, offset).wrapping_mul(PRIME64_1);
        hash = hash
            .rotate_left(23)
            .wrapping_mul(PRIME64_2)
            .wrapping_add(PRIME64_3);
        offset += 4;
    }
    while offset < bytes.len() {
        hash ^= (bytes[offset] as u64).wrapping_mul(PRIME64_5);
        hash = hash.rotate_left(11).wrapping_mul(PRIME64_1);
        offset += 1;
    }

    hash ^= hash >> 33;
    hash = hash.wrapping_mul(PRIME64_2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(PRIME64_3);
    hash ^ (hash >> 32)
}

pub(crate) fn version_fingerprint(first_user_prompt: &str) -> String {
    let units = first_user_prompt.encode_utf16().collect::<Vec<_>>();
    let selected = [4usize, 7, 20]
        .into_iter()
        .map(|index| units.get(index).copied().unwrap_or(u16::from(b'0')))
        .collect::<Vec<_>>();
    let selected = String::from_utf16_lossy(&selected);
    let digest = Sha256::digest(format!("59cf53e54c78{selected}{CLAUDE_CODE_VERSION}").as_bytes());
    format!("{:02x}{:02x}", digest[0], digest[1])[..3].to_string()
}

pub(crate) fn billing_header(first_user_prompt: &str) -> String {
    format!(
        "x-anthropic-billing-header: cc_version={CLAUDE_CODE_VERSION}.{}; cc_entrypoint={CLAUDE_CODE_ENTRYPOINT}; {CCH_PLACEHOLDER};",
        version_fingerprint(first_user_prompt)
    )
}

pub(crate) fn user_agent() -> String {
    format!("claude-cli/{CLAUDE_CODE_VERSION} (external, {CLAUDE_CODE_ENTRYPOINT})")
}

pub(crate) fn random_uuid_v4() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).context("generate Claude request id")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

fn valid_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => byte == b'-',
        14 => matches!(byte, b'1'..=b'5'),
        19 => matches!(byte.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b'),
        _ => byte.is_ascii_hexdigit(),
    })
}

pub(crate) fn parse_identity(value: &Value) -> Option<ClaudeIdentity> {
    let device_id = value.get("userID")?.as_str()?;
    let account_uuid = value.get("oauthAccount")?.get("accountUuid")?.as_str()?;
    if device_id.len() != 64
        || !device_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || !valid_uuid(account_uuid)
    {
        return None;
    }
    Some(ClaudeIdentity {
        device_id: device_id.to_string(),
        account_uuid: account_uuid.to_string(),
    })
}

fn claude_state_path() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(user_home_dir)
        .join(".claude.json")
}

fn read_identity(path: &Path) -> Result<Option<ClaudeIdentity>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > CLAUDE_STATE_MAX_BYTES
    {
        return Ok(None);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Ok(None);
        }
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(CLAUDE_STATE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > CLAUDE_STATE_MAX_BYTES {
        return Ok(None);
    }
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    Ok(parse_identity(&value))
}

pub(crate) fn discover_identity() -> Option<ClaudeIdentity> {
    read_identity(&claude_state_path()).ok().flatten()
}

pub(crate) fn metadata_user_id(identity: &ClaudeIdentity, session_id: &str) -> Result<String> {
    #[derive(Serialize)]
    struct MetadataUserId<'a> {
        device_id: &'a str,
        account_uuid: &'a str,
        session_id: &'a str,
    }

    serde_json::to_string(&MetadataUserId {
        device_id: &identity.device_id,
        account_uuid: &identity.account_uuid,
        session_id,
    })
    .context("serialize Claude identity metadata")
}

pub(crate) fn transform_body(
    mut serialized_body: Vec<u8>,
    first_user_prompt: &str,
    session_id: &str,
    identity: Option<&ClaudeIdentity>,
) -> Result<Vec<u8>> {
    let parsed: Value = serde_json::from_slice(&serialized_body)
        .context("Claude subscription request must be a JSON object")?;
    let object = parsed
        .as_object()
        .context("Claude subscription request must be a JSON object")?;
    if object.contains_key("metadata") {
        anyhow::bail!("Claude subscription request already contains metadata");
    }
    let (_, _, system_start, system_end) = top_level_member(&serialized_body, "system")
        .context("Claude subscription request is missing system")?;
    let existing_system = &serialized_body[system_start..system_end];
    if existing_system.first() != Some(&b'[') || existing_system.last() != Some(&b']') {
        anyhow::bail!("Claude subscription request system must be an array");
    }
    let billing_text = billing_header(first_user_prompt);
    let billing_json = serde_json::to_string(&billing_text)?;
    let agent_json = serde_json::to_string(AGENT_SDK_SYSTEM_PROMPT)?;
    let billing_block = format!("{{\"type\":\"text\",\"text\":{billing_json}}}").into_bytes();
    let agent_block = format!("{{\"type\":\"text\",\"text\":{agent_json}}}").into_bytes();
    let mut system =
        Vec::with_capacity(existing_system.len() + billing_block.len() + agent_block.len() + 2);
    system.push(b'[');
    system.extend_from_slice(&billing_block);
    system.push(b',');
    system.extend_from_slice(&agent_block);
    if existing_system.len() > 2 {
        system.push(b',');
        system.extend_from_slice(&existing_system[1..existing_system.len() - 1]);
    }
    system.push(b']');
    serialized_body.splice(system_start..system_end, system);

    if let Some(identity) = identity {
        let metadata = serde_json::to_vec(&serde_json::json!({
            "metadata": {
                "user_id": metadata_user_id(identity, session_id)?,
            }
        }))?;
        let fields = metadata
            .strip_prefix(b"{")
            .and_then(|value| value.strip_suffix(b"}"))
            .context("serialize Claude identity metadata object")?;
        let closing = serialized_body
            .iter()
            .rposition(|byte| *byte == b'}')
            .context("Claude subscription request is not an object")?;
        let mut suffix = Vec::with_capacity(fields.len() + 1);
        suffix.push(b',');
        suffix.extend_from_slice(fields);
        serialized_body.splice(closing..closing, suffix);
    }

    patch_cch(serialized_body, &billing_text)
}

fn top_level_member(bytes: &[u8], key: &str) -> Option<(usize, usize, usize, usize)> {
    let marker = format!("\"{key}\":");
    let marker = marker.as_bytes();
    let mut depth = 0usize;
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' if depth == 1 && bytes[index..].starts_with(marker) => {
                let member_start = index;
                let value_start = index + marker.len();
                let mut cursor = value_start;
                let mut nested = 0usize;
                let mut value_string = false;
                let mut value_escaped = false;
                while cursor < bytes.len() {
                    let current = bytes[cursor];
                    if value_string {
                        if value_escaped {
                            value_escaped = false;
                        } else if current == b'\\' {
                            value_escaped = true;
                        } else if current == b'"' {
                            value_string = false;
                        }
                    } else {
                        match current {
                            b'"' => value_string = true,
                            b'{' | b'[' => nested += 1,
                            b'}' | b']' if nested > 0 => nested -= 1,
                            b',' | b'}' if nested == 0 => break,
                            _ => {}
                        }
                    }
                    cursor += 1;
                }
                let value_end = cursor;
                let member_end = if bytes.get(cursor) == Some(&b',') {
                    cursor + 1
                } else {
                    cursor
                };
                let delete_start = if bytes.get(cursor) != Some(&b',')
                    && member_start > 1
                    && bytes.get(member_start - 1) == Some(&b',')
                {
                    member_start - 1
                } else {
                    member_start
                };
                return Some((delete_start, member_end, value_start, value_end));
            }
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        index += 1;
    }
    None
}

pub(crate) fn patch_cch(serialized_body: Vec<u8>, billing_text: &str) -> Result<Vec<u8>> {
    let parsed: Value = serde_json::from_slice(&serialized_body)
        .context("Claude subscription request must be a JSON object")?;
    let object = parsed
        .as_object()
        .context("Claude subscription request must be a JSON object")?;
    let first_text = object
        .get("system")
        .and_then(Value::as_array)
        .and_then(|system| system.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .context("Claude subscription request is missing its billing system block")?;
    if first_text != billing_text || !billing_text.contains(CCH_PLACEHOLDER) {
        anyhow::bail!("Claude subscription request has an invalid billing system block");
    }
    if object.get("model").and_then(Value::as_str).is_none() {
        anyhow::bail!("Claude subscription request is missing model");
    }
    if !object.get("max_tokens").is_some_and(Value::is_number) {
        anyhow::bail!("Claude subscription request has an invalid max_tokens");
    }

    let (_, _, model_start, model_end) = top_level_member(&serialized_body, "model")
        .context("Claude subscription request is missing model")?;
    let (max_start, max_end, _, _) = top_level_member(&serialized_body, "max_tokens")
        .context("Claude subscription request is missing max_tokens")?;
    let mut operations = vec![
        (model_start, model_end, b"\"\"".as_slice()),
        (max_start, max_end, b"".as_slice()),
    ];
    operations.sort_by_key(|operation| std::cmp::Reverse(operation.0));
    let mut normalized = serialized_body.clone();
    for (start, end, replacement) in operations {
        normalized.splice(start..end, replacement.iter().copied());
    }
    let checksum = xxhash64(&normalized, CCH_SEED) & 0x000f_ffff;
    let patched_text = billing_text.replace(CCH_PLACEHOLDER, &format!("cch={checksum:05x}"));

    let billing_json = serde_json::to_string(billing_text)?;
    let patched_json = serde_json::to_string(&patched_text)?;
    let marker = format!("\"system\":[{{\"type\":\"text\",\"text\":{billing_json}");
    let marker_start = serialized_body
        .windows(marker.len())
        .position(|window| window == marker.as_bytes())
        .context("Claude subscription billing block is not first")?;
    let value_start = marker_start + marker.len() - billing_json.len();
    let value_end = value_start + billing_json.len();
    let mut patched = Vec::with_capacity(serialized_body.len());
    patched.extend_from_slice(&serialized_body[..value_start]);
    patched.extend_from_slice(patched_json.as_bytes());
    patched.extend_from_slice(&serialized_body[value_end..]);
    Ok(patched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xxhash64_matches_standard_vectors() {
        assert_eq!(xxhash64(b"", 0), 0xef46_db37_51d8_e999);
        assert_eq!(xxhash64(b"hello", 0), 0x26c7_827d_889f_6da3);
    }

    #[test]
    fn billing_header_matches_recovered_prompt_vector() {
        assert_eq!(version_fingerprint("Reply with exactly: PROBE_OK"), "f97");
        assert_eq!(
            billing_header("Reply with exactly: PROBE_OK"),
            "x-anthropic-billing-header: cc_version=2.1.224.f97; cc_entrypoint=sdk-cli; cch=00000;"
        );
    }

    #[test]
    fn cch_matches_recovered_normalized_body_vector() {
        let body = br#"{"model":"claude-opus-5","messages":[{"role":"user","content":"A"}],"max_tokens":64000,"stream":true,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.224.000; cc_entrypoint=sdk-cli; cch=00000;"}]}"#.to_vec();
        let billing =
            "x-anthropic-billing-header: cc_version=2.1.224.000; cc_entrypoint=sdk-cli; cch=00000;";
        let patched =
            String::from_utf8(patch_cch(body, billing).expect("patch cch")).expect("utf8 body");
        assert!(patched.contains("cch=7ba34"), "{patched}");
    }

    #[test]
    fn transform_only_prepends_protocol_blocks_and_top_level_metadata() {
        let body = br#"{"model":"claude-opus-5","max_tokens":64000,"system":[{"type":"text","text":"Dext system","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"cch=00000","model":"nested","max_tokens":7}],"tools":[{"name":"probe","description":"model max_tokens cch=00000","input_schema":{"type":"object"}}],"stream":true}"#.to_vec();
        let identity = ClaudeIdentity {
            device_id: "f".repeat(64),
            account_uuid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".to_string(),
        };
        let transformed = transform_body(
            body,
            "Reply with exactly: PROBE_OK",
            "11111111-2222-4333-8444-555555555555",
            Some(&identity),
        )
        .expect("transform body");
        let value: Value = serde_json::from_slice(&transformed).expect("JSON body");
        let system = value["system"].as_array().expect("system array");
        assert!(
            system[0]["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("x-anthropic-billing-header: "))
        );
        assert_eq!(system[1]["text"], AGENT_SDK_SYSTEM_PROMPT);
        assert_eq!(system[2]["text"], "Dext system");
        assert_eq!(value["messages"][0]["content"], "cch=00000");
        assert_eq!(value["messages"][0]["model"], "nested");
        assert_eq!(value["messages"][0]["max_tokens"], 7);
        assert_eq!(
            value["tools"][0]["description"],
            "model max_tokens cch=00000"
        );
        let metadata: Value = serde_json::from_str(
            value["metadata"]["user_id"]
                .as_str()
                .expect("metadata string"),
        )
        .expect("metadata JSON");
        assert_eq!(metadata["device_id"], "f".repeat(64));
        assert_eq!(
            metadata["session_id"],
            "11111111-2222-4333-8444-555555555555"
        );
    }

    #[cfg(unix)]
    #[test]
    fn identity_discovery_rejects_symlinked_state() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "dext-claude-identity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir_all(&root)?;
        let target = root.join("target.json");
        let link = root.join(".claude.json");
        std::fs::write(
            &target,
            serde_json::to_vec(&serde_json::json!({
                "userID": "f".repeat(64),
                "oauthAccount": { "accountUuid": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee" }
            }))?,
        )?;
        symlink(&target, &link)?;
        assert!(read_identity(&link)?.is_none());
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn cch_rejects_non_numeric_top_level_max_tokens() {
        let billing =
            "x-anthropic-billing-header: cc_version=2.1.224.000; cc_entrypoint=sdk-cli; cch=00000;";
        let body = format!(
            "{{\"model\":\"claude-opus-5\",\"max_tokens\":\"64000\",\"system\":[{{\"type\":\"text\",\"text\":{} }}]}}",
            serde_json::to_string(billing).expect("billing JSON")
        )
        .into_bytes();
        assert!(patch_cch(body, billing).is_err());
    }

    #[test]
    fn identity_validation_rejects_unexpected_shapes() {
        let valid = serde_json::json!({
            "userID": "f".repeat(64),
            "oauthAccount": { "accountUuid": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee" }
        });
        assert!(parse_identity(&valid).is_some());
        assert!(
            parse_identity(&serde_json::json!({
                "userID": "bad",
                "oauthAccount": { "accountUuid": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee" }
            }))
            .is_none()
        );
    }
}
