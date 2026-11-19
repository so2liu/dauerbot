//! Native project model controls. HTTP work stays off the GPUI thread.
use crate::{action_icons::ActionIcon, agent_tree, locale::Language};
use gpui::{prelude::*, *};
use gpui_component::{
    Disableable, Selectable, Sizable,
    button::*,
    input::{Enter, Escape, Input, InputEvent, InputState, MoveDown, MoveUp},
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

fn match_score(text: &str, query: &str) -> u32 {
    if text == query {
        return 40;
    }
    if text.starts_with(query) {
        return 30;
    }
    if text.contains(query) {
        return 20;
    }
    let mut remaining = query.chars();
    let mut next = remaining.next();
    for ch in text.chars() {
        if next == Some(ch) {
            next = remaining.next();
            if next.is_none() {
                return 10;
            }
        }
    }
    0
}

/// Keep the original catalogue index and provider identity after ranking.
/// Filtering is local; typing never changes a preference or submits a model ID.
fn matching_models<'a>(options: &'a [Value], query: &str) -> Vec<(usize, &'a Value)> {
    let query = query.to_lowercase();
    let terms: Vec<_> = query.split_whitespace().collect();
    let mut matches: Vec<_> = options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            let id = option["id"].as_str().unwrap_or("").to_lowercase();
            let label = option["label"].as_str().unwrap_or("").to_lowercase();
            let provider = option["provider"].as_str().unwrap_or("").to_lowercase();
            let mut score = 0;
            for term in &terms {
                let best = match_score(&id, term)
                    .max(match_score(&label, term))
                    .max(match_score(&provider, term).min(10));
                if best == 0 {
                    return None;
                }
                score += best;
            }
            Some((score, index, option))
        })
        .collect();
    // Stable ties preserve the native catalogue order, including duplicate IDs
    // belonging to different providers. Never collapse those choices.
    matches.sort_by_key(|(score, _, _)| std::cmp::Reverse(*score));
    matches
        .into_iter()
        .map(|(_, index, option)| (index, option))
        .collect()
}

/// Cursor identity is the original catalogue index, never a filtered position.
/// Updating results only changes navigation; it never requests a preference.
#[derive(Default, Debug)]
struct ModelCursor {
    items: Vec<usize>,
    current: Option<usize>,
}

impl ModelCursor {
    fn reconcile(&mut self, items: Vec<usize>) {
        self.current = self
            .current
            .filter(|item| items.contains(item))
            .or_else(|| items.first().copied());
        self.items = items;
    }

    fn step(&mut self, forward: bool) {
        if self.items.is_empty() {
            self.current = None;
            return;
        }
        let position = self
            .current
            .and_then(|item| self.items.iter().position(|candidate| *candidate == item));
        let next = match position {
            None => 0,
            Some(position) if forward => (position + 1) % self.items.len(),
            Some(position) => (position + self.items.len() - 1) % self.items.len(),
        };
        self.current = Some(self.items[next]);
    }

    fn hover(&mut self, item: usize) {
        if self.items.contains(&item) {
            self.current = Some(item);
        }
    }

    fn selected(&self, enabled: bool) -> Option<usize> {
        self.current
            .filter(|item| enabled && self.items.contains(item))
    }

    fn position(&self) -> Option<usize> {
        self.current
            .and_then(|item| self.items.iter().position(|candidate| *candidate == item))
    }
}

/// Plan a native nearest-scroll request from the measured visible interval.
/// Missing/new-layout geometry is deliberately deferred to GPUI prepaint.
fn cursor_scroll_target(
    position: Option<usize>,
    offset: Pixels,
    viewport: (Pixels, Pixels),
    item: Option<(Pixels, Pixels)>,
) -> Option<usize> {
    let index = position? + 1; // The explicit native-default button is first.
    if let Some((top, bottom)) = item
        && viewport.1 > viewport.0
        && top + offset >= viewport.0
        && bottom + offset <= viewport.1
    {
        None
    } else {
        Some(index)
    }
}

/// GPUI applies the wheel delta before this containment step. Clamp that
/// resulting offset and consume every wheel inside the list, including edges
/// and lists too short to scroll. Outside events keep their normal behavior.
fn contain_list_wheel(offset: Pixels, maximum: Pixels, inside: bool) -> (Pixels, bool) {
    if inside {
        (offset.clamp(-maximum.max(px(0.)), px(0.)), true)
    } else {
        (offset, false)
    }
}

fn selection_enabled(project: &Value, catalog: &Value, busy: bool, catalog_error: bool) -> bool {
    !busy
        && !catalog_error
        && project["paused"] != true
        && project["lifecycle"] != "taken_over"
        && matches!(
            project["agent"].as_str(),
            Some("codex" | "claude" | "pi" | "agy")
        )
        && catalog["agent"] == project["agent"]
        && catalog["generation"] == project["generation"]
        && catalog["options"].is_array()
        && catalog.get("configured").is_some()
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
    cursor: ModelCursor,
    options_scroll: ScrollHandle,
    search: Entity<InputState>,
    model: Entity<InputState>,
    provider: Entity<InputState>,
}

impl ModelPicker {
    pub fn new(data: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = cx.new(|cx| InputState::new(window, cx).placeholder("Model ID"));
        let provider = cx.new(|cx| InputState::new(window, cx).placeholder("Provider (optional)"));
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search models or providers"));
        cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
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
            cursor: ModelCursor::default(),
            options_scroll: ScrollHandle::new(),
            search,
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
            self.cursor = ModelCursor::default();
            self.catalog = Value::Null;
            self.catalog_error = false;
            self.clear_inputs = true;
            self.open = false;
            self.message.clear();
            self.request(None, cx);
        }
    }

    fn search_owns_action(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.open
            && self.search.read(cx).focus_handle(cx).is_focused(window)
            && self.search.update(cx, |input, cx| {
                input.marked_text_range(window, cx).is_none()
            })
    }

    fn reconcile_cursor(&mut self, cx: &App) {
        let query = self.search.read(cx).value().to_string();
        let items = matching_models(
            self.catalog["options"]
                .as_array()
                .map_or(&[], Vec::as_slice),
            &query,
        )
        .into_iter()
        .map(|(index, _)| index)
        .collect();
        self.cursor.reconcile(items);
    }

    fn move_cursor(&mut self, forward: bool, cx: &mut Context<Self>) {
        let rendered_items = self.cursor.items.clone();
        self.reconcile_cursor(cx);
        self.cursor.step(forward);
        self.scroll_cursor(self.cursor.items == rendered_items);
        cx.notify();
    }

    fn scroll_cursor(&self, layout_current: bool) {
        let viewport = self.options_scroll.bounds();
        let item = self.cursor.position().and_then(|position| {
            layout_current
                .then(|| self.options_scroll.bounds_for_item(position + 1))
                .flatten()
        });
        if let Some(index) = cursor_scroll_target(
            self.cursor.position(),
            self.options_scroll.offset().y,
            (viewport.top(), viewport.bottom()),
            item.map(|bounds| (bounds.top(), bounds.bottom())),
        ) {
            self.options_scroll.scroll_to_item(index);
        }
    }

    fn choose_cursor(&mut self, cx: &mut Context<Self>) {
        self.reconcile_cursor(cx);
        let enabled = selection_enabled(
            &self.project,
            &self.catalog,
            self.ticket.is_some(),
            self.catalog_error,
        );
        if let Some(index) = self.cursor.selected(enabled)
            && let Some(option) = self.catalog["options"].get(index)
        {
            let selection = json!({"model":option["id"], "provider":option["provider"]});
            self.request(Some(selection), cx);
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

impl ModelPicker {
    pub fn trigger(&self, cx: &mut Context<Self>) -> Button {
        let palette = crate::ui_theme::palette(cx);
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

        let status = if pending {
            "pending"
        } else if self.catalog_error {
            "error"
        } else if busy
            || (configured["model"].is_string() && !effective["model"].is_string())
        {
            "pending"
        } else if effective["model"].is_string() {
            "ready"
        } else {
            "unknown"
        };
        let title = self
            .language
            .text(if crate::manager_settings::enabled(&self.project) {
                "Coding worker model"
            } else {
                "Model"
            });
        let label = self.language.text(label);
        let caption = self.language.text(caption);
        let display = if label == self.language.text("Native CLI/session settings") {
            self.language.text("CLI default")
        } else {
            label.clone()
        };
        Button::new("choose-project-model")
            .small()
            .compact()
            .h(px(28.))
            .max_w_full()
            .cursor_pointer()
            .child(ActionIcon::Model.draw(palette.accent, 14.))
            .child(control_label(display).max_w(px(92.)).truncate())
            .child(agent_tree::status_dot(
                status,
                caption.clone(),
                "model-confirmation".into(),
                palette,
            ))
            .child(ActionIcon::Chevron.draw(palette.muted, 13.))
            .tooltip(format!("{title} · {label}\n{caption}"))
            .disabled(!supported)
            .selected(self.open)
            .on_click(cx.listener(|this, _, window, cx| {
                this.open = !this.open;
                if this.open {
                    this.cursor = ModelCursor::default();
                    this.search.update(cx, |search, cx| {
                        search.set_value("", window, cx);
                        search.focus(window, cx);
                    });
                }
                // Native catalogues and Feishu preferences can change without a
                // new generation. Reopening must not reuse a stale snapshot.
                if this.open && this.ticket.is_none() {
                    this.request(None, cx);
                }
                cx.notify();
            }))
    }
}

impl Render for ModelPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::ui_theme::palette(cx);
        if std::mem::take(&mut self.clear_inputs) {
            self.search
                .update(cx, |input, cx| input.set_value("", window, cx));
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
            self.search.update(cx, |input, cx| {
                input.set_placeholder(self.language.text("Search models or providers"), window, cx)
            });
        }
        let configured = &self.catalog["configured"];
        let busy = self.ticket.is_some();
        let unavailable =
            self.project["paused"] == true || self.project["lifecycle"] == "taken_over";
        let can_choose = selection_enabled(&self.project, &self.catalog, busy, self.catalog_error);
        let mut view = div().flex().flex_col().gap_2().min_w_0();
        if self.open {
            let mut panel = div()
                .rounded_lg()
                .border_1()
                .border_color(rgb(palette.line))
                .bg(rgb(palette.surface))
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
                            ActionIcon::Refresh
                                .button(
                                    "reload-model-catalog",
                                    self.language.text("Refresh"),
                                    palette.muted,
                                )
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| this.request(None, cx))),
                        ),
                );
            let mut options = div()
                .id("native-model-options")
                .max_h(px(220.))
                .overflow_y_scroll()
                .track_scroll(&self.options_scroll)
                .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                    // GPUI registers the native listener after this callback and
                    // dispatches bubble listeners in reverse. It has already applied
                    // the delta once; do not apply it again or scroll an ancestor.
                    let offset = this.options_scroll.offset();
                    let (y, consume) = contain_list_wheel(
                        offset.y,
                        this.options_scroll.max_offset().height,
                        this.options_scroll.bounds().contains(&event.position),
                    );
                    if consume {
                        this.options_scroll.set_offset(point(offset.x, y));
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
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
                    .disabled(!can_choose)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.request(Some(json!({"model":"","provider":null})), cx)
                    })),
            );
            let query = self.search.read(cx).value().to_string();
            let matches = matching_models(
                self.catalog["options"]
                    .as_array()
                    .map_or(&[], Vec::as_slice),
                &query,
            );
            let previous_cursor = (self.cursor.current, self.cursor.position());
            self.cursor
                .reconcile(matches.iter().map(|(index, _)| *index).collect());
            if previous_cursor != (self.cursor.current, self.cursor.position()) {
                // Filtered row geometry belongs to the preceding frame. GPUI must
                // measure the new children before choosing their nearest offset.
                self.scroll_cursor(false);
            }
            let no_matches = !query.trim().is_empty() && matches.is_empty();
            for (index, option) in matches {
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
                    div()
                        .border_1()
                        .rounded_md()
                        .border_color(if self.cursor.current == Some(index) {
                            rgb(palette.accent)
                        } else {
                            rgba(0x00000000)
                        })
                        .child(
                            Button::new(("native-model", index))
                                .small()
                                .text_size(px(13.))
                                .max_w_full()
                                .overflow_hidden()
                                .child(control_label(name).max_w(px(180.)).truncate())
                                .selected(selected)
                                .disabled(!can_choose)
                                .on_hover(cx.listener(move |this, hovered, _, cx| {
                                    if *hovered {
                                        this.cursor.hover(index);
                                        cx.notify();
                                    }
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.reconcile_cursor(cx);
                                    if this.cursor.items.contains(&index) {
                                        this.cursor.hover(index);
                                        this.choose_cursor(cx);
                                    }
                                })),
                        ),
                );
            }
            panel = panel
                .child(
                    div()
                        // Input key bindings dispatch actions before raw key-down events.
                        // Capture only this search field, leaving manual IDs and Terminal alone.
                        .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                            if this.search_owns_action(window, cx) {
                                cx.stop_propagation();
                                this.move_cursor(true, cx);
                            } else {
                                cx.propagate();
                            }
                        }))
                        .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                            if this.search_owns_action(window, cx) {
                                cx.stop_propagation();
                                this.move_cursor(false, cx);
                            } else {
                                cx.propagate();
                            }
                        }))
                        .capture_action(cx.listener(|this, action: &Enter, window, cx| {
                            if !action.secondary && this.search_owns_action(window, cx) {
                                cx.stop_propagation();
                                this.choose_cursor(cx);
                            } else {
                                cx.propagate();
                            }
                        }))
                        .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                            if this.search_owns_action(window, cx) {
                                cx.stop_propagation();
                                this.open = false;
                                cx.notify();
                            } else {
                                cx.propagate();
                            }
                        }))
                        .child(Input::new(&self.search).small().text_size(px(13.))),
                )
                .child(options);
            if no_matches && !busy && !self.catalog_error {
                panel = panel.child(div().text_size(px(11.)).child(self.language.text(
                    "No matching models. Enter an exact model ID below or clear the search.",
                )));
            }
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
                        .text_color(rgb(palette.muted))
                        .child(self.language.text(warning)),
                );
            }
            view = view.child(panel);
        }
        if !self.message.is_empty() {
            view = view.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(palette.muted))
                    .child(self.language.text(&self.message)),
            );
        }
        view
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ModelCursor, contain_list_wheel, cursor_scroll_target, matching_models, selection_enabled,
    };
    use gpui::px;
    use serde_json::json;

    #[test]
    fn cursor_visibility_uses_scroll_offset_and_only_requests_hidden_rows() {
        let viewport = (px(100.), px(200.));
        // Unscrolled row bounds plus the real list offset give visible bounds.
        assert_eq!(
            cursor_scroll_target(Some(2), px(-100.), viewport, Some((px(220.), px(250.)))),
            None
        );
        assert_eq!(
            cursor_scroll_target(Some(2), px(-100.), viewport, Some((px(190.), px(220.)))),
            Some(3)
        );
        assert_eq!(
            cursor_scroll_target(Some(2), px(-100.), viewport, Some((px(280.), px(320.)))),
            Some(3)
        );
        assert_eq!(
            cursor_scroll_target(Some(0), px(0.), viewport, Some((px(100.), px(200.)))),
            None
        );
    }

    #[test]
    fn cursor_visibility_defers_new_geometry_and_has_no_empty_target() {
        let viewport = (px(100.), px(200.));
        assert_eq!(cursor_scroll_target(None, px(0.), viewport, None), None);
        // First model row is child 1 because child 0 is the native-default button.
        assert_eq!(
            cursor_scroll_target(Some(0), px(0.), viewport, None),
            Some(1)
        );
        assert_eq!(
            cursor_scroll_target(Some(4), px(0.), (px(0.), px(0.)), Some((px(0.), px(0.)))),
            Some(5)
        );
    }

    #[test]
    fn wheel_containment_clamps_native_offsets_and_consumes_at_both_edges() {
        // Native GPUI has already added each delta to the list's offset.
        assert_eq!(
            contain_list_wheel(px(-30.), px(100.), true),
            (px(-30.), true)
        );
        assert_eq!(contain_list_wheel(px(20.), px(100.), true), (px(0.), true));
        assert_eq!(
            contain_list_wheel(px(-140.), px(100.), true),
            (px(-100.), true)
        );
        assert_eq!(contain_list_wheel(px(0.), px(100.), true), (px(0.), true));
        assert_eq!(
            contain_list_wheel(px(-100.), px(100.), true),
            (px(-100.), true)
        );
    }

    #[test]
    fn wheel_containment_handles_empty_short_lists_and_preserves_outside_events() {
        for maximum in [px(0.), px(-10.)] {
            for offset in [px(-25.), px(0.), px(25.)] {
                assert_eq!(contain_list_wheel(offset, maximum, true), (px(0.), true));
            }
        }
        assert_eq!(
            contain_list_wheel(px(-140.), px(100.), false),
            (px(-140.), false)
        );
        assert_eq!(contain_list_wheel(px(20.), px(0.), false), (px(20.), false));
    }

    #[test]
    fn cursor_wraps_and_hover_preserves_catalogue_identity() {
        let mut cursor = ModelCursor::default();
        cursor.reconcile(vec![8, 2, 5]);
        assert_eq!(cursor.selected(true), Some(8));
        cursor.step(false);
        assert_eq!(cursor.selected(true), Some(5));
        cursor.step(true);
        assert_eq!(cursor.selected(true), Some(8));
        cursor.hover(2);
        assert_eq!(cursor.position(), Some(1));
        cursor.step(true);
        assert_eq!(cursor.selected(true), Some(5));
        cursor.hover(99);
        assert_eq!(cursor.selected(true), Some(5));
    }

    #[test]
    fn filtering_reconciles_identity_and_empty_results_cannot_select() {
        let mut cursor = ModelCursor::default();
        cursor.reconcile(vec![4, 7, 9]);
        cursor.hover(9);
        cursor.reconcile(vec![9, 7]);
        assert_eq!(cursor.position(), Some(0));
        cursor.reconcile(vec![7]);
        assert_eq!(cursor.selected(true), Some(7));
        cursor.reconcile(vec![]);
        cursor.step(true);
        cursor.step(false);
        assert_eq!(cursor.selected(true), None);
        assert_eq!(cursor.position(), None);
        cursor.reconcile(vec![4]);
        assert_eq!(cursor.selected(false), None);
        assert_eq!(cursor.selected(true), Some(4));
    }

    #[test]
    fn keyboard_and_pointer_selection_share_scope_and_control_guards() {
        let project = json!({"agent":"codex", "generation":"current"});
        let catalog =
            json!({"agent":"codex", "generation":"current", "options":[], "configured":null});
        assert!(selection_enabled(&project, &catalog, false, false));
        assert!(!selection_enabled(&project, &catalog, true, false));
        assert!(!selection_enabled(&project, &catalog, false, true));
        for changed in [
            json!({"paused":true}),
            json!({"lifecycle":"taken_over"}),
            json!({"generation":"new"}),
            json!({"agent":"pi"}),
        ] {
            let mut project = project.clone();
            for (key, value) in changed.as_object().unwrap() {
                project[key] = value.clone();
            }
            assert!(!selection_enabled(&project, &catalog, false, false));
        }
        assert!(!selection_enabled(&project, &json!(null), false, false));
    }

    #[test]
    fn model_search_ranks_names_before_provider_matches_and_preserves_catalogue_order() {
        let options = vec![
            json!({"id":"xgpt-6","label":"Other","provider":"alpha"}),
            json!({"id":"gpt-6-plus","label":"Extended","provider":"alpha"}),
            json!({"id":"gpt-6","label":"Sol","provider":"beta"}),
            json!({"id":"unrelated","label":"Another","provider":"gpt-6"}),
        ];
        let indices = |query| {
            matching_models(&options, query)
                .into_iter()
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        };
        assert_eq!(indices("  GpT-6  "), vec![2, 1, 0, 3]);
        assert_eq!(indices(""), vec![0, 1, 2, 3]);
        assert_eq!(indices(" \t "), vec![0, 1, 2, 3]);
        assert!(indices("model-that-is-not-listed").is_empty());
    }

    #[test]
    fn model_search_keeps_duplicate_provider_choices_and_matches_all_query_terms() {
        let options = vec![
            json!({"id":"gpt-6.2-sol","label":"Sol","provider":"magpie-macmini"}),
            json!({"id":"gpt-6.2-sol","label":"Sol","provider":"other"}),
            json!({"id":"deepseek-flash","label":"闪光","provider":"blade"}),
        ];
        let both = matching_models(&options, "g62s");
        assert_eq!(both.len(), 2, "subsequence search retains both providers");
        assert_eq!((both[0].0, both[1].0), (0, 1));
        assert_eq!(both[0].1["provider"], "magpie-macmini");
        assert_eq!(both[1].1["provider"], "other");
        let scoped = matching_models(&options, "MACMINI sol");
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].0, 0);
        assert_eq!(matching_models(&options, "闪光")[0].0, 2);
        assert!(matching_models(&options, "sol blade").is_empty());
    }
}
