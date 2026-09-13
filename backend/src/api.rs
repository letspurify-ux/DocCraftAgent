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
        let text = self.0.to_string();
        let text = if text.contains("http://") || text.contains("https://") {
            "Network operation failed; check connection settings".into()
        } else {
            text
        };
        (StatusCode::BAD_REQUEST, Json(json!({"error":text}))).into_response()
    }
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
        resume_run
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
        .route("/runs/{id}/files", get(files))
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
    s.vault.save(&new)?;
    sqlx::query(
        "INSERT INTO app_settings(id,data) VALUES(1,?) ON DUPLICATE KEY UPDATE data=VALUES(data)",
    )
    .bind(s.vault.encrypt(&serde_json::to_string(&new)?)?)
    .execute(&pool)
    .await?;
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
    Ok(Json(
        json!({"database":db_ok,"active_runs":s.controls.lock().await.len(),"llm_configured":!settings.llm.model.is_empty(),"token_mode":settings.llm.token_mode,"source_roots":settings.source_roots,"output_roots":settings.output_roots,"data_dir":s.vault.dir,"node":tokio::process::Command::new("node").arg("--version").output().await.ok().map(|o|String::from_utf8_lossy(&o.stdout).trim().to_string())}),
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
    Ok(Json(db::runs(&s.db().await?).await?))
}
#[utoipa::path(post,path="/api/v1/runs/{id}/cancel",params(("id"=String,Path)),responses((status=200)))]
async fn cancel_run(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Value> {
    let cancelled = {
        let controls = s.controls.lock().await;
        if let Some(c) = controls.get(&id) {
            let _gate = c.gate.lock.lock().unwrap_or_else(|p| p.into_inner());
            if c.gate.published.load(std::sync::atomic::Ordering::Acquire) {
                false
            } else {
                c.token.cancel();
                true
            }
        } else {
            false
        }
    };
    if cancelled {
        config::atomic_private(
            &s.vault.dir.join("terminal").join(format!("{id}.json")),
            json!({"status":"cancelled","error":"Cancelled by user"})
                .to_string()
                .as_bytes(),
        )?;
        let state = s.clone();
        let run = id.clone();
        tokio::spawn(async move {
            if let Ok(pool) = state.db().await {
                let _=sqlx::query("UPDATE runs SET cancel_requested=TRUE,status='cancelling' WHERE id=? AND status IN ('queued','running')").bind(run).execute(&pool).await;
            }
        });
    }
    Ok(Json(json!({"accepted":cancelled,"id":id})))
}
#[derive(Default, Deserialize)]
struct ResumeOptions {
    #[serde(default)]
    current_llm: bool,
    #[serde(default)]
    current_token_limit: bool,
}
#[utoipa::path(post,path="/api/v1/runs/{id}/resume",params(("id"=String,Path),("current_llm"=Option<bool>,Query,description="Apply current LLM settings while preserving completed sections and task budget"),("current_token_limit"=Option<bool>,Query,description="Apply the saved task token limit explicitly; other run limits stay fixed")),responses((status=200,body=RunId)))]
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
    let partial = status == "completed_with_warnings"
        && row
            .try_get::<String, _>("progress")?
            .contains("this document is incomplete");
    if !partial && !["failed", "cancelled", "interrupted"].contains(&status.as_str()) {
        bail_api("Only stopped or failed runs can resume")?;
    }
    let mut snapshot: RunSnapshot =
        serde_json::from_str(&s.vault.decrypt(&row.try_get::<String, _>("snapshot")?)?)?;
    if partial
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
            budget["tokens"] = json!(
                budget
                    .get("tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .saturating_sub(released)
            );
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
    if options.current_token_limit {
        let config: String = sqlx::query_scalar("SELECT config FROM tasks WHERE id=?")
            .bind(&snapshot.task.id)
            .fetch_one(&pool)
            .await?;
        snapshot.task.max_tokens = serde_json::from_str::<TaskConfig>(&config)?.max_tokens;
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
    let mut after = q
        .after
        .or_else(|| {
            headers
                .get("last-event-id")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0);
    let stream = async_stream::stream! {
        loop{
            match db::events(&pool,&id,after).await{
                Ok(events)=>{for e in events{after=e.id;if let Ok(event)=Event::default().id(e.id.to_string()).event("progress").json_data(e){yield Ok(event);}}},
                Err(_)=>{yield Ok(Event::default().event("connection").data("database temporarily unavailable"));}
            }
            if s.shutdown.is_cancelled(){break;}
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(10))))
}
async fn files(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Cursor>,
) -> ApiResult<Value> {
    let rows=sqlx::query("SELECT id,path,language,status,detail FROM files WHERE run_id=? AND id>? ORDER BY id LIMIT 200").bind(id).bind(q.after.unwrap_or(0)).fetch_all(&s.db().await?).await?;
    let mut values = vec![];
    for r in rows {
        values.push(json!({"id":r.try_get::<u64,_>("id")?,"path":r.try_get::<String,_>("path")?,"language":r.try_get::<String,_>("language")?,"status":r.try_get::<String,_>("status")?,"detail":r.try_get::<String,_>("detail")?}));
    }
    Ok(Json(json!({"files":values})))
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
}
