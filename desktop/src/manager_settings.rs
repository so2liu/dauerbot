//! Shared manager configuration; provider credentials never enter public state or snapshots.
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
    serial: u64,
}
struct Work {
    ticket: Ticket,
    method: &'static str,
    body: Option<Value>,
}
struct Reply {
    ticket: Ticket,
    changing: bool,
    result: Result<Value, String>,
}

pub fn status(project: &Value) -> &str {
    project["manager"]["status"]
        .as_str()
        .unwrap_or("unavailable")
}

const PROTOCOLS: [(&str, &str); 3] = [
    ("openai_completions", "OpenAI Chat Completions"),
    ("openai_responses", "OpenAI Responses"),
    ("anthropic", "Anthropic Messages"),
];

fn configured_protocol(config: &Value) -> &'static str {
    PROTOCOLS
        .iter()
        .find(|(id, _)| config["provider_kind"] == *id)
        .map_or(PROTOCOLS[0].0, |(id, _)| *id)
}

fn parse_context_window(input: &str) -> Result<Option<u32>, &'static str> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    input
        .parse::<u32>()
        .ok()
        .filter(|value| *value >= 1024)
        .map(Some)
        .ok_or("Context window must be a whole number of at least 1024.")
}

fn provider_payload(
    config: &Value,
    endpoint: &str,
    model: &str,
    protocol: &str,
    context_window: Option<u32>,
) -> Value {
    json!({"base_url":endpoint,"model":model,"provider_kind":protocol,
        "context_window":context_window,"expected_revision":config["revision"]})
}

fn image_identity_changed(
    event: &InputEvent,
    previous: &(String, String, &str),
    current: &(String, String, &str),
) -> bool {
    matches!(event, InputEvent::Change) && previous != current
}

pub struct ManagerSettings {
    config: Value,
    language: Language,
    ticket: Option<Ticket>,
    serial: u64,
    clear_inputs: bool,
    fill_inputs: bool,
    open: bool,
    dirty: bool,
    supports_images: bool,
    image_identity: (String, String, &'static str),
    protocol: &'static str,
    message: String,
    tx: mpsc::Sender<Work>,
    endpoint: Entity<InputState>,
    model: Entity<InputState>,
    context_window: Entity<InputState>,
    key: Entity<InputState>,
}
impl ManagerSettings {
    pub fn new(data: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let endpoint =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://api.openai.com/v1"));
        let model = cx.new(|cx| InputState::new(window, cx).placeholder("Model ID"));
        let context_window = cx.new(|cx| InputState::new(window, cx));
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
                                format!("{}/api/manager/provider", cfg.url()),
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
                        if this.ticket.as_ref() != Some(&reply.ticket) {
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
        for (index, input) in [&endpoint, &model, &context_window, &key]
            .into_iter()
            .enumerate()
        {
            cx.subscribe(input, move |this, _, event: &InputEvent, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                if index < 2 {
                    let identity = (
                        this.endpoint.read(cx).value().to_string(),
                        this.model.read(cx).value().to_string(),
                        this.protocol,
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
            config: Value::Null,
            language: Language::default(),
            ticket: None,
            serial: 0,
            clear_inputs: false,
            fill_inputs: false,
            open: false,
            dirty: false,
            supports_images: false,
            image_identity: (String::new(), String::new(), PROTOCOLS[0].0),
            protocol: PROTOCOLS[0].0,
            message: String::new(),
            tx,
            endpoint,
            model,
            context_window,
            key,
        }
    }
    pub fn refresh(&mut self, provider: &Value, language: Language) {
        self.language = language;
        if self.ticket.is_some() || self.dirty {
            return;
        }
        if provider["revision"].as_u64() > self.config["revision"].as_u64() {
            let first = self.config.is_null();
            self.config = provider.clone();
            self.fill_inputs = true;
            if first
                && (self.config["configured"] != true || self.config["migration_conflict"] == true)
            {
                self.open = true;
            }
        }
    }
    fn request(&mut self, method: &'static str, body: Option<Value>, cx: &mut Context<Self>) {
        self.serial += 1;
        let ticket = Ticket {
            serial: self.serial,
        };
        self.ticket = Some(ticket.clone());
        if self
            .tx
            .send(Work {
                ticket,
                method,
                body,
            })
            .is_err()
        {
            self.ticket = None;
            self.message = "Manager settings are unavailable".into();
        }
        cx.notify();
    }
    fn field(
        &self,
        title: &str,
        input: &Entity<InputState>,
        palette: crate::ui_theme::Palette,
    ) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(palette.muted))
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
        let palette = crate::ui_theme::palette(cx);
        if std::mem::take(&mut self.clear_inputs) {
            for input in [&self.endpoint, &self.model, &self.context_window, &self.key] {
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
            self.protocol = configured_protocol(&self.config);
            self.context_window.update(cx, |input, cx| {
                let limit = self.config["context_window"]
                    .as_u64()
                    .map(|value| value.to_string())
                    .unwrap_or_default();
                input.set_value(limit, window, cx);
            });
            self.image_identity = (
                self.config["base_url"].as_str().unwrap_or("").into(),
                self.config["model"].as_str().unwrap_or("").into(),
                self.protocol,
            );
            self.supports_images = self.config["supports_images"] == true;
            self.dirty = false;
        }
        let context_window = parse_context_window(self.context_window.read(cx).value().as_ref());
        self.dirty = !self.config.is_null()
            && (self.endpoint.read(cx).value().as_ref()
                != self.config["base_url"].as_str().unwrap_or("")
                || self.model.read(cx).value().as_ref()
                    != self.config["model"].as_str().unwrap_or("")
                || self.protocol != configured_protocol(&self.config)
                || context_window
                    .as_ref()
                    .map_or(true, |value| json!(value) != self.config["context_window"])
                || !self.key.read(cx).value().is_empty()
                || self.supports_images != (self.config["supports_images"] == true));
        let busy = self.ticket.is_some();
        let revision = self.config["revision"].as_u64();
        let mut view=div().flex().flex_col().gap_3().child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(self.language.text("Shared manager provider")))
            .child(div().text_size(px(11.)).text_color(rgb(palette.muted)).child(self.language.text("Every project uses this manager endpoint, model, and API key. Coding CLI login and settings stay separate.")))
            .child(Button::new("manager-config-toggle").small().child(control_label(self.language.text(if self.open {"Hide manager settings"}else{"Manager model settings"})))
                .on_click(cx.listener(|this,_,_,cx|{this.open = !this.open;cx.notify();})));
        if self.config["migration_conflict"] == true {
            view = view.child(div().text_size(px(11.)).text_color(rgb(palette.error))
                .child(self.language.text("The previous projects used different managers. Review and save the shared provider.")));
        }
        if self.open {
            view = view
                .child(div().flex().flex_col().gap_1()
                    .child(div().text_size(px(11.)).text_color(rgb(palette.muted)).child(self.language.text("Provider protocol")))
                    .child(div().flex().flex_wrap().gap_2().children(PROTOCOLS.map(|(protocol, label)| {
                        Button::new(protocol).small().selected(self.protocol == protocol).disabled(busy)
                            .child(control_label(self.language.text(label)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.protocol != protocol {
                                    this.protocol = protocol;
                                    this.image_identity.2 = protocol;
                                    this.supports_images = false;
                                    this.dirty = true;
                                    cx.notify();
                                }
                            }))
                    }))))
                .child(self.field("Provider endpoint", &self.endpoint, palette))
                .child(self.field("Manager model", &self.model, palette))
                .child(self.field("Context window (tokens, optional)", &self.context_window, palette))
                .child(div().text_size(px(11.)).text_color(rgb(if context_window.is_ok() { palette.muted } else { palette.error }))
                    .child(self.language.text(context_window.as_ref().err().copied().unwrap_or("Leave blank for automatic context detection."))))
                .child(self.field("Manager API key", &self.key, palette))
                .child(Button::new("manager-image-capability").small().selected(self.supports_images).disabled(busy||!self.config["supports_images"].is_boolean())
                    .child(control_label(self.language.text(if self.supports_images {"Image input declared supported"}else{"Declare image input support"})))
                    .on_click(cx.listener(|this,_,_,cx| { this.supports_images = !this.supports_images; cx.notify(); })))
                .child(div().text_size(px(11.)).text_color(rgb(palette.muted)).child(self.language.text("Enable only when this endpoint, model, and protocol support image input. Otherwise images remain explicit file references.")))
                .child(div().text_size(px(11.)).text_color(rgb(palette.muted)).child(self.language.text(if self.config["has_api_key"]==true {"API key saved. Leave blank to keep it."}else{"No manager API key saved"})))
                .child(div().flex().flex_wrap().gap_2()
                    .child(Button::new("save-manager-config").small().primary().child(control_label(self.language.text("Save manager settings")))
                        .disabled(busy||revision.is_none()||context_window.is_err())
                        .on_click(cx.listener(|this,_,_,cx| {
                            let Ok(limit) = parse_context_window(this.context_window.read(cx).value().as_ref()) else { return; };
                            let mut body = provider_payload(&this.config, this.endpoint.read(cx).value().as_ref(),
                                this.model.read(cx).value().as_ref(), this.protocol, limit);
                            if this.config["supports_images"].is_boolean() { body["supports_images"] = json!(this.supports_images); }
                            let key=this.key.read(cx).value().to_string();if !key.is_empty(){body["api_key"]=json!(key);}
                            this.request("PUT",Some(body),cx);
                        })))
                    .child(Button::new("reload-manager-config").small().child(control_label(self.language.text("Reload"))).disabled(busy)
                        .on_click(cx.listener(|this,_,_,cx|{this.clear_inputs=true;this.request("GET",None,cx);})))
                    .child(Button::new("clear-manager-key").small().child(control_label(self.language.text("Clear saved API key")))
                        .disabled(busy||revision.is_none()||self.config["has_api_key"]!=true||self.dirty)
                        .on_click(cx.listener(|this,_,_,cx|{
                            this.request("PUT",Some(json!({"base_url":this.config["base_url"],"model":this.config["model"],"clear_api_key":true,"expected_revision":this.config["revision"]})),cx);
                        }))));
        }
        if !self.message.is_empty() {
            view = view.child(
                div()
                    .text_size(px(12.))
                    .child(self.language.text(&self.message)),
            );
        }
        view
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InputEvent, configured_protocol, image_identity_changed, parse_context_window,
        provider_payload,
    };
    use serde_json::{Value, json};
    #[test]
    fn declaration_resets_only_for_real_provider_identity_changes() {
        let saved: (String, String, &str) = (
            "https://fixture.invalid/v1".into(),
            "vision".into(),
            "openai_completions",
        );
        let changed = (saved.0.clone(), "another-model".into(), saved.2);
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
            &(
                "https://another.invalid/v1".into(),
                saved.1.clone(),
                saved.2
            )
        ));
        assert!(image_identity_changed(
            &InputEvent::Change,
            &saved,
            &(saved.0.clone(), saved.1.clone(), "anthropic")
        ));
        // A freshly populated form or a declaration for edited values is bound
        // to that identity, even when it differs from the previously saved form.
        assert!(!image_identity_changed(
            &InputEvent::Change,
            &changed,
            &changed
        ));
    }
    #[test]
    fn provider_protocol_and_context_limit_serialize_including_clear() {
        let config = json!({"revision":5});
        assert_eq!(configured_protocol(&config), "openai_completions");
        assert_eq!(
            configured_protocol(&json!({"provider_kind":"anthropic"})),
            "anthropic"
        );
        let payload = provider_payload(
            &config,
            "https://fixture.invalid",
            "model",
            "openai_responses",
            parse_context_window(" 8192 ").unwrap(),
        );
        assert_eq!(payload["provider_kind"], "openai_responses");
        assert_eq!(payload["context_window"], 8192);
        assert_eq!(payload["expected_revision"], 5);
        let payload = provider_payload(
            &config,
            "https://fixture.invalid",
            "model",
            "anthropic",
            parse_context_window(" ").unwrap(),
        );
        assert_eq!(payload.get("context_window"), Some(&Value::Null));
        for invalid in ["0", "1023", "-1024", "2.5", "4294967296"] {
            assert!(parse_context_window(invalid).is_err());
        }
    }
}
