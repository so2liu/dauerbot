//! Opt-in manager configuration; provider credentials never enter public state or snapshots.
use crate::{control_label, locale::Language};
use gpui::{prelude::*, *};
use gpui_component::{
    Disableable, Selectable, Sizable,
    button::*,
    input::{Input, InputEvent, InputState},
};
use herdr_feishu_bridge::config::Config;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::mpsc, time::Duration};

#[derive(Clone, PartialEq, Eq)]
struct Ticket {
    project: String,
    serial: u64,
}
struct Work {
    ticket: Ticket,
    method: &'static str,
    suffix: &'static str,
    body: Option<Value>,
}
struct Reply {
    ticket: Ticket,
    changing: bool,
    result: Result<Value, String>,
}

pub fn enabled(project: &Value) -> bool {
    project["agent_mode"] == "manager"
}
pub fn status(project: &Value) -> &str {
    project["manager"]["status"]
        .as_str()
        .unwrap_or("unavailable")
}

fn image_identity_changed(
    event: &InputEvent,
    previous: &(String, String),
    current: &(String, String),
) -> bool {
    matches!(event, InputEvent::Change) && previous != current
}

pub struct ManagerSettings {
    project: Value,
    config: Value,
    language: Language,
    ticket: Option<Ticket>,
    serial: u64,
    clear_inputs: bool,
    fill_inputs: bool,
    open: bool,
    dirty: bool,
    supports_images: bool,
    image_identity: (String, String),
    message: String,
    tx: mpsc::Sender<Work>,
    endpoint: Entity<InputState>,
    model: Entity<InputState>,
    key: Entity<InputState>,
}
impl ManagerSettings {
    pub fn new(data: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let endpoint =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://api.openai.com/v1"));
        let model = cx.new(|cx| InputState::new(window, cx).placeholder("Model ID"));
        let key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("API key")
                .masked(true)
        });
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
                let changing = work.method != "GET";
                let redact = work
                    .body
                    .as_ref()
                    .and_then(|v| v["api_key"].as_str())
                    .unwrap_or("")
                    .to_owned();
                let result = runtime
                    .block_on(async {
                        let cfg: Config =
                            serde_json::from_slice(&std::fs::read(data.join("config.json"))?)?;
                        let mut request = http
                            .request(
                                reqwest::Method::from_bytes(work.method.as_bytes())?,
                                format!(
                                    "{}/api/projects/{}/{}",
                                    cfg.url(),
                                    work.ticket.project,
                                    work.suffix
                                ),
                            )
                            .bearer_auth(&cfg.token);
                        if let Some(body) = work.body {
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
                                .unwrap_or("Manager settings are unavailable")
                        );
                        // The daemon contract is public-only. Discard unexpected credential fields.
                        let mut public = value;
                        if let Some(o) = public.as_object_mut() {
                            o.remove("api_key");
                            o.remove("token");
                        }
                        Ok::<_, anyhow::Error>(public)
                    })
                    .map_err(|e| {
                        let error = e.to_string();
                        if redact.is_empty() {
                            error
                        } else {
                            error.replace(&redact, "[redacted]")
                        }
                    });
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
                        if this.ticket.as_ref() != Some(&reply.ticket)
                            || this.project["id"] != reply.ticket.project
                        {
                            return;
                        }
                        this.ticket = None;
                        match reply.result {
                            Ok(config) => {
                                this.config = config;
                                this.fill_inputs = true;
                                this.clear_inputs = reply.changing;
                                this.dirty = false;
                                this.message = if reply.changing {
                                    "Manager settings saved".into()
                                } else {
                                    String::new()
                                };
                            }
                            Err(error) => this.message = error,
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
        for (index, input) in [&endpoint, &model, &key].into_iter().enumerate() {
            cx.subscribe(input, move |this, _, event: &InputEvent, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                if index < 2 {
                    let identity = (
                        this.endpoint.read(cx).value().to_string(),
                        this.model.read(cx).value().to_string(),
                    );
                    if image_identity_changed(event, &this.image_identity, &identity) {
                        this.supports_images = false;
                    }
                    this.image_identity = identity;
                }
                this.dirty = true;
                cx.notify();
            })
            .detach();
        }
        Self {
            project: Value::Null,
            config: Value::Null,
            language: Language::default(),
            ticket: None,
            serial: 0,
            clear_inputs: false,
            fill_inputs: false,
            open: false,
            dirty: false,
            supports_images: false,
            image_identity: (String::new(), String::new()),
            message: String::new(),
            tx,
            endpoint,
            model,
            key,
        }
    }
    pub fn select(&mut self, project: &Value, language: Language, cx: &mut Context<Self>) {
        let changed = self.project["id"] != project["id"];
        self.project = project.clone();
        self.language = language;
        if changed {
            self.config = Value::Null;
            self.clear_inputs = true;
            self.fill_inputs = false;
            self.open = false;
            self.dirty = false;
            self.supports_images = false;
            self.message.clear();
            self.request("GET", "manager", None, cx);
        } else if self.ticket.is_none()
            && !self.dirty
            && !self.config.is_null()
            && project["manager"]["revision"].as_u64().unwrap_or(0)
                > self.config["revision"].as_u64().unwrap_or(0)
        {
            self.request("GET", "manager", None, cx);
        }
    }
    fn request(
        &mut self,
        method: &'static str,
        suffix: &'static str,
        body: Option<Value>,
        cx: &mut Context<Self>,
    ) {
        let project = self.project["id"].as_str().unwrap_or("").to_owned();
        if project.is_empty() {
            return;
        }
        self.serial += 1;
        let ticket = Ticket {
            project,
            serial: self.serial,
        };
        self.ticket = Some(ticket.clone());
        if self
            .tx
            .send(Work {
                ticket,
                method,
                suffix,
                body,
            })
            .is_err()
        {
            self.ticket = None;
            self.message = "Manager settings are unavailable".into();
        }
        cx.notify();
    }
    fn field(&self, title: &str, input: &Entity<InputState>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(0x85858d))
                    .child(self.language.text(title)),
            )
            .child(
                Input::new(input)
                    .small()
                    .text_size(px(13.))
                    .disabled(self.ticket.is_some()),
            )
    }
}
impl Render for ManagerSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.clear_inputs) {
            for input in [&self.endpoint, &self.model, &self.key] {
                input.update(cx, |input, cx| input.set_value("", window, cx));
            }
        }
        if std::mem::take(&mut self.fill_inputs) {
            for (input, field) in [(&self.endpoint, "base_url"), (&self.model, "model")] {
                input.update(cx, |input, cx| {
                    input.set_value(
                        self.config[field].as_str().unwrap_or("").to_owned(),
                        window,
                        cx,
                    )
                });
            }
            self.image_identity = (
                self.config["base_url"].as_str().unwrap_or("").into(),
                self.config["model"].as_str().unwrap_or("").into(),
            );
            self.supports_images = self.config["supports_images"] == true;
            self.dirty = false;
        }
        self.dirty = !self.config.is_null()
            && (self.endpoint.read(cx).value().as_ref()
                != self.config["base_url"].as_str().unwrap_or("")
                || self.model.read(cx).value().as_ref()
                    != self.config["model"].as_str().unwrap_or("")
                || !self.key.read(cx).value().is_empty()
                || self.supports_images != (self.config["supports_images"] == true));
        let busy = self.ticket.is_some();
        let mode = self.config["mode"]
            .as_str()
            .unwrap_or(if enabled(&self.project) {
                "manager"
            } else {
                "native"
            });
        let blocked = self.project["paused"] == true || self.project["lifecycle"] == "taken_over";
        let revision = self.config["revision"].as_u64();
        let mut modes = div().flex().flex_wrap().gap_2();
        for (key, label) in [("native", "Native CLI"), ("manager", "Built-in manager")] {
            modes=modes.child(Button::new(SharedString::from(format!("agent-mode-{key}"))).small()
                .child(control_label(self.language.text(label))).selected(mode==key)
                .disabled(busy||blocked||revision.is_none()||mode==key||self.dirty)
                .on_click(cx.listener(move |this,_,_,cx| {
                    this.request("POST","agent/mode",Some(json!({"mode":key,"request_id":uuid::Uuid::new_v4().to_string(),"expected_revision":revision})),cx);
                })));
        }
        let mut view=div().flex().flex_col().gap_3().child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(self.language.text("Project Agent")))
            .child(modes).child(div().text_size(px(11.)).text_color(rgb(0x85858d)).child(self.language.text("The manager uses its own model and delegates coding to your native CLI. CLI login and model settings stay separate.")))
            .child(Button::new("manager-config-toggle").small().child(control_label(self.language.text(if self.open {"Hide manager settings"}else{"Manager model settings"})))
                .on_click(cx.listener(|this,_,_,cx|{this.open = !this.open;cx.notify();})));
        if self.open {
            view=view.child(self.field("OpenAI-compatible endpoint",&self.endpoint)).child(self.field("Manager model",&self.model)).child(self.field("Manager API key",&self.key))
                .child(Button::new("manager-image-capability").small().selected(self.supports_images).disabled(busy||!self.config["supports_images"].is_boolean())
                    .child(control_label(self.language.text(if self.supports_images {"Image input declared supported"}else{"Declare image input support"})))
                    .on_click(cx.listener(|this,_,_,cx| { this.supports_images = !this.supports_images; cx.notify(); })))
                .child(div().text_size(px(11.)).text_color(rgb(0x85858d)).child(self.language.text("Enable only when this exact endpoint and model support image input. Otherwise images remain explicit file references.")))
                .child(div().text_size(px(11.)).text_color(rgb(0x85858d)).child(self.language.text(if self.config["has_api_key"]==true {"API key saved. Leave blank to keep it."}else{"No manager API key saved"})))
                .child(div().flex().flex_wrap().gap_2()
                    .child(Button::new("save-manager-config").small().primary().child(control_label(self.language.text("Save manager settings")))
                        .disabled(busy||revision.is_none())
                        .on_click(cx.listener(|this,_,_,cx| {
                            let mut body=json!({"base_url":this.endpoint.read(cx).value().to_string(),"model":this.model.read(cx).value().to_string(),"expected_revision":this.config["revision"]});
                            if this.config["supports_images"].is_boolean() { body["supports_images"] = json!(this.supports_images); }
                            let key=this.key.read(cx).value().to_string();if !key.is_empty(){body["api_key"]=json!(key);}
                            this.request("PUT","manager",Some(body),cx);
                        })))
                    .child(Button::new("reload-manager-config").small().child(control_label(self.language.text("Reload"))).disabled(busy)
                        .on_click(cx.listener(|this,_,_,cx|{this.clear_inputs=true;this.request("GET","manager",None,cx);})))
                    .child(Button::new("clear-manager-key").small().child(control_label(self.language.text("Clear saved API key")))
                        .disabled(busy||revision.is_none()||self.config["has_api_key"]!=true||self.dirty)
                        .on_click(cx.listener(|this,_,_,cx|{
                            this.request("PUT","manager",Some(json!({"base_url":this.config["base_url"],"model":this.config["model"],"clear_api_key":true,"expected_revision":this.config["revision"]})),cx);
                        }))));
        }
        if !self.message.is_empty() {
            view = view.child(
                div()
                    .text_size(px(12.))
                    .child(self.language.text(&self.message)),
            );
        }
        if mode == "manager" {
            view = view.child(
                div()
                    .text_size(px(12.))
                    .child(self.language.text(status(&self.project))),
            );
            if let Some(error) = self.project["manager"]["error"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                view = view.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(0xb44343))
                        .child(self.language.text(error)),
                );
            }
        }
        if mode == "manager" {
            view=view.child(div().text_size(px(11.)).text_color(rgb(0x85858d)).child(self.language.text("The terminal belongs to the coding worker. Model and repair controls remain available if the manager cannot connect.")));
        }
        view
    }
}

#[cfg(test)]
mod tests {
    use super::{InputEvent, image_identity_changed};
    #[test]
    fn declaration_resets_only_for_real_provider_identity_changes() {
        let saved: (String, String) = ("https://fixture.invalid/v1".into(), "vision".into());
        let changed = (saved.0.clone(), "another-model".into());
        for event in [
            InputEvent::PressEnter { secondary: false },
            InputEvent::PressEnter { secondary: true },
            InputEvent::Focus,
            InputEvent::Blur,
        ] {
            assert!(!image_identity_changed(&event, &saved, &saved));
            assert!(!image_identity_changed(&event, &saved, &changed));
        }
        assert!(!image_identity_changed(&InputEvent::Change, &saved, &saved));
        assert!(image_identity_changed(
            &InputEvent::Change,
            &saved,
            &changed
        ));
        assert!(image_identity_changed(
            &InputEvent::Change,
            &saved,
            &("https://another.invalid/v1".into(), saved.1.clone())
        ));
        // A freshly populated form or a declaration for edited values is bound
        // to that identity, even when it differs from the previously saved form.
        assert!(!image_identity_changed(
            &InputEvent::Change,
            &changed,
            &changed
        ));
    }
}
