//! Compact project controls and read-only native continuity documents.
use crate::control_label;
use crate::locale::Language;
use gpui::{prelude::*, *};
use gpui_component::{Disableable, Selectable, Sizable, button::*, text::TextView};
use herdr_feishu_bridge::{
    config::Config,
    project_documents::{Content, Document},
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::mpsc, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Ticket {
    project: String,
    document: Option<Document>,
    revision: u64,
}
enum Work {
    Document(Ticket),
    Switch(Ticket, String),
    Repair(Ticket),
}
struct Reply {
    ticket: Ticket,
    result: Result<Value, String>,
    recovery: bool,
}
pub struct ProjectTools {
    project: Value,
    language: Language,
    tx: mpsc::Sender<Work>,
    document: Option<Document>,
    ticket: Option<Ticket>,
    revision: u64,
    content: Option<Content>,
    pending_cli: Option<String>,
    cli_open: bool,
    message: String,
}
impl ProjectTools {
    pub fn new(data: PathBuf, cx: &mut Context<Self>) -> Self {
        let (tx, requests) = mpsc::channel();
        let (replies, rx) = flume::unbounded::<Reply>();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let Ok(runtime) = runtime else { return };
            let http = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(12))
                .build();
            let Ok(http) = http else { return };
            while let Ok(work) = requests.recv() {
                let recovery = matches!(&work, Work::Repair(_));
                let (ticket, method, suffix, body) = match work {
                    Work::Document(ticket) => {
                        let suffix = format!("documents/{}", ticket.document.unwrap().key());
                        (ticket, "GET", suffix, None)
                    }
                    Work::Switch(ticket, agent) => (
                        ticket,
                        "POST",
                        "agent/transition".into(),
                        Some(json!({"agent":agent})),
                    ),
                    Work::Repair(ticket) => {
                        (ticket, "POST", "agent/repair".into(), Some(json!({})))
                    }
                };
                let result = runtime
                    .block_on(async {
                        // Read existing configuration; never provision or rewrite it.
                        let cfg: Config =
                            serde_json::from_slice(&std::fs::read(data.join("config.json"))?)?;
                        let mut request = http
                            .request(
                                method.parse()?,
                                format!("{}/api/projects/{}/{}", cfg.url(), ticket.project, suffix),
                            )
                            .bearer_auth(&cfg.token);
                        if let Some(body) = body {
                            request = request.json(&body);
                        }
                        let response = request.send().await?;
                        let status = response.status();
                        anyhow::ensure!(
                            status != reqwest::StatusCode::NOT_FOUND,
                            "Project controls are not available yet."
                        );
                        let mut value: Value = response.json().await?;
                        anyhow::ensure!(
                            status.is_success(),
                            "{}",
                            value["error"]
                                .as_str()
                                .unwrap_or("Cannot reach this project")
                        );
                        if ticket.document.is_some() && value["status"] == "available" {
                            value["text"] =
                                json!(display_markdown(value["text"].as_str().unwrap_or(""))?);
                        }
                        Ok::<_, anyhow::Error>(value)
                    })
                    .map_err(|error| error.to_string());
                if replies
                    .send(Reply {
                        ticket,
                        result,
                        recovery,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        cx.spawn(async move |entity, cx| {
            while let Ok(reply) = rx.recv_async().await {
                if entity
                    .update(cx, |this, cx| {
                        if !accept_reply(&this.project, this.ticket.as_ref(), &reply.ticket) {
                            return;
                        }
                        match reply.result {
                            Ok(value) if reply.ticket.document.is_some() => {
                                match serde_json::from_value(value) {
                                    Ok(content) => this.content = Some(content),
                                    Err(_) => {
                                        this.message = "Cannot read this document response".into()
                                    }
                                }
                            }
                            Ok(_) => {
                                this.message = if reply.recovery {
                                    "CLI recovery requested."
                                } else {
                                    "CLI handoff requested."
                                }
                                .into();
                            }
                            Err(error) => {
                                this.pending_cli = None;
                                this.message = error;
                            }
                        }
                        this.ticket = None;
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
            project: Value::Null,
            language: Language::default(),
            tx,
            document: None,
            ticket: None,
            revision: 0,
            content: None,
            pending_cli: None,
            cli_open: false,
            message: String::new(),
        }
    }
    pub fn select(&mut self, project: &Value, language: Language, cx: &mut Context<Self>) {
        let changed = self.project != *project || self.language != language;
        if self.project["id"] != project["id"] {
            self.document = None;
            self.content = None;
            self.ticket = None;
            self.pending_cli = None;
            self.cli_open = false;
            self.message.clear();
            self.revision += 1;
        }
        if matches!(
            project["cli_transition"]["phase"].as_str(),
            Some(
                "failed"
                    | "startup_failed"
                    | "uncertain"
                    | "cancelled"
                    | "recovery_pending"
                    | "recovery_stopping"
                    | "recovery_starting"
                    | "repairing"
            )
        ) {
            self.pending_cli = None;
        }
        if self
            .pending_cli
            .as_deref()
            .is_some_and(|agent| project["agent"] == agent)
        {
            self.pending_cli = None;
        }
        self.project = project.clone();
        if project["cli_transition"]["phase"].is_string()
            && matches!(
                self.message.as_str(),
                "CLI handoff requested." | "CLI recovery requested."
            )
        {
            self.message.clear();
        }
        self.language = language;
        if changed {
            cx.notify();
        }
    }
    fn request(
        &mut self,
        document: Option<Document>,
        agent: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.revision += 1;
        let ticket = Ticket {
            project: self.project["id"].as_str().unwrap_or("").into(),
            document,
            revision: self.revision,
        };
        self.ticket = Some(ticket.clone());
        self.message.clear();
        let sent = if let Some(agent) = agent {
            self.pending_cli = Some(agent.clone());
            self.tx.send(Work::Switch(ticket, agent))
        } else if document.is_some() {
            self.document = document;
            self.content = None;
            self.tx.send(Work::Document(ticket))
        } else {
            self.tx.send(Work::Repair(ticket))
        };
        if sent.is_err() {
            self.ticket = None;
            self.pending_cli = None;
            self.message = "The project viewer is unavailable. Reopen the app to reconnect.".into();
        }
        cx.notify();
    }
}
fn accept_reply(project: &Value, current: Option<&Ticket>, incoming: &Ticket) -> bool {
    project["id"].as_str() == Some(incoming.project.as_str()) && current == Some(incoming)
}
fn recovery_control(project: &Value, busy: bool) -> Option<(&'static str, bool)> {
    let phase = project["cli_transition"]["phase"].as_str().unwrap_or("");
    let owner = project["generation"]
        .as_str()
        .filter(|g| !g.is_empty())
        .is_some_and(|generation| project["cli_transition"]["recovery_generation"] == generation);
    let unavailable = matches!(
        project["lifecycle"].as_str(),
        Some("offline" | "error" | "needs_attention" | "update_required" | "exited")
    );
    let label = match phase {
        "failed" | "startup_failed" | "uncertain" | "repairing" => "Repair Agent",
        "recovery_pending" | "recovery_stopping" | "recovery_starting" => "Retry recovery",
        "requested" | "preparing" if owner => "Repair Agent",
        _ if unavailable => "Repair Agent",
        _ => return None,
    };
    Some((
        label,
        busy || project["paused"] == true || project["lifecycle"] == "taken_over",
    ))
}

impl Render for ProjectTools {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let transition = &self.project["cli_transition"];
        let switching = self.pending_cli.is_some()
            || matches!(
                transition["phase"].as_str(),
                Some(
                    "preparing"
                        | "requested"
                        | "switching"
                        | "starting"
                        | "uncertain"
                        | "recovery_pending"
                        | "recovery_stopping"
                        | "recovery_starting"
                        | "repairing"
                )
            );
        let unavailable =
            self.project["paused"] == true || self.project["lifecycle"] == "taken_over";
        let status = match transition["phase"].as_str() {
            Some("preparing" | "requested") => "Preparing handoff…",
            Some("switching" | "starting") => "Switching CLI…",
            Some("recovery_pending" | "recovery_stopping") => {
                "Recovering the previous Agent automatically…"
            }
            Some("recovery_starting") => "Starting the repair Agent with the saved conversation…",
            Some("repairing") => {
                "Repair Agent is available. Messages remain saved for the selected CLI."
            }
            Some("ready")
                if self.project["ready"] == true
                    && transition["target"] == self.project["agent"] =>
            {
                "Selected CLI is ready."
            }
            Some("failed" | "startup_failed" | "uncertain") => transition["error"]
                .as_str()
                .unwrap_or("CLI handoff did not finish."),
            _ => "",
        };
        let busy = self.ticket.is_some();
        let agent = self
            .pending_cli
            .as_deref()
            .or(self.project["agent"].as_str())
            .unwrap_or("");
        let mut choices = div().flex().flex_wrap().gap_2().items_center().child(
            div()
                .text_size(px(11.))
                .text_color(rgb(0x85858d))
                .child(
                    self.language
                        .text(if self.project["agent_mode"] == "manager" {
                            "Coding CLI"
                        } else {
                            "CLI"
                        }),
                ),
        );
        let current_name = match agent {
            "codex" => "Codex",
            "claude" => "Claude Code",
            "pi" => "Pi",
            "agy" => "Antigravity",
            _ => agent,
        };
        choices = choices.child(
            Button::new("project-cli-menu")
                .small()
                .text_size(px(13.))
                .child(control_label(format!("{current_name}  ⌄")))
                .selected(self.cli_open)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.cli_open = !this.cli_open;
                    cx.notify();
                })),
        );
        for (index, (key, label)) in [
            ("codex", "Codex"),
            ("claude", "Claude Code"),
            ("pi", "Pi"),
            ("agy", "Antigravity"),
        ]
        .into_iter()
        .enumerate()
        .take(if self.cli_open { 4 } else { 0 })
        {
            choices = choices.child(
                Button::new(("standby-cli", index))
                    .small()
                    .text_size(px(13.))
                    .child(control_label(label))
                    .selected(agent == key)
                    .disabled(unavailable || switching || busy || agent == key)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.cli_open = false;
                        this.request(None, Some(key.into()), cx);
                    })),
            );
        }
        if let Some((label, disabled)) = recovery_control(&self.project, busy) {
            choices = choices.child(
                Button::new("repair-cli")
                    .small()
                    .text_size(px(13.))
                    .child(control_label(self.language.text(label)))
                    .disabled(disabled)
                    .on_click(cx.listener(|this, _, _, cx| this.request(None, None, cx))),
            );
        }
        let mut documents = div().flex().flex_wrap().items_center().gap_2().child(
            div()
                .text_size(px(11.))
                .text_color(rgb(0x85858d))
                .child(self.language.text("Project files")),
        );
        for (index, document) in [Document::Handoff, Document::Agents]
            .into_iter()
            .enumerate()
        {
            documents = documents.child(
                Button::new(("project-document", index))
                    .small()
                    .text_size(px(13.))
                    .child(control_label(document.filename()))
                    .selected(self.document == Some(document))
                    .disabled(busy)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.request(Some(document), None, cx)),
                    ),
            );
        }
        let folder = self.project["path"].as_str().unwrap_or("").to_owned();
        let copy_folder = folder.clone();
        documents = documents
            .child(
                Button::new("reveal-project-folder")
                    .small()
                    .text_size(px(13.))
                    .child(control_label(self.language.text("Reveal folder")))
                    .disabled(folder.is_empty())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.reveal_path(std::path::Path::new(&folder))
                    })),
            )
            .child(
                Button::new("copy-project-folder")
                    .small()
                    .text_size(px(13.))
                    .child(control_label(self.language.text("Copy path")))
                    .disabled(copy_folder.is_empty())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_folder.clone()));
                        this.message = "Path copied".into();
                        cx.notify();
                    })),
            );
        if self.document.is_some() {
            documents = documents.child(
                Button::new("close-project-document")
                    .small()
                    .text_size(px(13.))
                    .child(control_label(self.language.text("Close")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.document = None;
                        this.content = None;
                        this.ticket = None;
                        this.revision += 1;
                        cx.notify();
                    })),
            );
        }
        let mut view = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(choices)
            .child(documents);
        if transition["holds_input"] == true {
            view = view.child(div().text_size(px(11.)).text_color(rgb(0x85858d)).child(
                self.language.text("Messages wait for the selected CLI. Stop pauses the project and cancels the switch."),
            ));
        }
        if !status.is_empty() {
            view = view.child(div().text_size(px(13.)).child(self.language.text(status)));
        }
        if unavailable {
            view = view.child(div().text_size(px(11.)).text_color(rgb(0x85858d)).child(
                self.language.text(
                    "Resume this project and release terminal input before changing its CLI.",
                ),
            ));
        }
        if !self.message.is_empty() {
            view = view.child(
                div()
                    .text_size(px(13.))
                    .child(self.language.text(&self.message)),
            );
        }
        if let Some(document) = self.document {
            let mut pane = div()
                .id("project-document-pane")
                .border_1()
                .border_color(rgb(0xe3e3e8))
                .rounded_md()
                .p_4();
            if let Some(content) = &self.content {
                if content.status == "available" {
                    let id = SharedString::from(format!(
                        "project-markdown-{}-{}",
                        self.project["id"].as_str().unwrap_or(""),
                        document.key()
                    ));
                    pane = pane.h(px(360.)).child(
                        TextView::markdown(id, content.text.clone(), window, cx)
                            .selectable(true)
                            .scrollable(true)
                            .size_full(),
                    );
                } else {
                    pane = pane.child(content.message.clone());
                }
            } else if self.ticket.is_some() {
                pane = pane.child(self.language.text("Loading document…"));
            } else {
                pane = pane.child(
                    self.language
                        .text("Document unavailable. Select it again to retry."),
                );
            }
            view = view.child(pane);
        }
        view
    }
}

/// The pinned native renderer has no image-disable switch. Remove parsed image
/// and HTML nodes before handing it Markdown, so rendering never fetches remote
/// images or opens local image files. Ordinary links remain click-only.
fn display_markdown(source: &str) -> anyhow::Result<String> {
    use markdown::{ParseOptions, mdast::Node};
    fn ranges(node: &Node, out: &mut Vec<std::ops::Range<usize>>) {
        if matches!(
            node,
            Node::Image(_) | Node::ImageReference(_) | Node::Html(_)
        ) {
            if let Some(position) = node.position() {
                out.push(position.start.offset..position.end.offset);
            }
        } else if let Some(children) = node.children() {
            for child in children {
                ranges(child, out);
            }
        }
    }
    let tree = markdown::to_mdast(source, &ParseOptions::gfm())
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let mut excluded = Vec::new();
    ranges(&tree, &mut excluded);
    let mut result = source.to_owned();
    for range in excluded.into_iter().rev() {
        anyhow::ensure!(
            result.is_char_boundary(range.start) && result.is_char_boundary(range.end),
            "Cannot render this document safely"
        );
        result.replace_range(range, " (embedded content omitted) ");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{Ticket, accept_reply, display_markdown, recovery_control};
    use herdr_feishu_bridge::project_documents::Document;
    use serde_json::json;
    #[test]
    fn failed_or_uncertain_switch_keeps_repair_reachable_until_user_pauses_or_takes_over() {
        for phase in [
            "failed",
            "startup_failed",
            "uncertain",
            "recovery_pending",
            "recovery_stopping",
            "recovery_starting",
            "repairing",
        ] {
            for terminal in [json!(null), json!("live-target-pane")] {
                let mut project = json!({"id":"one", "agent":"agy", "terminal":terminal, "cli_transition":{"phase":phase,"holds_input":true}});
                assert_eq!(
                    recovery_control(&project, false).map(|v| v.1),
                    Some(false),
                    "{phase}"
                );
                assert_eq!(recovery_control(&project, true).map(|v| v.1), Some(true));
                project["paused"] = json!(true);
                assert_eq!(recovery_control(&project, false).map(|v| v.1), Some(true));
                project["paused"] = json!(false);
                project["lifecycle"] = json!("taken_over");
                assert_eq!(recovery_control(&project, false).map(|v| v.1), Some(true));
            }
        }
        for phase in ["preparing", "requested"] {
            let mut project = json!({"generation":"repair-owner","cli_transition":{"phase":phase,"recovery_generation":"repair-owner","holds_input":true}});
            assert_eq!(
                recovery_control(&project, false),
                Some(("Repair Agent", false))
            );
            project["paused"] = json!(true);
            assert_eq!(recovery_control(&project, false).map(|v| v.1), Some(true));
        }
        let mut offline = json!({"lifecycle":"offline","generation":"saved-owner"});
        assert_eq!(
            recovery_control(&offline, false),
            Some(("Repair Agent", false))
        );
        assert_eq!(recovery_control(&offline, true).map(|v| v.1), Some(true));
        offline["paused"] = json!(true);
        assert_eq!(recovery_control(&offline, false).map(|v| v.1), Some(true));
        for phase in ["ready", "cancelled", "preparing", "requested", "starting"] {
            assert!(recovery_control(&json!({"cli_transition":{"phase":phase}}), false).is_none());
        }
    }
    #[test]
    fn stale_project_document_and_reload_replies_are_ignored() {
        let ticket = Ticket {
            project: "one".into(),
            document: Some(Document::Agents),
            revision: 1,
        };
        assert!(accept_reply(&json!({"id":"one"}), Some(&ticket), &ticket));
        assert!(!accept_reply(&json!({"id":"two"}), Some(&ticket), &ticket));
        let newer = Ticket {
            revision: 2,
            ..ticket.clone()
        };
        assert!(!accept_reply(&json!({"id":"one"}), Some(&newer), &ticket));
        let handoff = Ticket {
            document: Some(Document::Handoff),
            ..ticket.clone()
        };
        assert!(!accept_reply(&json!({"id":"one"}), Some(&handoff), &ticket));
    }
    #[test]
    fn markdown_keeps_structure_without_automatic_external_content() {
        let source = "# 标题\n\n- item\n\n| A | B |\n| - | - |\n| x | y |\n\n```rust\nlet x = 1;\n```\n\n![secret](https://host/image.png) ![local][img]\n\n[img]: file:///private/image.png\n\n<img src=\"https://host/raw.png\">\n\n[Open](https://example.com)";
        let rendered = display_markdown(source).unwrap();
        let tree = markdown::to_mdast(&rendered, &markdown::ParseOptions::gfm()).unwrap();
        fn safe(node: &markdown::mdast::Node) {
            assert!(!matches!(
                node,
                markdown::mdast::Node::Image(_)
                    | markdown::mdast::Node::ImageReference(_)
                    | markdown::mdast::Node::Html(_)
            ));
            if let Some(children) = node.children() {
                for child in children {
                    safe(child);
                }
            }
        }
        safe(&tree);
        assert!(
            rendered.contains("# 标题")
                && rendered.contains("| A | B |")
                && rendered.contains("```rust")
                && rendered.contains("[Open](https://example.com)")
        );
    }
}
