use crate::{
    App,
    db::{self, ProjectInput},
    feishu, herdr,
};
use anyhow::{Context, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
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

pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/state", get(state))
        .route("/service/shutdown", post(shutdown_service))
        .route("/settings", get(settings).put(save_settings))
        .route("/settings/check", post(check_settings))
        .route("/conversations", get(conversations))
        .route("/projects", post(create_project))
        .route(
            "/projects/{id}",
            get(project).put(update_project).delete(delete_project),
        )
        .route("/projects/{id}/start", post(start))
        .route("/projects/{id}/resume", post(resume))
        .route("/projects/{id}/agent", post(switch_agent))
        .route("/projects/{id}/stop", post(stop))
        .route("/projects/{id}/pause", post(pause))
        .route("/projects/{id}/show", post(show))
        .route("/projects/{id}/screen", get(screen))
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
async fn auth(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
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
        && ["ready", "ack", "outbox"].contains(&parts[3])
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
    Ok(Json(
        json!({"projects":app.db.projects().await?,"bot_project":app.db.bot_project().await?,"conversations":app.db.conversations().await?,"runtime":*app.runtime.read().await,"settings":feishu::public_config(&*app.config.read().await),"version":env!("CARGO_PKG_VERSION")}),
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
async fn save_settings(State(app): State<Arc<App>>, Json(input): Json<Settings>) -> ApiResult {
    let mut cfg = app.config.write().await;
    let changed_id = cfg.app_id != input.app_id.trim();
    cfg.app_id = input.app_id.trim().into();
    if input.clear_secret || changed_id {
        cfg.app_secret.clear();
    }
    if !input.app_secret.is_empty() {
        cfg.app_secret = input.app_secret;
    }
    cfg.save(&app.dir)?;
    feishu::reset_cache(&mut *app.token.lock().await);
    let result = feishu::public_config(&cfg);
    drop(cfg);
    app.settings_tx.send_modify(|v| *v += 1);

    Ok(Json(result))
}
async fn check_settings(State(app): State<Arc<App>>) -> ApiResult {
    feishu::token(&app).await?;
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
    let p = app.db.update(&id, input).await?;

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
    app.db.remove_project(&id).await?;

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
    app.db.pause(&id, input.paused).await?;
    app.notify.notify_one();

    Ok(Json(json!({"ok":true})))
}
async fn show(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(app.show(&id).await?))
}
async fn screen(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let p = app.db.project(&id).await?;
    let t = p.terminal.context("The project has no main session")?;
    let cfg = app.config.read().await.clone();
    let v = herdr::rpc(&cfg, "terminal.read", json!({"terminal":t,"lines":80})).await?;
    Ok(Json(v))
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
    Json(input): Json<Outbox>,
) -> ApiResult {
    if input.content.trim().is_empty() && input.path.is_none() {
        return Err(anyhow::anyhow!("Provide message content or a file path").into());
    }
    let attachment = if let Some(path) = input.path {
        let p = app.db.project(&id).await?;
        json!({"path":feishu::allowed_file(&p,std::path::Path::new(&path),&app.dir)?})
    } else {
        json!({})
    };
    let m = app
        .outbox_to(
            &id,
            &input.request_id,
            &input.content,
            attachment,
            input.to.as_deref(),
        )
        .await?;
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
    Json(input): Json<Ready>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    if p.terminal.as_deref() != Some(&input.terminal) || p.generation != input.generation {
        return Err(anyhow::anyhow!("Session identity does not match").into());
    }
    let cfg = app.config.read().await.clone();
    let t = herdr::rpc(&cfg, "terminal.get", json!({"terminal":input.terminal})).await?;
    herdr::check_identity(&p, &t)?;
    app.db
        .ready(
            &p.id,
            &input.generation,
            &input.terminal,
            &input.native_session,
        )
        .await?;
    app.notify.notify_one();

    Ok(Json(json!({"ok":true,"project":p.name})))
}
#[derive(Deserialize)]
struct Ack {
    message_id: String,
}
async fn ack(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<Ack>,
) -> ApiResult {
    let p = app.db.project(&id).await?;
    app.db.ack(&p.id, &input.message_id).await?;
    app.notify.notify_one();

    Ok(Json(json!({"ok":true})))
}
async fn retry(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let m = app.db.message(&id).await?;
    if !["uncertain", "failed"].contains(&m.status.as_str()) {
        return Err(anyhow::anyhow!("Only failed or uncertain messages can be retried").into());
    }
    app.db
        .state(
            &id,
            if m.direction == "in" {
                "queued"
            } else {
                "pending"
            },
            "",
        )
        .await?;
    app.notify.notify_one();

    Ok(Json(json!({"ok":true})))
}
async fn dismiss(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult {
    let m = app.db.message(&id).await?;
    if !["queued", "submitted", "uncertain", "failed", "pending"].contains(&m.status.as_str()) {
        return Err(anyhow::anyhow!("This message cannot be dismissed").into());
    }
    app.db
        .state(&id, "dismissed", "Dismissed by the user")
        .await?;
    app.notify.notify_one();

    Ok(Json(json!({"ok":true})))
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
