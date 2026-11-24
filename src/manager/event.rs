use serde_json::Value;
use std::fmt::Write;

pub(super) fn event_envelope(
    id: &str,
    origin: Option<&str>,
    kind: &str,
    data: Value,
    source: Value,
) -> String {
    let platform = &source["source"];
    let group = &source["group_context"];
    let source_kind = source["source_kind"].as_str().or(data["source"].as_str());
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
    let mut output = String::from("<dauerbot_event");
    for (name, value) in [
        ("event_id", event_id),
        ("kind", kind),
        ("source", source_kind),
    ] {
        attribute(&mut output, name, Some(value));
    }
    match kind {
        "user" if source_kind == "feishu" => {
            for field in ["chat_id", "chat_type", "thread_id", "root_id", "parent_id"] {
                attribute(&mut output, field, platform[field].as_str());
            }
            attribute(&mut output, "message_id", source["message_id"].as_str());
            if let Some(id) = platform["sender"]["open_id"].as_str() {
                let sender = group["sender_name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .map_or_else(|| id.to_owned(), |name| format!("{name}({id})"));
                attribute(&mut output, "sender", Some(&sender));
            }
            attribute(
                &mut output,
                "time",
                time(&platform["create_time"]).as_deref(),
            );
            attribute(
                &mut output,
                "chat_name",
                group["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .or(platform["chat_name"].as_str()),
            );
            if platform["mention_all"] == true {
                attribute(&mut output, "mention_all", Some("true"));
            }
        }
        "worker" => attribute(&mut output, "origin_event_id", origin),
        "timer" | "proposal" => {
            for field in ["purpose", "to", "proposal_id"] {
                attribute(
                    &mut output,
                    field,
                    source[field].as_str().or(data[field].as_str()),
                );
            }
            if kind == "proposal" {
                attribute(&mut output, "message_id", source["message_id"].as_str());
            }
            let scheduled = source
                .get("scheduled_at")
                .filter(|value| !value.is_null())
                .unwrap_or(&data["scheduled_at"]);
            attribute(&mut output, "scheduled_at", time(scheduled).as_deref());
        }
        "user" => attribute(
            &mut output,
            "local_source_id",
            source["local_source_id"].as_str(),
        ),
        _ => {}
    }
    output.push_str(">\n");
    let input = &data["user_input"];
    match kind {
        "user" => {
            let resources = input["resources"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let direct = resources
                .iter()
                .filter(|r| r["quoted"] != true && r["preceding"] != true)
                .collect::<Vec<_>>();
            if let Some(body) = input["body"].as_str()
                && (direct.is_empty()
                    || !media_placeholder(body, platform["message_type"].as_str()))
            {
                let body = crate::feishu::render_mentions(body, &platform["mentions"]);
                element(&mut output, "body", &Value::from(body), 1);
            }
            attachments(&mut output, &direct, 1);
            let quote = &data["quoted_message"];
            if quote["status"] == "ok" {
                let quoted = resources
                    .iter()
                    .filter(|r| r["quoted"] == true)
                    .collect::<Vec<_>>();
                context_message(&mut output, "quoted_message", quote, &quoted, 1);
            }
            let preceding = &data["preceding_messages"];
            if preceding["status"] == "ok" && present(&preceding["items"]) {
                output.push_str("  <preceding_messages>\n");
                for message in preceding["items"].as_array().into_iter().flatten().take(3) {
                    let matched = resources
                        .iter()
                        .filter(|r| {
                            r["preceding"] == true && r["message_id"] == message["message_id"]
                        })
                        .collect::<Vec<_>>();
                    context_message(&mut output, "message", message, &matched, 2);
                }
                output.push_str("  </preceding_messages>\n");
            } else if preceding["status"] == "unavailable" {
                output.push_str("  <preceding_messages status=\"unavailable\"");
                attribute(&mut output, "reason", preceding["reason"].as_str());
                output.push_str("/>\n");
            }
        }
        "timer" | "proposal" => {
            let instructions = input.get("body").unwrap_or(&data["instructions"]);
            element(&mut output, "instructions", instructions, 1);
        }
        "worker" if data["kind"] == "worker_run" => {
            output.push_str("  <worker_run");
            attribute(&mut output, "status", data["delivery_status"].as_str());
            attribute(
                &mut output,
                "worker_generation",
                data["worker_generation"].as_str(),
            );
            let mut report = data["worker_report"].clone();
            if let Some(fields) = report.as_object_mut() {
                for field in ["kind", "run_id", "event_id", "semantic_task_success"] {
                    fields.remove(field);
                }
            }
            if present(&data["error"]) || present(&report) {
                output.push_str(">\n");
                element(&mut output, "error", &data["error"], 2);
                element(&mut output, "worker_report", &report, 2);
                output.push_str("  </worker_run>\n");
            } else {
                output.push_str("/>\n");
            }
        }
        "worker" | "background_tool_result" => {
            let name = match data["kind"].as_str() {
                Some(
                    name @ ("worker_message"
                    | "worker_intent"
                    | "cli_transition"
                    | "repair_finished"
                    | "background_tool_result"),
                ) => name,
                _ if data["status"] == "definitely_not_submitted" => "worker_intent",
                _ => kind,
            };
            let mut payload = data.clone();
            if let Some(fields) = payload.as_object_mut() {
                for field in [
                    "kind",
                    "source",
                    "trusted_source",
                    "user_input",
                    "quoted_message",
                    "preceding_messages",
                    "source_event_id",
                ] {
                    fields.remove(field);
                }
            }
            element(&mut output, name, &payload, 1);
        }
        _ => element(&mut output, "body", &input["body"], 1),
    }
    if data["full_input_retained"] == true {
        element(&mut output, "continuation", &data["continuation"], 1);
    }
    output.push_str("</dauerbot_event>");
    output
}

fn time(value: &Value) -> Option<String> {
    let millis = crate::feishu::timestamp_millis(value)?;
    Some(
        chrono::DateTime::from_timestamp_millis(millis)?
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

fn attribute(output: &mut String, name: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        let _ = write!(output, " {name}=\"{}\"", crate::xml(value));
    }
}

fn media_placeholder(body: &str, kind: Option<&str>) -> bool {
    let Some(kind @ ("image" | "file" | "video" | "media" | "audio" | "sticker")) = kind else {
        return false;
    };
    let body = body.trim();
    body == format!("[{kind} message]")
        || body.starts_with(&format!("[{kind} message] "))
        || body == format!("[{kind}]")
        || body.starts_with(&format!("[{kind}] "))
}

fn context_message(
    output: &mut String,
    tag: &str,
    message: &Value,
    resources: &[&Value],
    depth: usize,
) {
    let indent = "  ".repeat(depth);
    let _ = write!(output, "{indent}<{tag}");
    for field in ["message_id", "sender"] {
        attribute(output, field, message[field].as_str());
    }
    if tag == "message" {
        attribute(output, "time", time(&message["create_time"]).as_deref());
    }
    if message["preview_truncated"] == true {
        attribute(output, "truncated", Some("true"));
    }
    output.push_str(">\n");
    if let Some(body) = message["body"].as_str().or(message["content"].as_str())
        && (resources.is_empty() || !media_placeholder(body, message["msg_type"].as_str()))
    {
        element(output, "body", &Value::from(body), depth + 1);
    }
    attachments(output, resources, depth + 1);
    let _ = writeln!(output, "{indent}</{tag}>");
}

fn attachments(output: &mut String, resources: &[&Value], depth: usize) {
    if resources.is_empty() {
        return;
    }
    let indent = "  ".repeat(depth);
    let _ = writeln!(output, "{indent}<attachments>");
    fn kind(resource: &Value) -> &str {
        let media = resource["media_type"].as_str().unwrap_or_default();
        match resource["kind"].as_str() {
            Some("media" | "video") => "video",
            _ if media.starts_with("video/") => "video",
            Some(kind) if valid_name(kind) => kind,
            _ => "file",
        }
    }
    for resource in resources {
        let tag = kind(resource);
        if !resource["frame_seconds"].is_null()
            && resources.iter().any(|video| {
                video["frame_seconds"].is_null()
                    && kind(video) == "video"
                    && video["file_key"] == resource["file_key"]
                    && video["message_id"] == resource["message_id"]
            })
        {
            continue;
        }
        let _ = write!(output, "{indent}  <{tag}");
        for (attr, field) in [
            ("name", "file_name"),
            ("path", "local_path"),
            ("error", "error"),
        ] {
            attribute(output, attr, resource[field].as_str());
        }
        if let Some(bytes) = resource["bytes"].as_u64() {
            let size = if bytes >= 1_000_000 {
                format!("{:.1} MB", bytes as f64 / 1_000_000.0)
            } else if bytes >= 1_000 {
                format!("{:.0} KB", bytes as f64 / 1_000.0)
            } else {
                format!("{bytes} B")
            };
            attribute(output, "size", Some(&size));
        }
        if matches!(tag, "video" | "audio") {
            let duration = resource["duration_seconds"]
                .as_f64()
                .or_else(|| resource["declared_duration_seconds"].as_f64())
                .map(|seconds| format!("{seconds}s"));
            attribute(output, "duration", duration.as_deref());
        } else if tag != "image" {
            attribute(output, "media_type", resource["media_type"].as_str());
        }
        if resource["image_delivery"].is_object() && resource["error"].is_null() {
            inline(output, resource, "inline", "reason");
        }
        if tag == "video" {
            if let Some(frame) = resources.iter().find(|frame| {
                !frame["frame_seconds"].is_null()
                    && frame["file_key"] == resource["file_key"]
                    && frame["message_id"] == resource["message_id"]
            }) {
                attribute(output, "preview_path", frame["local_path"].as_str());
                let at = frame["frame_seconds"]
                    .as_f64()
                    .map(|seconds| format!("{seconds:.1}s"));
                attribute(output, "preview_at", at.as_deref());
                inline(output, frame, "preview_inline", "preview_reason");
            }
            attribute(
                output,
                "preview_error",
                resource["frame_extraction_error"].as_str(),
            );
        }
        output.push_str("/>\n");
    }
    let _ = writeln!(output, "{indent}</attachments>");
}

fn inline(output: &mut String, resource: &Value, name: &str, reason: &str) {
    let delivery = &resource["image_delivery"];
    let inline = delivery["mode"] == "image_reference";
    attribute(output, name, Some(if inline { "true" } else { "false" }));
    if !inline {
        attribute(output, reason, delivery["reason"].as_str());
    }
}

fn valid_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.is_empty(),
        Value::Object(values) => values.values().any(present),
        Value::Array(values) => values.iter().any(present),
        _ => true,
    }
}

fn element(output: &mut String, name: &str, value: &Value, depth: usize) {
    if !present(value) {
        return;
    }
    let indent = "  ".repeat(depth);
    if !valid_name(name) {
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
            for item in items {
                element(output, "item", item, depth + 1);
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

// Dauerbot emits a restricted XML grammar without namespaces, comments or raw markup.
pub(crate) fn redact_envelope(
    text: &str,
    mut redact: impl FnMut(&str, &str) -> String,
) -> Option<String> {
    let root = ["dauerbot_event", "project_memory"]
        .into_iter()
        .find(|root| {
            text.strip_prefix('<')
                .and_then(|text| text.strip_prefix(root))
                .is_some_and(|rest| {
                    rest.is_empty() || rest.starts_with([' ', '\t', '\n', '\r', '>', '/'])
                })
        })?;
    Some(
        redact_xml(text, root, &mut redact)
            .unwrap_or_else(|| format!("<{root}>[redacted]</{root}>")),
    )
}

fn redact_xml(
    text: &str,
    root: &str,
    redact: &mut impl FnMut(&str, &str) -> String,
) -> Option<String> {
    let mut result = String::new();
    let mut stack = Vec::new();
    let mut parts = text.split('<').skip(1).peekable();
    while let Some(part) = parts.next() {
        let mut quoted = false;
        let end = part.char_indices().find_map(|(index, ch)| match ch {
            '"' => {
                quoted = !quoted;
                None
            }
            '>' if !quoted => Some(index),
            _ => None,
        })?;
        let (tag, value) = (&part[..end], &part[end + 1..]);
        if value.contains("]]>") {
            return None;
        }
        if let Some(name) = tag.strip_prefix('/') {
            let name = name.trim_end_matches(xml_space);
            if stack.pop()? != name {
                return None;
            }
            let _ = write!(result, "</{name}>");
        } else {
            let (tag, closed) = tag
                .strip_suffix('/')
                .map_or((tag, false), |tag| (tag, true));
            let (name, mut attrs) = tag
                .split_once(xml_space)
                .map_or((tag, ""), |(name, attrs)| {
                    (name, attrs.trim_start_matches(xml_space))
                });
            if !valid_name(name)
                || crate::manager_context::sensitive(name)
                || stack.is_empty() && (name != root || !result.is_empty())
            {
                return None;
            }
            let _ = write!(result, "<{name}");
            let mut seen = Vec::new();
            while !attrs.is_empty() {
                let (key, rest) = attrs.split_once('=')?;
                let key = key.trim_end_matches(xml_space);
                let (escaped, rest) = rest
                    .trim_start_matches(xml_space)
                    .strip_prefix('"')?
                    .split_once('"')?;
                if !valid_name(key) || seen.contains(&key) || crate::manager_context::sensitive(key)
                {
                    return None;
                }
                seen.push(key);
                let cleaned = redact_value(key, escaped, redact)?;
                let _ = write!(result, " {key}=\"{cleaned}\"");
                attrs = if rest.is_empty() {
                    ""
                } else {
                    rest.strip_prefix(xml_space)?.trim_start_matches(xml_space)
                };
            }
            if closed {
                result.push_str("/>");
            } else {
                stack.push(name);
                result.push('>');
            }
        }
        if let Some(name) = stack.last() {
            result.push_str(&redact_value(name, value, redact)?);
        } else if !value.chars().all(xml_space) || parts.peek().is_some() {
            return None;
        } else {
            result.push_str(value);
        }
    }
    (stack.is_empty()).then_some(result)
}

fn xml_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

fn redact_value(
    name: &str,
    escaped: &str,
    redact: &mut impl FnMut(&str, &str) -> String,
) -> Option<String> {
    if escaped.chars().any(|ch| !matches!(ch, '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')) {
        return None;
    }
    let mut parts = escaped.split('&');
    let mut decoded = parts.next()?.to_owned();
    for part in parts {
        let (entity, rest) = part.split_once(';')?;
        decoded.push(match entity {
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            "amp" => '&',
            _ => return None,
        });
        decoded.push_str(rest);
    }
    if name == "name" && crate::manager_context::sensitive(&decoded) {
        return None;
    }
    let cleaned = redact(name, &decoded);
    if cleaned == decoded
        || matches!(
            (serde_json::from_str::<Value>(&decoded), serde_json::from_str::<Value>(&cleaned)),
            (Ok(before), Ok(after)) if before == after
        )
    {
        Some(escaped.to_owned())
    } else {
        Some(crate::xml(&cleaned))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn feishu_projects_routing_mentions_and_resources() {
        let source = json!({"source_kind":"feishu","message_id":"om_current","bot_app_id":"cli_private",
            "source":{"chat_id":"oc_group","chat_type":"group","message_type":"post",
                "sender":{"open_id":"ou_sender","union_id":"on_private","tenant_key":"private"},
                "thread_id":"omt_thread","root_id":"om_root","parent_id":"om_parent",
                "create_time":"0","mention_all":true,"group_admission":"own_mention",
                "mentions":[{"key":"@_user_1","name":"One","id":{"open_id":"ou_one"}},
                    {"key":"@_user_10","name":"Ten","id":{"open_id":"ou_ten"}}]},
            "group_context":{"name":"Group & chat","sender_name":"Sender","members":[{"name":"Other"}]}});
        let data = json!({"user_input":{"body":"@_user_10 @_user_1 @_all","resources":[
            {"kind":"media","file_key":"secret-key","message_id":"om_current","file_name":"clip.mp4",
                "local_path":"/cache/clip.mp4","bytes":184000,"duration_seconds":30,"cache_root":"secret-root"},
            {"kind":"image","file_key":"secret-key","message_id":"om_current","frame_seconds":6,
                "local_path":"/cache/frame.png","image_delivery":{"mode":"image_reference"}},
            {"kind":"image","quoted":true,"file_name":"parent.png","error":"Unavailable"}]},
            "quoted_message":{"status":"ok","message_id":"om_parent","sender":"ou_parent","body":"Quote < &","preview_truncated":true}});
        let xml = event_envelope("event", None, "user", data, source);
        assert!(xml.contains(r#"thread_id="omt_thread" root_id="om_root" parent_id="om_parent" message_id="om_current" sender="Sender(ou_sender)" time="1970-01-01T00:00:00Z" chat_name="Group &amp; chat" mention_all="true""#));
        assert!(xml.contains("<body>@Ten(ou_ten) @One(ou_one) @所有人</body>"));
        assert!(xml.contains(r#"<video name="clip.mp4" path="/cache/clip.mp4" size="184 KB" duration="30s" preview_path="/cache/frame.png" preview_at="6.0s" preview_inline="true"/>"#));
        let (current, quote) = xml.split_once("<quoted_message").unwrap();
        assert!(!current.contains("<image"));
        assert!(quote.starts_with(r#" message_id="om_parent" sender="ou_parent" truncated="true">"#), "{quote}");
        assert!(quote.contains(r#"<image name="parent.png" error="Unavailable"/>"#));
        assert!(quote.contains("<body>Quote &lt; &amp;</body>"));
        for noise in [
            "secret-",
            "cli_private",
            "on_private",
            "Other",
            "group_admission",
            "<mentions",
        ] {
            assert!(!xml.contains(noise), "{xml}");
        }
        assert_eq!(redact_envelope(&xml, |_, value| value.into()), Some(xml));
    }

    #[test]
    fn timer_projects_schedule_and_inline_instructions() {
        let xml = event_envelope(
            "event",
            None,
            "schedule",
            json!({"user_input":{"body":"Do <work>"},"to":""}),
            json!({"scheduled_at":0,"purpose":"reminder","to":""}),
        );
        assert_eq!(
            xml,
            "<dauerbot_event event_id=\"event\" kind=\"timer\" source=\"timer\" purpose=\"reminder\" scheduled_at=\"1970-01-01T00:00:00Z\">\n  <instructions>Do &lt;work&gt;</instructions>\n</dauerbot_event>"
        );
    }

    #[test]
    fn redaction_checks_attributes_and_rejects_malformed_markup() {
        let xml = r#"<dauerbot_event event_id="event"><image name="a &amp; b"/><body>secret</body></dauerbot_event>"#;
        assert_eq!(
            redact_envelope(xml, |name, value| match name {
                "name" => {
                    assert_eq!(value, "a & b");
                    "<name>".into()
                }
                "body" => "[redacted]".into(),
                _ => value.into(),
            })
            .unwrap(),
            r#"<dauerbot_event event_id="event"><image name="&lt;name&gt;"/><body>[redacted]</body></dauerbot_event>"#
        );
        for malformed in [
            r#"<dauerbot_event api_key="secret"/>"#,
            r#"<dauerbot_event><api_key>secret</api_key></dauerbot_event>"#,
            r#"<dauerbot_event><image name="a" name="b"/></dauerbot_event>"#,
            r#"<dauerbot_event><image broken="secret/></dauerbot_event>"#,
            r#"<dauerbot_event><body>secret</dauerbot_event>"#,
            r#"<dauerbot_event><body>&unknown;</body></dauerbot_event>"#,
        ] {
            assert_eq!(
                redact_envelope(malformed, |_, v| v.into()).as_deref(),
                Some("<dauerbot_event>[redacted]</dauerbot_event>")
            );
        }
    }
}
