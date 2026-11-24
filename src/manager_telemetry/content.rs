//! Redaction for opt-in Manager telemetry.
use rig_core::{
    completion::CompletionRequest,
    message::{AssistantContent, DocumentSourceKind, Message, UserContent},
};
use serde_json::{Value, json};

pub(super) fn sensitive_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("dev.herdr.feishu-bridge")
        || lower
            .split([
                '/', '\\', ' ', '"', '\'', ':', '=', '\t', '\r', '\n', '`', '(', ')',
            ])
            .any(|part| {
                part.starts_with(".env")
                    || matches!(
                        part,
                        ".ssh"
                            | ".aws"
                            | ".config"
                            | ".codex"
                            | ".gemini"
                            | ".claude"
                            | "langfuse.json"
                            | "credentials.json"
                            | "auth.json"
                            | "manager_provider_config"
                            | "bridge.sqlite3"
                            | "id_rsa"
                            | "id_ed25519"
                    )
                    || part.starts_with("secrets.")
                    || part.starts_with("credential.")
                    || part.ends_with(".pem")
                    || part.ends_with(".p12")
                    || part.ends_with(".key")
            })
}

fn known_token(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    [
        "sk-",
        "pk-",
        "sk_",
        "pk_",
        "akia",
        "asia",
        "aiza",
        "ya29",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "xox",
        "eyj",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix) && token.len() >= 8)
}

fn token_like(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    let tenant_token = (lower.starts_with("t-") || lower.starts_with("u-"))
        && token.len() >= 16
        && token.bytes().any(|b| b.is_ascii_digit());
    let path_like = token.starts_with('/')
        || token.contains('/') && !token.contains('+') && !token.ends_with('=');
    let identifier = match token.len() {
        40 => token.bytes().all(|b| b.is_ascii_hexdigit()),
        36 => uuid::Uuid::parse_str(token).is_ok(),
        _ => false,
    };
    known_token(token)
        || tenant_token
        || (token.len() >= 32
            && !path_like
            && !identifier
            && token
                .bytes()
                .any(|b| b.is_ascii_lowercase() || b.is_ascii_uppercase())
            && (token.bytes().any(|b| b.is_ascii_digit())
                || token.bytes().any(|b| b.is_ascii_lowercase())
                    && token.bytes().any(|b| b.is_ascii_uppercase())))
}

fn basic_value(value: &str) -> bool {
    use base64::Engine as _;
    let token = value.trim_matches(|ch: char| matches!(ch, '"' | '\'' | '`' | '(' | ')' | ',' | ';'));
    if token.len() < 4
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=_-".contains(&b))
    {
        return false;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(token)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(token))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(token))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token));
    decoded.is_ok_and(|bytes| {
        bytes.contains(&b':') && bytes.iter().all(|b| matches!(b, 0x20..=0x7E))
    })
}

pub(super) fn clean(text: &str, secrets: &[&str]) -> String {
    clean_text(text, secrets, 0)
}

fn clean_text(text: &str, secrets: &[&str], depth: usize) -> String {
    if depth >= 24 {
        return "[omitted: structured credential inspection limit]".into();
    }
    crate::manager::redact_envelope(text, |name, value| {
        if name == "sender"
            && let Some((label, id)) = value.strip_suffix(')').and_then(|value| value.rsplit_once('('))
            && envelope_identifier("open_id", id)
        {
            format!("{}({})", clean_plain(label, secrets, depth + 1), scrub_known(id, secrets))
        } else if envelope_identifier(name, value) {
            scrub_known(value, secrets).into_owned()
        } else {
            clean_plain(value, secrets, depth + 1)
        }
    })
    .unwrap_or_else(|| clean_plain(text, secrets, depth))
}

fn envelope_identifier(name: &str, value: &str) -> bool {
    let prefix = match name {
        "event_id" | "origin_event_id" | "local_source_id" | "worker_generation" => {
            return uuid::Uuid::parse_str(value).is_ok();
        }
        "message_id" | "root_id" | "parent_id" | "sent_message_id" | "reply_to_message_id" => "om_",
        "thread_id" => "omt_",
        "chat_id" | "sent_chat_id" => "oc_",
        "open_id" | "sender" => "ou_",
        "union_id" => "on_",
        "bot_app_id" => "cli_",
        _ => return false,
    };
    value.strip_prefix(prefix).is_some_and(|id| {
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    })
}

fn clean_plain(text: &str, secrets: &[&str], depth: usize) -> String {
    if text.trim_start().starts_with(['{', '[', '"']) {
        if let Ok(mut value) = serde_json::from_str::<Value>(text) {
            sanitize(&mut value, secrets, depth + 1);
            return value.to_string();
        }
        let checked = crate::manager_context::clean_for_export(text);
        if checked != text {
            return scrub_known(&checked, secrets).into_owned();
        }
    }
    let scrubbed = scrub_known(text, secrets);
    let text = scrubbed.as_ref();
    let lower = text.to_ascii_lowercase();
    if lower.contains("-----begin") && lower.contains("private key-----") {
        return "[redacted private key material]".into();
    }
    let mut result = String::new();
    for line in text.split_inclusive('\n') {
        let lower = line.to_ascii_lowercase();
        let normalized = lower.replace(['_', '-', ' ', '\t', '"', '\'', '\\'], "");
        let credential_header = line
            .split_whitespace()
            .zip(line.split_whitespace().skip(1))
            .any(|(scheme, value)| {
                let scheme = scheme.trim_start_matches(['"', '\'', '`', '(']);
                scheme.eq_ignore_ascii_case("basic") && basic_value(value)
            });
        if credential_header {
            result.push_str("[redacted credential-bearing text]");
            if line.ends_with('\n') {
                result.push('\n');
            }
            continue;
        }
        let credential_assignment = normalized.match_indices([':', '=']).any(|(end, _)| {
            crate::manager_context::SENSITIVE_FIELDS
                .iter()
                .any(|field| normalized[..end].ends_with(field))
        });
        let credential_command = lower
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|word| word == "sshpass")
            || (lower.contains("curl")
                && [
                    " -u",
                    "--user",
                    "--proxy-user",
                    "--oauth2-bearer",
                    "--netrc",
                ]
                .iter()
                .any(|flag| lower.contains(flag)));
        let credential_url = lower.match_indices("://").any(|(start, _)| {
            lower[start + 3..]
                .split(['/', '?', '#', ' ', '\t', '\n', '"', '\''])
                .next()
                .is_some_and(|authority| authority.contains('@'))
        });
        if sensitive_path(line) || credential_assignment || credential_command || credential_url {
            // Explicit credential context can continue across YAML blocks,
            // quoted values, or pasted file contents on subsequent lines.
            // Omit the remainder instead of guessing where the value ends.
            result.push_str("[redacted credential-bearing text]");
            break;
        }
        let mut start = 0;
        for (index, ch) in line
            .char_indices()
            .chain(std::iter::once((line.len(), '\0')))
        {
            if ch.is_ascii_alphanumeric() || "+/=_-".contains(ch) {
                continue;
            }
            let token = &line[start..index];
            // A path or a pasted URL may contain a credential after a slash.
            let protected = token_like(token) || token.split('/').any(known_token);
            result.push_str(if protected { "[redacted]" } else { token });
            if index < line.len() {
                result.push(ch);
            }
            start = index + ch.len_utf8();
        }
    }
    crate::manager_context::clean_for_export(&result)
}

pub(super) fn model_name(text: &str, secrets: &[&str]) -> String {
    let mut name = clean(text, secrets);
    if name.len() > 256 {
        name.truncate(name.floor_char_boundary(256));
    }
    name
}

fn scrub_known<'a>(text: &'a str, secrets: &[&str]) -> std::borrow::Cow<'a, str> {
    let mut text = std::borrow::Cow::Borrowed(text);
    for secret in secrets {
        if !secret.is_empty() && text.contains(secret) {
            text = std::borrow::Cow::Owned(text.replace(secret, "[redacted]"));
        }
    }
    text
}

pub(super) fn protected_arguments(path: Option<&Value>) -> bool {
    path.and_then(Value::as_str).is_some_and(sensitive_path)
}

pub(super) fn has_protected_tool(request: &CompletionRequest) -> bool {
    request.chat_history.iter().any(|message| match message {
        Message::Assistant(message) => message.content.iter().any(|item| match item {
            AssistantContent::ToolCall(call) => {
                protected_arguments(call.function.arguments.get("path"))
            }
            _ => false,
        }),
        _ => false,
    })
}

fn sanitize(value: &mut Value, secrets: &[&str], depth: usize) {
    if depth >= 24 {
        *value = json!("[omitted: structured credential inspection limit]");
        return;
    }
    match value {
        Value::Object(fields) => {
            if protected_arguments(fields.get("path")) {
                *value = json!("[redacted path]");
                return;
            }
            for (key, mut value) in std::mem::take(fields) {
                if crate::manager_context::sensitive(&key) {
                    value = json!("[redacted sensitive field]");
                } else {
                    sanitize(&mut value, secrets, depth + 1);
                }
                fields.insert(scrub_known(&key, secrets).into_owned(), value);
            }
        }
        Value::Array(values) => {
            for value in values {
                sanitize(value, secrets, depth + 1);
            }
        }
        Value::String(text) => *text = clean_text(text, secrets, depth + 1),
        _ => {}
    }
}

pub(super) fn request_text(request: &CompletionRequest, secrets: &[&str]) -> String {
    let mut messages = Vec::with_capacity(request.chat_history.len());
    for message in &request.chat_history {
        let mut value = json!(message);
        sanitize(&mut value, secrets, 0);
        // Only host-hydrated image blocks may retain their base64 data.
        // Strings inside event envelopes or tool results take the normal
        // redaction path even if they claim to contain image data.
        if let Message::User { content } = message {
            for (index, block) in content.iter().enumerate() {
                if let UserContent::Image(image) = block
                    && let DocumentSourceKind::Base64(pixels) = &image.data
                    && let Some(data) = value
                        .get_mut("content")
                        .and_then(|content| content.get_mut(index))
                        .and_then(|block| block.get_mut("data"))
                        .and_then(Value::as_object_mut)
                {
                    data.insert("value".into(), json!(pixels));
                }
            }
        }
        messages.push(value);
    }
    json!({"chat_history": messages}).to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn envelope_keeps_feishu_identifiers_and_redacts_sender_credentials() {
        let xml = r#"<dauerbot_event event_id="2dfef1b8-357f-4e6a-b301-d039ff8e5174" kind="user" source="feishu" thread_id="omt_0123456789abcdef0123456789abcdef" chat_id="oc_0123456789abcdef0123456789abcdef" sender="fixture-secret(ou_0123456789abcdef0123456789abcdef)"><quoted_message message_id="om_0123456789abcdef0123456789abcdef"><body>fixture-secret</body></quoted_message></dauerbot_event>"#;
        assert_eq!(clean(xml, &["fixture-secret"]), xml.replace("fixture-secret", "[redacted]"));
    }

    #[test]
    fn request_preserves_full_history_and_image_pixels() {
        use rig_core::message::{CallId, ToolName};

        let mut request = CompletionRequest::new(json!({
            "event":"user","data":{"user_input":{"body":"hello from the channel","resources":["a resource"]}}
        }).to_string()).preamble("System instructions");
        if let Message::User { content } = &mut request.chat_history[1] {
            content.push(UserContent::image_base64("aGVs".repeat(16), None, None));
        }
        request
            .chat_history
            .push(Message::from(AssistantContent::tool_call(
                "call",
                ToolName::new("read_file").unwrap(),
                json!({"path":"notes.md"}),
            )));
        request.chat_history.push(Message::tool_result(
            CallId::from_wire("call"),
            ToolName::new("read_file").unwrap(),
            r#"{"password":"fixture-short!","body":"ordinary result","caption":"exact-secret"}"#,
        ));
        let text = request_text(&request, &["exact-secret"]);
        for expected in [
            "System instructions",
            "hello from the channel",
            "a resource",
            "aGVs".repeat(16).as_str(),
            "read_file",
            "notes.md",
            "ordinary result",
        ] {
            assert!(text.contains(expected), "{expected}");
        }
        assert!(!text.contains("fixture-short!") && !text.contains("exact-secret"));
    }

    use super::*;

    fn test_clean(text: &str) -> String {
        clean(text, &[])
    }

    #[test]
    fn redacts_known_credentials_and_sensitive_paths() {
        for secret in [
            "sk-fixture-secret",
            "pk-fixture-public",
            "Bearer fixture-short",
            "Bearer abcdefghijklmno",
            "Bearer   abcdefghijklmno",
            "Basic Zm9vOmJhcg==",
            "Authorization: Basic Zml4dHVyZTpzZWNyZXQ=",
            "Authorization: Bearer fixture-token",
            "ghp_fixtureToken123456",
            "xoxp-fixture-token",
            "api_key = fixture-short",
            "app_secret=fixture-short",
            "secret_key = \"fixture-short!\"",
            "provider_key = \"short-provider!\"",
            r#"{"base64":"aGVsbG8="}"#,
            "Token: fixture-short",
            "secret: \n fixture-multiline-value",
            "secret: fixture-short",
            "AKIAIOSFODNN7EXAMPLE",
            "AIzaSyFixtureValue",
            "t-fixture-token-123456",
            "v1DH53qT+j3Cpm6FeUk/PLbNMX3Aj4Tw==",
            "abcdef1234567890abcdef1234567890",
            "/private/.dauerbot/sessions/credentials.json",
            "~/.ssh/id_ed25519",
            "nested/.env.production",
            "langfuse.json",
        ] {
            assert!(!test_clean(secret).contains(secret), "{secret}");
        }
        for escaped in [
            r#"{"config":"{\"password\":\"fixture-short!\"}"}"#,
            r#"pasted JSON: {\"secret_key\":\"fixture-short!\"}"#,
            r#"{"config":"{\"pass\\u0077ord\":\"fixture-short!\"}"}"#,
        ] {
            assert!(!test_clean(escaped).contains("fixture-short!"));
        }
        for command in [
            "sshpass -p 'fixture-short!' ssh fixture-host",
            "curl --user account:fixture-short! https://fixture.invalid",
            "git clone https://account:fixture-short!@fixture.invalid/repo",
        ] {
            assert!(!test_clean(command).contains("fixture-short!"));
        }
        assert!(
            !crate::manager_context::clean(r#"{"base64":"aGVsbG8="}"#)
                .0
                .contains("aGVsbG8=")
        );
        let yaml = "ordinary text\npassword: |\n  first-part\n  second-part\nregular after secret";
        let redacted = test_clean(yaml);
        assert!(redacted.starts_with("ordinary text"));
        assert!(!redacted.contains("first-part") && !redacted.contains("second-part"));
    }

    #[test]
    fn preserves_benign_prose_and_project_paths() {
        let text = "basic auth overview\n0123456789abcdef0123456789abcdef01234567\n550e8400-e29b-41d4-a716-446655440000\n/workspace/Package123456789012345678901234567890/config.json\nt-test, t-shirt, t-shirt-style and u-turn";
        assert_eq!(test_clean(text), text);
        let bearer_prose = test_clean("discussing bearer tokens\nfollowing line survives");
        assert!(bearer_prose.contains("following line survives"));
        assert!(!sensitive_path("project/config.json"));
        assert!(sensitive_path("dev.herdr.feishu-bridge/config.json"));
        let config = test_clean(r#"{"theme":"dark","password":"fixture-password"}"#);
        assert!(config.contains("dark") && !config.contains("fixture-password"));
    }

    #[test]
    fn preserves_large_plain_and_structured_text_without_size_omission() {
        let body = format!("BEGIN\n{}\nEND", "消息正文。".repeat(30000));
        assert_eq!(test_clean(&body), body);
        let safe_document = serde_json::json!({"body": body}).to_string();
        assert_eq!(test_clean(&safe_document), safe_document);
        let structured =
            serde_json::json!({"body": body, "password": "fixture-hidden!"}).to_string();
        let redacted: Value = serde_json::from_str(&test_clean(&structured)).unwrap_or(Value::Null);
        // The structured scanner must not bypass credential detection at large sizes.
        assert!(!test_clean(&structured).contains("fixture-hidden!"));
        assert!(
            redacted.is_object()
                || test_clean(&structured).ends_with("[redacted credential-bearing text]")
        );
    }

    #[test]
    fn malformed_large_structured_text_fails_closed() {
        let text = format!(
            r#"{{"pass\u0077ord":"fixture-hidden!"{}"#,
            " ".repeat(300_000)
        );
        assert_eq!(
            test_clean(&text),
            "[omitted: structured credential inspection failed]"
        );
    }

    #[test]
    fn tool_history_can_omit_path_without_bypassing_protected_paths() {
        use rig_core::message::ToolName;
        use serde_json::json;

        let call = |name: &str, arguments| {
            Message::from(AssistantContent::tool_call(
                "call",
                ToolName::new(name).unwrap(),
                arguments,
            ))
        };
        let mut request: CompletionRequest = serde_json::from_value(json!({
            "chat_history": [],
            "documents": [],
            "tools": []
        }))
        .unwrap();
        request
            .chat_history
            .push(call("send_md", json!({"text":"Reply"})));
        assert!(!has_protected_tool(&request));
        request
            .chat_history
            .push(call("read_file", json!({"path":"private/.env"})));
        assert!(has_protected_tool(&request));
    }
}
