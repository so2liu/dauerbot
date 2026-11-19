//! Read-only, explicitly staged manager request inspection. No requests are replayed.
use crate::locale::Language;
use gpui::{prelude::*, *};
use gpui_component::{Disableable, Sizable, button::*};
use herdr_feishu_bridge::config::Config;
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashSet, path::PathBuf, sync::mpsc, time::Duration};

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const PREVIEW_CHARS: usize = 1_200;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Ticket {
    project: String,
    revision: u64,
    serial: u64,
}
fn accepts_reply(pending: Option<&Ticket>, reply: &Ticket, project: &str, revision: u64) -> bool {
    pending == Some(reply) && reply.project == project && reply.revision == revision
}

#[derive(Default, Deserialize)]
struct Snapshot {
    project_id: String,
    #[serde(default)]
    requests: Vec<Request>,
    #[serde(default)]
    limitations: Vec<String>,
}
#[derive(Default, Deserialize)]
struct Request {
    id: String,
    #[serde(default)]
    run_id: String,
    #[serde(default)]
    turn: u64,
    #[serde(default)]
    prepared_at_ms: Option<u64>,
    #[serde(default)]
    finished_at_ms: Option<u64>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    blocks: Vec<Block>,
    #[serde(default)]
    tools: Value,
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    redactions: Value,
}
#[derive(Default, Deserialize)]
struct Block {
    index: usize,
    #[serde(default)]
    role: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    truncated: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct Section {
    heading: String,
    text: String,
    shortened: bool,
    truncated: bool,
}
fn section(block: &Block, expanded: bool) -> Section {
    let category = match block.kind.as_str() {
        "tool_call" => "Tool call",
        "tool_result" => "Tool result",
        "attachment" | "image" => "Attachment / image representation",
        _ => match block.role.as_str() {
            "system" => "System / instructions",
            "user" => "User / additional message",
            "assistant" => "Assistant",
            "tool" => "Tool result",
            _ => "Other context",
        },
    };
    let shortened = !expanded && block.text.chars().count() > PREVIEW_CHARS;
    Section {
        heading: format!("{} · {}", block.index, category),
        text: if shortened {
            block.text.chars().take(PREVIEW_CHARS).collect()
        } else {
            block.text.clone()
        },
        shortened,
        truncated: block.truncated,
    }
}
fn source_label(source: &str) -> &'static str {
    match source {
        "logical_request" => "Prepared logical request — not proof of provider acceptance",
        "checkpoint_normalized" => {
            "Normalized checkpoint — not a historical wire request; original timing unavailable"
        }
        _ => "Unknown source — exact provider input is unverified",
    }
}
fn timestamp(value: Option<u64>) -> String {
    value.map_or_else(
        || "Unavailable".into(),
        |v| format!("{v} ms since Unix epoch"),
    )
}
fn has_redactions(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Array(items) => !items.is_empty(),
        Value::Object(items) => !items.is_empty(),
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_u64() != Some(0),
        _ => true,
    }
}
fn tool_names(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            tool.get("name")
                .or_else(|| tool.get("function")?.get("name"))?
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

pub struct ManagerContext {
    project: String,
    revision: u64,
    language: Language,
    serial: u64,
    pending: Option<Ticket>,
    open: bool,
    snapshot: Option<Snapshot>,
    error: bool,
    expanded: HashSet<usize>,
    full_blocks: HashSet<(usize, usize)>,
    tx: mpsc::Sender<Ticket>,
}
impl ManagerContext {
    pub fn new(data: PathBuf, cx: &mut Context<Self>) -> Self {
        let (tx, requests) = mpsc::channel::<Ticket>();
        let (replies, rx) = flume::unbounded::<(Ticket, Result<Snapshot, ()>)>();
        std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let Ok(http) = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(25))
                .build()
            else {
                return;
            };
            while let Ok(ticket) = requests.recv() {
                let result = runtime
                    .block_on(async {
                        let cfg: Config =
                            serde_json::from_slice(&std::fs::read(data.join("config.json"))?)?;
                        let mut response = http
                            .get(format!(
                                "{}/api/projects/{}/manager/context",
                                cfg.url(),
                                ticket.project
                            ))
                            .bearer_auth(&cfg.token)
                            .send()
                            .await?;
                        anyhow::ensure!(response.status().is_success(), "Context unavailable");
                        let mut bytes = Vec::new();
                        while let Some(chunk) = response.chunk().await? {
                            anyhow::ensure!(
                                bytes.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
                                "Context exceeds limit"
                            );
                            bytes.extend_from_slice(&chunk);
                        }
                        let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
                        anyhow::ensure!(snapshot.project_id == ticket.project, "Project changed");
                        Ok::<_, anyhow::Error>(snapshot)
                    })
                    .map_err(|_| ());
                // Do not forward HTTP bodies, URLs, authentication, or parser errors to the UI.
                if replies.send((ticket, result)).is_err() {
                    break;
                }
            }
        });
        cx.spawn(async move |entity, cx| {
            while let Ok((ticket, result)) = rx.recv_async().await {
                if entity
                    .update(cx, |this, cx| {
                        if !accepts_reply(
                            this.pending.as_ref(),
                            &ticket,
                            &this.project,
                            this.revision,
                        ) {
                            return;
                        }
                        this.pending = None;
                        match result {
                            Ok(snapshot) => {
                                this.snapshot = Some(snapshot);
                                this.error = false;
                            }
                            Err(()) => this.error = true,
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            project: String::new(),
            revision: 0,
            language: Language::default(),
            serial: 0,
            pending: None,
            open: false,
            snapshot: None,
            error: false,
            expanded: HashSet::new(),
            full_blocks: HashSet::new(),
            tx,
        }
    }
    /// Selection changes invalidate outstanding requests and remove the previous project's content.
    pub fn select(&mut self, project: &Value, language: Language, cx: &mut Context<Self>) {
        let id = project["id"].as_str().unwrap_or("");
        let revision = project["manager"]["revision"].as_u64().unwrap_or(0);
        self.language = language;
        if self.project != id || self.revision != revision {
            self.project = id.to_owned();
            self.revision = revision;
            self.pending = None;
            self.snapshot = None;
            self.error = false;
            self.expanded.clear();
            self.full_blocks.clear();
            self.open = false;
            cx.notify();
        }
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.project.is_empty() {
            return;
        }
        self.serial += 1;
        let ticket = Ticket {
            project: self.project.clone(),
            revision: self.revision,
            serial: self.serial,
        };
        self.pending = Some(ticket.clone());
        self.error = self.tx.send(ticket).is_err();
        if self.error {
            self.pending = None;
        }
        self.expanded.clear();
        self.full_blocks.clear();
        cx.notify();
    }
}
impl Render for ManagerContext {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::ui_theme::palette(cx);
        let mut view = div().flex().flex_col().gap_2().min_w_0().child(
            Button::new("view-model-context")
                .label(self.language.text("View model context"))
                .small()
                .disabled(self.project.is_empty())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open = !this.open;
                    if this.open {
                        this.refresh(cx);
                    }
                    cx.notify();
                })),
        );
        if !self.open {
            return view;
        }
        view = view.child(div().text_size(px(12.)).text_color(rgb(p.muted)).child(self.language.text(
            "Read-only. Prepared requests are not proof of provider acceptance. Redacted snapshots are temporary and disappear after restart.")))
            .child(Button::new("refresh-model-context").label(self.language.text("Refresh")).small()
                .disabled(self.pending.is_some()).on_click(cx.listener(|this, _, _, cx| this.refresh(cx))));
        if self.pending.is_some() {
            view = view.child(self.language.text("Loading model context…"));
        }
        if self.error {
            view = view.child(
                div().text_color(rgb(p.error)).child(
                    self.language
                        .text("Model context is unavailable. No request was replayed."),
                ),
            );
        }
        let Some(snapshot) = &self.snapshot else {
            return view;
        };
        for limitation in &snapshot.limitations {
            view = view.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(p.muted))
                    .child(limitation.clone()),
            );
        }
        if snapshot.requests.is_empty() {
            view = view.child(
                self.language
                    .text("No captured manager requests are available."),
            );
        }
        let mut list = div()
            .id("manager-context-requests")
            .max_h(px(560.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2();
        for (index, request) in snapshot.requests.iter().enumerate() {
            let is_open = self.expanded.contains(&index);
            let mut card = div()
                .min_w_0()
                .p_2()
                .border_1()
                .border_color(rgb(p.line))
                .rounded_md()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    Button::new(("context-request", index))
                        .label(format!(
                            "{} Turn {} · {}",
                            if is_open { "▾" } else { "▸" },
                            request.turn,
                            request.status
                        ))
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.expanded.remove(&index) {
                                this.expanded.insert(index);
                            }
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(p.muted))
                        .child(self.language.text(source_label(&request.source))),
                );
            if is_open {
                card = card
                    .child(format!("Request: {} · Run: {}", request.id, request.run_id))
                    .child(format!(
                        "Prepared: {} · Finished: {}",
                        timestamp(request.prepared_at_ms),
                        timestamp(request.finished_at_ms)
                    ));
                if request.truncated {
                    card = card.child(self.language.text("Some context was truncated or omitted."));
                }
                if has_redactions(&request.redactions) {
                    card = card.child(self.language.text("Sensitive content was redacted."));
                }
                let names = tool_names(&request.tools);
                if !names.is_empty() {
                    card = card.child(format!("Available tools: {}", names.join(", ")));
                }
                for (block_index, block) in request.blocks.iter().enumerate() {
                    let key = (index, block_index);
                    let rendered = section(block, self.full_blocks.contains(&key));
                    let mut section = div()
                        .min_w_0()
                        .p_2()
                        .bg(rgb(p.canvas))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(p.accent))
                                .child(rendered.heading),
                        )
                        .child(
                            div()
                                .whitespace_normal()
                                .text_size(px(12.))
                                .child(rendered.text),
                        );
                    if rendered.truncated {
                        section =
                            section.child(self.language.text("Truncated in captured context"));
                    }
                    if rendered.shortened || self.full_blocks.contains(&key) {
                        section = section.child(
                            Button::new(("context-block", index * 10000 + block_index))
                                .small()
                                .label(self.language.text(if rendered.shortened {
                                    "Show full captured block"
                                } else {
                                    "Collapse block"
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.full_blocks.remove(&key) {
                                        this.full_blocks.insert(key);
                                    }
                                    cx.notify();
                                })),
                        );
                    }
                    card = card.child(section);
                }
            }
            list = list.child(card);
        }
        view.child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Block, PREVIEW_CHARS, Ticket, accepts_reply, has_redactions, section, source_label,
        timestamp, tool_names,
    };
    use serde_json::json;

    #[test]
    fn context_sections_classify_without_reordering_or_inventing_pixels() {
        let blocks: Vec<Block> = serde_json::from_value(json!([
            {"index":0,"role":"system","text":"instructions"},
            {"index":1,"role":"user","text":"追加消息"},
            {"index":2,"role":"assistant","kind":"tool_call","text":"read_file"},
            {"index":3,"role":"tool","kind":"tool_result","text":"result"},
            {"index":4,"kind":"attachment","text":"path_only; pixels unavailable"}
        ]))
        .unwrap();
        let sections: Vec<_> = blocks.iter().map(|b| section(b, false)).collect();
        assert_eq!(sections[0].heading, "0 · System / instructions");
        assert_eq!(sections[1].text, "追加消息");
        assert_eq!(sections[2].heading, "2 · Tool call");
        assert_eq!(sections[3].heading, "3 · Tool result");
        assert_eq!(sections[4].text, "path_only; pixels unavailable");
    }
    #[test]
    fn preview_preserves_unicode_and_distinguishes_capture_truncation() {
        let block = Block {
            text: "中".repeat(PREVIEW_CHARS + 1),
            truncated: true,
            ..Default::default()
        };
        let preview = section(&block, false);
        assert_eq!(preview.text.chars().count(), PREVIEW_CHARS);
        assert!(preview.shortened && preview.truncated);
        let full = section(&block, true);
        assert_eq!(full.text, block.text);
        assert!(!full.shortened);
        assert!(full.truncated);
    }
    #[test]
    fn stale_project_revision_and_superseded_refresh_are_rejected() {
        let ticket = Ticket {
            project: "a".into(),
            revision: 3,
            serial: 2,
        };
        assert!(accepts_reply(Some(&ticket), &ticket, "a", 3));
        assert!(!accepts_reply(Some(&ticket), &ticket, "b", 3));
        assert!(!accepts_reply(Some(&ticket), &ticket, "a", 4));
        assert!(!accepts_reply(None, &ticket, "a", 3));
        let newer = Ticket {
            serial: 3,
            ..ticket.clone()
        };
        assert!(!accepts_reply(Some(&newer), &ticket, "a", 3));
    }
    #[test]
    fn normalized_history_and_unknown_times_do_not_imply_wire_acceptance() {
        assert!(source_label("checkpoint_normalized").contains("not a historical wire request"));
        assert!(source_label("logical_request").contains("not proof"));
        assert_eq!(timestamp(None), "Unavailable");
        assert!(source_label("future").contains("unverified"));
        assert!(has_redactions(&json!(["removed"])));
        assert!(!has_redactions(&json!([])));
        assert_eq!(
            tool_names(
                &json!([{"name":"read_file","api_key":"not-rendered"},{"function":{"name":"send_md"}}])
            ),
            vec!["read_file", "send_md"]
        );
    }
}
