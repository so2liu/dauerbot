//! Activity queries run off the GPUI thread and are fenced to the current view.
use crate::{action_icons::ActionIcon, locale::Language, ui_theme::Palette};
use chrono::{DateTime, Offset, Utc};
use gpui::{prelude::*, *};
use gpui_component::{
    Disableable, Sizable,
    button::*,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu, PopupMenuItem},
    tooltip::Tooltip,
};
use herdr_feishu_bridge::config::Config;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const PAGE_SIZE: &str = "80";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Filter {
    #[default]
    All,
    NeedsAction,
    Outbound,
}

impl Filter {
    const fn query(self) -> &'static str {
        match self {
            Self::All => "all_user",
            Self::NeedsAction => "needs_action",
            Self::Outbound => "with_outbound",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Scope {
    project: String,
    chat: String,
    range: &'static str,
    filter: Filter,
    search: String,
    bucket: Option<(i64, i64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cursor {
    time: i64,
    id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Job {
    Dashboard(Scope, Option<Cursor>),
    Detail(String),
    Recover {
        row: String,
        target: RecoveryTarget,
        retry: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RecoveryTarget {
    Event(String),
    Message(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Ticket {
    serial: u64,
    job: Job,
}

fn accepts(pending: Option<&Ticket>, ticket: &Ticket) -> bool {
    pending == Some(ticket)
}

#[derive(Default)]
struct Route {
    detail: Option<String>,
    pending: Option<Ticket>,
}

impl Route {
    fn push(&mut self, id: String) {
        self.detail = Some(id);
        self.pending = None;
    }

    fn back(&mut self) -> bool {
        self.pending = None;
        self.detail.take().is_some()
    }

    fn accepts(&self, ticket: &Ticket) -> bool {
        matches!(&ticket.job, Job::Detail(id) if Some(id) == self.detail.as_ref())
            && accepts(self.pending.as_ref(), ticket)
    }
}

#[derive(Default)]
struct Dashboard {
    overview: Value,
    messages: Vec<Value>,
    next: Option<Cursor>,
    zone: Option<chrono_tz::Tz>,
}

enum Response {
    Dashboard(Dashboard),
    Detail(Value),
    Recovered,
}

type Navigate = Rc<dyn Fn(String, bool, &mut Window, &mut App)>;

pub struct ObservationPanel {
    scope: Scope,
    projects: Vec<(String, String)>,
    language: Language,
    by_project: bool,
    show_duration: bool,
    started: bool,
    serial: u64,
    pending: Option<Ticket>,
    recovery: Option<Ticket>,
    route: Route,
    dashboard: Option<Dashboard>,
    detail: Option<Value>,
    error: bool,
    detail_error: bool,
    recovery_error: bool,
    focus: FocusHandle,
    prior_focus: Option<WeakFocusHandle>,
    message_scroll: ScrollHandle,
    list_offset: Option<Point<Pixels>>,
    search: Entity<InputState>,
    search_language: Language,
    navigate: Option<Navigate>,
    tx: mpsc::Sender<Ticket>,
}

impl ObservationPanel {
    pub fn new(data: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (tx, requests) = mpsc::channel::<Ticket>();
        let (replies, rx) = flume::unbounded();
        std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let Ok(http) = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(15))
                .build()
            else {
                return;
            };
            for ticket in requests {
                let result = runtime
                    .block_on(fetch(&data, &http, &ticket.job))
                    .map_err(|_| ());
                if replies.send((ticket, result)).is_err() {
                    break;
                }
            }
        });
        cx.spawn(async move |entity, cx| {
            while let Ok((ticket, result)) = rx.recv_async().await {
                if entity
                    .update(cx, |this, cx| this.receive(ticket, result, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(Language::default().text("Search messages"))
        });
        cx.subscribe(&search, |this, search, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let next = search.read(cx).value().trim().to_owned();
                if this.scope.search != next {
                    this.scope.search = next;
                    this.refresh(cx);
                }
            }
        })
        .detach();
        Self {
            scope: Scope {
                range: "24h",
                ..Scope::default()
            },
            projects: Vec::new(),
            language: Language::default(),
            by_project: false,
            show_duration: false,
            started: false,
            serial: 0,
            pending: None,
            recovery: None,
            route: Route::default(),
            dashboard: None,
            detail: None,
            error: false,
            detail_error: false,
            recovery_error: false,
            focus: cx.focus_handle(),
            prior_focus: None,
            message_scroll: ScrollHandle::new(),
            list_offset: None,
            search,
            search_language: Language::default(),
            navigate: None,
            tx,
        }
    }

    pub fn activate(&mut self, projects: &Value, language: Language, cx: &mut Context<Self>) {
        let projects = list(projects)
            .iter()
            .map(|item| (text(item, "id").to_owned(), text(item, "name").to_owned()))
            .collect::<Vec<_>>();
        let changed = self.projects != projects || self.language != language;
        self.projects = projects;
        self.language = language;
        let deleted_project = !self.scope.project.is_empty()
            && !self
                .projects
                .iter()
                .any(|(id, _)| id == &self.scope.project);
        if deleted_project {
            self.scope.project.clear();
        }
        if !self.started || deleted_project {
            self.started = true;
            self.refresh(cx);
        } else if changed {
            cx.notify();
        }
    }

    pub fn set_navigation(
        &mut self,
        navigate: impl Fn(String, bool, &mut Window, &mut App) + 'static,
    ) {
        self.navigate = Some(Rc::new(navigate));
    }

    fn issue(&mut self, job: Job) -> Option<Ticket> {
        self.serial += 1;
        let ticket = Ticket {
            serial: self.serial,
            job,
        };
        self.tx.send(ticket.clone()).ok().map(|()| ticket)
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.pending = self.issue(Job::Dashboard(self.scope.clone(), None));
        self.error = self.pending.is_none();
        cx.notify();
    }

    fn refresh_detail(&mut self, id: String, cx: &mut Context<Self>) {
        self.route.pending = self.issue(Job::Detail(id));
        self.detail_error = self.route.pending.is_none();
        cx.notify();
    }

    fn receive(&mut self, ticket: Ticket, result: Result<Response, ()>, cx: &mut Context<Self>) {
        match &ticket.job {
            Job::Dashboard(_, cursor) if accepts(self.pending.as_ref(), &ticket) => {
                self.pending = None;
                self.error = result.is_err();
                if let Ok(Response::Dashboard(next)) = result {
                    if cursor.is_some() {
                        if let Some(existing) = &mut self.dashboard {
                            existing.messages.extend(next.messages);
                            existing.next = next.next;
                        }
                    } else {
                        self.dashboard = Some(next);
                    }
                }
            }
            Job::Detail(_) if self.route.accepts(&ticket) => {
                self.route.pending = None;
                self.detail_error = result.is_err();
                if let Ok(Response::Detail(detail)) = result {
                    self.detail = Some(detail);
                }
            }
            Job::Recover { row, .. } if accepts(self.recovery.as_ref(), &ticket) => {
                self.recovery = None;
                self.recovery_error = result.is_err();
                self.refresh(cx);
                if Some(row) == self.route.detail.as_ref() {
                    self.refresh_detail(row.clone(), cx);
                }
            }
            _ => return,
        }
        cx.notify();
    }

    fn open(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.prior_focus = window.focused(cx).map(|focus| focus.downgrade());
        self.list_offset = Some(self.message_scroll.offset());
        self.route.push(id.clone());
        self.detail = None;
        self.recovery_error = false;
        self.focus.focus(window);
        self.refresh_detail(id, cx);
    }

    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.route.back() {
            self.detail = None;
            if let Some(offset) = self.list_offset.take() {
                self.message_scroll.set_offset(offset);
            }
            let focus = self
                .prior_focus
                .take()
                .and_then(|handle| handle.upgrade())
                .unwrap_or_else(|| self.focus.clone());
            focus.focus(window);
            cx.notify();
        }
    }

    fn recover(&mut self, retry: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.recovery.is_some() || self.route.pending.is_some() {
            return;
        }
        let Some(detail) = &self.detail else { return };
        let Some(target) = recovery_target(detail, retry) else {
            return;
        };
        let row = text(detail, "id").to_owned();
        let title = self.tr(if retry {
            "Confirm retry"
        } else {
            "Confirm dismissal"
        });
        let action = self.tr(if retry {
            "Retry delivery"
        } else {
            "Dismiss message"
        });
        let description = self.tr(if retry {
            "The agent may already have changed files or sent a reply. Continue after reviewing its work."
        } else {
            "The input will stop waiting. Existing files and sent replies remain."
        });
        let result = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(&description),
            &[self.tr("Cancel").as_str(), action.as_str()],
            cx,
        );
        cx.spawn(async move |entity, cx| {
            if result.await.ok() == Some(1) {
                let _ = entity.update(cx, |this, cx| {
                    if this.route.detail.as_ref() == Some(&row) && this.recovery.is_none() {
                        this.recovery = this.issue(Job::Recover { row, target, retry });
                        this.recovery_error = this.recovery.is_none();
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    fn tr(&self, key: &str) -> String {
        self.language.text(key)
    }

    fn time(&self, at: &Value) -> String {
        let Some(time) = at.as_i64().and_then(DateTime::<Utc>::from_timestamp_millis) else {
            return "—".into();
        };
        if let Some(zone) = self.dashboard.as_ref().and_then(|d| d.zone) {
            time.with_timezone(&zone).format("%m/%d %H:%M").to_string()
        } else {
            time.format("%m/%d %H:%M UTC").to_string()
        }
    }

    fn duration(&self, value: &Value) -> String {
        let Some(ms) = value.as_u64() else {
            return "—".into();
        };
        if ms < 1_000 {
            format!("{ms} {}", self.tr("ms"))
        } else if ms < 60_000 {
            format!("{} {}", ms / 1_000, self.tr("s"))
        } else {
            format!("{:.1} {}", ms as f64 / 60_000., self.tr("min"))
        }
    }
}

fn recovery_target(detail: &Value, retry: bool) -> Option<RecoveryTarget> {
    let recovery = &detail["recovery"];
    let key = if retry { "can_retry" } else { "can_dismiss" };
    if recovery[key] != true {
        return None;
    }
    recovery["event_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(|id| RecoveryTarget::Event(id.to_owned()))
        .or_else(|| {
            recovery["message_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(|id| RecoveryTarget::Message(id.to_owned()))
        })
}

fn timezone() -> Option<chrono_tz::Tz> {
    iana_time_zone::get_timezone().ok()?.parse().ok()
}

fn offset_minutes(zone: Option<chrono_tz::Tz>) -> i32 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    match (zone, DateTime::<Utc>::from_timestamp(seconds, 0)) {
        (Some(zone), Some(time)) => time.with_timezone(&zone).offset().fix().local_minus_utc() / 60,
        _ => 0,
    }
}

fn endpoint(cfg: &Config, segments: &[&str]) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&cfg.url())?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid daemon URL"))?
        .pop_if_empty()
        .extend(std::iter::once("api").chain(segments.iter().copied()));
    Ok(url)
}

fn scope_query(scope: &Scope, zone: Option<chrono_tz::Tz>) -> Vec<(&'static str, String)> {
    let mut query = vec![
        ("range", scope.range.to_owned()),
        ("tz_offset_minutes", offset_minutes(zone).to_string()),
    ];
    for (key, value) in [("project_id", &scope.project), ("chat_id", &scope.chat)] {
        if !value.is_empty() {
            query.push((key, value.clone()));
        }
    }
    query
}

async fn json_response(request: reqwest::RequestBuilder) -> anyhow::Result<Value> {
    let mut response = request.send().await?.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
            "Activity response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

async fn overview(
    cfg: &Config,
    http: &reqwest::Client,
    query: &[(&'static str, String)],
) -> anyhow::Result<Value> {
    json_response(
        http.get(endpoint(cfg, &["panel", "overview"])?)
            .bearer_auth(&cfg.token)
            .query(query),
    )
    .await
}

pub(crate) async fn load_overview(data: &Path, http: &reqwest::Client) -> anyhow::Result<Value> {
    let cfg: Config = serde_json::from_slice(&std::fs::read(data.join("config.json"))?)?;
    let scope = Scope {
        range: "24h",
        ..Scope::default()
    };
    overview(&cfg, http, &scope_query(&scope, timezone())).await
}

async fn fetch(data: &Path, http: &reqwest::Client, job: &Job) -> anyhow::Result<Response> {
    let cfg: Config = serde_json::from_slice(&std::fs::read(data.join("config.json"))?)?;
    match job {
        Job::Dashboard(scope, cursor) => {
            let zone = timezone();
            let mut query = scope_query(scope, zone);
            let snapshot = if cursor.is_none() {
                overview(&cfg, http, &query).await?
            } else {
                Value::Null
            };
            query.extend([
                ("filter", scope.filter.query().to_owned()),
                ("limit", PAGE_SIZE.to_owned()),
            ]);
            if !scope.search.is_empty() {
                query.push(("q", scope.search.clone()));
            }
            if let Some((start, end)) = scope.bucket {
                query.push(("bucket_start_ms", start.to_string()));
                query.push(("bucket_end_ms", end.to_string()));
            }
            if let Some(cursor) = cursor {
                query.push(("before_ms", cursor.time.to_string()));
                query.push(("before_id", cursor.id.clone()));
            }
            let mut messages = json_response(
                http.get(endpoint(&cfg, &["panel", "messages"])?)
                    .bearer_auth(&cfg.token)
                    .query(&query),
            )
            .await?;
            let next = messages["next_before_ms"]
                .as_i64()
                .zip(messages["next_before_id"].as_str())
                .map(|(time, id)| Cursor {
                    time,
                    id: id.to_owned(),
                });
            let mut items = messages["items"].take();
            Ok(Response::Dashboard(Dashboard {
                overview: snapshot,
                messages: items.as_array_mut().map(std::mem::take).unwrap_or_default(),
                next,
                zone,
            }))
        }
        Job::Detail(id) => {
            let value = json_response(
                http.get(endpoint(&cfg, &["panel", "messages", id])?)
                    .bearer_auth(&cfg.token),
            )
            .await?;
            anyhow::ensure!(value["id"] == *id, "Activity record changed");
            Ok(Response::Detail(value))
        }
        Job::Recover { target, retry, .. } => {
            let action = if *retry { "retry" } else { "dismiss" };
            let request = match target {
                RecoveryTarget::Event(id) => http
                    .post(endpoint(&cfg, &["events", id, "resolve"])?)
                    .json(&json!({ "action": action })),
                RecoveryTarget::Message(id) => {
                    http.post(endpoint(&cfg, &["messages", id, action])?)
                }
            };
            request
                .bearer_auth(&cfg.token)
                .send()
                .await?
                .error_for_status()?;
            Ok(Response::Recovered)
        }
    }
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

fn list(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn number(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or_default()
}

fn count(value: &Value, key: &str) -> String {
    value[key]
        .as_u64()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".into())
}

fn card(p: Palette) -> Div {
    div()
        .min_w_0()
        .bg(rgb(p.surface))
        .border_1()
        .border_color(rgb(p.line))
        .rounded_lg()
}

fn muted(p: Palette, value: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .text_color(rgb(p.muted))
        .child(value.into())
}

fn small_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id).small().compact().label(label)
}

fn state_color(state: &str, p: Palette) -> u32 {
    match state {
        "failed_or_uncertain" => p.error,
        "stuck" | "held" | "unanswered" => p.warning,
        "closed" => p.success,
        "running" => p.accent,
        _ => p.muted,
    }
}

const STATE_KEYS: &[&str] = &[
    "panel_failed_or_uncertain",
    "panel_stuck",
    "panel_held",
    "panel_unanswered",
    "panel_running",
    "panel_closed",
    "panel_control",
    "panel_outbound",
];

const REASON_KEYS: &[&str] = &[
    "panel_delivery_failure",
    "panel_worker_failure",
    "panel_manager_failure",
    "panel_routing_timeout",
    "panel_resource_timeout",
    "panel_manager_queue_timeout",
    "panel_native_queue_timeout",
    "panel_paused",
    "panel_takeover",
    "panel_cli_switch",
    "panel_no_reply",
    "panel_receipt_only",
    "panel_routing_pending",
    "panel_preparing_resources",
    "panel_manager_work",
    "panel_worker_work",
    "panel_pending_delivery",
    "panel_replied",
    "panel_control",
    "panel_dismissed",
    "panel_outbound",
];

const TIMELINE_KEYS: &[&str] = &[
    "panel_inbound",
    "panel_routing",
    "panel_worker_event",
    "panel_worker_run",
    "panel_manager_input",
    "panel_manager_run",
    "panel_control",
    "panel_reply",
    "panel_receipt",
    "panel_notice",
];

fn label_key(code: &str, keys: &[&'static str]) -> &'static str {
    keys.iter()
        .copied()
        .find(|label| label.strip_prefix("panel_") == Some(code))
        .unwrap_or("panel_outbound")
}

fn evidence_status(code: &str) -> &'static str {
    match code {
        "pending" | "skipped_pending" => "Waiting",
        "acknowledged" => "Recorded",
        "delivered" | "sent" => "Delivered",
        "dismissed" => "Dismissed",
        "consumed" => "Consumed",
        "complete" | "completed" | "settled" | "resolved" => "Finished",
        "processing" | "working" | "managed" | "requested" | "submitting" | "submitted" => {
            "In progress"
        }
        "building" => "Preparing",
        "card_ready" => "Card ready",
        "accepted" => "Accepted",
        "applying" => "Applying",
        "interrupted" => "Interrupted",
        "superseded" => "Superseded",
        "queued" => "queued",
        "running" => "running",
        "stopped" => "stopped",
        "uncertain" => "uncertain",
        "failed" => "failed",
        "cancelled" => "cancelled",
        "deferred" => "deferred",
        _ => "Unknown status",
    }
}

fn admission_label(admission: &str) -> Option<&'static str> {
    match admission {
        "own_mention" => Some("Bot mention"),
        "mention_all" => Some("Everyone"),
        "topic" => Some("Topic reply"),
        _ => None,
    }
}

impl ObservationPanel {
    fn toolbar(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let all_projects = self.tr("All projects");
        let project_label = self
            .projects
            .iter()
            .find(|(id, _)| id == &self.scope.project)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| all_projects.clone());
        let selection = self.scope.project.clone();
        let mut options = vec![(String::new(), all_projects)];
        options.extend(self.projects.clone());
        let entity = cx.entity().downgrade();
        let projects =
            small_button("activity-scope", project_label).dropdown_menu(move |menu, _, _| {
                options.iter().fold(menu, |menu, (id, label)| {
                    let selected = id == &selection;
                    let id = id.clone();
                    let entity = entity.clone();
                    menu.item(
                        PopupMenuItem::new(label.clone())
                            .checked(selected)
                            .on_click(move |_, _, cx| {
                                let _ = entity.update(cx, |this, cx| {
                                    this.scope.project.clone_from(&id);
                                    this.scope.chat.clear();
                                    this.scope.bucket = None;
                                    this.refresh(cx);
                                });
                            }),
                    )
                })
            });
        let period = ["24h", "7d", "30d"].into_iter().map(|range| {
            small_button(
                SharedString::from(format!("activity-range-{range}")),
                self.tr(range),
            )
            .when(self.scope.range == range, |button| button.primary())
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.scope.range != range {
                    this.scope.range = range;
                    this.scope.bucket = None;
                    this.refresh(cx);
                }
            }))
        });
        let chat_title = self
            .dashboard
            .as_ref()
            .and_then(|d| {
                list(&d.overview["chats"])
                    .iter()
                    .find(|chat| text(chat, "chat_id") == self.scope.chat)
            })
            .map(|chat| self.chat_title(chat));
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(projects)
            .when(!self.scope.chat.is_empty(), |toolbar| {
                toolbar.child(
                    small_button(
                        "activity-clear-chat",
                        format!("{} ×", chat_title.unwrap_or_else(|| self.tr("Chat"))),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.scope.chat.clear();
                        this.refresh(cx);
                    })),
                )
            })
            .child(div().flex_1())
            .children(period)
            .child(
                ActionIcon::Refresh
                    .button("activity-refresh", self.tr("Refresh activity"), p.muted)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
    }

    fn metrics(&self, p: Palette, overview: &Value, cx: &mut Context<Self>) -> Div {
        let k = &overview["kpis"];
        let breakdown = &k["needs_action_breakdown"];
        let needs = div()
            .id("activity-needs-action")
            .cursor_pointer()
            .hover(|s| s.bg(rgb(p.hover)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.set_filter(Filter::NeedsAction, cx);
            }));
        let inbound = format!(
            "{} {} · {} {}",
            count(k, "group_in"),
            self.tr("Group"),
            count(k, "p2p_in"),
            self.tr("Direct")
        );
        let replies = format!(
            "{} {} · {} {}",
            count(k, "bot_out_reply"),
            self.tr("Replies"),
            count(k, "bot_out_receipt_or_notice"),
            self.tr("Receipts and notices")
        );
        let response = format!(
            "{} {} · {} {}",
            self.tr("P90"),
            self.duration(&k["p90_first_reply_ms"]),
            self.tr("Worker queue"),
            self.duration(&k["median_worker_queue_ms"])
        );
        let anomalies = [
            ("stuck_resource", "panel_resource_timeout"),
            ("held_switch_or_pause", "panel_held"),
            ("failed_or_uncertain", "panel_failed_or_uncertain"),
            ("unanswered_closed", "panel_unanswered"),
            ("routing_pending", "panel_routing_pending"),
            ("stuck_queue", "Queued too long"),
        ]
        .into_iter()
        .filter_map(|(field, key)| {
            let n = number(breakdown, field);
            (n > 0).then(|| format!("{} {}", n, self.tr(key)))
        })
        .collect::<Vec<_>>()
        .join(" · ");
        div()
            .flex()
            .gap_2()
            .child(metric_card(
                p,
                self.tr("User messages"),
                count(k, "user_in"),
                inbound,
                p.foreground,
            ))
            .child(card(p).flex_1().child(needs.rounded_lg().child(metric(
                p,
                self.tr("Needs action"),
                count(k, "needs_action"),
                anomalies,
                p.warning,
            ))))
            .child(metric_card(
                p,
                self.tr("Sent messages"),
                count(k, "bot_out_sent"),
                replies,
                p.foreground,
            ))
            .child(metric_card(
                p,
                self.tr("Median first reply"),
                self.duration(&k["median_first_reply_ms"]),
                response,
                p.foreground,
            ))
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        if self.scope.filter != filter {
            self.scope.filter = filter;
            self.refresh(cx);
        }
    }

    fn chart(&self, p: Palette, overview: &Value, cx: &mut Context<Self>) -> Div {
        let series = list(&overview["series"]);
        let measure = |bucket: &Value| {
            if self.show_duration {
                number(bucket, "median_first_reply_ms")
            } else {
                number(bucket, "user_in")
                    .max(number(bucket, "out_reply") + number(bucket, "out_receipt_or_notice"))
            }
        };
        let peak = series.iter().map(measure).max().unwrap_or(0).max(1);
        let height = |value: u64| {
            if value == 0 {
                0.
            } else {
                (60. * value as f32 / peak as f32).max(2.)
            }
        };
        let legend = div()
            .flex()
            .items_center()
            .gap_3()
            .when(!self.show_duration, |legend| {
                legend
                    .child(legend_item(p, p.accent, self.tr("User messages")))
                    .child(legend_item(p, p.success, self.tr("Replies")))
                    .child(legend_item(p, p.muted, self.tr("Receipts and notices")))
            });
        let interval = if series.len() == 7 { 1 } else { 6 };
        let cols = series.iter().enumerate().map(|(index, bucket)| {
            let label = text(bucket, "label").to_owned();
            let key = format!("activity-bucket-{index}");
            let start = bucket["bucket_start_ms"].as_i64();
            let end = bucket["bucket_end_ms"].as_i64();
            let bucket_time = start.zip(end);
            let selected = self.scope.bucket.is_some() && self.scope.bucket == bucket_time;
            let anomalies = number(bucket, "anomaly_count");
            let detail = format!(
                "{} · {} {} · {} {} · {} {} · {} {} · {} {}",
                label,
                self.tr("User messages"),
                count(bucket, "user_in"),
                self.tr("Replies"),
                count(bucket, "out_reply"),
                self.tr("Receipts and notices"),
                count(bucket, "out_receipt_or_notice"),
                self.tr("Needs action"),
                anomalies,
                self.tr("Median first reply"),
                self.duration(&bucket["median_first_reply_ms"]),
            );
            let stacks = div()
                .w_full()
                .h(px(63.))
                .flex()
                .items_end()
                .justify_center()
                .gap(px(2.))
                .when(self.show_duration, |stacks| {
                    stacks.child(
                        div()
                            .w_1_2()
                            .h(px(height(measure(bucket))))
                            .bg(rgb(p.accent)),
                    )
                })
                .when(!self.show_duration, |stacks| {
                    stacks
                        .child(
                            div()
                                .flex_1()
                                .h(px(height(number(bucket, "user_in"))))
                                .bg(rgb(p.accent)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .h(px(height(number(bucket, "out_receipt_or_notice"))))
                                        .bg(rgb(p.muted)),
                                )
                                .child(
                                    div()
                                        .h(px(height(number(bucket, "out_reply"))))
                                        .bg(rgb(p.success)),
                                ),
                        )
                });
            div()
                .id(SharedString::from(key))
                .relative()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .rounded_sm()
                .cursor_pointer()
                .hover(|s| s.bg(rgb(p.hover)))
                .when(selected, |col| col.bg(rgb(p.selected)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(time) = bucket_time {
                        this.scope.bucket = if selected { None } else { Some(time) };
                        this.refresh(cx);
                    }
                }))
                .tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx))
                .child(
                    div()
                        .flex()
                        .justify_center()
                        .h(px(5.))
                        .when(anomalies > 0, |row| {
                            row.child(div().size(px(5.)).rounded_full().bg(rgb(p.warning)))
                        }),
                )
                .child(stacks)
                .child(
                    div()
                        .relative()
                        .h(px(14.))
                        .when(index % interval == 0, |axis| {
                            axis.child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top_0()
                                    .whitespace_nowrap()
                                    .text_size(px(10.))
                                    .text_color(rgb(p.muted))
                                    .child(label),
                            )
                        }),
                )
        });
        card(p)
            .px_3()
            .py_2()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::MEDIUM)
                            .child(self.tr("Message trends")),
                    )
                    .child(
                        small_button("activity-volume", self.tr("Volume"))
                            .when(!self.show_duration, |b| b.primary())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_duration = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        small_button("activity-latency", self.tr("Response time"))
                            .when(self.show_duration, |b| b.primary())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_duration = true;
                                cx.notify();
                            })),
                    ),
            )
            .child(legend)
            .child(div().flex().gap(px(2.)).min_w_0().children(cols))
    }

    fn chat_title(&self, chat: &Value) -> String {
        let name = text(chat, "display_name");
        if chat["name_missing"] != true && !name.is_empty() {
            return name.to_owned();
        }
        let id = text(chat, "chat_id");
        let tail = id.char_indices().rev().nth(3).map_or(id, |(i, _)| &id[i..]);
        let kind = self.tr(if chat["kind"] == "p2p" {
            "Direct"
        } else {
            "Group"
        });
        format!("{kind} · …{tail}")
    }

    fn bot_chip(&self, p: Palette, bot: &Value, key: String) -> Stateful<Div> {
        let ws = text(bot, "ws_status");
        let identity = text(bot, "identity_status");
        let (status, color) = if bot["bound"] != true {
            ("Bot unbound", p.muted)
        } else if !text(bot, "ws_error").is_empty() || matches!(ws, "disconnected" | "reconnecting")
        {
            ("Bot disconnected", p.error)
        } else if matches!(identity, "pending" | "failed") {
            ("Checking group identity", p.warning)
        } else if matches!(ws, "receiving" | "awaiting_events") {
            ("Bot connected", p.success)
        } else {
            ("Bot status unavailable", p.muted)
        };
        let description = format!(
            "{} · {} · {}",
            text(bot, "project_name"),
            self.tr(status),
            text(bot, "app_id")
        );
        div()
            .id(SharedString::from(format!("{key}-chip")))
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .text_size(px(11.))
            .rounded_sm()
            .px_1()
            .bg(rgb(p.pill))
            .tooltip(move |window, cx| Tooltip::new(description.clone()).build(window, cx))
            .child(crate::agent_tree::cli_icon(text(bot, "agent"), key, p))
            .child(div().size(px(6.)).rounded_full().bg(rgb(color)))
    }

    fn chat_row(
        &self,
        p: Palette,
        chat: &Value,
        group: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = text(chat, "chat_id").to_owned();
        let key = format!("activity-chat-{}-{id}", group.unwrap_or(""));
        let active = id == self.scope.chat;
        let bots = list(&chat["bots"])
            .iter()
            .filter(|bot| group.is_none_or(|project| bot["project_id"] == project))
            .collect::<Vec<_>>();
        let project = group.map(str::to_owned);
        let mut chips = div().flex().flex_wrap().gap_1().items_center();
        for (index, bot) in bots.iter().take(3).enumerate() {
            chips = chips.child(self.bot_chip(p, bot, format!("{key}-bot-{index}")));
        }
        if bots.len() > 3 {
            let more = bots
                .iter()
                .map(|bot| format!("{} · {}", text(bot, "project_name"), text(bot, "app_id")))
                .collect::<Vec<_>>()
                .join("\n");
            chips = chips.child(
                div()
                    .id(SharedString::from(format!("{key}-extra")))
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(format!("+{}", bots.len() - 3))
                    .tooltip(move |window, cx| Tooltip::new(more.clone()).build(window, cx)),
            );
        }
        let known_chat = if chat["kind"] == "p2p" {
            "Direct"
        } else {
            "Group"
        };
        let activity = format!(
            "{} · {} · {}",
            self.tr(known_chat),
            count(chat, "message_count_in_range"),
            self.time(&chat["last_seen_at"])
        );
        div()
            .id(SharedString::from(key))
            .px_2()
            .py_2()
            .flex()
            .flex_col()
            .gap_1()
            .min_w_0()
            .border_b_1()
            .border_color(rgb(p.line))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(p.hover)))
            .when(active, |row| row.bg(rgb(p.selected)))
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(project) = &project {
                    this.scope.project.clone_from(project);
                }
                this.scope.chat = if active { String::new() } else { id.clone() };
                this.refresh(cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(self.chat_title(chat)),
                    )
                    .when(number(chat, "unhandled_count") > 0, |row| {
                        row.child(
                            div()
                                .text_color(rgb(p.warning))
                                .text_size(px(11.))
                                .child(count(chat, "unhandled_count")),
                        )
                    }),
            )
            .child(muted(p, activity))
            .child(chips)
    }

    fn conversations(&self, p: Palette, overview: &Value, cx: &mut Context<Self>) -> Div {
        let chats = list(&overview["chats"]);
        let mut rows = div()
            .id("activity-chat-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        if self.by_project {
            let mut grouped: BTreeMap<(&str, &str), Vec<&Value>> = BTreeMap::new();
            for chat in chats {
                for bot in list(&chat["bots"]) {
                    grouped
                        .entry((text(bot, "project_name"), text(bot, "project_id")))
                        .or_default()
                        .push(chat);
                }
            }
            for ((name, id), chats) in grouped {
                rows = rows
                    .child(muted(p, name.to_owned()).px_2().py_1().bg(rgb(p.pill)))
                    .children(
                        chats
                            .into_iter()
                            .map(|chat| self.chat_row(p, chat, Some(id), cx)),
                    );
            }
        } else {
            rows = rows.children(chats.iter().map(|chat| self.chat_row(p, chat, None, cx)));
        }
        card(p)
            .w(px(240.))
            .max_w(relative(0.37))
            .flex()
            .flex_col()
            .min_h_0()
            .child(
                div()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(self.tr("Groups and direct chats")),
                    )
                    .child(muted(p, self.tr("Active conversations only")))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                small_button("activity-by-chat", self.tr("By chat"))
                                    .when(!self.by_project, |b| b.primary())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.by_project = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                small_button("activity-by-project", self.tr("By project"))
                                    .when(self.by_project, |b| b.primary())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.by_project = true;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when(chats.is_empty(), |view| {
                view.child(muted(p, self.tr("No conversations in this scope")).p_2())
            })
            .child(rows)
    }

    fn state_badge(&self, p: Palette, value: &Value) -> Div {
        let state = text(value, "closure_state");
        let color = state_color(state, p);
        div()
            .flex_shrink_0()
            .rounded_sm()
            .px_1()
            .bg(rgb(p.pill))
            .text_color(rgb(color))
            .text_size(px(11.))
            .font_weight(FontWeight::MEDIUM)
            .child(self.tr(label_key(state, STATE_KEYS)))
    }

    fn preview_badges(&self, p: Palette, row: &Value) -> Div {
        let b = &row["badges"];
        let mut badges = div()
            .flex()
            .flex_wrap()
            .gap_1()
            .items_center()
            .text_size(px(11.));
        let count = number(b, "image_count");
        if count > 0 {
            badges = badges.child(muted(p, format!("{} ×{count}", self.tr("Images"))));
        }
        let names = list(&b["file_names"]);
        if let Some(file) = names.first().and_then(Value::as_str) {
            badges = badges.child(muted(p, format!("{} · {file}", self.tr("Files"))).truncate());
        }
        for (field, label) in [
            ("topic_reply", "Topic reply"),
            ("mention_all", "Everyone"),
            ("card", "Card"),
            ("control", "Control"),
        ] {
            if b[field] == true {
                badges = badges.child(muted(p, self.tr(label)));
            }
        }
        if b["resource_failed"] == true {
            badges = badges.child(
                div()
                    .text_color(rgb(p.error))
                    .child(self.tr("Download failed")),
            );
        }
        badges
    }

    fn message_row(&self, p: Palette, row: &Value, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = text(row, "id").to_owned();
        let keyboard_id = id.clone();
        let title = text(row, "preview").to_owned();
        let reply = &row["reply_summary"];
        let reason = if row["direction"] == "out" {
            self.tr(evidence_status(text(row, "status")))
        } else {
            self.tr(label_key(text(row, "closure_reason"), REASON_KEYS))
        };
        let mut summary = reason;
        if number(reply, "count") > 0 {
            summary.push_str(&format!(
                " · {} {} · {} · {}",
                count(reply, "count"),
                self.tr("Replies"),
                self.duration(&reply["first_reply_ms"]),
                text(reply, "preview")
            ));
        }
        let source = [
            text(row, "project_name"),
            text(row, "chat_name"),
            &self.time(&row["created_at"]),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        div()
            .id(SharedString::from(format!("activity-row-{id}")))
            .focusable()
            .tab_index(0)
            .px_2()
            .py_2()
            .flex()
            .flex_col()
            .gap_1()
            .border_b_1()
            .border_color(rgb(p.line))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(p.hover)))
            .focus(|s| s.bg(rgb(p.selected)))
            .on_click(cx.listener(move |this, _, window, cx| this.open(id.clone(), window, cx)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.open(keyboard_id.clone(), window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.state_badge(p, row))
                    .child(div().flex_1().min_w_0().truncate().child(title)),
            )
            .child(muted(p, summary).truncate())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_between()
                    .gap_1()
                    .child(self.preview_badges(p, row))
                    .child(muted(p, source)),
            )
    }

    fn message_stream(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let mut stream = div()
            .id("activity-message-scroll")
            .track_scroll(&self.message_scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        if let Some(dashboard) = &self.dashboard {
            stream = stream.children(
                dashboard
                    .messages
                    .iter()
                    .map(|row| self.message_row(p, row, cx)),
            );
            if let Some(next) = &dashboard.next {
                let next = next.clone();
                stream = stream.child(
                    small_button("activity-more", self.tr("More messages"))
                        .m_2()
                        .disabled(self.pending.is_some() || self.error)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pending =
                                this.issue(Job::Dashboard(this.scope.clone(), Some(next.clone())));
                            this.error = this.pending.is_none();
                            cx.notify();
                        })),
                );
            }
            if dashboard.messages.is_empty() {
                stream = stream.child(muted(p, self.tr("No messages in this scope")).p_2());
            }
        }
        let filters = [
            (Filter::All, "All messages"),
            (Filter::NeedsAction, "Needs action"),
            (Filter::Outbound, "Include outbound"),
        ]
        .into_iter()
        .map(|(filter, label)| {
            small_button(
                SharedString::from(format!("activity-filter-{}", filter.query())),
                self.tr(label),
            )
            .when(filter == self.scope.filter, |b| b.primary())
            .on_click(cx.listener(move |this, _, _, cx| this.set_filter(filter, cx)))
        });
        card(p)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(self.tr("Message stream")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .flex_wrap()
                            .children(filters),
                    )
                    .child(Input::new(&self.search).small()),
            )
            .child(stream)
    }
}

fn metric_card(p: Palette, title: String, value: String, note: String, color: u32) -> Div {
    card(p).flex_1().child(metric(p, title, value, note, color))
}

fn metric(p: Palette, title: String, value: String, note: String, color: u32) -> Div {
    div()
        .p_2()
        .flex()
        .flex_col()
        .gap_1()
        .child(muted(p, title).truncate())
        .child(
            div()
                .text_size(px(23.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(color))
                .child(value),
        )
        .child(muted(p, note).truncate())
}

fn legend_item(p: Palette, color: u32, text: String) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(div().size(px(7.)).rounded_sm().bg(rgb(color)))
        .child(muted(p, text))
}

impl ObservationPanel {
    fn navigate_action(
        &self,
        detail: &Value,
        context: bool,
        cx: &mut Context<Self>,
    ) -> Option<Button> {
        let nav = self.navigate.clone()?;
        let project = text(detail, "project_id").to_owned();
        if !self.projects.iter().any(|(id, _)| id == &project) {
            return None;
        }
        let label = if context {
            "View model context"
        } else {
            "Open project workspace"
        };
        Some(
            small_button(("activity-navigate", usize::from(context)), self.tr(label)).on_click(
                cx.listener(move |_, _, window, cx| nav(project.clone(), context, window, cx)),
            ),
        )
    }

    fn detail_view(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let back = small_button(
            "activity-back",
            format!("‹ {}", self.tr("Back to message stream")),
        )
        .on_click(cx.listener(|this, _, window, cx| this.back(window, cx)));
        let header = div().flex().items_center().gap_2().child(back);
        let status = self.request_status(p, true);
        let mut root = div().flex().flex_col().gap_2().min_h_0().flex_1();
        let Some(detail) = &self.detail else {
            return root.child(header).child(status);
        };
        let reason = self.tr(label_key(text(detail, "closure_reason"), REASON_KEYS));
        root = root
            .child(
                header
                    .child(self.state_badge(p, detail))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_color(rgb(p.muted))
                            .child(reason),
                    )
                    .children(self.navigate_action(detail, false, cx))
                    .children(self.navigate_action(detail, true, cx)),
            )
            .child(status);

        let source = &detail["source"];
        let sender = &source["sender"];
        let sender_label = sender.as_str().unwrap_or_else(|| {
            let name = text(sender, "name");
            if name.is_empty() {
                text(sender, "open_id")
            } else {
                name
            }
        });
        let admission =
            admission_label(text(source, "group_admission")).map(|label| self.tr(label));
        let mut metadata = div().flex().flex_col().gap_1();
        for (label, value) in [
            ("Project", text(detail, "project_name")),
            ("Chat", text(detail, "chat_name")),
            ("Chat", text(source, "chat_id")),
            ("Sender", sender_label),
            ("Message ID", text(source, "message_id")),
            ("Thread", text(source, "thread_id")),
            ("Root message", text(source, "root_id")),
            ("Parent message", text(source, "parent_id")),
            ("Admission", admission.as_deref().unwrap_or_default()),
        ] {
            if !value.is_empty() {
                metadata = metadata.child(muted(p, format!("{} · {value}", self.tr(label))));
            }
        }
        let body = detail["content"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if detail["content"].is_null() {
                    String::new()
                } else {
                    detail["content"].to_string()
                }
            });
        let resources = list(&detail["resources"]);
        let timeline = list(&detail["timeline"]);
        let mut inner = div()
            .id("activity-detail-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .child(section(p, self.tr("Source"), metadata))
            .child(section(p, self.tr("Message content"), div().child(body)));
        if !resources.is_empty() {
            let mut rows = div().flex().flex_col().gap_2();
            for item in resources {
                let name = text(item, "file_name");
                let title = [name, text(item, "file_key"), text(item, "resource_key")]
                    .into_iter()
                    .find(|name| !name.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| self.tr("Attachments"));
                let path = text(item, "local_path");
                let failed = !text(item, "error").is_empty();
                let mut row = div().flex().flex_col().gap_1().child(div().child(title));
                if !path.is_empty() {
                    row = row.child(muted(p, path.to_owned()));
                } else {
                    row = row.child(muted(p, self.tr("Resource not downloaded")));
                }
                if failed {
                    row = row.child(
                        div()
                            .text_color(rgb(p.error))
                            .child(self.tr("Attachment download failed")),
                    );
                }
                let width = number(item, "width");
                let height = number(item, "height");
                if width > 0 && height > 0 {
                    row = row.child(muted(p, format!("{width} × {height}")));
                }
                let bytes = item["bytes"]
                    .as_u64()
                    .map(|n| format!("{n} {}", self.tr("bytes")));
                let metadata = [
                    text(item, "media_type").to_owned(),
                    bytes.unwrap_or_default(),
                    text(item, "sha256").to_owned(),
                ]
                .into_iter()
                .filter(|word| !word.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
                if !metadata.is_empty() {
                    row = row.child(muted(p, metadata));
                }
                rows = rows.child(row);
            }
            if let Some(support) = detail["supports_images"].as_bool() {
                rows = rows.child(muted(
                    p,
                    format!(
                        "{} · {}",
                        self.tr("Image support setting"),
                        self.tr(if support {
                            "Image input supported"
                        } else {
                            "Image input unavailable"
                        })
                    ),
                ));
            }
            inner = inner.child(section(p, self.tr("Attachments"), rows));
        }
        if !timeline.is_empty() {
            let items = timeline.iter().map(|item| {
                let kind = self.tr(label_key(text(item, "kind"), TIMELINE_KEYS));
                let at = self.time(&item["at_ms"]);
                let state = self.tr(evidence_status(text(item, "status")));
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .flex_shrink_0()
                            .bg(rgb(p.accent)),
                    )
                    .child(muted(p, at).w(px(95.)).flex_shrink_0())
                    .child(div().flex_1().min_w_0().child(kind))
                    .child(muted(p, state))
            });
            inner = inner.child(section(
                p,
                self.tr("Timeline"),
                div().flex().flex_col().gap_1().children(items),
            ));
        }
        let recovery = [(true, "Retry delivery"), (false, "Dismiss message")]
            .into_iter()
            .filter_map(|(retry, label)| {
                recovery_target(detail, retry)?;
                Some(
                    small_button(("activity-recover", usize::from(retry)), self.tr(label))
                        .disabled(self.recovery.is_some() || self.route.pending.is_some())
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.recover(retry, window, cx)),
                        ),
                )
            });
        root.child(inner)
            .child(div().flex().gap_2().children(recovery))
            .when(self.recovery_error, |view| {
                view.child(
                    div()
                        .text_color(rgb(p.error))
                        .child(self.tr("Delivery action failed")),
                )
            })
    }

    fn request_status(&self, p: Palette, detail: bool) -> Div {
        let (pending, error) = if detail {
            (self.route.pending.is_some(), self.detail_error)
        } else {
            (self.pending.is_some(), self.error)
        };
        if error {
            div()
                .text_size(px(12.))
                .text_color(rgb(p.error))
                .child(self.tr("Activity is unavailable"))
        } else if pending {
            muted(p, self.tr("Updating activity…"))
        } else {
            div()
        }
    }
}

fn section(p: Palette, title: String, content: Div) -> Div {
    card(p)
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
        .child(content)
}

impl Render for ObservationPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::ui_theme::palette(cx);
        if self.search_language != self.language {
            self.search_language = self.language;
            let placeholder = self.tr("Search messages");
            self.search.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx);
            });
        }
        let root = div()
            .id("activity-panel")
            .size_full()
            .min_w_0()
            .min_h_0()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && this.route.detail.is_some() {
                    this.back(window, cx);
                    cx.stop_propagation();
                }
            }));
        if self.route.detail.is_some() {
            return root.child(self.detail_view(p, cx));
        }
        let empty = Value::Null;
        let overview = self.dashboard.as_ref().map_or(&empty, |d| &d.overview);
        root.child(self.toolbar(p, cx))
            .child(self.metrics(p, overview, cx))
            .child(self.chart(p, overview, cx))
            .child(self.request_status(p, false))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .child(self.conversations(p, overview, cx))
                    .child(self.message_stream(p, cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{Job, REASON_KEYS, Route, STATE_KEYS, Scope, TIMELINE_KEYS, Ticket, accepts};
    use crate::locale::Language;

    #[test]
    fn request_acceptance_requires_current_serial_and_scope() {
        let scope = Scope {
            range: "24h",
            ..Scope::default()
        };
        let old = Ticket {
            serial: 1,
            job: Job::Dashboard(scope.clone(), None),
        };
        let current = Ticket {
            serial: 2,
            job: Job::Dashboard(scope, None),
        };
        assert!(!accepts(Some(&current), &old));
        assert!(accepts(Some(&current), &current));
        assert!(!accepts(None, &current));
        let stale = Ticket {
            serial: 2,
            job: Job::Dashboard(
                Scope {
                    range: "7d",
                    ..Scope::default()
                },
                None,
            ),
        };
        assert!(!accepts(Some(&current), &stale));
    }

    #[test]
    fn opening_and_escaping_details_invalidates_pending_response() {
        let mut route = Route::default();
        route.push("first".into());
        let first = Ticket {
            serial: 1,
            job: Job::Detail("first".into()),
        };
        route.pending = Some(first.clone());
        assert!(route.accepts(&first));
        assert!(route.back());
        assert!(route.detail.is_none());
        assert!(!route.accepts(&first));
        route.push("next".into());
        let next = Ticket {
            serial: 2,
            job: Job::Detail("next".into()),
        };
        route.pending = Some(next.clone());
        assert!(!route.accepts(&first));
        assert!(route.accepts(&next));
    }

    #[test]
    fn panel_evidence_codes_and_primary_navigation_are_localized() {
        for key in STATE_KEYS.iter().chain(REASON_KEYS).chain(TIMELINE_KEYS) {
            let en = Language::En.text(key);
            let zh = Language::Zh.text(key);
            assert_ne!(en, *key, "{key} missing in English");
            assert_ne!(zh, *key, "{key} missing in Simplified Chinese");
            assert_ne!(en, zh, "{key} uses the same label in both locales");
        }
        for key in [
            "Activity",
            "Needs action",
            "Message trends",
            "Message stream",
            "Back to message stream",
        ] {
            assert_ne!(Language::Zh.text(key), key);
        }
    }
}
