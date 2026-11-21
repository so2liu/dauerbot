//! Outbound content projection. Untrusted tool output never enters this path.
use rig_core::{
    completion::CompletionRequest,
    message::{AssistantContent, Message, UserContent},
};
use serde_json::Value;

pub(super) fn sensitive_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("library/application support/dev.herdr.feishu-bridge")
        || lower
            .split([
                '/', '\\', ' ', '"', '\'', ':', '=', '\t', '\r', '\n', '`', '(', ')',
            ])
            .any(|part| {
                part.starts_with(".env")
                    || matches!(
                        part,
                        ".dauerbot"
                            | ".ssh"
                            | ".aws"
                            | ".config"
                            | ".codex"
                            | ".gemini"
                            | ".claude"
                            | "langfuse.json"
                            | "credentials.json"
                            | "auth.json"
                            | "config.json"
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

fn token_like(token: &str) -> bool {
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
        "t-",
        "u-",
        "ghp_",
        "github_pat_",
        "xoxb-",
        "eyj",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix) && token.len() >= 8)
        || (token.len() >= 32
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=_-".contains(&b)))
}

pub(super) fn clean(text: &str) -> String {
    // Reuse the diagnostic walk for complete JSON values, including strings
    // that encode other JSON documents. Free-form text still goes through
    // the stricter lexical export filter below.
    let structured;
    let text = if text.trim_start().starts_with(['{', '[', '"']) {
        structured = crate::manager_context::clean_for_export(text);
        structured.as_str()
    } else {
        text
    };
    let lower = text.to_ascii_lowercase();
    if lower.contains("-----begin") && lower.contains("private key-----") {
        return "[redacted private key material]".into();
    }
    let mut result = String::new();
    for line in text.split_inclusive('\n') {
        let lower = line.to_ascii_lowercase();
        let normalized = lower.replace(['_', '-', ' ', '\t', '"', '\'', '\\'], "");
        let credential_header = lower
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|word| word == "bearer" || word == "basic");
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
        if sensitive_path(line)
            || credential_header
            || credential_assignment
            || credential_command
            || credential_url
        {
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
            let protected = token_like(token) || token.split('/').any(token_like);
            result.push_str(if protected { "[redacted]" } else { token });
            if index < line.len() {
                result.push(ch);
            }
            start = index + ch.len_utf8();
        }
    }
    crate::manager_context::clean_for_export(&result)
}

pub(super) fn model_name(text: &str) -> String {
    let mut name = clean(text);
    if name.len() > 256 {
        name.truncate(name.floor_char_boundary(256));
    }
    name
}

pub(super) fn has_protected_tool(request: &CompletionRequest) -> bool {
    request.chat_history.iter().any(|message| match message {
        Message::Assistant(message) => message.content.iter().any(|item| match item {
            AssistantContent::ToolCall(call) => call.function.arguments["path"]
                .as_str()
                .is_some_and(sensitive_path),
            _ => false,
        }),
        _ => false,
    })
}

pub(super) fn request_text(request: &CompletionRequest) -> String {
    let mut result = String::new();
    for message in request.chat_history.iter() {
        match message {
            Message::User { content } => {
                for item in content.iter() {
                    if let UserContent::Text(text) = item {
                        // Manager user text is a host-created context envelope.
                        // Allow only human text and leave files, observations,
                        // configuration, system instructions and metadata behind.
                        if let Ok(context) = serde_json::from_str::<Value>(&text.text)
                            && context["trusted_project"].is_object()
                        {
                            for field in ["new_inputs", "continuing_inputs"] {
                                for input in context[field].as_array().into_iter().flatten() {
                                    if input["kind"] == "user" {
                                        append(
                                            &mut result,
                                            "user",
                                            input["data"]["user_input"]["body"].as_str(),
                                        );
                                    }
                                }
                            }
                            append(
                                &mut result,
                                "user",
                                context["latest_user_steering"]["data"]["user_input"]["body"]
                                    .as_str(),
                            );
                        }
                    }
                }
            }
            Message::Assistant(message) => {
                for item in message.content.iter() {
                    if let AssistantContent::Text(text) = item {
                        append(&mut result, "assistant", Some(&text.text));
                    }
                }
            }
            _ => {}
        }
    }
    result
}

pub(super) fn append(target: &mut String, role: &str, value: Option<&str>) {
    if let Some(value) = value {
        target.push_str(role);
        target.push_str(": ");
        target.push_str(&clean(value));
        target.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_known_credentials_and_sensitive_paths() {
        for secret in [
            "sk-fixture-secret",
            "pk-fixture-public",
            "Bearer fixture-short",
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
            "t-fixture-token",
            "abcdef1234567890abcdef1234567890",
            "/private/.dauerbot/sessions/credentials.json",
            "~/.ssh/id_ed25519",
            "nested/.env.production",
            "langfuse.json",
        ] {
            assert!(!clean(secret).contains(secret), "{secret}");
        }
        for escaped in [
            r#"{"config":"{\"password\":\"fixture-short!\"}"}"#,
            r#"pasted JSON: {\"secret_key\":\"fixture-short!\"}"#,
            r#"{"config":"{\"pass\\u0077ord\":\"fixture-short!\"}"}"#,
        ] {
            assert!(!clean(escaped).contains("fixture-short!"));
        }
        for command in [
            "sshpass -p 'fixture-short!' ssh fixture-host",
            "curl --user account:fixture-short! https://fixture.invalid",
            "git clone https://account:fixture-short!@fixture.invalid/repo",
        ] {
            assert!(!clean(command).contains("fixture-short!"));
        }
        assert!(
            !crate::manager_context::clean(r#"{"base64":"aGVsbG8="}"#)
                .0
                .contains("aGVsbG8=")
        );
        let yaml = "ordinary text\npassword: |\n  first-part\n  second-part\nregular after secret";
        let redacted = clean(yaml);
        assert!(redacted.starts_with("ordinary text"));
        assert!(!redacted.contains("first-part") && !redacted.contains("second-part"));
    }

    #[test]
    fn preserves_large_plain_and_structured_text_without_size_omission() {
        let body = format!("BEGIN\n{}\nEND", "消息正文。".repeat(30000));
        assert_eq!(clean(&body), body);
        let safe_document = serde_json::json!({"body": body}).to_string();
        assert_eq!(clean(&safe_document), safe_document);
        let structured =
            serde_json::json!({"body": body, "password": "fixture-hidden!"}).to_string();
        let redacted: Value = serde_json::from_str(&clean(&structured)).unwrap_or(Value::Null);
        // The structured scanner must not bypass credential detection at large sizes.
        assert!(!clean(&structured).contains("fixture-hidden!"));
        assert!(
            redacted.is_object()
                || clean(&structured).ends_with("[redacted credential-bearing text]")
        );
    }
}
