//! Outbound content projection. Untrusted tool output never enters this path.
use rig_core::{
    completion::CompletionRequest,
    message::{AssistantContent, Message, UserContent},
};
use serde_json::Value;

pub(super) const FIELD_LIMIT: usize = 8192;
pub(super) const BATCH_LIMIT: usize = 65536;
const SCAN_LIMIT: usize = 256 * 1024;
const TRUNCATED: &str = "\n[truncated]";

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

fn clean(text: &str) -> String {
    if text.len() > SCAN_LIMIT {
        return "[omitted: inspection limit]".into();
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("-----begin") && lower.contains("private key-----") {
        return "[redacted private key material]".into();
    }
    let mut result = String::new();
    let mut continuation = false;
    for line in text.split_inclusive('\n') {
        let lower = line.to_ascii_lowercase();
        let normalized = lower.replace(['_', '-', ' ', '\t', '"', '\''], "");
        let credential_header = lower
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|word| word == "bearer" || word == "basic");
        let credential_assignment = normalized.match_indices([':', '=']).any(|(end, _)| {
            [
                "apikey",
                "token",
                "secret",
                "password",
                "authorization",
                "credentials",
                "privatekey",
                "publickey",
                "cookie",
                "accesskeyid",
            ]
            .iter()
            .any(|field| normalized[..end].ends_with(field))
        });
        let protected_line =
            continuation || sensitive_path(line) || credential_header || credential_assignment;
        continuation = (credential_assignment
            && normalized.trim_end().ends_with([':', '=', '|', '>']))
            || (credential_header && lower.trim_end().ends_with("bearer"));
        if protected_line {
            result.push_str("[redacted credential-bearing line]");
            if line.ends_with('\n') {
                result.push('\n');
            }
            continue;
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
    result
}

pub(super) fn bounded(text: &str, limit: usize) -> String {
    let text = clean(text);
    if text.len() <= limit {
        return text;
    }
    if limit < TRUNCATED.len() {
        return String::new();
    }
    let mut end = limit - TRUNCATED.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATED}", &text[..end])
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
    for message in request.chat_history.iter().take(64) {
        match message {
            Message::User { content } => {
                for item in content.iter().take(32) {
                    if let UserContent::Text(text) = item {
                        // Manager user text is a host-created context envelope.
                        // Allow only human text and leave files, observations,
                        // configuration, system instructions and metadata behind.
                        if text.text.len() <= SCAN_LIMIT
                            && let Ok(context) = serde_json::from_str::<Value>(&text.text)
                            && context["trusted_project"].is_object()
                        {
                            for field in ["new_inputs", "continuing_inputs"] {
                                for input in
                                    context[field].as_array().into_iter().flatten().take(64)
                                {
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
                for item in message.content.iter().take(32) {
                    if let AssistantContent::Text(text) = item {
                        append(&mut result, "assistant", Some(&text.text));
                    }
                }
            }
            _ => {}
        }
        if result.len() >= FIELD_LIMIT {
            break;
        }
    }
    result
}

pub(super) fn append(target: &mut String, role: &str, value: Option<&str>) {
    if let Some(value) = value
        && target.len() + role.len() + 3 < FIELD_LIMIT
    {
        target.push_str(role);
        target.push_str(": ");
        target.push_str(&bounded(value, FIELD_LIMIT - target.len() - 1));
        target.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_known_credentials_and_sensitive_paths_before_utf8_truncation() {
        for secret in [
            "sk-fixture-secret",
            "pk-fixture-public",
            "Bearer fixture-short",
            "api_key = fixture-short",
            "app_secret=fixture-short",
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
            assert!(!bounded(secret, FIELD_LIMIT).contains(secret), "{secret}");
        }
        let oversized = "消息正文。".repeat(FIELD_LIMIT);
        let output = bounded(&oversized, FIELD_LIMIT);
        assert!(output.starts_with("消息正文。"));
        assert!(output.ends_with("[truncated]"));
        assert!(output.len() <= FIELD_LIMIT);
    }
}
