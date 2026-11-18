//! Native project model controls. HTTP work stays off the GPUI thread.
use crate::locale::Language;
use gpui::{prelude::*, *};
use gpui_component::{
    Disableable, Selectable, Sizable,
    button::*,
    input::{Input, InputState},
};
use herdr_feishu_bridge::config::Config;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::mpsc, time::Duration};

fn control_label(text: impl Into<SharedString>) -> Div {
    let text: SharedString = text.into();
    div()
        .min_w_0()
        .text_size(px(13.))
        .line_height(relative(1.))
        .child(text)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Ticket {
    project: String,
    agent: String,
    generation: String,
    revision: u64,
}
struct Work {
    ticket: Ticket,
    selection: Option<Value>,
}
struct Reply {
    ticket: Ticket,
    changing: bool,
    result: Result<Value, String>,
}

pub struct ModelPicker {
    project: Value,
    language: Language,
    input_language: Language,
    catalog: Value,
    catalog_error: bool,
    clear_inputs: bool,
    open: bool,
    ticket: Option<Ticket>,
    revision: u64,
    message: String,
    tx: mpsc::Sender<Work>,
    model: Entity<InputState>,
    provider: Entity<InputState>,
}

impl ModelPicker {
    pub fn new(data: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = cx.new(|cx| InputState::new(window, cx).placeholder("Model ID"));
        let provider = cx.new(|cx| InputState::new(window, cx).placeholder("Provider (optional)"));
        let (tx, requests) = mpsc::channel::<Work>();
        let (replies, rx) = flume::unbounded::<Reply>();
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
            while let Ok(work) = requests.recv() {
                let changing = work.selection.is_some();
                let result = runtime
                    .block_on(async {
                        let cfg: Config =
                            serde_json::from_slice(&std::fs::read(data.join("config.json"))?)?;
                        let suffix = if changing { "model" } else { "models" };
                        let mut request = http
                            .request(
                                if changing {
                                    reqwest::Method::POST
                                } else {
                                    reqwest::Method::GET
                                },
                                format!(
                                    "{}/api/projects/{}/{}",
                                    cfg.url(),
                                    work.ticket.project,
                                    suffix
                                ),
                            )
                            .bearer_auth(&cfg.token);
                        if let Some(mut body) = work.selection {
                            body["agent"] = json!(work.ticket.agent);
                            body["generation"] = json!(work.ticket.generation);
                            request = request.json(&body);
                        }
                        let response = request.send().await?;
                        let status = response.status();
                        let value: Value = response.json().await?;
                        anyhow::ensure!(
                            status.is_success(),
                            "{}",
                            value["error"]
                                .as_str()
                                .unwrap_or("Cannot load this project's models")
                        );
                        Ok::<_, anyhow::Error>(value)
                    })
                    .map_err(|e| e.to_string());
                if replies
                    .send(Reply {
                        ticket: work.ticket,
                        changing,
                        result,
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
                        if this.ticket.as_ref() != Some(&reply.ticket) {
                            return;
                        }
                        this.ticket = None;
                        match reply.result {
                            Ok(value) if !reply.changing => {
                                let scoped = value["agent"] == reply.ticket.agent
                                    && value["generation"] == reply.ticket.generation
                                    && value["options"].is_array()
                                    && value.get("configured").is_some();
                                this.catalog_error = !scoped;
                                if scoped {
                                    this.catalog = value;
                                    this.message.clear();
                                } else {
                                    this.catalog = Value::Null;
                                    this.message =
                                        "Invalid or stale model catalogue response".into();
                                }
                            }
                            Ok(_) => {
                                this.open = false;
                                this.message = "Model change requested".into();
                                this.request(None, cx);
                            }
                            Err(error) => {
                                if !reply.changing {
                                    this.catalog_error = true;
                                }
                                this.message = error;
                            }
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
            project: Value::Null,
            language: Language::default(),
            input_language: Language::default(),
            catalog: Value::Null,
            catalog_error: false,
            clear_inputs: false,
            open: false,
            ticket: None,
            revision: 0,
            message: String::new(),
            tx,
            model,
            provider,
        }
    }

    pub fn select(&mut self, project: &Value, language: Language, cx: &mut Context<Self>) {
        let changed = self.project["id"] != project["id"]
            || self.project["agent"] != project["agent"]
            || self.project["generation"] != project["generation"];
        self.project = project.clone();
        self.language = language;
        if changed {
            self.catalog = Value::Null;
            self.catalog_error = false;
            self.clear_inputs = true;
            self.open = false;
            self.message.clear();
            self.request(None, cx);
        }
    }

    fn request(&mut self, selection: Option<Value>, cx: &mut Context<Self>) {
        self.revision += 1;
        let ticket = Ticket {
            project: self.project["id"].as_str().unwrap_or("").into(),
            agent: self.project["agent"].as_str().unwrap_or("").into(),
            generation: self.project["generation"].as_str().unwrap_or("").into(),
            revision: self.revision,
        };
        if ticket.project.is_empty() {
            return;
        }
        if selection.is_none() {
            self.catalog_error = false;
        }
        self.ticket = Some(ticket.clone());
        if self.tx.send(Work { ticket, selection }).is_err() {
            self.ticket = None;
            self.message = "Model controls are unavailable".into();
        }
        cx.notify();
    }
}

impl Render for ModelPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.clear_inputs) {
            self.model
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.provider
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        if self.input_language != self.language {
            self.input_language = self.language;
            self.model.update(cx, |input, cx| {
                input.set_placeholder(self.language.text("Model ID"), window, cx)
            });
            self.provider.update(cx, |input, cx| {
                input.set_placeholder(self.language.text("Provider (optional)"), window, cx)
            });
        }
        let supported = matches!(
            self.project["agent"].as_str(),
            Some("codex" | "claude" | "pi" | "agy")
        );
        let configured = &self.catalog["configured"];
        let effective = &self.catalog["effective"];
        let transition = &self.project["cli_transition"];
        let pending = transition["holds_input"] == true && transition["intent"] == "model";
        let requested = &transition["requested_model"];
        let busy = self.ticket.is_some();
        let known = self.catalog.is_object();
        let label = if pending {
            requested["model"]
                .as_str()
                .unwrap_or("Native CLI/session settings")
        } else if self.catalog_error {
            "Model unavailable"
        } else if !known {
            if busy {
                "Loading models…"
            } else {
                "Model unknown"
            }
        } else {
            effective["model"]
                .as_str()
                .or(configured["model"].as_str())
                .unwrap_or("Native CLI/session settings")
        };
        let caption = if pending {
            "Requested · switching model"
        } else if self.catalog_error {
            "Native model catalogue could not be loaded"
        } else if busy {
            "Refreshing model catalogue…"
        } else if !known {
            "Native model selection is unknown"
        } else if effective["model"].is_string() {
            "Confirmed by CLI"
        } else if configured["model"].is_string() {
            "Requested · awaiting native confirmation"
        } else {
            "Uses native CLI/session settings"
        };
        let unavailable =
            self.project["paused"] == true || self.project["lifecycle"] == "taken_over";
        let mut view = div().flex().flex_col().gap_2().min_w_0().child(
            div()
                .flex()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(
                    Button::new("choose-project-model")
                        .small()
                        .text_size(px(13.))
                        .max_w_full()
                        .child(
                            control_label(format!(
                                "{}  ·  {}  ⌄",
                                self.language.text("Model"),
                                self.language.text(label)
                            ))
                            .w(px(200.))
                            .truncate(),
                        )
                        .disabled(!supported)
                        .selected(self.open)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open = !this.open;
                            if this.open && this.catalog.is_null() && this.ticket.is_none() {
                                this.request(None, cx);
                            }
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0x85858d))
                        .child(self.language.text(caption)),
                ),
        );
        if self.open {
            let mut panel = div()
                .rounded_lg()
                .border_1()
                .border_color(rgb(0xe3e3e8))
                .bg(rgb(0xffffff))
                .p_3()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(self.language.text("Choose model")),
                        )
                        .child(
                            Button::new("reload-model-catalog")
                                .small()
                                .text_size(px(13.))
                                .child(control_label(self.language.text("Refresh")))
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| this.request(None, cx))),
                        ),
                )
                .child(div().text_size(px(11.)).text_color(rgb(0x85858d)).child(
                    self.language.text(
                        "This project only. Native login and global CLI settings stay in place.",
                    ),
                ));
            let mut options = div()
                .id("native-model-options")
                .max_h(px(220.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_1();
            options = options.child(
                Button::new("model-native-default")
                    .small()
                    .text_size(px(13.))
                    .child(control_label(
                        self.language.text("Remove project model override"),
                    ))
                    .selected(!configured["model"].is_string())
                    .disabled(busy || unavailable)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.request(Some(json!({"model":"","provider":null})), cx)
                    })),
            );
            for (index, option) in self.catalog["options"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                let id = option["id"].as_str().unwrap_or("").to_owned();
                let provider = option["provider"].as_str().map(str::to_owned);
                let name = option["label"].as_str().unwrap_or(&id);
                let name = if let Some(provider) = &provider {
                    format!("{provider} / {name}")
                } else {
                    name.into()
                };
                let selected = configured["model"] == id
                    && configured["provider"].as_str() == provider.as_deref();
                options = options.child(
                    Button::new(("native-model", index))
                        .small()
                        .text_size(px(13.))
                        .max_w_full()
                        .overflow_hidden()
                        .child(control_label(name).max_w(px(180.)).truncate())
                        .selected(selected)
                        .disabled(busy || unavailable)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.request(Some(json!({"model":id,"provider":provider})), cx)
                        })),
                );
            }
            panel = panel.child(options);
            if busy {
                panel = panel.child(
                    div()
                        .text_size(px(11.))
                        .child(self.language.text("Loading models…")),
                );
            }
            panel = panel.child(Input::new(&self.model).small().text_size(px(13.)));
            if matches!(self.project["agent"].as_str(), Some("pi" | "codex")) {
                panel = panel.child(Input::new(&self.provider).small().text_size(px(13.)));
            }
            panel = panel.child(
                Button::new("apply-custom-model")
                    .small()
                    .text_size(px(13.))
                    .child(control_label(self.language.text("Use model ID")))
                    .disabled(busy || unavailable)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let model = this.model.read(cx).value().to_string();
                        let provider =
                            if matches!(this.project["agent"].as_str(), Some("pi" | "codex")) {
                                let value = this.provider.read(cx).value().to_string();
                                (!value.trim().is_empty()).then_some(value)
                            } else {
                                None
                            };
                        if model.trim().is_empty() {
                            this.message = "Enter a model ID".into();
                            cx.notify();
                            return;
                        }
                        this.request(Some(json!({"model":model,"provider":provider})), cx);
                    })),
            );
            for warning in self.catalog["warnings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                panel = panel.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0x85858d))
                        .child(self.language.text(warning)),
                );
            }
            view = view.child(panel);
        }
        if !self.message.is_empty() {
            view = view.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(0x63636b))
                    .child(self.language.text(&self.message)),
            );
        }
        view
    }
}
