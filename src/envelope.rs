//! XML is a data envelope, not an authority boundary. All user text is escaped.
use crate::db::Message;
use serde_json::Value;
use std::path::Path;

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(ch),
            ch if ('\u{20}'..='\u{d7ff}').contains(&ch)
                || ('\u{e000}'..='\u{fffd}').contains(&ch)
                || ch >= '\u{10000}' =>
            {
                out.push(ch)
            }
            _ => out.push('\u{fffd}'),
        }
    }
    out
}
fn attr(value: &Value, key: &str) -> String {
    escape(value[key].as_str().unwrap_or(""))
}
pub fn render(message: &Message, context: &Value, attachment: Option<&Path>) -> String {
    let source = &message.attachment["source"];
    let platform = if source.is_object() {
        "feishu"
    } else {
        "local"
    };
    let mut xml = format!(
        "<feishu_message version=\"1\" message_id=\"{}\">\n<source platform=\"{platform}\" chat_type=\"{}\" chat_id=\"{}\" root_id=\"{}\" parent_id=\"{}\" create_time=\"{}\"/>\n<sender open_id=\"{}\" user_id=\"{}\" union_id=\"{}\" name=\"{}\"/>\n<chat name=\"{}\"/>\n<members status=\"{}\" truncated=\"{}\" includes_bots=\"false\">",
        escape(&message.source_id),
        attr(source, "chat_type"),
        attr(source, "chat_id"),
        attr(source, "root_id"),
        attr(source, "parent_id"),
        attr(source, "create_time"),
        attr(&source["sender"], "open_id"),
        attr(&source["sender"], "user_id"),
        attr(&source["sender"], "union_id"),
        attr(context, "sender_name"),
        attr(context, "name"),
        attr(context, "members_status"),
        context["truncated"].as_bool().unwrap_or(false)
    );
    if let Some(members) = context["members"].as_array() {
        for member in members {
            xml.push_str(&format!(
                "\n<member id=\"{}\" id_type=\"open_id\" name=\"{}\"/>",
                attr(member, "member_id"),
                attr(member, "name")
            ));
        }
    }
    xml.push_str("\n</members>\n<mentions>");
    if let Some(mentions) = source["mentions"].as_array() {
        for mention in mentions {
            xml.push_str(&format!(
                "\n<mention open_id=\"{}\" name=\"{}\" key=\"{}\"/>",
                attr(&mention["id"], "open_id"),
                attr(mention, "name"),
                attr(mention, "key")
            ));
        }
    }
    xml.push_str(&format!(
        "\n</mentions>\n<body type=\"{}\" trust=\"user_input\">{}</body>",
        attr(source, "message_type"),
        escape(&message.content)
    ));
    if let Some(path) = attachment {
        xml.push_str(&format!(
            "\n<attachment local_path=\"{}\" name=\"{}\"/>",
            escape(&path.to_string_lossy()),
            attr(&message.attachment, "file_name")
        ));
    }
    xml.push_str("\n</feishu_message>");
    xml
}
