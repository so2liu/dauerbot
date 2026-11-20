//! Local main-window navigation. No provider requests or session creation.
use crate::locale::Language;
use gpui::{prelude::*, *};
use gpui_component::{
    ActiveTheme,
    input::{Enter, Escape, Input, InputEvent, InputState, MoveDown, MoveUp},
};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Project(String),
    Session {
        project: String,
        generation: String,
        session: String,
    },
    Page(usize),
}

#[derive(Clone, Debug)]
struct Entry {
    target: Target,
    title: String,
    detail: String,
    search: String,
}

fn entry(target: Target, title: String, detail: String, aliases: &str) -> Entry {
    let search = format!("{title} {detail} {aliases}").to_lowercase();
    Entry {
        target,
        title,
        detail,
        search,
    }
}

fn entries(state: &Value, language: Language) -> Vec<Entry> {
    let mut entries = Vec::new();
    for project in state["projects"].as_array().into_iter().flatten() {
        let Some(id) = project["id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        let name = project["name"].as_str().unwrap_or("Project");
        let path = project["path"].as_str().unwrap_or("");
        entries.push(entry(
            Target::Project(id.into()),
            name.into(),
            language.text("Projects"),
            &format!("project 项目 {path}"),
        ));
        if let (Some(session), Some(generation)) = (
            project["native_session"].as_str().filter(|s| !s.is_empty()),
            project["generation"].as_str().filter(|s| !s.is_empty()),
        ) {
            entries.push(entry(
                Target::Session {
                    project: id.into(),
                    generation: generation.into(),
                    session: session.into(),
                },
                format!("{} · {name}", language.text("Current session")),
                project["agent"].as_str().unwrap_or("").into(),
                &format!("session 会话 {name} {session}"),
            ));
        }
    }
    for (page, label, aliases) in [
        (
            8,
            "Settings",
            "settings appearance theme dark light 设置 外观 主题 暗色 深色 浅色",
        ),
        (1, "Feishu bot setup", "feishu bot setup 飞书 机器人"),
        (4, "macOS permissions", "permissions access 权限"),
        (7, "App Updates", "updates version 升级 版本"),
        (5, "Add project", "add project 添加 项目"),
        (9, "Activity", "activity messages conversations trends 观察 消息 群聊 趋势"),
    ] {
        entries.push(entry(
            Target::Page(page),
            language.text(label),
            language.text(if page == 9 { "Message trends" } else { "Settings" }),
            aliases,
        ));
    }
    entries
}

/// Navigation never treats a stale session snapshot as the current execution.
pub fn target_available(target: &Target, state: &Value) -> bool {
    match target {
        Target::Page(page) => matches!(page, 1 | 4 | 5 | 7 | 8 | 9),
        Target::Project(id) => state["projects"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|p| p["id"] == *id),
        Target::Session {
            project,
            generation,
            session,
        } => state["projects"].as_array().into_iter().flatten().any(|p| {
            p["id"] == *project && p["generation"] == *generation && p["native_session"] == *session
        }),
    }
}

/// Only the main window owns this action; takeover keeps all its keyboard input.
pub fn can_open(takeover: bool, terminal_controlling: bool, frozen: bool, quitting: bool) -> bool {
    !takeover && !terminal_controlling && !frozen && !quitting
}

#[derive(Default)]
struct FinderState {
    entries: Vec<Entry>,
    matches: Vec<usize>,
    cursor: Option<Target>,
}
impl FinderState {
    fn reconcile(&mut self, entries: Vec<Entry>, query: &str) {
        let terms: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
        self.matches = entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                terms
                    .iter()
                    .all(|term| entry.search.contains(term))
                    .then_some(i)
            })
            .collect();
        self.cursor = self
            .cursor
            .take()
            .filter(|target| self.matches.iter().any(|i| entries[*i].target == *target))
            .or_else(|| self.matches.first().map(|i| entries[*i].target.clone()));
        self.entries = entries;
    }
    fn position(&self) -> Option<usize> {
        self.matches
            .iter()
            .position(|i| Some(&self.entries[*i].target) == self.cursor.as_ref())
    }
    fn step(&mut self, down: bool) {
        if self.matches.is_empty() {
            self.cursor = None;
            return;
        }
        let len = self.matches.len();
        let position = self.position().unwrap_or(0);
        let next = if down {
            (position + 1) % len
        } else {
            (position + len - 1) % len
        };
        self.cursor = Some(self.entries[self.matches[next]].target.clone());
    }
    fn hover_target(&mut self, target: &Target) -> bool {
        if self
            .matches
            .iter()
            .any(|index| self.entries[*index].target == *target)
        {
            self.cursor = Some(target.clone());
            true
        } else {
            false
        }
    }
    fn selected(&self) -> Option<Target> {
        self.position()
            .map(|position| self.entries[self.matches[position]].target.clone())
    }
}

pub struct Navigate(pub Target);
pub struct QuickFinder {
    open: bool,
    state: Value,
    language: Language,
    model: FinderState,
    search: Entity<InputState>,
    prior_focus: Option<WeakFocusHandle>,
    fallback_focus: Option<FocusHandle>,
    scroll: ScrollHandle,
}
impl EventEmitter<Navigate> for QuickFinder {}
impl QuickFinder {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe(&search, |this, _, _: &InputEvent, cx| {
            this.reconcile(cx);
            if let Some(position) = this.model.position() {
                this.scroll.scroll_to_item(position);
            }
            cx.notify();
        })
        .detach();
        Self {
            open: false,
            state: Value::Null,
            language: Language::En,
            model: FinderState::default(),
            search,
            prior_focus: None,
            fallback_focus: None,
            scroll: ScrollHandle::new(),
        }
    }
    pub fn sync(&mut self, state: &Value, language: Language, cx: &mut Context<Self>) {
        // Keep only navigation metadata, not the full application snapshot.
        let snapshot = json!({"projects": state["projects"].as_array().into_iter().flatten().map(|project| json!({
            "id": project["id"], "name": project["name"], "path": project["path"],
            "agent": project["agent"], "generation": project["generation"], "native_session": project["native_session"]
        })).collect::<Vec<_>>()});
        let changed = self.state != snapshot || self.language != language;
        self.state = snapshot;
        self.language = language;
        if self.open && changed {
            self.reconcile(cx);
            cx.notify();
        }
    }
    pub fn toggle(&mut self, fallback: FocusHandle, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.close(window, cx);
            return;
        }
        self.prior_focus = window.focused(cx).map(|focus| focus.downgrade());
        self.fallback_focus = Some(fallback);
        self.open = true;
        self.model = FinderState::default();
        self.search.update(cx, |input, cx| {
            input.set_placeholder(
                self.language.text("Search projects, sessions and settings"),
                window,
                cx,
            );
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        self.reconcile(cx);
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }
    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.close(window, cx);
        }
    }
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        if let Some(focus) = self
            .prior_focus
            .take()
            .and_then(|focus| focus.upgrade())
            .or_else(|| self.fallback_focus.take())
        {
            focus.focus(window);
        }
        cx.notify();
    }
    fn reconcile(&mut self, cx: &App) {
        self.model.reconcile(
            entries(&self.state, self.language),
            self.search.read(cx).value().as_ref(),
        );
    }
    fn owns_action(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.open
            && self.search.read(cx).focus_handle(cx).is_focused(window)
            && self.search.update(cx, |input, cx| {
                input.marked_text_range(window, cx).is_none()
            })
    }
    fn step(&mut self, down: bool, cx: &mut Context<Self>) {
        self.reconcile(cx);
        self.model.step(down);
        if let Some(position) = self.model.position() {
            self.scroll.scroll_to_item(position);
        }
        cx.notify();
    }
    fn choose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reconcile(cx);
        if let Some(target) = self
            .model
            .selected()
            .filter(|target| target_available(target, &self.state))
        {
            self.close(window, cx);
            cx.emit(Navigate(target));
        }
    }
}
impl Render for QuickFinder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let colors = cx.theme().colors;
        self.reconcile(cx);
        let mut rows = div()
            .id("quick-find-results")
            .max_h(px(300.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .gap_1()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
        for index in self.model.matches.clone() {
            let entry = &self.model.entries[index];
            let selected = self.model.cursor.as_ref() == Some(&entry.target);
            let hover_target = entry.target.clone();
            let click_target = entry.target.clone();
            rows = rows.child(
                div()
                    .id(("quick-find-row", index))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if selected {
                        colors.list_active
                    } else {
                        colors.popover
                    })
                    .text_color(colors.popover_foreground)
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered {
                            this.model.hover_target(&hover_target);
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.reconcile(cx);
                        if this.model.hover_target(&click_target) {
                            this.choose(window, cx);
                        }
                    }))
                    .child(div().text_size(px(13.)).child(entry.title.clone()))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(colors.muted_foreground)
                            .child(entry.detail.clone()),
                    ),
            );
        }
        if self.model.matches.is_empty() {
            rows = rows.child(
                div().p_3().text_color(colors.muted_foreground).child(
                    self.language
                        .text("No matching projects, sessions or settings"),
                ),
            );
        }
        div()
            .absolute()
            .inset_0()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .occlude()
            .bg(rgba(0x00000066))
            .pt(px(64.))
            .px_6()
            .flex()
            .justify_center()
            .items_start()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.close(window, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .w(px(560.))
                    .max_w_full()
                    .p_4()
                    .rounded_xl()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.popover)
                    .text_color(colors.popover_foreground)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(13.))
                            .child(self.language.text("Quick find")),
                    )
                    .child(
                        div()
                            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                                if this.owns_action(window, cx) {
                                    cx.stop_propagation();
                                    this.step(true, cx);
                                } else {
                                    cx.propagate();
                                }
                            }))
                            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                                if this.owns_action(window, cx) {
                                    cx.stop_propagation();
                                    this.step(false, cx);
                                } else {
                                    cx.propagate();
                                }
                            }))
                            .capture_action(cx.listener(|this, action: &Enter, window, cx| {
                                if !action.secondary && this.owns_action(window, cx) {
                                    cx.stop_propagation();
                                    this.choose(window, cx);
                                } else {
                                    cx.propagate();
                                }
                            }))
                            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                                if this.owns_action(window, cx) {
                                    cx.stop_propagation();
                                    this.close(window, cx);
                                } else {
                                    cx.propagate();
                                }
                            }))
                            .child(Input::new(&self.search)),
                    )
                    .child(rows)
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(colors.muted_foreground)
                            .child(
                                self.language
                                    .text("↑/↓ to move · Enter to open · Esc to close"),
                            ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{FinderState, Target, can_open, entries, target_available};
    use crate::locale::Language;
    use serde_json::json;

    #[test]
    fn mixed_targets_filter_locally_and_only_index_real_current_sessions() {
        let state = json!({"projects":[{"id":"p","name":"Billing","path":"/work/billing","agent":"codex","generation":"g","native_session":"n"},{"id":"q","name":"Research","generation":"","native_session":""}]});
        let all = entries(&state, Language::En);
        assert_eq!(
            all.iter()
                .filter(|entry| matches!(entry.target, Target::Session { .. }))
                .count(),
            1
        );
        let mut model = FinderState::default();
        model.reconcile(all.clone(), "billing session");
        assert!(matches!(model.selected(), Some(Target::Session { .. })));
        model.reconcile(all.clone(), "permissions");
        assert_eq!(model.selected(), Some(Target::Page(4)));
        model.reconcile(all.clone(), "dark theme");
        assert_eq!(model.selected(), Some(Target::Page(8)));
        model.reconcile(all.clone(), "深色");
        assert_eq!(model.selected(), Some(Target::Page(8)));
        model.reconcile(all, "research project");
        assert_eq!(model.selected(), Some(Target::Project("q".into())));
    }
    #[test]
    fn cursor_wraps_matches_pointer_and_survives_filter_contraction() {
        let state = json!({"projects":[{"id":"p","name":"Alpha"},{"id":"q","name":"Beta"}]});
        let all = entries(&state, Language::En);
        let mut model = FinderState::default();
        model.reconcile(all.clone(), "project");
        assert!(model.hover_target(&Target::Project("q".into())));
        assert_eq!(model.selected(), Some(Target::Project("q".into())));
        model.step(true);
        assert_eq!(model.selected(), Some(Target::Page(5)));
        model.step(true);
        assert_eq!(model.selected(), Some(Target::Project("p".into())));
        model.step(false);
        assert_eq!(model.selected(), Some(Target::Page(5)));
        model.reconcile(all.clone(), "beta");
        assert_eq!(model.selected(), Some(Target::Project("q".into())));
        assert!(!model.hover_target(&Target::Project("gone".into())));
        assert_eq!(model.selected(), Some(Target::Project("q".into())));
        model.reconcile(all, "no-such-result");
        model.step(true);
        model.step(false);
        assert_eq!(model.selected(), None);
    }
    #[test]
    fn navigation_rejects_removed_projects_and_replaced_sessions() {
        let state = json!({"projects":[{"id":"p","generation":"new","native_session":"fresh"}]});
        assert!(!target_available(&Target::Project("gone".into()), &state));
        assert!(!target_available(
            &Target::Session {
                project: "p".into(),
                generation: "old".into(),
                session: "stale".into()
            },
            &state
        ));
        assert!(target_available(
            &Target::Session {
                project: "p".into(),
                generation: "new".into(),
                session: "fresh".into()
            },
            &state
        ));
        assert!(!target_available(&Target::Page(100), &state));
    }
    #[test]
    fn takeover_and_activation_authority_prevent_finder_interception() {
        assert!(can_open(false, false, false, false));
        assert!(!can_open(true, false, false, false));
        assert!(!can_open(false, true, false, false));
        assert!(!can_open(false, false, true, false));
        assert!(!can_open(false, false, false, true));
    }
}
