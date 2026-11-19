//! Typed manager input over the same legacy PTY runtime used by the desktop.
use super::{ExecutionRef, check_identity, observe, rpc};
use crate::{config::Config, db::Project};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_SEGMENTS: usize = 32;
pub const MAX_INPUT_BYTES: usize = 16 * 1024;
pub const MAX_SCREEN_BYTES: usize = 32 * 1024;
pub const MAX_SCREEN_LINES: usize = 200;

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum Segment {
    Text { text: String },
    Key { key: String },
}

// Emit the compact form; retain strict legacy decoding for saved checkpoints.
impl<'de> Deserialize<'de> for Segment {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
        enum Legacy {
            Text { text: String },
            Key { key: String },
        }
        #[derive(Deserialize)]
        #[serde(untagged, deny_unknown_fields)]
        enum Input {
            Text { text: String },
            Key { key: String },
            Legacy(Legacy),
        }
        Ok(match Input::deserialize(deserializer)? {
            Input::Text { text } | Input::Legacy(Legacy::Text { text }) => Self::Text { text },
            Input::Key { key } | Input::Legacy(Legacy::Key { key }) => Self::Key { key },
        })
    }
}

/// Parse every segment before submitting any input. Strings never imply keys.
pub fn validate(segments: &[Segment]) -> Result<()> {
    ensure!(
        !segments.is_empty() && segments.len() <= MAX_SEGMENTS,
        "Provide between 1 and 32 input segments"
    );
    let mut bytes = 0usize;
    for segment in segments {
        match segment {
            Segment::Text { text } => {
                ensure!(!text.is_empty(), "Text segments cannot be empty");
                ensure!(
                    !text
                        .chars()
                        .any(|c| c.is_control() && !matches!(c, '\n' | '\t')),
                    "Text contains a terminal control character; use an explicit key segment"
                );
                bytes = bytes.saturating_add(text.len());
            }
            Segment::Key { key } => {
                canonical_key(key)?;
                bytes = bytes.saturating_add(key.len());
            }
        }
    }
    ensure!(
        bytes <= MAX_INPUT_BYTES,
        "Input sequence exceeds 16384 bytes"
    );
    Ok(())
}

/// A small documented subset of HERDR's key parser, not CLI-specific commands.
pub fn canonical_key(key: &str) -> Result<String> {
    let lower = key.trim().to_ascii_lowercase();
    let mut pieces: Vec<_> = lower.split('+').collect();
    let code = pieces.pop().unwrap_or("");
    let mut modifiers = Vec::new();
    for modifier in pieces {
        ensure!(
            matches!(modifier, "ctrl" | "alt" | "shift") && !modifiers.contains(&modifier),
            "Unknown or repeated key modifier"
        );
        modifiers.push(modifier);
    }
    let code = match code {
        "return" | "回车" => "enter",
        "escape" => "esc",
        value => value,
    };
    let named = matches!(
        code,
        "enter"
            | "tab"
            | "esc"
            | "backspace"
            | "delete"
            | "up"
            | "down"
            | "left"
            | "right"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
            | "space"
    );
    ensure!(
        named
            || (code.len() == 1
                && code.as_bytes()[0].is_ascii_alphabetic()
                && !modifiers.is_empty()),
        "Unknown key; use a text segment for printable text"
    );
    modifiers.push(code);
    Ok(modifiers.join("+"))
}

pub fn input_params(pane: &str, segment: &Segment) -> Result<Value> {
    Ok(match segment {
        Segment::Text { text } => json!({"pane_id":pane,"text":text,"keys":[]}),
        Segment::Key { key } => json!({"pane_id":pane,"text":"","keys":[canonical_key(key)?]}),
    })
}

/// Inspect immutable execution identity without admitting any input.
pub async fn inspect_input(config: &Config, project: &Project) -> Result<Value> {
    let observed = observe(config, project).await?;
    check_identity(project, &observed)?;
    Ok(observed)
}

/// Submit only after the caller's final control-authority check. No observation
/// RPC may be inserted between that check and this bounded input RPC.
pub async fn submit_observed(
    config: &Config,
    project: &Project,
    observed: &Value,
    segment: &Segment,
) -> Result<()> {
    check_identity(project, observed)?;
    let execution = ExecutionRef::from_project(project)?;
    rpc(
        config,
        "pane.send_input",
        input_params(&execution.pane_id, segment)?,
    )
    .await?;
    Ok(())
}

/// Standalone transport helper for isolated backend fixtures. Manager input uses
/// inspect_input followed by its project-control lock and submit_observed.
pub async fn submit(config: &Config, project: &Project, segment: &Segment) -> Result<()> {
    let observed = inspect_input(config, project).await?;
    submit_observed(config, project, &observed, segment).await
}

/// The legacy pane API returns rendered visible text, not an ANSI byte replay.
/// It does not expose cursor/column geometry, so those fields stay null.
pub async fn screen(config: &Config, project: &Project) -> Result<Value> {
    let observed = observe(config, project).await?;
    check_identity(project, &observed)?;
    let execution = ExecutionRef::from_project(project)?;
    let raw = rpc(config, "pane.read", json!({"pane_id":execution.pane_id,"source":"visible","strip_ansi":true,"lines":MAX_SCREEN_LINES})).await?;
    // Verify immutable ownership again before returning a reusable pane address's text.
    let after = observe(config, project).await?;
    check_identity(project, &after)?;
    let read = raw.get("read").unwrap_or(&raw);
    ensure!(
        read.get("pane_id").is_none_or(|p| p == &execution.pane_id),
        "Terminal screen identity changed"
    );
    let text = read["text"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("HERDR returned no terminal text"))?;
    let (text, clipped, redacted) = present_screen(text);
    Ok(
        json!({"source":"legacy_rendered_visible_text","project_id":project.id,"generation":project.generation,"native_session":project.native_session,"captured_at":crate::db::now(),"text":text,"truncated":clipped || read["truncated"]==true,"redacted":redacted,"rows":null,"cols":null,"cursor":null,"metadata":"The legacy pane.read API does not provide cursor or viewport geometry."}),
    )
}

/// Conservative diagnostic filtering; no raw screen is logged or written here.
/// Unknown, unlabelled secrets cannot be identified reliably from terminal text.
pub fn present_screen(raw: &str) -> (String, bool, bool) {
    let mut shown = String::new();
    let mut redacted = false;
    let mut clipped = false;
    let mut private_key_block = false;
    for (index, line) in raw.lines().enumerate() {
        if index >= MAX_SCREEN_LINES {
            clipped = true;
            break;
        }
        let lower = line.to_ascii_lowercase();
        private_key_block |= lower.contains("-----begin") && lower.contains("private key");
        let sensitive = private_key_block
            || [
                "api_key",
                "api key",
                "authorization",
                "bearer ",
                "password",
                "secret",
                "token",
                "sk-",
                "ghp_",
                "github_pat_",
                "private key",
            ]
            .iter()
            .any(|part| lower.contains(part));
        let line = if sensitive {
            redacted = true;
            "[redacted sensitive terminal line]"
        } else {
            line
        };
        let separator = usize::from(!shown.is_empty());
        let remaining = MAX_SCREEN_BYTES.saturating_sub(shown.len() + separator);
        if remaining == 0 {
            clipped = true;
            break;
        }
        if separator > 0 {
            shown.push('\n');
        }
        if line.len() > remaining {
            let mut boundary = remaining;
            while !line.is_char_boundary(boundary) {
                boundary -= 1;
            }
            shown.push_str(&line[..boundary]);
            clipped = true;
            break;
        }
        shown.push_str(line);
        if lower.contains("-----end") && lower.contains("private key") {
            private_key_block = false;
        }
    }
    (shown, clipped, redacted)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_input_is_unambiguous_and_prevalidates_the_entire_sequence() {
        let segments: Vec<Segment> = serde_json::from_value(json!([
            {"text":"停止当前工作\n下一行"},{"key":"alt+m"},{"key":"enter"}
        ]))
        .unwrap();
        validate(&segments).unwrap();
        assert_eq!(
            input_params("p", &segments[0]).unwrap()["text"],
            "停止当前工作\n下一行"
        );
        assert_eq!(input_params("p", &segments[0]).unwrap()["keys"], json!([]));
        assert_eq!(
            input_params("p", &segments[1]).unwrap()["keys"],
            json!(["alt+m"])
        );
        assert!(serde_json::from_value::<Vec<Segment>>(json!(["text", "alt+m"])).is_err());
        assert!(
            serde_json::from_value::<Segment>(json!({"type":"text","text":"x","key":"enter"}))
                .is_err()
        );
        for invalid in [
            json!({}),
            json!({"text":"x","key":"enter"}),
            json!({"key":"enter","other":1}),
            json!({"type":"key","text":"x"}),
        ] {
            assert!(serde_json::from_value::<Segment>(invalid).is_err());
        }
        let legacy: Segment = serde_json::from_value(json!({"type":"key","key":"enter"})).unwrap();
        assert_eq!(
            serde_json::to_value(legacy).unwrap(),
            json!({"key":"enter"})
        );
        let literal = Segment::Text {
            text: "enter".into(),
        };
        assert_eq!(input_params("p", &literal).unwrap()["keys"], json!([]));
        for bad in ["not-a-key", "ctrl+ctrl+a", "cmd+x", "abc", "", "ctrl+"] {
            assert!(canonical_key(bad).is_err(), "{bad}");
        }
        assert_eq!(canonical_key("Shift+Alt+M").unwrap(), "shift+alt+m");
        assert!(
            validate(&[
                literal,
                Segment::Key {
                    key: "unknown".into()
                }
            ])
            .is_err()
        );
        assert!(
            validate(&[Segment::Text {
                text: "\x1b[31m".into()
            }])
            .is_err()
        );
    }
    #[test]
    fn screen_is_bounded_utf8_redacted_and_explicit_about_truncation() {
        let (screen, clipped, redacted) =
            present_screen("hello\nAPI key: fixture-sensitive\nworld");
        assert_eq!(screen, "hello\n[redacted sensitive terminal line]\nworld");
        assert!(!clipped);
        assert!(redacted);
        let (screen, _, redacted) = present_screen(
            "-----BEGIN PRIVATE KEY-----\nfixture-private-body\n-----END PRIVATE KEY-----\nafter",
        );
        assert!(redacted);
        assert!(!screen.contains("fixture-private-body"));
        assert!(screen.ends_with("after"));
        let (screen, clipped, _) = present_screen(&"字".repeat(MAX_SCREEN_BYTES));
        assert!(screen.len() <= MAX_SCREEN_BYTES);
        assert!(clipped);
        assert!(screen.chars().all(|c| c == '字'));
        let (screen, clipped, _) = present_screen(&"line\n".repeat(MAX_SCREEN_LINES + 1));
        assert_eq!(screen.lines().count(), MAX_SCREEN_LINES);
        assert!(clipped);
    }
}
