use serde_json::{Map, Value};
use std::fmt::Write;

pub(super) fn event_envelope(
    id: &str,
    origin: Option<&str>,
    kind: &str,
    mut data: Value,
    mut source: Value,
) -> String {
    let source_kind = source["source_kind"].take();
    let platform = source["source"].take();
    let group = source["group_context"].take();
    let source_kind = source_kind.as_str().or(data["source"].as_str());
    let (kind, source_kind, event_id) = match kind {
        "worker" if data["kind"] == "background_tool_result" => {
            ("background_tool_result", "manager", id)
        }
        "worker" => ("worker", "worker", id),
        "schedule" => (
            "timer",
            source_kind.unwrap_or("timer"),
            origin.unwrap_or(id),
        ),
        _ => (kind, source_kind.unwrap_or("local"), origin.unwrap_or(id)),
    };
    let mut output = format!(
        "<dauerbot_event event_id=\"{}\" kind=\"{}\" source=\"{}\">\n",
        crate::xml(event_id),
        crate::xml(kind),
        crate::xml(source_kind)
    );

    let mut metadata = Map::new();
    if let Some(fields) = source.as_object_mut() {
        fields.remove("event_id");
        metadata.append(fields);
    }
    if let Value::Object(mut platform) = platform {
        metadata.append(&mut platform);
    }
    if let Value::Object(mut group) = group {
        if group.get("chat_id") == metadata.get("chat_id") {
            group.remove("chat_id");
        }
        if group.get("app_id") == metadata.get("bot_app_id") {
            group.remove("app_id");
        }
        metadata.insert("group_context".into(), Value::Object(group));
    }
    if kind == "worker"
        && let Some(origin) = origin
    {
        metadata.insert("origin_event_id".into(), Value::from(origin));
    }
    metadata.retain(|_, value| !value.is_null());
    if !metadata.is_empty() {
        element(&mut output, "metadata", &Value::Object(metadata), 1);
    }
    element(&mut output, "quoted_message", &data["quoted_message"], 1);
    if let Value::Object(mut input) = data["user_input"].take() {
        if let Some(body) = input.remove("body") {
            if matches!(kind, "timer" | "proposal") {
                data["instructions"] = body;
            } else {
                element(&mut output, "body", &body, 1);
            }
        }
        for (name, value) in &input {
            element(&mut output, name, value, 1);
        }
    }
    if let Value::Object(mut payload) = data {
        for field in [
            "source",
            "trusted_source",
            "user_input",
            "quoted_message",
            "source_event_id",
        ] {
            payload.remove(field);
        }
        if matches!(
            kind,
            "user" | "timer" | "proposal" | "background_tool_result"
        ) {
            payload.remove("event_id");
        }
        if kind == "background_tool_result" {
            payload.remove("kind");
        }
        if !payload.is_empty() {
            element(
                &mut output,
                if kind == "user" { "preview" } else { kind },
                &Value::Object(payload),
                1,
            );
        }
    }
    output.push_str("</dauerbot_event>");
    output
}

fn element(output: &mut String, name: &str, value: &Value, depth: usize) {
    if value.is_null() {
        return;
    }
    let indent = "  ".repeat(depth);
    if !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        let _ = writeln!(output, "{indent}<field>");
        element(output, "name", &Value::from(name), depth + 1);
        element(output, "value", value, depth + 1);
        let _ = writeln!(output, "{indent}</field>");
        return;
    }
    let _ = write!(output, "{indent}<{name}>");
    match value {
        Value::Object(fields) => {
            output.push('\n');
            for (key, value) in fields {
                element(output, key, value, depth + 1);
            }
            output.push_str(&indent);
        }
        Value::Array(items) => {
            output.push('\n');
            let item_name = match name {
                "resources" => "resource",
                "mentions" => "mention",
                "members" => "member",
                _ => "item",
            };
            for item in items {
                element(output, item_name, item, depth + 1);
            }
            output.push_str(&indent);
        }
        Value::String(text) => output.push_str(&crate::xml(text)),
        _ => {
            let _ = write!(output, "{value}");
        }
    }
    let _ = writeln!(output, "</{name}>");
}

// Event values are escaped before storage, so only the formatter emits markup.
pub(crate) fn redact_envelope(
    text: &str,
    mut redact: impl FnMut(&str, &str) -> String,
) -> Option<String> {
    let root = if text.starts_with("<dauerbot_event event_id=\"") {
        "dauerbot_event"
    } else if text.starts_with("<project_memory path=\"AGENTS.md\"") {
        "project_memory"
    } else {
        return None;
    };
    let omitted = || Some(format!("<{root}>[redacted]</{root}>"));
    let mut redact_value = |name: &str, escaped: &str| {
        let decoded = escaped
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&");
        let cleaned = redact(name, &decoded);
        if cleaned == decoded
            || matches!(
                (serde_json::from_str::<Value>(&decoded), serde_json::from_str::<Value>(&cleaned)),
                (Ok(before), Ok(after)) if before == after
            )
        {
            escaped.to_owned()
        } else {
            crate::xml(&cleaned)
        }
    };
    let mut result = String::new();
    for part in text.split('<').skip(1) {
        let Some((tag, value)) = part.split_once('>') else {
            return omitted();
        };
        if crate::manager_context::sensitive(tag.trim_start_matches('/'))
            || (tag == "name" && crate::manager_context::sensitive(value))
        {
            return omitted();
        }
        result.push('<');
        let attributes = tag.split('"').collect::<Vec<_>>();
        if !matches!(
            attributes.as_slice(),
            [_] | [
                "dauerbot_event event_id=",
                _,
                " kind=",
                _,
                " source=",
                _,
                ""
            ] | ["project_memory path=", "AGENTS.md", ""]
                | [
                    "project_memory path=",
                    "AGENTS.md",
                    " status=",
                    "absent",
                    "/"
                ]
        ) {
            return omitted();
        }
        let mut name = "";
        for (index, part) in attributes.into_iter().enumerate() {
            if index > 0 {
                result.push('"');
            }
            if index % 2 == 1 {
                result.push_str(&redact_value(name, part));
            } else {
                result.push_str(part);
                name = part
                    .trim_end_matches('=')
                    .rsplit(' ')
                    .next()
                    .unwrap_or_default();
            }
        }
        result.push('>');
        if value.trim().is_empty() {
            result.push_str(value);
        } else {
            result.push_str(&redact_value(tag, value));
        }
    }
    Some(result)
}
