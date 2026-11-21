use crate::{
    App,
    db::{self, ProjectInput},
    feishu, herdr, work,
};
use anyhow::{Context, bail};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;

pub struct ApiError(pub anyhow::Error);
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(e)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            if self.0.to_string().starts_with("Manager settings changed") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            },
            Json(json!({"error":self.0.to_string()})),
        )
            .into_response()
    }
}
type ApiResult = std::result::Result<Json<Value>, ApiError>;
// API 7 adds managed Pi child evidence and conservative retirement semantics.
// API 6 services cannot satisfy this contract, even when HTTP shapes overlap.
pub const PROTOCOL: u64 = 7;

pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/state", get(state))
        .route("/service/shutdown", post(shutdown_service))
        .route("/service/activation/prepare", post(activation_prepare))
        .route("/service/activation/commit", post(activation_commit))
        .route("/service/activation/abort", post(activation_abort))
        .route("/service/activation/status", get(activation_status))
        .route(
            "/service/activation/ingest",
            post(activation_ingest).layer(DefaultBodyLimit::max(3 * 1024 * 1024)),
        )
        .route("/settings", get(settings))
        .route("/conversations", get(conversations))
        .route("/projects/{id}/autonomy", get(autonomy_settings).put(update_autonomy))
        .route("/proposals", get(proposals))
        .route("/proposals/{id}/decide", post(decide_proposal))
        .route("/panel/overview", get(panel_overview))
        .route("/panel/messages", get(panel_messages))
        .route("/panel/messages/{id}", get(panel_message_detail))
        .route("/projects", post(create_project))
        .route(
            "/projects/{id}",
            get(project).put(update_project).delete(delete_project),
        )
        .route("/projects/{id}/start", post(start))
        .route(
            "/projects/{id}/bot",
            get(project_bot).put(save_bot).delete(disconnect_bot),
        )
        .route("/projects/{id}/bot/check", post(check_bot))
        .route("/projects/{id}/resume", post(resume))
        .route(
            "/projects/{id}/recover-after-update",
            post(recover_after_update),
        )
        .route("/projects/{id}/agent", post(switch_agent))
        .route("/projects/{id}/agent/standby", post(switch_standby_agent))
        .route("/projects/{id}/agent/transition", post(transition_agent))
        .route("/projects/{id}/agent/repair", post(repair_agent))
        .route(
            "/projects/{id}/manager",
            get(manager_config).put(manager_save),
        )
        .route("/manager/provider", get(manager_provider).put(manager_save_provider))
        .route("/projects/{id}/agent/mode", post(manager_mode))
        .route("/projects/{id}/manager/context", get(manager_context))
        .route("/projects/{id}/models", get(project_models))
        .route("/projects/{id}/model", post(select_project_model))
        .route("/projects/{id}/documents/{document}", get(project_document))
        .route("/projects/{id}/stop", post(stop))
        .route("/projects/{id}/pause", post(pause))
        .route("/projects/{id}/debug", post(project_debug))
        .route("/projects/{id}/show", post(show))
        .route("/projects/{id}/screen", get(screen))
        .route("/projects/{id}/terminal", get(show))
        .route("/projects/{id}/takeover", post(takeover))
        .route("/projects/{id}/work", get(work_state))
        .route("/projects/{id}/timers", get(timers).post(create_timer))
        .route(
            "/timers/{id}",
            axum::routing::put(update_timer).delete(delete_timer),
        )
        .route("/timers/{id}/pause", post(pause_timer))
        .route("/events/{id}/resolve", post(resolve_event))
        .route("/projects/{id}/messages", post(inject))
        .route("/projects/{id}/outbox", post(outbox))
        .route("/projects/{id}/register", post(register))
        .route(
            "/projects/{id}/upload",
            post(upload).layer(DefaultBodyLimit::max(
                feishu::MAX_FILE as usize + 1024 * 1024,
            )),
        )
        .route("/messages/{id}/retry", post(retry))
        .route("/messages/{id}/dismiss", post(dismiss))
        .route_layer(middleware::from_fn_with_state(app.clone(), admission))
        .route_layer(middleware::from_fn_with_state(app.clone(), auth));
    Router::new()
        .nest("/api", api)
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn(headers))
        .with_state(app)
}

fn equal(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}
#[derive(Clone)]
struct SessionAuthority {
    project: String,
    generation: String,
}
type Session = Option<Extension<SessionAuthority>>;
fn authority(session: Session) -> Option<SessionAuthority> {
    session.map(|Extension(value)| value)
}
/// Recheck at the database operation, after any asynchronous handler work.
fn authorize_generation(
    c: &rusqlite::Connection,
    authority: &Option<SessionAuthority>,
    project: &str,
) -> anyhow::Result<()> {
    if let Some(authority) = authority {
        anyhow::ensure!(authority.project == project, "The project authority changed");
        let parent = db::project_by_key(c,project)?;
        anyhow::ensure!(!authority.generation.is_empty(),"Agent generation is missing");
        if !equal(&authority.generation,&parent.generation) {
            crate::manager_workers::authorize(c,&parent,&authority.generation,false)?;
        }
    }
    Ok(())
}
fn authorize_session(
    c: &rusqlite::Connection,
    authority: &Option<SessionAuthority>,
    project: &str,
) -> anyhow::Result<()> {
    authorize_generation(c, authority, project)?;
    if let Some(authority) = authority {
        let parent = db::project_by_key(c,project)?;
        if !equal(&authority.generation,&parent.generation) {
            crate::manager_workers::authorize(c,&parent,&authority.generation,true)?;
            return Ok(());
        }
    }
    if authority.is_some() && crate::repair_lane::current(c, project, true)?.is_none() {
        let registered:bool=c.query_row("SELECT ready=1 AND COALESCE(json_extract(execution,'$.mcp_registered'),0)=1 AND json_type(execution,'$.repair_lane') IS NULL FROM projects WHERE id=?",[project],|r|r.get(0))?;
        anyhow::ensure!(
            registered,
            "Register the verified native execution before using its tools"
        );
    }
    Ok(())
}
async fn auth(State(app): State<Arc<App>>, mut req: Request, next: Next) -> Response {
    let cfg = app.config.read().await.clone();
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or("");
        if origin != cfg.url() && origin != format!("http://localhost:{}", cfg.port) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error":"Untrusted origin"})),
            )
                .into_response();
        }
    }
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if equal(token, &cfg.token) {
        return next.run(req).await;
    }
    // A project credential can only access the project's MCP operations.
    let path = req.uri().path().trim_start_matches("/api");
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() == 4
        && parts[1] == "projects"
        && ["register", "outbox", "timers"].contains(&parts[3])
        && req.method() == "POST"
        && let Ok(p) = app.db.project(parts[2]).await
    {
        let generation = req
            .headers()
            .get("x-hfb-generation")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if equal(token, &p.mcp_token)
            && !p.generation.is_empty()
            && equal(generation, &p.generation)
        {
            if App::update_required(&p) {
                return (StatusCode::CONFLICT, Json(json!({"error":"update_required: Open this project in Dauerbot and choose Recover Agent after update. Saved messages and uncertain results are preserved."}))).into_response();
            }
            req.extensions_mut().insert(SessionAuthority {
                project: p.id,
                generation: p.generation,
            });
            return next.run(req).await;
        }
        if !generation.is_empty() && !equal(generation,&p.generation) {
            let parent = p.clone();
            let requested = generation.to_owned();
            let candidate = app.db.call(move |c| {
                crate::manager_workers::authorize(c,&parent,&requested,false)
            }).await;
            if let Ok(seat) = candidate
                && !seat.project.mcp_token.is_empty()
                && equal(token,&seat.project.mcp_token)
            {
                req.extensions_mut().insert(SessionAuthority {
                    project: p.id,
                    generation: seat.project.generation,
                });
                return next.run(req).await;
            }
        }
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(
            json!({"error":"A local admin token or valid project-session credential is required"}),
        ),
    )
        .into_response()
}
async fn admission(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let control = matches!(
        req.uri().path(),
        "/api/service/activation/prepare"
            | "/api/service/activation/commit"
            | "/api/service/activation/abort"
            | "/api/service/activation/status"
            | "/service/activation/prepare"
            | "/service/activation/commit"
            | "/service/activation/abort"
            | "/service/activation/status"
    );
    if control {
        return match tokio::spawn(async move {next.run(req).await}).await {
            Ok(response)=>response,
            Err(_)=>(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":"Activation interrupted; inspect its durable status before retrying"}))).into_response(),
        };
    }
    // Read-only probes remain available while an existing operation drains.
    // Published MCP startup performs these before its registration request.
    if req.method() == axum::http::Method::GET {
        return next.run(req).await;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(95);
    loop {
        if let Some(permit) = app.service.gate.enter() {
            return match crate::service::admitted(permit,async move {next.run(req).await}).await {
                Ok(response)=>response,
                Err(_)=>(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":"The admitted operation ended without a result; verify its recorded outcome before retrying"}))).into_response(),
            };
        }
        if app.service.relay.load(std::sync::atomic::Ordering::SeqCst) {
            let state = app.service.state();
            // Ownership may commit before the candidate opens its listener.
            // Wait for serving ACK before forwarding an old accepted request.
            if state["phase"] == "serving" {
                return proxy_to_owner(app, req).await;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER,"1")], Json(json!({"error":"Service activation has not admitted this request; retry with the same request ID"}))).into_response();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
async fn proxy_to_owner(app: Arc<App>, req: Request) -> Response {
    let cfg = app.config.read().await.clone();
    let (parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let path = if path.starts_with("/api/") {
        path.to_owned()
    } else {
        format!("/api{path}")
    };
    let mut request = app
        .http
        .request(parts.method, format!("{}{path}", cfg.url()))
        .body(reqwest::Body::wrap_stream(body.into_data_stream()));
    for (name, value) in parts.headers.iter() {
        if name != header::HOST && name != header::CONNECTION {
            request = request.header(name, value);
        }
    }
    match request.send().await {
        Ok(response)=>{
            let status=response.status();
            let headers=response.headers().clone();
            let mut result=Response::new(axum::body::Body::from_stream(response.bytes_stream()));
            *result.status_mut()=status;
            for (name,value) in headers.iter() {
                if name!=header::CONNECTION && name!=header::TRANSFER_ENCODING {result.headers_mut().insert(name,value.clone());}
            }
            result
        }
        Err(_)=>(StatusCode::BAD_GATEWAY,Json(json!({"error":"The committed service did not return a result; preserve this request ID and verify its recorded outcome"}))).into_response(),
    }
}


async fn autonomy_settings(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(
        app.db
            .call(move |c| {
                let project = crate::autonomy::project_id(c, &id)?;
                Ok(json!(crate::autonomy::settings(c, &project)?))
            })
            .await?,
    ))
}

async fn update_autonomy(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(update): Json<crate::autonomy::Update>,
) -> ApiResult {
    Ok(Json(
        app.db
            .call(move |c| {
                let project = crate::autonomy::project_id(c, &id)?;
                Ok(json!(crate::autonomy::configure(c, &project, update, db::now())?))
            })
            .await?,
    ))
}

async fn proposals(State(app): State<Arc<App>>, Query(query): Query<crate::autonomy::List>) -> ApiResult {
    Ok(Json(
        app.db
            .call(move |c| Ok(json!({"items":crate::autonomy::list(c, &query, db::now())?})))
            .await?,
    ))
}

async fn decide_proposal(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<crate::autonomy::Decide>,
) -> ApiResult {
    let proposal = app.db.call(move |c| {
        let tx = c.unchecked_transaction()?;
        let proposal = crate::autonomy::decide_inside(&tx, &id, input.decision, crate::autonomy::Authority::Admin, db::now())?;
        tx.commit()?;
        Ok(proposal)
    }).await?;
    app.notify.notify_one();
    Ok(Json(json!(proposal)))
}

async fn panel_overview(
    State(app): State<Arc<App>>,
    Query(query): Query<crate::panel::Query>,
) -> ApiResult {
    let bots = app.runtime.read().await.bots.clone();
    Ok(Json(
        app.db
            .call(move |c| crate::panel::overview(c, &bots, &query, db::now()))
            .await?,
    ))
}

async fn panel_messages(
    State(app): State<Arc<App>>,
    Query(query): Query<crate::panel::Query>,
) -> ApiResult {
    Ok(Json(
        app.db
            .call(move |c| crate::panel::messages(c, &query, db::now()))
            .await?,
    ))
}

async fn panel_message_detail(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    let value = app
        .db
        .call(move |c| crate::panel::message_detail(c, &id, db::now()))
        .await?;
    Ok(match value {
        Some(value) => Json(value).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"Activity message not found"})),
        )
            .into_response(),
    })
}

async fn activation_prepare(
    State(app): State<Arc<App>>,
    Json(input): Json<crate::service::Prepare>,
) -> ApiResult {
    Ok(Json(json!(crate::service::prepare(&app, input).await?)))
}
async fn activation_commit(
    State(app): State<Arc<App>>,
    Json(input): Json<crate::service::Activation>,
) -> ApiResult {
    Ok(Json(json!(
        crate::service::commit(&app, &input.activation_id).await?
    )))
}
async fn activation_abort(
    State(app): State<Arc<App>>,
    Json(input): Json<crate::service::Activation>,
) -> ApiResult {
    Ok(Json(json!(
        crate::service::abort(&app, &input.activation_id).await?
    )))
}
async fn activation_status(
    State(app): State<Arc<App>>,
    Query(input): Query<crate::service::Activation>,
) -> ApiResult {
    let mut value = json!(app.service.receipt(&input.activation_id)?);
    value["owner_pid"] = json!(std::process::id());
    Ok(Json(value))
}
async fn activation_ingest(
    State(app): State<Arc<App>>,
    Json(input): Json<crate::service::Ingest>,
) -> ApiResult {
    let binding = app
        .db
        .bots()
        .await?
        .into_iter()
        .find(|b| b.project_id == input.project && b.app_id == input.app_id)
        .context("The bot binding changed")?;
    crate::feishu::ingest(
        &app,
        &binding,
        &serde_json::to_vec(&input.payload).map_err(anyhow::Error::from)?,
    )?;
    Ok(Json(json!({"committed":true})))
}
async fn headers(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let h = response.headers_mut();
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert("x-frame-options", "DENY".parse().unwrap());
    h.insert("referrer-policy", "no-referrer".parse().unwrap());
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    h.insert("content-security-policy","default-src 'self'; connect-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'".parse().unwrap());
    response
}
async fn shutdown_service(State(app): State<Arc<App>>) -> ApiResult {
    app.shutdown.cancel();
    Ok(Json(json!({"stopping":true})))
}
async fn recover_after_update(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(json!(app.recover_after_update(&id).await?)))
}
async fn state(State(app): State<Arc<App>>) -> ApiResult {
    let projects = app.db.projects().await?;
    let runtime = serde_json::to_value(&*app.runtime.read().await).map_err(anyhow::Error::from)?;
    let mut records = Vec::new();
    let mut work_states = serde_json::Map::new();
    let bindings = app.db.bots().await?;
    let manager_provider = app.db.call(crate::manager::public_provider).await?;
    for p in projects {
        let id = p.id.clone();
        let snapshot = app.db.call(move |c| work::snapshot(c, &id)).await?;
        let mut record = json!(p);
        let manager_project = p.id.clone();
        let manager = app
            .db
            .call(move |c| crate::manager::public(c, &manager_project))
            .await?;
        record["agent_mode"] = manager["mode"].clone();
        record["manager"] = manager;
        record["execution"]["agent_tree"] = crate::agent_tree::observe(p.clone()).await;
        let project_id = p.id.clone();
        record["cli_transition"] = json!(
            app.db
                .call(move |c| crate::cli_switch::latest(c, &project_id))
                .await?
        );
        record["bot"] = bindings
            .iter()
            .find(|b| b.project_id == p.id)
            .map(|b| json!({"app_id":b.app_id,"has_secret":!b.app_secret.is_empty()}))
            .unwrap_or(Value::Null);
        if App::update_required(&p) {
            record["lifecycle"] = json!("update_required");
        } else if let Some(phase) = runtime["projects"].get(&p.id).and_then(Value::as_object) {
            record.as_object_mut().unwrap().extend(phase.clone());
        } else {
            record["lifecycle"] = json!(if p.paused {
                "paused"
            } else if p.terminal.is_none() {
                "sleeping"
            } else {
                "connecting"
            });
        }
        work_states.insert(p.id.clone(), json!(snapshot));
        records.push(record);
    }
    Ok(Json(
        json!({"projects":records,"manager_provider":manager_provider,"work":work_states,"conversations":app.db.conversations().await?,"runtime":runtime,"settings":feishu::public_config(&*app.config.read().await),"version":env!("CARGO_PKG_VERSION"),"api_protocol":PROTOCOL,"mcp_contracts":[crate::mcp::CONTRACT],"service_activation":app.service.state()}),
    ))
}
async fn settings(State(app): State<Arc<App>>) -> ApiResult {
    Ok(Json(feishu::public_config(&*app.config.read().await)))
}
#[derive(Deserialize)]
struct Settings {
    app_id: String,
    #[serde(default)]
    app_secret: String,
    #[serde(default)]
    clear_secret: bool,
}
async fn project_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let project = app.db.project(&id).await?;
    let binding = app
        .db
        .bots()
        .await?
        .into_iter()
        .find(|b| b.project_id == project.id);
    Ok(Json(
        binding
            .map(|b| json!({"app_id":b.app_id,"has_secret":true}))
            .unwrap_or(Value::Null),
    ))
}
async fn save_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<Settings>,
) -> ApiResult {
    let project = app.db.project(&id).await?;
    let project_id = project.id.clone();
    app.db
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            crate::bot::save(
                &tx,
                &project_id,
                input.app_id.trim(),
                &input.app_secret,
                input.clear_secret,
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    app.token.lock().await.remove(&project.id);
    app.settings_tx.send_modify(|v| *v += 1);
    project_bot(State(app), Path(project.id)).await
}
async fn disconnect_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let project = app.db.project(&id).await?;
    let project_id = project.id.clone();
    app.db
        .call(move |c| {
            c.execute("DELETE FROM project_bots WHERE project_id=?", [project_id])?;
            Ok(())
        })
        .await?;
    app.token.lock().await.remove(&project.id);
    app.settings_tx.send_modify(|v| *v += 1);
    Ok(Json(json!({"disconnected":true})))
}
async fn check_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let project = app.db.project(&id).await?;
    let binding = app.db.bot(&project.id).await?;
    feishu::token(&app, &binding).await?;
    Ok(Json(
        json!({"ok":true,"message":"App credentials are valid; subscribe to im.message.receive_v1 in the Feishu console"}),
    ))
}
async fn create_project(State(app): State<Arc<App>>, Json(input): Json<ProjectInput>) -> ApiResult {
    let p = app.db.create_from_api(input).await?;

    Ok(Json(serde_json::to_value(p).unwrap()))
}
async fn update_project(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<ProjectInput>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    let lock = app.project_lock(&p.id).await;
    let _guard = lock.lock().await;
    let p = app.db.update(&p.id, input).await?;

    Ok(Json(serde_json::to_value(p).unwrap()))
}
async fn project(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let p = app.db.project(&id).await?;
    let messages = app.db.messages(&p.id).await?;
    let terminal = p.terminal.as_ref().and_then(|t| {
        app.runtime
            .try_read()
            .ok()
            .and_then(|r| r.terminals.get(t).cloned())
    });
    Ok(Json(
        json!({"project":p,"messages":messages,"terminal":terminal}),
    ))
}
#[derive(Deserialize)]
struct AgentSelection {
    agent: String,
}
async fn switch_agent(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<AgentSelection>,
) -> ApiResult {
    Ok(Json(json!(app.switch_agent(&id, &input.agent).await?)))
}
async fn switch_standby_agent(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<AgentSelection>,
) -> ApiResult {
    Ok(Json(json!(
        app.switch_agent_mode(&id, &input.agent, true).await?
    )))
}
async fn transition_agent(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<AgentSelection>,
) -> ApiResult {
    let project = app.request_cli_switch(&id, &input.agent).await?;
    let id = project.id.clone();
    let transition = app
        .db
        .call(move |c| crate::cli_switch::latest(c, &id))
        .await?;
    let mut value = json!(project);
    value["cli_transition"] = json!(transition);
    Ok(Json(value))
}
async fn repair_agent(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let project = app.request_cli_repair(&id).await?;
    let key = project.id.clone();
    let transition = app
        .db
        .call(move |c| crate::cli_switch::latest(c, &key))
        .await?;
    let mut value = json!(project);
    value["cli_transition"] = json!(transition);
    Ok(Json(value))
}
async fn project_models(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let project = app.db.project(&id).await?;
    let key = project.id.clone();
    let agent = project.agent.clone();
    let configured = app
        .db
        .call(move |c| crate::models::load(c, &key, &agent))
        .await?;
    let catalog = crate::models::discover(&project, configured).await?;
    let mut value = json!(catalog);
    value["generation"] = json!(project.generation);
    Ok(Json(value))
}

#[derive(Deserialize)]
struct ProjectModelSelection {
    model: String,
    provider: Option<String>,
    request_id: Option<String>,
    agent: Option<String>,
    generation: Option<String>,
}

async fn select_project_model(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<ProjectModelSelection>,
) -> ApiResult {
    let selection = if input.model.trim().is_empty() {
        if input
            .provider
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty())
        {
            return Err(
                anyhow::anyhow!("A native-default model cannot override its provider").into(),
            );
        }
        None
    } else {
        Some(crate::models::Selection {
            model: input.model,
            provider: input.provider,
        })
    };
    let expected = match (input.agent, input.generation) {
        (Some(agent), Some(generation)) => Some(crate::models::Scope { agent, generation }),
        (None, None) => None,
        _ => return Err(anyhow::anyhow!("Provide both the catalogue CLI and generation").into()),
    };
    let request_id = input.request_id.unwrap_or_else(crate::db::id);
    let project = app
        .request_model_switch_with_id(&id, selection, &request_id, expected)
        .await?;
    let key = project.id.clone();
    let transition = app
        .db
        .call(move |c| crate::cli_switch::latest(c, &key))
        .await?;
    let mut value = json!(project);
    value["cli_transition"] = json!(transition);
    Ok(Json(value))
}

async fn project_document(
    State(app): State<Arc<App>>,
    Path((id, document)): Path<(String, crate::project_documents::Document)>,
) -> ApiResult {
    let project = app.db.project(&id).await?;
    let content = tokio::task::spawn_blocking(move || {
        crate::project_documents::read(std::path::Path::new(&project.path), document)
    })
    .await
    .map_err(anyhow::Error::from)?;
    Ok(Json(json!(content)))
}
async fn delete_project(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let p = app.db.project(&id).await?;
    let lock = app.project_lock(&p.id).await;
    let _guard = lock.lock().await;
    // Removal and process cleanup share the wakeup lock so pending input cannot
    // recreate an execution between two separate stop/delete requests.
    let p = app.db.project(&p.id).await?;
    app.stop_inner(&p).await?;
    app.db.remove_project(&p.id).await?;
    app.token.lock().await.remove(&p.id);
    app.settings_tx.send_modify(|v| *v += 1);

    Ok(Json(json!({"ok":true})))
}
async fn start(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(
        serde_json::to_value(app.start(&id, false).await?).unwrap(),
    ))
}
async fn resume(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(
        serde_json::to_value(app.start(&id, true).await?).unwrap(),
    ))
}
async fn stop(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    app.stop(&id).await?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct Pause {
    paused: bool,
}
async fn pause(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<Pause>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    let lock = app.project_lock(&p.id).await;
    let _guard = lock.lock().await;
    app.db.pause(&p.id, input.paused).await?;
    app.notify.notify_one();

    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct ProjectDebug {
    debug_mode: bool,
}
async fn project_debug(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<ProjectDebug>,
) -> ApiResult {
    app.db.set_debug_mode(&id, input.debug_mode).await?;
    Ok(Json(json!({"debug_mode":input.debug_mode})))
}
async fn show(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(app.show(&id).await?))
}
async fn screen(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let p = app.db.project(&id).await?;
    let cfg = app.config.read().await.clone();
    Ok(Json(json!({"text":herdr::screen(&cfg,&p,80).await?})))
}
#[derive(Deserialize)]
struct Text {
    content: String,
}
async fn manager_config(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let p = app.db.project(&id).await?;
    Ok(Json(
        app.db
            .call(move |c| crate::manager::public(c, &p.id))
            .await?,
    ))
}
async fn manager_context(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let p = app.db.project(&id).await?;
    let mut view = app.manager_context.view(&p.id);
    if view["requests"].as_array().is_some_and(Vec::is_empty) {
        let project = p.id;
        let fallback = app.db.call(move |c| {
            use rusqlite::OptionalExtension;
            let row: Option<(String,String)> = c.query_row("SELECT id,checkpoint FROM manager_runs WHERE project_id=? ORDER BY rowid DESC LIMIT 1", [&project], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
            Ok(row.and_then(|(run,checkpoint)| serde_json::from_str(&checkpoint).ok().and_then(|checkpoint| crate::manager_context::normalized(&checkpoint,&run))))
        }).await?;
        if let Some(fallback) = fallback {
            view["requests"] = json!([fallback]);
        }
    }
    Ok(Json(crate::manager_context::bounded(view)))
}
async fn manager_provider(State(app): State<Arc<App>>) -> ApiResult {
    Ok(Json(app.db.call(crate::manager::public_provider).await?))
}
async fn manager_save_provider(
    State(app): State<Arc<App>>,
    Json(input): Json<crate::manager::ConfigInput>,
) -> ApiResult {
    // The same project locks fence individual manager effects while a shared
    // update invalidates their provider revisions.
    let mut projects = app.db.projects().await?;
    projects.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    let mut guards = Vec::with_capacity(projects.len());
    for project in projects {
        guards.push(app.project_lock(&project.id).await.lock_owned().await);
    }
    let value = app.db.call(move |c| crate::manager::save_provider(c,input)).await?;
    app.notify.notify_one();
    Ok(Json(value))
}
async fn manager_save() -> ApiResult {
    Err(anyhow::anyhow!("Manager credentials are shared. Update them at /api/manager/provider.").into())
}
async fn manager_mode(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<crate::manager::ModeInput>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    let lock = app.project_lock(&p.id).await;
    let _guard = lock.lock().await;
    let value = app
        .db
        .call(move |c| crate::manager::set_mode(c, &p.id, input))
        .await?;
    app.notify.notify_one();
    Ok(Json(value))
}
async fn inject(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<Text>,
) -> ApiResult {
    if input.content.trim().is_empty() {
        return Err(anyhow::anyhow!("Message content cannot be empty").into());
    }
    let p = app.db.project(&id).await?;
    let key = db::id();
    let source = format!("local-{}", db::id());
    let out = key.clone();
    app.db.call(move|c|{let tx=c.unchecked_transaction()?;let attachment=json!({});tx.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at) VALUES(?,?,'in',?,?,'queued',?,?,?)",
        rusqlite::params![key,p.id,source,input.content,attachment.to_string(),db::now(),db::now()])?;tx.commit()?;Ok(())}).await?;
    app.notify.notify_one();

    Ok(Json(
        serde_json::to_value(app.db.message(&out).await?).unwrap(),
    ))
}
#[derive(Deserialize)]
struct Outbox {
    #[serde(default)]
    to: Option<String>,
    request_id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    reply_to_message_id: Option<String>,
    #[serde(default)]
    source_event_id: Option<String>,
    #[serde(default)]
    intent: Option<String>,
}
async fn outbox(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<Outbox>,
) -> ApiResult {
    if input.content.trim().is_empty() && input.path.is_none() {
        return Err(anyhow::anyhow!("Provide message content or a file path").into());
    }
    let p = app.db.project(&id).await?;
    let mut attachment = if let Some(path) = input.path {
        json!({"path":feishu::allowed_file(&p,std::path::Path::new(&path),&app.dir)?})
    } else {
        json!({})
    };
    let to = input.to.as_deref().unwrap_or(&p.chat_id);
    feishu::recipient_type(to)?;
    attachment["to"] = json!(to);
    if let Some(anchor) = input.reply_to_message_id {
        attachment["reply_to_message_id"] = json!(anchor);
    }
    if let Some(intent) = &input.intent {
        if !matches!(intent.as_str(), "result" | "decision" | "progress") {
            return Err(anyhow::anyhow!(
                "Choose result, decision or progress as the message intent"
            )
            .into());
        }
        attachment["discussion_intent"] = json!(intent);
    }
    if input.request_id.is_empty() || input.request_id.len() > 200 {
        return Err(anyhow::anyhow!("request_id must contain 1 to 200 characters").into());
    }
    let authority = authority(session);
    let project = p.id.clone();
    let request = input.request_id.clone();
    let (message_id, progress) = app
        .db
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            authorize_session(&tx, &authority, &p.id)?;
            let current = db::project_by_key(&tx,&p.id)?;
            if let Some(seat) = authority.as_ref().filter(|a| a.generation!=current.generation) {
                attachment["task_worker_generation"]=json!(seat.generation);
                anyhow::ensure!(input.intent.as_deref()!=Some("progress"),
                    "A task worker must return a final result; transient progress is not supported");
            }
            if authority.as_ref().is_some_and(|a| a.generation==current.generation)
                && let Some(lane) = crate::repair_lane::current(&tx, &p.id, true)?
            {
                attachment[crate::repair_lane::OUTBOX_KEY] = serde_json::to_value(lane)?;
            }
            let binding = crate::bot::get(&tx, &p.id)?
                .context("Connect a Feishu bot to this project first")?;
            if let Some(event) = &input.source_event_id {
                if !attachment["reply_to_message_id"].is_string() && input.intent.is_none() {
                    anyhow::bail!(
                        "Choose result or decision before sending without a reply anchor"
                    );
                }
                let to = attachment["to"].as_str().unwrap_or_default().to_owned();
                crate::discussion::origin(
                    &tx,
                    &p.id,
                    event,
                    &binding.app_id,
                    &to,
                    input.to.is_some(),
                    &mut attachment,
                )?;
            }
            if input.intent.as_deref() == Some("progress") {
                anyhow::ensure!(
                    attachment[crate::repair_lane::OUTBOX_KEY].is_null(),
                    "Progress is unavailable in a repair-only session"
                );
                tx.commit()?;
                return Ok((
                    String::new(),
                    Some((binding.app_id, input.content, attachment)),
                ));
            }
            let message = db::enqueue_out_request(
                &tx,
                &p.id,
                &binding.app_id,
                &input.request_id,
                &input.content,
                attachment,
            )?;
            tx.commit()?;
            Ok((message, None))
        })
        .await?;
    if let Some((app_id, content, attachment)) = progress {
        let result =
            crate::discussion::progress(&app, project, app_id, request, content, attachment)
                .await?;
        return Ok(Json(result));
    }
    app.send_out(&message_id).await?;
    let m = app.db.message(&message_id).await?;
    Ok(Json(serde_json::to_value(m).unwrap()))
}
#[derive(Deserialize)]
struct Registration {
    generation: String,
    terminal: String,
    #[serde(default)]
    mcp_contract: Option<u64>,
}
async fn register(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<Registration>,
) -> ApiResult {
    let authority = authority(session);
    let parent = app.db.project(&id).await?;
    if input.generation != parent.generation {
        if authority.as_ref().is_some_and(|a| a.project!=parent.id || a.generation!=input.generation) {
            return Err(anyhow::anyhow!("Session identity does not match").into());
        }
        let generation = input.generation.clone();
        let worker = app.db.call(move |c| {
            Ok(crate::manager_workers::authorize(c,&parent,&generation,false)?.project)
        }).await?;
        return Ok(Json(crate::manager_workers::register(&app,worker,&input.terminal,input.mcp_contract).await?));
    }
    let cfg = app.config.read().await.clone();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    // Reread every probe: the daemon may admit a repair-only role while this
    // companion is waiting for native detection, without changing generation.
    let (p, observed) = loop {
        let p = app.db.project(&id).await?;
        if p.execution["pane_id"].as_str() != Some(&input.terminal)
            || p.generation != input.generation
            || authority
                .as_ref()
                .is_some_and(|a| a.project != p.id || a.generation != p.generation)
        {
            return Err(anyhow::anyhow!("Session identity does not match").into());
        }
        let contract = crate::mcp::generation_contract(&p.execution)
            .context("Unsupported Agent MCP contract")?;
        if input.mcp_contract.is_some_and(|v| v != contract) {
            return Err(anyhow::anyhow!("Agent MCP contract differs from its execution").into());
        }
        let observed = herdr::observe(&cfg, &p).await?;
        let checked = if crate::repair_lane::descriptor(&p).is_some() {
            crate::repair_lane::check_identity(&p, &observed)
        } else {
            herdr::check_identity(&p, &observed)
        };
        match checked {
            Ok(()) => break (p, observed),
            Err(error)
                if error.is::<herdr::NativeSessionChanged>()
                    && crate::repair_lane::descriptor(&p).is_none() =>
            {
                let current = p.clone();
                let record = observed.clone();
                let admission_authority = authority.clone();
                let admitted = app
                    .db
                    .call(move |c| {
                        authorize_generation(c, &admission_authority, &current.id)?;
                        crate::repair_lane::admit(c, &current, &record)
                    })
                    .await?;
                if admitted.is_some() {
                    app.notify.notify_one();
                    continue;
                }
                return Err(error.into());
            }
            Err(error)
                if error.is::<herdr::SessionStarting>()
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let contract =
        crate::mcp::generation_contract(&p.execution).context("Unsupported Agent MCP contract")?;
    if input.mcp_contract.is_some_and(|v| v != contract) {
        return Err(anyhow::anyhow!("Agent MCP contract differs from its execution").into());
    }
    let registration_api = p.execution["api_protocol"].as_u64().unwrap_or(PROTOCOL);
    let native = observed["native_session"]["value"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let repair = crate::repair_lane::descriptor(&p);
    let expected = repair.as_ref().map(|lane| lane.expected_native.clone());
    let name = p.name.clone();
    app.db.call(move|c|{
        let tx=c.unchecked_transaction()?;authorize_generation(&tx,&authority,&p.id)?;
        let terminal=p.terminal.as_deref().context("Execution is not running")?;
        if let Some(lane)=repair {
            let current=crate::repair_lane::authorize(&tx,&p,false)?.context("Repair role retired")?;
            crate::repair_lane::effect_phase(&tx,&current)?;
            anyhow::ensure!(herdr::same_native_session(&p.agent,&lane.observed_native,&native),"The observed repair identity changed");
            let changed=tx.execute("UPDATE projects SET execution=json_set(execution,'$.repair_lane.mcp_registered',json('true')) WHERE id=? AND generation=? AND terminal=? AND ready=0",rusqlite::params![p.id,input.generation,terminal])?;
            anyhow::ensure!(changed==1,"Repair session changed before registration");
        }else{
            // A concurrently admitted repair role must never inherit this old
            // normal-readiness result after the native observation.
            let repair_exists:bool=tx.query_row("SELECT json_type(execution,'$.repair_lane') IS NOT NULL FROM projects WHERE id=?",[&p.id],|r|r.get(0))?;
            anyhow::ensure!(!repair_exists,"Session became repair-only; retry registration");
            let changed=tx.execute("UPDATE projects SET ready=1,native_session=CASE WHEN ?1='' THEN native_session ELSE ?1 END,execution=json_set(CASE WHEN ?1!='' AND execution!='null' THEN json_set(execution,'$.native_confirmed',json('true')) ELSE execution END,'$.mcp_registered',json('true')) WHERE id=?2 AND generation=?3 AND terminal=?4",rusqlite::params![native,p.id,input.generation,terminal])?;
            anyhow::ensure!(changed==1,"Session identity changed; reconnect MCP");
            if !native.is_empty(){work::set_native_session(&tx,&p.id,&input.generation,&native,db::now())?;}
        }
        tx.commit()?;Ok(())
    }).await?;
    app.notify.notify_one();
    Ok(Json(
        json!({"ok":true,"project":name,"api_protocol":registration_api,"mcp_contract":contract,"service_api_protocol":PROTOCOL,"repair_only":expected.is_some(),"original_native":expected}),
    ))
}

async fn retry(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let m = app.db.message(&id).await?;
    let lock = app.project_lock(&m.project_id).await;
    let _guard = lock.lock().await;
    let m = app.db.message(&id).await?;
    if !["uncertain", "failed"].contains(&m.status.as_str()) {
        return Err(anyhow::anyhow!("Only failed or uncertain messages can be retried").into());
    }
    recover_message(&app, m, true).await?;
    app.notify.notify_one();

    Ok(Json(json!({"ok":true})))
}
async fn dismiss(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let m = app.db.message(&id).await?;
    let lock = app.project_lock(&m.project_id).await;
    let _guard = lock.lock().await;
    let m = app.db.message(&id).await?;
    if !["queued", "submitted", "uncertain", "failed", "pending"].contains(&m.status.as_str()) {
        return Err(anyhow::anyhow!("This message cannot be dismissed").into());
    }
    recover_message(&app, m, false).await?;
    app.notify.notify_one();

    Ok(Json(json!({"ok":true})))
}
async fn recover_message(app: &App, m: db::Message, retry: bool) -> anyhow::Result<()> {
    app.db
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            if m.direction == "in" {
                work::sync_inbound(c)?;
                let event_id: Option<String> = c
                    .query_row(
                        "SELECT id FROM work_events WHERE project_id=? AND message_id=?",
                        rusqlite::params![m.project_id, m.id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(event_id) = event_id {
                    work::resolve_event(c, &m.project_id, &event_id, retry, db::now())?;
                    tx.commit()?;
                    return Ok(());
                }
            }
            c.execute(
                "UPDATE messages SET status=?,error='',updated_at=? WHERE id=?",
                rusqlite::params![if retry { "pending" } else { "dismissed" }, db::now(), m.id],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
}
async fn upload(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> ApiResult {
    app.db.project(&id).await?;
    let field = multipart
        .next_field()
        .await
        .map_err(anyhow::Error::from)?
        .context("Missing file")?;
    let name = field
        .file_name()
        .and_then(|s| std::path::Path::new(s).file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("attachment")
        .to_owned();
    let path = app.dir.join("files").join(format!("{}-{name}", db::id()));
    let result = async {
        let mut field = field;
        let mut file = tokio::fs::File::create(&path).await?;
        let mut size = 0u64;
        while let Some(chunk) = field.chunk().await? {
            size += chunk.len() as u64;
            if size > feishu::MAX_FILE {
                bail!("File exceeds 100 MB");
            }
            file.write_all(&chunk).await?;
        }
        if size == 0 {
            bail!("File is empty");
        }
        file.sync_all().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&path).await;
    }
    result?;
    Ok(Json(json!({"path":path,"name":name})))
}
async fn conversations(State(app): State<Arc<App>>) -> ApiResult {
    Ok(Json(app.db.conversations().await?))
}

async fn work_state(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    let authority = authority(session);
    Ok(Json(json!(
        app.db
            .call(move |c| {
                authorize_session(c, &authority, &p.id)?;
                work::snapshot(c, &p.id)
            })
            .await?
    )))
}
async fn timers(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let p = app.db.project(&id).await?;
    let state = app.db.call(move |c| work::snapshot(c, &p.id)).await?;
    Ok(Json(json!(state.timers)))
}
async fn create_timer(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<work::TimerInput>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    if let Some(to) = input.to.as_deref().filter(|s| !s.is_empty()) {
        feishu::recipient_type(to)?;
    }
    let authority = authority(session);
    let timer = app
        .db
        .call(move |c| {
            authorize_session(c, &authority, &p.id)?;
            let current = db::project_by_key(c,&p.id)?;
            anyhow::ensure!(
                authority.as_ref().is_none_or(|a| a.generation!=current.generation) || crate::repair_lane::current(c, &p.id, true)?.is_none(),
                "A repair-only Agent may not schedule work; restore the intended conversation first"
            );
            work::create_timer(c, &p.id, input, db::now())
        })
        .await?;
    app.notify.notify_one();
    Ok(Json(json!(timer)))
}
async fn project_for_record(app: &App, table: &'static str, id: &str) -> anyhow::Result<String> {
    let id = id.to_owned();
    app.db
        .call(move |c| {
            Ok(c.query_row(
                &format!("SELECT project_id FROM {table} WHERE id=?"),
                [id],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
}
async fn update_timer(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<work::TimerInput>,
) -> ApiResult {
    let project = project_for_record(&app, "work_timers", &id).await?;
    let timer = app
        .db
        .call(move |c| work::update_timer(c, &project, &id, input, db::now()))
        .await?;
    app.notify.notify_one();
    Ok(Json(json!(timer)))
}
async fn delete_timer(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let project = project_for_record(&app, "work_timers", &id).await?;
    app.db
        .call(move |c| work::delete_timer(c, &project, &id, db::now()))
        .await?;
    app.notify.notify_one();
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct TimerEnabled {
    enabled: bool,
}
async fn pause_timer(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<TimerEnabled>,
) -> ApiResult {
    let project = project_for_record(&app, "work_timers", &id).await?;
    let timer = app
        .db
        .call(move |c| work::set_timer_enabled(c, &project, &id, input.enabled, db::now()))
        .await?;
    app.notify.notify_one();
    Ok(Json(json!(timer)))
}
#[derive(Deserialize)]
struct Takeover {
    generation: String,
    active: bool,
    #[serde(default)]
    heartbeat: bool,
}
async fn takeover(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<Takeover>,
) -> ApiResult {
    if input.heartbeat {
        return Ok(Json(app.takeover_heartbeat(&id, &input.generation).await?));
    }
    Ok(Json(
        app.takeover(&id, &input.generation, input.active).await?,
    ))
}
#[derive(Deserialize)]
struct EventResolution {
    action: String,
}
async fn resolve_event(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<EventResolution>,
) -> ApiResult {
    if !["retry", "dismiss"].contains(&input.action.as_str()) {
        return Err(anyhow::anyhow!("Choose retry or dismiss").into());
    }
    let project = project_for_record(&app, "work_events", &id).await?;
    let lock = app.project_lock(&project).await;
    let _guard = lock.lock().await;
    let result = app
        .db
        .call(move |c| work::resolve_event(c, &project, &id, input.action == "retry", db::now()))
        .await?;
    app.notify.notify_one();
    Ok(Json(json!(result)))
}

#[cfg(test)]
mod manager_context_tests {
    use super::*;
    #[tokio::test]
    async fn new_projects_use_automatic_workers_only_when_manager_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().join("store"), crate::config::Config::default()).unwrap();
        let input = |name| ProjectInput { name, agent:"agy".into(),
            path:dir.path().to_string_lossy().into(), chat_id:String::new(),
            native_session:String::new(), extra_args:vec![] };
        let standalone = app.db.create_from_api(input("Standalone fixture".into())).await.unwrap();
        app.db.sync(|c| {
            assert!(!crate::manager::uses_manager(c,&standalone.id)?);
            assert_eq!(standalone.agent,"agy");
            crate::manager::save_provider(c, crate::manager::ConfigInput {
                base_url:"http://127.0.0.1:1/v1".into(), model:"fixture".into(),
                api_key:Some("fixture-not-a-credential".into()), clear_api_key:false,
                expected_revision:0, supports_images:None,
            })?;
            Ok(())
        }).unwrap();
        let automatic = app.db.create_from_api(input("Automatic fixture".into())).await.unwrap();
        app.db.sync(|c| {
            assert!(crate::manager::uses_manager(c,&automatic.id)?);
            assert_eq!(automatic.agent,"auto");
            assert!(!crate::manager::uses_manager(c,&standalone.id)?);
            assert!(automatic.terminal.is_none());
            Ok(())
        }).unwrap();
    }

    #[tokio::test]
    async fn context_endpoint_requires_admin_and_does_not_mutate_store_or_database() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().join("store"), crate::config::Config::default()).unwrap();
        let project = app
            .db
            .create(ProjectInput {
                name: "Context fixture".into(),
                path: dir.path().to_string_lossy().into(),
                agent: "codex".into(),
                chat_id: String::new(),
                native_session: String::new(),
                extra_args: vec![],
            })
            .await
            .unwrap();
        let before = app.db.sync(|c| Ok(c.total_changes())).unwrap();
        let request = rig_core::completion::CompletionRequest::new("fixture context");
        let guard = app
            .manager_context
            .capture(&project.id, "fixture-run", 1, &request);
        let snapshot = app.manager_context.view(&project.id);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_app = app.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(server_app)).await.unwrap();
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!(
            "http://{address}/api/projects/{}/manager/context",
            project.id
        );
        assert_eq!(
            http.get(&url).send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let response: Value = http
            .get(&url)
            .bearer_auth(&app.config.read().await.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response, snapshot);
        assert_eq!(app.manager_context.view(&project.id), snapshot);
        assert_eq!(app.db.sync(|c| Ok(c.total_changes())).unwrap(), before);
        assert_eq!(response["project_id"], project.id);
        assert_eq!(response["requests"][0]["status"], "prepared");
        drop(guard);
        server.abort();
        let _ = server.await;
    }
}

#[cfg(test)]
mod repair_authority_tests {
    use super::*;
    #[tokio::test]
    async fn repair_tools_require_registered_role_and_cannot_schedule_or_survive_retirement() {
        let (_dir, app, p, _record) = crate::repair_lane::tests::fixture("codex").await;
        let authority = Some(SessionAuthority {
            project: p.id.clone(),
            generation: p.generation.clone(),
        });
        app.db.sync(|c|{
            assert!(authorize_session(c,&authority,&p.id).is_err());
            c.execute("UPDATE projects SET execution=json_set(execution,'$.repair_lane.mcp_registered',json('true')) WHERE id=?",[&p.id])?;
            assert!(authorize_session(c,&authority,&p.id).is_ok());Ok(())
        }).unwrap();
        let input:work::TimerInput=serde_json::from_value(json!({"name":"not authorized","prompt":"must not run","kind":"once","at":db::now()+60000,"request_id":"repair-schedule"})).unwrap();
        let denied = create_timer(
            State(app.clone()),
            Path(p.id.clone()),
            Some(Extension(authority.clone().unwrap())),
            Json(input),
        )
        .await;
        assert!(denied.is_err());
        app.db.sync(|c|{
            assert_eq!(c.query_row("SELECT count(*) FROM work_timers WHERE project_id=?",[&p.id],|r|r.get::<_,i64>(0))?,0);
            c.execute("UPDATE cli_switch_requests SET phase='switching' WHERE project_id=?",[&p.id])?;
            assert!(authorize_session(c,&authority,&p.id).is_err());
            c.execute("UPDATE projects SET generation='successor',execution='null',ready=0 WHERE id=?",[&p.id])?;
            assert!(authorize_generation(c,&authority,&p.id).is_err());Ok(())
        }).unwrap();
    }
    #[tokio::test]
    async fn task_worker_credential_is_bound_to_its_generation_and_project_controls() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().join("store"), crate::config::Config::default()).unwrap();
        let p = app.db.create(ProjectInput {
            name: "Task worker fixture".into(),
            path: dir.path().to_string_lossy().into(),
            agent: "agy".into(), chat_id: String::new(), native_session: String::new(),
            extra_args: vec![],
        }).await.unwrap();
        app.db.sync(|c| {
            c.execute("UPDATE projects SET generation='primary' WHERE id=?", [&p.id])?;
            for (generation, agent, token) in [("first", "codex", "fixture-first"), ("second", "claude", "fixture-second")] {
                c.execute("INSERT INTO manager_workers(generation,project_id,agent,token,state,created_at)
                    VALUES(?,?,?,?,'ready',0)", rusqlite::params![generation,p.id,agent,token])?;
            }
            Ok(())
        }).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let actor = app.clone();
        let server = tokio::spawn(async move { axum::serve(listener, router(actor)).await.unwrap() });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let schedule = |generation| http.post(format!("http://{address}/api/projects/{}/timers", p.id))
            .bearer_auth("fixture-first").header("x-hfb-generation",generation)
            .json(&json!({"name":"fixture","prompt":"fixture","kind":"once","at":db::now()+60000}));
        for generation in ["primary", "second"] {
            assert_eq!(schedule(generation).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
        }
        assert!(schedule("first").send().await.unwrap().status().is_success());
        app.db.sync(|c| {
            c.execute("UPDATE projects SET paused=1 WHERE id=?", [&p.id])?;
            Ok(())
        }).unwrap();
        assert!(!schedule("first").send().await.unwrap().status().is_success());
        app.db.sync(|c| {
            c.execute("UPDATE projects SET paused=0 WHERE id=?", [&p.id])?;
            crate::manager_workers::closed(c, "first")
        }).unwrap();
        assert_eq!(schedule("first").send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
        server.abort();
    }

    #[tokio::test]
    async fn unregistered_native_credential_cannot_bypass_repair_admission_through_outbox() {
        let (_dir, app, p, _record) = crate::repair_lane::tests::fixture("codex").await;
        let authority = Some(SessionAuthority {
            project: p.id.clone(),
            generation: p.generation.clone(),
        });
        app.db.sync(|c|{
            c.execute("UPDATE cli_switch_requests SET holds_input=0,phase='cancelled' WHERE project_id=?",[&p.id])?;
            c.execute("UPDATE projects SET execution=json_remove(execution,'$.repair_lane') WHERE id=?",[&p.id])?;
            assert!(authorize_generation(c,&authority,&p.id).is_ok());
            assert!(authorize_session(c,&authority,&p.id).is_err());
            assert!(authorize_session(c,&None,&p.id).is_ok(),"daemon/admin messages keep their existing authority");Ok(())
        }).unwrap();
    }
}
