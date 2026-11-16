use crate::{
    App,
    db::{self, ProjectInput},
    feishu, herdr, work,
};
use anyhow::{Context, bail};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, Request, State},
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
            StatusCode::BAD_REQUEST,
            Json(json!({"error":self.0.to_string()})),
        )
            .into_response()
    }
}
type ApiResult = std::result::Result<Json<Value>, ApiError>;
pub const PROTOCOL: u64 = 3;

pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/state", get(state))
        .route("/service/shutdown", post(shutdown_service))
        .route("/settings", get(settings))
        .route("/conversations", get(conversations))
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
        .route("/projects/{id}/agent", post(switch_agent))
        .route("/projects/{id}/stop", post(stop))
        .route("/projects/{id}/pause", post(pause))
        .route("/projects/{id}/show", post(show))
        .route("/projects/{id}/screen", get(screen))
        .route("/projects/{id}/terminal", get(show))
        .route("/projects/{id}/takeover", post(takeover))
        .route("/projects/{id}/work", get(work_state).post(work_state))
        .route("/projects/{id}/tasks", get(tasks).post(create_task))
        .route("/projects/{id}/timers", get(timers).post(create_timer))
        .route("/projects/{id}/finish", post(finish_run))
        .route("/projects/{id}/event-ack", post(ack_event))
        .route("/projects/{id}/checkpoint", post(checkpoint))
        .route("/projects/{id}/decision", post(request_decision))
        .route("/projects/{id}/decision-answer", post(answer_decision))
        .route("/projects/{id}/jobs", post(register_job))
        .route("/projects/{id}/job-complete", post(complete_job))
        .route("/tasks/{id}/cancel", post(cancel_task))
        .route(
            "/timers/{id}",
            axum::routing::put(update_timer).delete(delete_timer),
        )
        .route("/timers/{id}/pause", post(pause_timer))
        .route("/events/{id}/resolve", post(resolve_event))
        .route("/decisions/{id}/answer", post(answer_decision_admin))
        .route("/jobs/{id}/abandon", post(abandon_job))
        .route("/projects/{id}/messages", post(inject))
        .route("/projects/{id}/outbox", post(outbox))
        .route("/projects/{id}/ready", post(ready))
        .route("/projects/{id}/ack", post(ack))
        .route(
            "/projects/{id}/upload",
            post(upload).layer(DefaultBodyLimit::max(
                feishu::MAX_FILE as usize + 1024 * 1024,
            )),
        )
        .route("/messages/{id}/retry", post(retry))
        .route("/messages/{id}/dismiss", post(dismiss))
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
fn authorize_session(
    c: &rusqlite::Connection,
    authority: &Option<SessionAuthority>,
    project: &str,
) -> anyhow::Result<()> {
    if let Some(authority) = authority {
        let current: String = c.query_row(
            "SELECT generation FROM projects WHERE id=?",
            [project],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            authority.project == project
                && !current.is_empty()
                && equal(&authority.generation, &current),
            "This Agent execution changed; reconnect before using its tools"
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
        && [
            "ready",
            "ack",
            "outbox",
            "work",
            "tasks",
            "timers",
            "finish",
            "event-ack",
            "checkpoint",
            "decision",
            "decision-answer",
            "jobs",
            "job-complete",
        ]
        .contains(&parts[3])
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
            req.extensions_mut().insert(SessionAuthority {
                project: p.id,
                generation: p.generation,
            });
            return next.run(req).await;
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
async fn state(State(app): State<Arc<App>>) -> ApiResult {
    let projects = app.db.projects().await?;
    let runtime = serde_json::to_value(&*app.runtime.read().await).map_err(anyhow::Error::from)?;
    let mut records = Vec::new();
    let mut work_states = serde_json::Map::new();
    let bindings = app.db.bots().await?;
    for p in projects {
        let id = p.id.clone();
        let snapshot = app.db.call(move |c| work::snapshot(c, &id)).await?;
        let mut record = json!(p);
        record["bot"] = bindings
            .iter()
            .find(|b| b.project_id == p.id)
            .map(|b| json!({"app_id":b.app_id,"has_secret":!b.app_secret.is_empty()}))
            .unwrap_or(Value::Null);
        if let Some(phase) = runtime["projects"].get(&p.id).and_then(Value::as_object) {
            record.as_object_mut().unwrap().extend(phase.clone());
        } else {
            record["lifecycle"] = json!(if !p.enabled {
                "stopped"
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
        json!({"projects":records,"work":work_states,"conversations":app.db.conversations().await?,"runtime":runtime,"settings":feishu::public_config(&*app.config.read().await),"version":env!("CARGO_PKG_VERSION"),"api_protocol":PROTOCOL}),
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
    let p = app.db.create(input).await?;

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
async fn delete_project(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let p = app.db.project(&id).await?;
    let lock = app.project_lock(&p.id).await;
    let _guard = lock.lock().await;
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
    app.db.call(move|c|{c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES(?,?,'in',?,?,'queued',?,?)",
        rusqlite::params![key,p.id,source,input.content,db::now(),db::now()])?;Ok(())}).await?;
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
    if input.request_id.is_empty() || input.request_id.len() > 200 {
        return Err(anyhow::anyhow!("request_id must contain 1 to 200 characters").into());
    }
    let authority = authority(session);
    let message_id=app.db.call(move |c|{
        authorize_session(c,&authority,&p.id)?;
        let binding=crate::bot::get(c,&p.id)?.context("Connect a Feishu bot to this project first")?;
        attachment["bot_app_id"]=json!(binding.app_id);
        c.execute("INSERT OR IGNORE INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at)
          VALUES(?,?,'out',?,?,'pending',?,?,?)",rusqlite::params![db::id(),p.id,input.request_id,input.content,attachment.to_string(),db::now(),db::now()])?;
        let (id,content,saved):(String,String,String)=c.query_row("SELECT id,content,attachment FROM messages WHERE project_id=? AND direction='out' AND source_id=?",
          rusqlite::params![p.id,input.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let mut saved:Value=serde_json::from_str(&saved)?;
        if let Some(saved)=saved.as_object_mut(){saved.remove("sent_message_id");}
        anyhow::ensure!(content==input.content&&saved==attachment,
          "This request_id refers to different content or recipient; use a new ID");
        Ok(id)
    }).await?;
    app.send_out(&message_id).await?;
    let m = app.db.message(&message_id).await?;
    Ok(Json(serde_json::to_value(m).unwrap()))
}
#[derive(Deserialize)]
struct Ready {
    generation: String,
    terminal: String,
    #[serde(default)]
    native_session: String,
}
async fn ready(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<Ready>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    if p.execution["pane_id"].as_str() != Some(&input.terminal) || p.generation != input.generation
    {
        return Err(anyhow::anyhow!("Session identity does not match").into());
    }
    let cfg = app.config.read().await.clone();
    // MCP can be ready just before HERDR's native-agent detection tick.
    // Keep this request alive briefly rather than requiring a model to recover
    // from a transient first-tool error. No project lock is held here.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let t = herdr::observe(&cfg, &p).await?;
        match herdr::check_identity(&p, &t) {
            Ok(()) => break,
            Err(error)
                if error.is::<herdr::SessionStarting>()
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let authority = authority(session);
    let name = p.name.clone();
    app.db.call(move|c|{
        let tx=c.unchecked_transaction()?;
        authorize_session(c,&authority,&p.id)?;
        let terminal=p.terminal.as_deref().context("Execution is not running")?;
        let changed=c.execute("UPDATE projects SET ready=1,native_session=CASE WHEN ?1='' THEN native_session ELSE ?1 END,
          execution=CASE WHEN ?1!='' AND execution!='null' THEN json_set(execution,'$.native_confirmed',json('true')) ELSE execution END
          WHERE id=?2 AND generation=?3 AND terminal=?4",rusqlite::params![input.native_session,p.id,input.generation,terminal])?;
        anyhow::ensure!(changed==1,"Session identity changed; reconnect MCP");
        if !input.native_session.is_empty(){
            work::set_native_session(c,&p.id,&input.generation,&input.native_session,db::now())?;
        }
        tx.commit()?;
        Ok(())
    }).await?;
    app.notify.notify_one();

    Ok(Json(json!({"ok":true,"project":name})))
}
#[derive(Deserialize)]
struct Ack {
    message_id: String,
}
async fn ack(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<Ack>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    let authority = authority(session);
    app.db.call(move|c|{
        let tx=c.unchecked_transaction()?;
        authorize_session(c,&authority,&p.id)?;
        let generation:String=c.query_row("SELECT generation FROM projects WHERE id=?",[&p.id],|r|r.get(0))?;
        let linked:Option<(String,String)>=c.query_row(
          "SELECT e.id,m.status FROM work_events e JOIN messages m ON m.id=e.message_id
           WHERE m.project_id=? AND m.source_id=? AND m.direction='in'",
          rusqlite::params![p.id,input.message_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((event_id,status))=linked {
            let current_run:bool=c.query_row(
              "SELECT EXISTS(SELECT 1 FROM work_runs WHERE project_id=? AND generation=? AND event_id=?)",
              rusqlite::params![p.id,generation,event_id],|r|r.get(0))?;
            if current_run {
                work::ack_event(c,&p.id,&generation,&event_id,db::now())?;
            } else {
                anyhow::ensure!(status=="acknowledged","This message was not submitted to the current execution");
            }
        }
        let changed=c.execute("UPDATE messages SET status='acknowledged',error='',updated_at=?
          WHERE project_id=? AND source_id=? AND direction='in' AND status IN ('submitting','submitted','uncertain','acknowledged')",
          rusqlite::params![db::now(),p.id,input.message_id])?;
        anyhow::ensure!(changed==1,"This message does not belong to the project or has not been submitted");
        tx.commit()?;
        Ok(())
    }).await?;
    app.notify.notify_one();

    Ok(Json(json!({"ok":true})))
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
async fn tasks(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let p = app.db.project(&id).await?;
    Ok(Json(json!(
        app.db.call(move |c| work::tasks(c, &p.id)).await?
    )))
}
async fn timers(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let p = app.db.project(&id).await?;
    let state = app.db.call(move |c| work::snapshot(c, &p.id)).await?;
    Ok(Json(json!(state.timers)))
}
async fn create_task(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<work::TaskInput>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    if let Some(to) = input.to.as_deref().filter(|s| !s.is_empty()) {
        feishu::recipient_type(to)?;
    }
    let source = db::id();
    let authority = authority(session);
    let event = app
        .db
        .call(move |c| {
            authorize_session(c, &authority, &p.id)?;
            work::submit_task(c, &p.id, input, "task", &source, db::now())
        })
        .await?;
    app.notify.notify_one();
    Ok(Json(json!(event)))
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
async fn ack_event(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<crate::mcp::EventAck>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    let authority = authority(session);
    app.db
        .call(move |c| {
            authorize_session(c, &authority, &p.id)?;
            work::ack_event(c, &p.id, &p.generation, &input.event_id, db::now())
        })
        .await?;
    app.notify.notify_one();
    Ok(Json(json!({"ok":true})))
}
async fn finish_run(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<crate::mcp::Finish>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    let lock = app.project_lock(&p.id).await;
    let _guard = lock.lock().await;
    let p = app.db.project(&p.id).await?;
    let authority = authority(session);
    let run = app
        .db
        .call(move |c| {
            authorize_session(c, &authority, &p.id)?;
            work::finish_run(
                c,
                &p.id,
                &p.generation,
                &input.run_id,
                &input.outcome,
                &input.summary,
                db::now(),
            )
        })
        .await?;
    app.notify.notify_one();
    Ok(Json(json!(run)))
}
async fn checkpoint(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<crate::mcp::Checkpoint>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    let project_path = std::path::PathBuf::from(&p.path);
    let authority = authority(session);
    let task = app
        .db
        .call(move |c| {
            authorize_session(c, &authority, &p.id)?;
            work::checkpoint(c, &p.id, &input.task_id, &input.summary, db::now())
        })
        .await?;
    let memory = project_path.join(".dauerbot/tasks");
    let note = format!(
        "# {}\n\n{}\n\n## Progress\n\n{}\n",
        task.title, task.instructions, task.checkpoint
    );
    let task_id = task.id.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        crate::config::private_dir(&memory)?;
        crate::config::write_private(&memory.join(format!("{task_id}.md")), note.as_bytes())
    })
    .await
    .context("Cannot save project memory")??;
    Ok(Json(json!(task)))
}
async fn request_decision(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<crate::mcp::Decision>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    if let Some(to) = input.to.as_deref().filter(|s| !s.is_empty()) {
        feishu::recipient_type(to)?;
    }
    let dest = input.to.clone().unwrap_or_default();
    let authority = authority(session);
    let decision = app
        .db
        .call(move |c| {
            authorize_session(c,&authority,&p.id)?;
            let tx=c.unchecked_transaction()?;
            let decision=work::request_decision(
                &tx,
                &p.id,
                &p.generation,
                &input.run_id,
                &input.question,
                &dest,
                json!([]),
                db::now(),
            )?;
            if !dest.is_empty() && let Some(binding)=crate::bot::get(&tx,&p.id)? {
                let source=format!("decision-{}",decision.id);
                let attachment=json!({"to":dest,"bot_app_id":binding.app_id});
                tx.execute("INSERT OR IGNORE INTO messages(id,project_id,direction,source_id,content,status,attachment,created_at,updated_at)
                  VALUES(?,?,'out',?,?,'pending',?,?,?)",rusqlite::params![db::id(),p.id,source,input.question,attachment.to_string(),db::now(),db::now()])?;
            }
            tx.commit()?;
            Ok(decision)
        })
        .await?;
    app.notify.notify_one();
    Ok(Json(json!(decision)))
}
async fn answer_decision(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<crate::mcp::Answer>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    let authority = authority(session);
    let event = app
        .db
        .call(move |c| {
            authorize_session(c, &authority, &p.id)?;
            work::answer_decision(c, &p.id, &input.decision_id, &input.answer, db::now())
        })
        .await?;
    app.notify.notify_one();
    Ok(Json(json!(event)))
}
#[derive(Deserialize)]
struct DecisionAnswer {
    answer: String,
}
async fn answer_decision_admin(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<DecisionAnswer>,
) -> ApiResult {
    let project = project_for_record(&app, "work_decisions", &id).await?;
    let event = app
        .db
        .call(move |c| work::answer_decision(c, &project, &id, &input.answer, db::now()))
        .await?;
    app.notify.notify_one();
    Ok(Json(json!(event)))
}
async fn register_job(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<crate::mcp::Job>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    let authority = authority(session);
    let job = app
        .db
        .call(move |c| {
            authorize_session(c, &authority, &p.id)?;
            work::register_job(
                c,
                &p.id,
                &p.generation,
                &input.run_id,
                &input.name,
                db::now(),
            )
        })
        .await?;
    Ok(Json(json!(job)))
}
async fn complete_job(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    session: Session,
    Json(input): Json<crate::mcp::JobDone>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    let authority = authority(session);
    if !["completed", "failed"].contains(&input.status.as_str()) {
        return Err(anyhow::anyhow!("Job status must be completed or failed").into());
    }
    let event = app
        .db
        .call(move |c| {
            authorize_session(c, &authority, &p.id)?;
            work::complete_job_with_status(
                c,
                &p.id,
                &p.generation,
                &input.job_id,
                &input.status,
                &input.summary,
                db::now(),
            )
        })
        .await?;
    app.notify.notify_one();
    Ok(Json(json!(event)))
}
async fn cancel_task(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let project = project_for_record(&app, "work_tasks", &id).await?;
    let result = app
        .db
        .call(move |c| work::cancel_task(c, &project, &id, db::now()))
        .await?;
    app.notify.notify_one();
    Ok(Json(json!(result)))
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
#[derive(Deserialize)]
struct JobAbandon {
    reason: String,
}
async fn abandon_job(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<JobAbandon>,
) -> ApiResult {
    let project = project_for_record(&app, "work_jobs", &id).await?;
    let lock = app.project_lock(&project).await;
    let _guard = lock.lock().await;
    let job = app
        .db
        .call(move |c| work::abandon_job(c, &project, &id, &input.reason, db::now()))
        .await?;
    app.notify.notify_one();
    Ok(Json(json!(job)))
}
