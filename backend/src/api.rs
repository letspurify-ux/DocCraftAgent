use crate::{
    config, db, llm,
    model::*,
    runner::{self, AppState},
    source,
};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tower_http::services::{ServeDir, ServeFile};
use utoipa::{OpenApi, ToSchema};

pub struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, text) = public_api_error(&self.0);
        (status, Json(json!({"error":text}))).into_response()
    }
}
fn public_api_error(error: &anyhow::Error) -> (StatusCode, String) {
    if let Some(error) = error.downcast_ref::<sqlx::Error>() {
        return match error {
            sqlx::Error::RowNotFound => (StatusCode::NOT_FOUND, "Resource not found".into()),
            error if db::temporarily_unavailable(error) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Database temporarily unavailable".into(),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".into(),
            ),
        };
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return if error.is_timeout() {
            (
                StatusCode::GATEWAY_TIMEOUT,
                "Upstream request timed out".into(),
            )
        } else {
            (
                StatusCode::BAD_GATEWAY,
                "Upstream service request failed".into(),
            )
        };
    }
    if error
        .downcast_ref::<tokio::time::error::Elapsed>()
        .is_some()
    {
        return (StatusCode::GATEWAY_TIMEOUT, "Operation timed out".into());
    }
    if error.downcast_ref::<serde_json::Error>().is_some()
        || error.downcast_ref::<std::io::Error>().is_some()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal server error".into(),
        );
    }
    let text = error.to_string();
    if text.starts_with("CONFLICT:") {
        return (StatusCode::CONFLICT, text);
    }
    let text = if text.contains("http://") || text.contains("https://") {
        "Network operation failed; check connection settings".into()
    } else {
        text
    };
    (StatusCode::BAD_REQUEST, text)
}
type ApiResult<T> = std::result::Result<Json<T>, ApiError>;
#[derive(Serialize, Deserialize, ToSchema)]
pub struct RunId {
    pub id: String,
}
#[derive(Deserialize, ToSchema)]
pub struct Batch {
    pub ids: Vec<String>,
}
#[derive(Deserialize)]
pub struct Cursor {
    pub after: Option<u64>,
}
#[derive(OpenApi)]
#[openapi(
    paths(
        settings_get,
        settings_put,
        tasks_list,
        tasks_save,
        task_delete,
        start_run,
        runs_list,
        cancel_run,
        resume_run,
        crate::composition::understanding,
        crate::composition::outline,
        crate::composition::revise,
        crate::composition::continue_outline
    ),
    components(schemas(
        Settings,
        DbConfig,
        LlmConfig,
        TaskConfig,
        RunView,
        RunId,
        EventView,
        SectionPlan,
        Outline,
        Issue
    ))
)]
struct ApiDoc;
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}
pub fn router(state: Arc<AppState>) -> Router {
    let frontend =
        std::env::var("DOCCRAFT_FRONTEND_DIR").unwrap_or_else(|_| "frontend/dist".into());
    let api = Router::new()
        .route("/settings", get(settings_get).put(settings_put))
        .route("/settings/test-db", post(test_db))
        .route("/settings/test-llm", post(test_llm))
        .route("/diagnostics", get(diagnostics))
        .route("/tasks", get(tasks_list).post(tasks_save))
        .route("/tasks/{id}", axum::routing::delete(task_delete))
        .route("/tasks/{id}/run", post(start_run))
        .route("/batch", post(batch))
        .route("/runs", get(runs_list))
        .route("/runs/{id}/cancel", post(cancel_run))
        .route("/runs/{id}/resume", post(resume_run))
        .route("/runs/{id}/events", get(events))
        .route("/runs/{id}/history", get(event_history))
        .route("/runs/{id}/files", get(files))
        .route(
            "/runs/{id}/understanding",
            get(crate::composition::understanding),
        )
        .route("/runs/{id}/understanding/continue", post(resume_run))
        .route("/runs/{id}/outline", get(crate::composition::outline))
        .route(
            "/runs/{id}/outline/revisions",
            post(crate::composition::revise),
        )
        .route(
            "/runs/{id}/outline/continue",
            post(crate::composition::continue_outline),
        )
        .route("/artifacts", get(artifacts))
        .route("/artifacts/{id}", get(artifact))
        .route("/openapi.json", get(|| async { Json(ApiDoc::openapi()) }))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .nest("/api/v1", api)
        .route("/api/v1/session", get(session))
        .route("/health", get(|| async { Json(json!({"alive":true})) }))
        .fallback_service(
            ServeDir::new(&frontend)
                .not_found_service(ServeFile::new(format!("{frontend}/index.html"))),
        )
        .layer(DefaultBodyLimit::max(256 * 1024))
        .layer(middleware::from_fn(local_only))
        .with_state(state)
}
fn allowed_origin(origin: &str, backend_port: u16, frontend_port: u16) -> bool {
    reqwest::Url::parse(origin).ok().is_some_and(|u| {
        u.scheme() == "http"
            && [Some("127.0.0.1"), Some("localhost")].contains(&u.host_str())
            && [Some(backend_port), Some(frontend_port)].contains(&u.port_or_known_default())
            && u.username().is_empty()
            && u.password().is_none()
            && u.path() == "/"
            && u.query().is_none()
            && u.fragment().is_none()
    })
}
async fn local_only(req: Request, next: Next) -> Response {
    let valid = |value: &str| -> bool {
        let value = value
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let host = value.split(':').next().unwrap_or_default();
        ["127.0.0.1", "localhost"].contains(&host) && !value.contains('@') && !value.contains('/')
    };
    if !req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(valid)
    {
        return (StatusCode::FORBIDDEN, Json(json!({"error":"로컬 요청의 Host 또는 Origin이 허용되지 않습니다. 프론트엔드·백엔드 포트 설정을 확인하세요."}))).into_response();
    }
    if let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        let port = |key: &str, fallback| {
            std::env::var(key)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(fallback)
        };
        let allowed = allowed_origin(
            origin,
            port("DOCCRAFT_PORT", 8765),
            port("DOCCRAFT_FRONTEND_PORT", 6001),
        );
        if !allowed {
            return (StatusCode::FORBIDDEN, Json(json!({"error":"로컬 요청의 Host 또는 Origin이 허용되지 않습니다. 프론트엔드·백엔드 포트 설정을 확인하세요."}))).into_response();
        }
    }
    if req
        .headers()
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        == Some("cross-site")
    {
        return (StatusCode::FORBIDDEN, Json(json!({"error":"로컬 요청의 Host 또는 Origin이 허용되지 않습니다. 프론트엔드·백엔드 포트 설정을 확인하세요."}))).into_response();
    }
    let mut response = next.run(req).await;
    response.headers_mut().insert(
        "x-content-type-options",
        header::HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert("x-frame-options", header::HeaderValue::from_static("DENY"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}
async fn authenticate(State(s): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let cookie = req
        .headers()
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    let ok = cookie
        .split(';')
        .any(|c| c.trim() == format!("doccraft_session={}", s.session));
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"Initialize local session"})),
        )
            .into_response();
    }
    next.run(req).await
}
async fn session(State(s): State<Arc<AppState>>) -> Response {
    let mut response = Json(json!({"ok":true})).into_response();
    if let Ok(cookie) = header::HeaderValue::from_str(&format!(
        "doccraft_session={}; HttpOnly; SameSite=Strict; Path=/",
        s.session
    )) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}
#[utoipa::path(get,path="/api/v1/settings",responses((status=200,body=Settings)))]
async fn settings_get(State(s): State<Arc<AppState>>) -> ApiResult<Settings> {
    Ok(Json(config::redacted(s.settings.read().await.clone())))
}
#[utoipa::path(put,path="/api/v1/settings",request_body=Settings,responses((status=200,body=Settings)))]
async fn settings_put(
    State(s): State<Arc<AppState>>,
    Json(mut new): Json<Settings>,
) -> ApiResult<Settings> {
    let _permit = s
        .lifecycle
        .try_acquire()
        .context("Another configuration operation is running")?;
    if !s.controls.lock().await.is_empty() {
        bail_api("Stop active runs before changing global settings")?;
    }
    let old = s.settings.read().await.clone();
    config::preserve_secrets(&mut new, &old);
    new.source_roots = new
        .source_roots
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    new.output_roots = new
        .output_roots
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    config::validate(&new)?;
    let pool = tokio::time::timeout(Duration::from_secs(20), db::connect(&new.db, true))
        .await
        .context("Database connection timed out")??;
    // The encrypted vault is the authoritative settings store. Keeping a second,
    // unread database copy made a partial save possible when its write failed.
    s.vault.save(&new)?;
    adjust_slots(&s.jobs, old.max_jobs, new.max_jobs);
    adjust_slots(&s.llm_slots, old.llm.concurrency, new.llm.concurrency);
    *s.pool.write().await = Some(pool);
    *s.settings.write().await = new.clone();
    Ok(Json(config::redacted(new)))
}
fn adjust_slots(s: &tokio::sync::Semaphore, old: usize, new: usize) {
    if new > old {
        s.add_permits(new - old);
    } else {
        s.forget_permits(old - new);
    }
}
fn bail_api(text: &str) -> Result<()> {
    bail!("{text}")
}
async fn test_db(
    State(s): State<Arc<AppState>>,
    Json(mut input): Json<Settings>,
) -> ApiResult<Value> {
    config::preserve_secrets(&mut input, &*s.settings.read().await);
    config::validate(&input)?;
    let pool = tokio::time::timeout(Duration::from_secs(10), db::connect(&input.db, false))
        .await
        .context("DB probe timeout")??;
    let row = sqlx::query("SELECT VERSION() version")
        .fetch_one(&pool)
        .await?;
    let version: String = row.try_get("version")?;
    let tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=DATABASE()",
    )
    .fetch_one(&pool)
    .await?;
    pool.close().await;
    Ok(Json(
        json!({"ok":true,"version":version,"tables":tables,"database":input.db.database}),
    ))
}
async fn test_llm(
    State(s): State<Arc<AppState>>,
    Json(mut input): Json<Settings>,
) -> ApiResult<Value> {
    config::preserve_secrets(&mut input, &*s.settings.read().await);
    config::validate(&input)?;
    Ok(Json(llm::test(&input.llm).await?))
}
async fn diagnostics(State(s): State<Arc<AppState>>) -> ApiResult<Value> {
    let db_ok = if let Ok(pool) = s.db().await {
        tokio::time::timeout(
            Duration::from_secs(3),
            sqlx::query("SELECT 1").execute(&pool),
        )
        .await
        .is_ok_and(|r| r.is_ok())
    } else {
        false
    };
    let settings = s.settings.read().await.clone();
    let node = s
        .node_version
        .get_or_init(|| async {
            tokio::process::Command::new("node")
                .arg("--version")
                .output()
                .await
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .await
        .clone();
    Ok(Json(
        json!({"database":db_ok,"active_runs":s.controls.lock().await.len(),"llm_configured":!settings.llm.model.is_empty(),"token_mode":settings.llm.token_mode,"source_roots":settings.source_roots,"output_roots":settings.output_roots,"data_dir":s.vault.dir,"node":node}),
    ))
}
#[utoipa::path(get,path="/api/v1/tasks",responses((status=200,body=[TaskConfig])))]
async fn tasks_list(State(s): State<Arc<AppState>>) -> ApiResult<Vec<TaskConfig>> {
    let rows = sqlx::query("SELECT config FROM tasks ORDER BY updated_at DESC")
        .fetch_all(&s.db().await?)
        .await?;
    let mut tasks = vec![];
    for row in rows {
        tasks.push(serde_json::from_str(&row.try_get::<String, _>("config")?)?);
    }
    Ok(Json(tasks))
}
#[utoipa::path(post,path="/api/v1/tasks",request_body=TaskConfig,responses((status=200,body=TaskConfig)))]
async fn tasks_save(
    State(s): State<Arc<AppState>>,
    Json(mut task): Json<TaskConfig>,
) -> ApiResult<TaskConfig> {
    let settings = s.settings.read().await.clone();
    task.sources = task
        .sources
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    task.include = task
        .include
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    task.exclude = task
        .exclude
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    source::validate_task(&task, &settings)?;
    task.target = source::target_path(&task.target, &settings.output_roots)?
        .to_string_lossy()
        .into();
    task.sources = task
        .sources
        .iter()
        .map(|p| std::fs::canonicalize(p).map(|p| p.to_string_lossy().to_string()))
        .collect::<std::io::Result<Vec<_>>>()?;
    if task.id.is_empty() {
        task.id = uuid::Uuid::new_v4().to_string();
    } else {
        uuid::Uuid::parse_str(&task.id)?;
    }
    sqlx::query(
        "INSERT INTO tasks(id,config) VALUES(?,?) ON DUPLICATE KEY UPDATE config=VALUES(config)",
    )
    .bind(&task.id)
    .bind(serde_json::to_string(&task)?)
    .execute(&s.db().await?)
    .await?;
    Ok(Json(task))
}
#[utoipa::path(delete,path="/api/v1/tasks/{id}",params(("id"=String,Path)),responses((status=200)))]
async fn task_delete(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Value> {
    sqlx::query("DELETE FROM tasks WHERE id=?")
        .bind(id)
        .execute(&s.db().await?)
        .await?;
    Ok(Json(json!({"ok":true})))
}
async fn start(s: Arc<AppState>, id: String) -> Result<String> {
    let _permit = s
        .lifecycle
        .try_acquire()
        .context("Another configuration/start operation is in progress")?;
    let pool = s.db().await?;
    let settings = s.settings.read().await.clone();
    if settings.llm.model.trim().is_empty() {
        bail!("Configure and test your LLM model first");
    }
    let row = sqlx::query("SELECT config FROM tasks WHERE id=?")
        .bind(&id)
        .fetch_one(&pool)
        .await?;
    let task: TaskConfig = serde_json::from_str(&row.try_get::<String, _>("config")?)?;
    source::validate_task(&task, &settings)?;
    let original_hash = if std::path::Path::new(&task.target).exists() {
        Some(source::hash(&tokio::fs::read(&task.target).await?))
    } else {
        None
    };
    runner::enqueue(
        s.clone(),
        RunSnapshot {
            task,
            settings,
            original_hash,
        },
        None,
    )
    .await
}
#[utoipa::path(post,path="/api/v1/tasks/{id}/run",params(("id"=String,Path)),responses((status=200,body=RunId)))]
async fn start_run(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<RunId> {
    Ok(Json(RunId {
        id: start(s, id).await?,
    }))
}
async fn batch(State(s): State<Arc<AppState>>, Json(batch): Json<Batch>) -> ApiResult<Value> {
    if batch.ids.len() > 64 {
        bail_api("Batch limit is 64 tasks")?;
    }
    let mut results = vec![];
    for id in batch.ids {
        match start(s.clone(), id.clone()).await {
            Ok(run) => results.push(json!({"task_id":id,"run_id":run})),
            Err(e) => results.push(json!({"task_id":id,"error":e.to_string()})),
        }
    }
    Ok(Json(json!({"results":results})))
}
#[utoipa::path(get,path="/api/v1/runs",responses((status=200,body=[RunView])))]
async fn runs_list(State(s): State<Arc<AppState>>) -> ApiResult<Vec<RunView>> {
    let mut runs = db::runs(&s.db().await?).await?;
    // Cancellation is durable in the journal and delivered directly to the
    // worker. Reflect it immediately without a DB task that could outlive the
    // attempt and mutate a later resume.
    let controls = s.controls.lock().await;
    for run in &mut runs {
        if ["queued", "running"].contains(&run.status.as_str())
            && controls
                .get(&run.id)
                .is_some_and(|c| c.token.is_cancelled())
        {
            run.status = "cancelling".into();
        }
    }
    Ok(Json(runs))
}
#[utoipa::path(post,path="/api/v1/runs/{id}/cancel",params(("id"=String,Path)),responses((status=200)))]
async fn cancel_run(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Value> {
    let cancelled = request_cancel(&s, &id).await?;
    Ok(Json(json!({"accepted":cancelled,"id":id})))
}
async fn request_cancel(s: &AppState, id: &str) -> Result<bool> {
    let control = {
        let controls = s.controls.lock().await;
        controls
            .get(id)
            .map(|control| (control.token.clone(), control.gate.clone()))
    };
    if let Some((token, gate)) = control {
        // Do not hold the global registry while waiting for the short publication
        // boundary. Revalidate under a single gate -> registry lock order so a
        // finishing worker cannot be cancelled after removing its control entry.
        let _gate_guard = gate.lock.lock().await;
        let controls = s.controls.lock().await;
        if controls
            .get(id)
            .is_some_and(|control| Arc::ptr_eq(&control.gate, &gate))
            && !gate.published.load(std::sync::atomic::Ordering::Acquire)
        {
            if token.is_cancelled() {
                return Ok(true);
            }
            // Persist intent before waking the worker, while its registry entry
            // cannot be removed. No deferred DB update may outlive this run and
            // accidentally cancel a later resume of the same ID.
            config::atomic_private(
                &s.vault.dir.join("terminal").join(format!("{id}.json")),
                json!({"status":"cancelled","error":"Cancelled by user"})
                    .to_string()
                    .as_bytes(),
            )?;
            token.cancel();
            Ok(true)
        } else {
            Ok(false)
        }
    } else {
        Ok(false)
    }
}
#[derive(Default, Deserialize)]
struct ResumeOptions {
    #[serde(default)]
    current_llm: bool,
    #[serde(default)]
    current_token_limit: bool,
    #[serde(default)]
    current_review_limit: bool,
}
#[utoipa::path(post,path="/api/v1/runs/{id}/resume",params(("id"=String,Path),("current_llm"=Option<bool>,Query,description="Apply current LLM settings while preserving completed sections and task budget"),("current_token_limit"=Option<bool>,Query,description="Apply the saved task token limit explicitly; other run limits stay fixed"),("current_review_limit"=Option<bool>,Query,description="Apply the saved task review limit so a warning-completed run can continue from its review checkpoints")),responses((status=200,body=RunId)))]
async fn resume_run(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(options): Query<ResumeOptions>,
) -> ApiResult<RunId> {
    let _permit = s
        .lifecycle
        .try_acquire()
        .context("Another lifecycle operation is running")?;
    let pool = s.db().await?;
    let row = sqlx::query("SELECT snapshot,status,progress FROM runs WHERE id=?")
        .bind(&id)
        .fetch_one(&pool)
        .await?;
    let status: String = row.try_get("status")?;
    let completed_with_warnings = status == "completed_with_warnings";
    let partial = completed_with_warnings
        && row
            .try_get::<String, _>("progress")?
            .contains("this document is incomplete");
    if !completed_with_warnings
        && ![
            "failed",
            "cancelled",
            "interrupted",
            "awaiting_source",
            "awaiting_outline",
        ]
        .contains(&status.as_str())
    {
        bail_api("Only stopped, failed, or warning-completed runs can resume")?;
    }
    let mut snapshot: RunSnapshot =
        serde_json::from_str(&s.vault.decrypt(&row.try_get::<String, _>("snapshot")?)?)?;
    let previous_review_limit = snapshot.task.max_iterations;
    if completed_with_warnings
        && let Some(artifact) = sqlx::query(
            "SELECT hash FROM artifacts WHERE run_id=? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&id)
        .fetch_optional(&pool)
        .await?
    {
        snapshot.original_hash = Some(artifact.try_get("hash")?);
    }
    // Credentials may have been repaired; analysis settings and sources stay fixed.
    let settings = s.settings.read().await.clone();
    if options.current_llm {
        snapshot.settings.llm = settings.llm;
        config::validate(&snapshot.settings)?;
        // Only reclaim confirmed legacy over-reservations. Unknown/failed calls remain charged.
        if db::load_checkpoint(&pool, &id, "legacy_reservations_reconciled")
            .await?
            .is_none()
        {
            let rows = sqlx::query("SELECT kind,data FROM events WHERE run_id=? AND kind IN ('llm_request','llm_response') ORDER BY id").bind(&id).fetch_all(&pool).await?;
            let mut pending = 0u64;
            let mut released = 0u64;
            for row in rows {
                let data: Value = serde_json::from_str(&row.try_get::<String, _>("data")?)?;
                if row.try_get::<String, _>("kind")? == "llm_request" {
                    pending = data
                        .pointer("/budget/input")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        .saturating_add(
                            data.pointer("/budget/output")
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                        );
                } else {
                    if data.get("reservation_released").is_none()
                        && data.get("usage_estimated").and_then(Value::as_bool) == Some(false)
                        && let (Some(input), Some(output)) = (
                            data.get("input_tokens").and_then(Value::as_u64),
                            data.get("output_tokens").and_then(Value::as_u64),
                        )
                    {
                        released = released
                            .saturating_add(pending.saturating_sub(input.saturating_add(output)));
                    }
                    pending = 0;
                }
            }
            let mut budget = db::load_checkpoint(&pool, &id, "budget")
                .await?
                .unwrap_or(json!({}));
            let tokens = json!(
                budget
                    .get("tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .saturating_sub(released)
            );
            budget
                .as_object_mut()
                .context("Invalid budget checkpoint: expected an object")?
                .insert("tokens".into(), tokens);
            let mut tx = pool.begin().await?;
            sqlx::query("UPDATE checkpoints SET data=? WHERE run_id=? AND step='budget'")
                .bind(budget.to_string())
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,'legacy_reservations_reconciled',?)").bind(&id).bind(json!(released).to_string()).execute(&mut *tx).await?;
            tx.commit().await?;
        }
    } else {
        snapshot.settings.llm.api_key = settings.llm.api_key;
        snapshot.settings.llm.proxy_password = settings.llm.proxy_password;
    }
    let current_task = if options.current_token_limit || options.current_review_limit {
        let config: String = sqlx::query_scalar("SELECT config FROM tasks WHERE id=?")
            .bind(&snapshot.task.id)
            .fetch_one(&pool)
            .await?;
        Some(serde_json::from_str::<TaskConfig>(&config)?)
    } else {
        None
    };
    if options.current_token_limit {
        snapshot.task.max_tokens = current_task
            .as_ref()
            .context("Missing current task")?
            .max_tokens;
    }
    if options.current_review_limit {
        snapshot.task.max_iterations = current_task
            .as_ref()
            .context("Missing current task")?
            .max_iterations;
    }
    if completed_with_warnings
        && !partial
        && (!options.current_review_limit || snapshot.task.max_iterations <= previous_review_limit)
    {
        bail_api("Increase the task review limit and apply it to continue this reviewed run")?;
    }
    source::validate_task(&snapshot.task, &snapshot.settings)?;
    Ok(Json(RunId {
        id: runner::enqueue(s.clone(), snapshot, Some(id)).await?,
    }))
}
async fn events(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Cursor>,
    headers: HeaderMap,
) -> std::result::Result<
    Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>>,
    ApiError,
> {
    let pool = s.db().await?;
    let mut after = event_cursor(q.after, &headers);
    let stream = async_stream::stream! {
        let mut idle_polls = 0u8;
        loop{
            let mut full_page = false;
            match db::events(&pool,&id,after).await{
                Ok(events)=>{
                    full_page = events.len() == 200;
                    for e in events{after=e.id;if let Ok(event)=Event::default().id(e.id.to_string()).event("progress").json_data(e){yield Ok(event);}}
                    if full_page {
                        idle_polls = 0;
                    } else {
                        idle_polls = idle_polls.saturating_add(1);
                    }
                },
                Err(_)=>{yield Ok(Event::default().event("connection").data("database temporarily unavailable"));}
            }
            if idle_polls == 1 || idle_polls >= 20 {
                idle_polls = 0;
                match sqlx::query_scalar::<_,String>("SELECT status FROM runs WHERE id=?").bind(&id).fetch_optional(&pool).await {
                    Ok(Some(status)) if stream_terminal(&status) => {
                        // Completion may commit after the first event query. Drain its
                        // events before closing; retry on DB failure without losing the cursor.
                        match db::events(&pool, &id, after).await {
                            Ok(tail) if tail.is_empty() => {
                                yield Ok(Event::default().event("stream-end").data("terminal"));
                                break;
                            },
                            Ok(tail) => {
                                for e in tail {
                                    after = e.id;
                                    if let Ok(event) = Event::default().id(e.id.to_string()).event("progress").json_data(e) {
                                        yield Ok(event);
                                    }
                                }
                                continue;
                            },
                            Err(_) => { yield Ok(Event::default().event("connection").data("database temporarily unavailable")); }
                        }
                    },
                    Ok(None) => {
                        yield Ok(Event::default().event("stream-end").data("not-found"));
                        break;
                    },
                    _ => {}
                }
            }
            if s.shutdown.is_cancelled(){break;}
            if !full_page {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(10))))
}
fn event_cursor(after: Option<u64>, headers: &HeaderMap) -> u64 {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .or(after)
        .unwrap_or(0)
}

fn stream_terminal(status: &str) -> bool {
    !["queued", "running", "cancelling", "interrupted"].contains(&status)
}
async fn event_history(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Value> {
    let rows =
        sqlx::query("SELECT id,kind,data FROM events WHERE run_id=? ORDER BY id DESC LIMIT 500")
            .bind(id)
            .fetch_all(&s.db().await?)
            .await?;
    let mut events = vec![];
    for row in rows.into_iter().rev() {
        events.push(json!({"id":row.try_get::<u64,_>("id")?,"kind":row.try_get::<String,_>("kind")?,"data":serde_json::from_str::<Value>(&row.try_get::<String,_>("data")?)?}));
    }
    Ok(Json(json!({"events":events})))
}
async fn files(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Cursor>,
) -> ApiResult<Value> {
    let mut rows=sqlx::query("SELECT id,path,language,status,detail FROM files WHERE run_id=? AND id>? ORDER BY id LIMIT 201").bind(id).bind(q.after.unwrap_or(0)).fetch_all(&s.db().await?).await?;
    let has_more = rows.len() > 200;
    rows.truncate(200);
    let mut values = vec![];
    for r in rows {
        values.push(json!({"id":r.try_get::<u64,_>("id")?,"path":r.try_get::<String,_>("path")?,"language":r.try_get::<String,_>("language")?,"status":r.try_get::<String,_>("status")?,"detail":r.try_get::<String,_>("detail")?}));
    }
    Ok(Json(json!({"files":values,"has_more":has_more})))
}
async fn artifacts(State(s): State<Arc<AppState>>) -> ApiResult<Value> {
    let rows=sqlx::query("SELECT id,run_id,task_id,path,warnings,CAST(created_at AS CHAR) created_at FROM artifacts ORDER BY created_at DESC LIMIT 200").fetch_all(&s.db().await?).await?;
    let mut values = vec![];
    for r in rows {
        values.push(json!({"id":r.try_get::<String,_>("id")?,"run_id":r.try_get::<String,_>("run_id")?,"task_id":r.try_get::<String,_>("task_id")?,"path":r.try_get::<String,_>("path")?,"warnings":serde_json::from_str::<Value>(&r.try_get::<String,_>("warnings")?)?,"created_at":r.try_get::<String,_>("created_at")?}));
    }
    Ok(Json(json!({"artifacts":values})))
}
async fn artifact(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Value> {
    let r = sqlx::query("SELECT markdown,path FROM artifacts WHERE id=?")
        .bind(id)
        .fetch_one(&s.db().await?)
        .await?;
    Ok(Json(
        json!({"markdown":r.try_get::<String,_>("markdown")?,"path":r.try_get::<String,_>("path")?}),
    ))
}

#[cfg(test)]
mod origin_tests {
    use super::*;
    #[tokio::test]
    async fn cancellation_is_not_signalled_when_its_journal_cannot_be_saved() -> Result<()> {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy_with(sqlx::mysql::MySqlConnectOptions::new());
        let run = crate::test_support::TestRun::new(pool)?;
        let state = &run.ctx.state;
        state.controls.lock().await.insert(
            run.ctx.id.clone(),
            runner::Control {
                token: run.ctx.cancel.clone(),
                target: run.ctx.snapshot.task.target.clone(),
                gate: run.ctx.gate.clone(),
            },
        );
        std::fs::write(state.vault.dir.join("terminal"), "blocks journal directory")?;
        assert!(
            cancel_run(State(state.clone()), Path(run.ctx.id.clone()))
                .await
                .is_err()
        );
        assert!(!run.ctx.cancel.is_cancelled());
        Ok(())
    }

    #[tokio::test]
    async fn repeated_cancellation_does_not_recreate_a_finished_journal() -> Result<()> {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy_with(sqlx::mysql::MySqlConnectOptions::new());
        let run = crate::test_support::TestRun::new(pool)?;
        let state = &run.ctx.state;
        state.controls.lock().await.insert(
            run.ctx.id.clone(),
            runner::Control {
                token: run.ctx.cancel.clone(),
                target: run.ctx.snapshot.task.target.clone(),
                gate: run.ctx.gate.clone(),
            },
        );
        assert!(request_cancel(state, &run.ctx.id).await?);
        let journal = state
            .vault
            .dir
            .join("terminal")
            .join(format!("{}.json", run.ctx.id));
        assert!(journal.exists());
        // The finishing worker removes the durable journal before releasing its
        // control entry. Another cancellation must not leave a stale record.
        std::fs::remove_file(&journal)?;
        assert!(request_cancel(state, &run.ctx.id).await?);
        assert!(!journal.exists());
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DOCCRAFT_TEST_DB_PORT pointing to a disposable MariaDB"]
    async fn malformed_resume_budget_returns_an_error_without_panicking() -> Result<()> {
        use futures_util::FutureExt;
        let pool = crate::test_support::pool(1).await?;
        let run = crate::test_support::TestRun::new(pool.clone())?;
        sqlx::query(
            "INSERT INTO runs(id,task_id,status,snapshot,progress) VALUES(?,?,'failed',?,'{}')",
        )
        .bind(&run.ctx.id)
        .bind(&run.ctx.snapshot.task.id)
        .bind(
            run.ctx
                .state
                .vault
                .encrypt(&serde_json::to_string(&run.ctx.snapshot)?)?,
        )
        .execute(&pool)
        .await?;
        db::checkpoint(&pool, &run.ctx.id, "budget", &json!([])).await?;
        let result = std::panic::AssertUnwindSafe(resume_run(
            State(run.ctx.state.clone()),
            Path(run.ctx.id.clone()),
            Query(ResumeOptions {
                current_llm: true,
                ..Default::default()
            }),
        ))
        .catch_unwind()
        .await;
        crate::test_support::close(pool).await?;
        assert!(
            result.is_ok(),
            "malformed checkpoint panicked in HTTP handler"
        );
        assert!(result.is_ok_and(|r| r.is_err()));
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DOCCRAFT_TEST_DB_PORT pointing to a disposable MariaDB"]
    async fn cancelled_runs_are_visible_while_the_worker_finishes() -> Result<()> {
        let pool = crate::test_support::pool(1).await?;
        let run = crate::test_support::TestRun::new(pool.clone())?;
        let state = &run.ctx.state;
        state.controls.lock().await.insert(
            run.ctx.id.clone(),
            runner::Control {
                token: run.ctx.cancel.clone(),
                target: run.ctx.snapshot.task.target.clone(),
                gate: run.ctx.gate.clone(),
            },
        );
        sqlx::query(
            "INSERT INTO runs(id,task_id,status,snapshot,progress) VALUES(?,?,'running','','{}')",
        )
        .bind(&run.ctx.id)
        .bind(&run.ctx.snapshot.task.id)
        .execute(&pool)
        .await?;
        let result: Result<()> = async {
            assert!(request_cancel(state, &run.ctx.id).await?);
            let Json(runs) = runs_list(State(state.clone())).await.map_err(|e| e.0)?;
            assert_eq!(runs.first().map(|r| r.status.as_str()), Some("cancelling"));
            Ok(())
        }
        .await;
        crate::test_support::close(pool).await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires DOCCRAFT_TEST_DB_PORT pointing to a disposable MariaDB"]
    async fn mysql_error_numbers_control_recovery_and_http_status() -> Result<()> {
        let pool = crate::test_support::pool(1).await?;
        for (number, state, retry, status) in [
            (1213, "40001", true, StatusCode::SERVICE_UNAVAILABLE),
            (1205, "HY000", true, StatusCode::SERVICE_UNAVAILABLE),
            (1040, "08004", false, StatusCode::SERVICE_UNAVAILABLE),
            (1062, "23000", false, StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            let error = sqlx::query(&format!(
                "SIGNAL SQLSTATE '{state}' SET MYSQL_ERRNO={number}, MESSAGE_TEXT='regression'"
            ))
            .execute(&pool)
            .await
            .err()
            .context("Expected server error")?;
            assert_eq!(db::transaction_conflict(&error), retry);
            assert_eq!(
                db::temporarily_unavailable(&error),
                status == StatusCode::SERVICE_UNAVAILABLE
            );
            assert_eq!(public_api_error(&error.into()).0, status);
        }
        crate::test_support::close(pool).await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DOCCRAFT_TEST_DB_PORT pointing to a disposable MariaDB"]
    async fn disconnected_resume_still_starts_and_cancels_its_worker() -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let pool = crate::test_support::pool(5).await?;
        let run = crate::test_support::TestRun::new(pool.clone())?;
        let state = &run.ctx.state;
        let id = &run.ctx.id;
        sqlx::query(
            "INSERT INTO runs(id,task_id,status,snapshot,progress) VALUES(?,?,'failed',?,'{}')",
        )
        .bind(id)
        .bind(&run.ctx.snapshot.task.id)
        .bind(
            state
                .vault
                .encrypt(&serde_json::to_string(&run.ctx.snapshot)?)?,
        )
        .execute(&pool)
        .await?;
        let mut blocker = pool.begin().await?;
        sqlx::query("SELECT id FROM runs WHERE id=? FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *blocker)
            .await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let app = router(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let result: Result<()> = async {
            let mut client = tokio::net::TcpStream::connect(address).await?;
            client.write_all(format!(
                "POST /api/v1/runs/{id}/resume HTTP/1.1\r\nHost: localhost\r\nCookie: doccraft_session={}\r\nContent-Length: 0\r\n\r\n", state.session
            ).as_bytes()).await?;
            tokio::time::timeout(Duration::from_secs(2), async {
                while !state.controls.lock().await.contains_key(id) {
                    tokio::task::yield_now().await;
                }
            }).await?;
            drop(client);
            // Wait until Axum drops the request's lifecycle permit, confirming
            // cancellation before the blocked database registration completes.
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if state.lifecycle.try_acquire().is_ok() { break; }
                    tokio::task::yield_now().await;
                }
            }).await?;
            assert!(request_cancel(state, id).await?);
            blocker.rollback().await?;
            tokio::time::timeout(Duration::from_secs(3), async {
                while state.controls.lock().await.contains_key(id) {
                    tokio::task::yield_now().await;
                }
            }).await?;
            let status: String = sqlx::query_scalar("SELECT status FROM runs WHERE id=?")
                .bind(id).fetch_one(&pool).await?;
            assert_eq!(status, "cancelled");
            assert!(!request_cancel(state, id).await?);
            Ok(())
        }.await;
        server.abort();
        let _ = server.await;
        crate::test_support::close(pool).await?;
        result
    }
    #[test]
    fn frontend_origin_matches_configured_ports_only() {
        assert!(allowed_origin("http://127.0.0.1:6001", 8765, 6001));
        assert!(allowed_origin("http://localhost:6001", 8765, 6001));
        assert!(allowed_origin("http://127.0.0.1:8765", 8765, 6001));
        assert!(allowed_origin("http://localhost:16001", 18765, 16001));
        for origin in [
            "http://localhost:5173",
            "http://localhost:6002",
            "http://evil.test:6001",
            "null",
            "http://evil@localhost:6001",
            "http://localhost:6001/extra",
        ] {
            assert!(!allowed_origin(origin, 8765, 6001), "{origin}");
        }
    }

    #[test]
    fn api_errors_have_safe_and_meaningful_status_codes() {
        let response = ApiError(sqlx::Error::RowNotFound.into()).into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = ApiError(sqlx::Error::PoolClosed.into()).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let response =
            ApiError(sqlx::Error::ColumnNotFound("missing".into()).into()).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let response = ApiError(anyhow::anyhow!("Invalid task configuration")).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn reconnect_cursor_uses_last_received_event_before_initial_query() -> Result<()> {
        let mut headers = HeaderMap::new();
        assert_eq!(event_cursor(Some(10), &headers), 10);
        headers.insert("last-event-id", "25".parse()?);
        assert_eq!(event_cursor(Some(10), &headers), 25);
        headers.insert("last-event-id", "invalid".parse()?);
        assert_eq!(event_cursor(Some(10), &headers), 10);
        assert_eq!(event_cursor(None, &headers), 0);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires DOCCRAFT_TEST_DB_PORT pointing to a disposable MariaDB"]
    async fn event_stream_drains_events_committed_during_status_check() -> Result<()> {
        use futures_util::StreamExt;
        let pool = crate::test_support::pool(3).await?;
        let run = crate::test_support::TestRun::new(pool.clone())?;
        let result: Result<()> = async {
            sqlx::query("INSERT INTO runs(id,task_id,status,snapshot,progress) VALUES(?,?,'running','{}','{}')")
                .bind(&run.ctx.id).bind(&run.ctx.snapshot.task.id).execute(&pool).await?;
            sqlx::query("INSERT INTO events(run_id,kind,data) VALUES(?,'started','{}')")
                .bind(&run.ctx.id).execute(&pool).await?;
            let response = events(State(run.ctx.state.clone()), Path(run.ctx.id.clone()),
                Query(Cursor { after: None }), HeaderMap::new()).await.map_err(|e| e.0)?;
            let mut body = response.into_response().into_body().into_data_stream();
            let first = body.next().await.context("Missing initial event")??;
            assert!(String::from_utf8_lossy(&first).contains("started"));
            let mut tx = pool.begin().await?;
            sqlx::query("INSERT INTO events(run_id,kind,data) VALUES(?,'terminal','{}')")
                .bind(&run.ctx.id).execute(&mut *tx).await?;
            sqlx::query("UPDATE runs SET status='completed' WHERE id=?")
                .bind(&run.ctx.id).execute(&mut *tx).await?;
            tx.commit().await?;
            let last = tokio::time::timeout(Duration::from_secs(2), body.next()).await?
                .context("Missing final event")??;
            let last = String::from_utf8_lossy(&last);
            assert!(last.contains("event: progress") && last.contains("terminal"), "{last}");
            let end = tokio::time::timeout(Duration::from_secs(2), body.next()).await?
                .context("Missing stream end")??;
            assert!(String::from_utf8_lossy(&end).contains("stream-end"));
            Ok(())
        }.await;
        crate::test_support::close(pool).await?;
        result
    }

    #[test]
    fn event_stream_uses_current_run_status_not_historical_terminal_events() {
        for active in ["queued", "running", "cancelling", "interrupted"] {
            assert!(!stream_terminal(active));
        }
        for terminal in [
            "completed",
            "completed_with_warnings",
            "failed",
            "cancelled",
        ] {
            assert!(stream_terminal(terminal));
        }
    }

    #[tokio::test]
    async fn cancellation_wait_does_not_lock_the_control_registry() -> anyhow::Result<()> {
        let data = tempfile::tempdir()?;
        let state = Arc::new(AppState::new(
            config::Vault::open(data.path().to_path_buf())?,
            Settings::default(),
            None,
        ));
        let token = tokio_util::sync::CancellationToken::new();
        let gate = Arc::new(runner::CommitGate {
            lock: tokio::sync::Mutex::new(()),
            published: std::sync::atomic::AtomicBool::new(false),
        });
        state.controls.lock().await.insert(
            "run".into(),
            runner::Control {
                token: token.clone(),
                target: "target.md".into(),
                gate: gate.clone(),
            },
        );

        let held = gate.lock.lock().await;
        let request = tokio::spawn({
            let state = state.clone();
            async move { request_cancel(&state, "run").await }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while Arc::strong_count(&gate) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await?;

        let registry = tokio::time::timeout(Duration::from_millis(100), state.controls.lock())
            .await
            .context("cancellation held the control registry while waiting for publication")?;
        drop(registry);
        drop(held);
        assert!(request.await??);
        assert!(token.is_cancelled());
        Ok(())
    }

    #[tokio::test]
    async fn stale_cancellation_cannot_override_a_finished_run() -> anyhow::Result<()> {
        let data = tempfile::tempdir()?;
        let state = Arc::new(AppState::new(
            config::Vault::open(data.path().to_path_buf())?,
            Settings::default(),
            None,
        ));
        let token = tokio_util::sync::CancellationToken::new();
        let gate = Arc::new(runner::CommitGate {
            lock: tokio::sync::Mutex::new(()),
            published: std::sync::atomic::AtomicBool::new(false),
        });
        state.controls.lock().await.insert(
            "run".into(),
            runner::Control {
                token: token.clone(),
                target: "target.md".into(),
                gate: gate.clone(),
            },
        );

        let held = gate.lock.lock().await;
        let request = tokio::spawn({
            let state = state.clone();
            async move { request_cancel(&state, "run").await }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while Arc::strong_count(&gate) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        state.controls.lock().await.remove("run");
        drop(held);

        assert!(!request.await??);
        assert!(!token.is_cancelled());
        Ok(())
    }
}
