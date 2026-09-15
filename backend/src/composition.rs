//! Reviewable source understanding and versioned outline editing.
use crate::{
    api::ApiError,
    db,
    model::*,
    planning,
    runner::{self, AppState},
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::Arc;
use utoipa::ToSchema;

#[derive(Deserialize)]
pub struct Page {
    after: Option<String>,
}
#[utoipa::path(get,path="/api/v1/runs/{id}/understanding",params(("id"=String,Path),("after"=Option<String>,Query)),responses((status=200,body=Value)))]
pub async fn understanding(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let pool = s.db().await?;
    let coverage = db::load_checkpoint(&pool, &id, "understanding:coverage").await?;
    let root = db::load_checkpoint(&pool, &id, "understanding:root").await?;
    let purpose = db::load_checkpoint(&pool, &id, "source_understanding").await?;
    let graph = db::load_checkpoint(&pool, &id, "graph:coverage").await?;
    let document_coverage = db::load_checkpoint(&pool, &id, "coverage:document").await?;
    let rows=sqlx::query("SELECT step,data FROM checkpoints WHERE run_id=? AND step LIKE 'understanding:node:%' AND step>? ORDER BY step LIMIT 21")
        .bind(&id).bind(page.after.unwrap_or_default()).fetch_all(&pool).await?;
    let has_more = rows.len() > 20;
    let mut nodes = vec![];
    let mut cursor = String::new();
    for row in rows.into_iter().take(20) {
        cursor = row.try_get("step")?;
        let n: crate::understanding::Node =
            serde_json::from_str(&row.try_get::<String, _>("data")?)?;
        if n.children.is_empty() {
            nodes.push(json!({"id":n.key,"files":n.files,"brief":n.discovery.brief,"validation_issues":n.validation_issues}));
        }
    }
    let counts=sqlx::query("SELECT COUNT(*) total,SUM(status='indexed') indexed,SUM(status='excluded') excluded,SUM(status='skipped') skipped FROM files WHERE run_id=?").bind(&id).fetch_one(&pool).await?;
    Ok(Json(
        json!({"coverage":coverage,"graph":graph,"document_coverage":document_coverage,"purpose":purpose.map(|v|json!({"brief":v.get("brief"),"validation_unresolved":v.get("validation_unresolved").and_then(Value::as_bool).unwrap_or(false)})),"overview":root.and_then(|v|v.pointer("/discovery/brief").cloned()),"batches":nodes,"next_cursor":cursor,"has_more":has_more,"total_files":counts.try_get::<i64,_>("total")?}),
    ))
}

#[derive(Deserialize)]
pub struct GraphPage {
    file_id: Option<u64>,
    after: Option<u64>,
}
#[utoipa::path(get,path="/api/v1/runs/{id}/graph",params(("id"=String,Path),("file_id"=Option<u64>,Query),("after"=Option<u64>,Query)),responses((status=200,body=Value)))]
pub async fn graph(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<GraphPage>,
) -> Result<Json<Value>, ApiError> {
    let pool = s.db().await?;
    if let Some(file_id) = page.file_id {
        let saved = db::load_checkpoint(&pool, &id, &format!("graph:file:{file_id}"))
            .await?
            .context("Source graph not available")?;
        let file: crate::graph::FileGraph = serde_json::from_value(saved)?;
        let offset = usize::try_from(page.after.unwrap_or(0)).unwrap_or(usize::MAX);
        let next = offset.saturating_add(50);
        let names: std::collections::HashMap<_, _> = file
            .graph
            .symbols
            .iter()
            .map(|s| (&s.id, &s.qualified_name))
            .collect();
        return Ok(Json(
            json!({"path":file.path,"supported":file.graph.supported,"parse_errors":file.graph.parse_errors,
            "symbols":file.graph.symbols.iter().skip(offset).take(50).collect::<Vec<_>>(),
            "edges":file.graph.edges.iter().skip(offset).take(50).map(|e|json!({"source_name":names.get(&e.source),"edge":e})).collect::<Vec<_>>(),
            "next_cursor":next,"has_more":next < file.graph.symbols.len().max(file.graph.edges.len())}),
        ));
    }
    let rows = sqlx::query("SELECT id,path FROM files WHERE run_id=? AND status='indexed' AND id>? ORDER BY id LIMIT 21")
        .bind(&id).bind(page.after.unwrap_or(0)).fetch_all(&pool).await?;
    let more = rows.len() > 20;
    let mut cursor = page.after.unwrap_or(0);
    let mut files = vec![];
    for row in rows.into_iter().take(20) {
        cursor = row.try_get("id")?;
        files.push(json!({"id":cursor,"path":row.try_get::<String,_>("path")?}));
    }
    Ok(Json(
        json!({"files":files,"has_more":more,"next_cursor":cursor}),
    ))
}

#[utoipa::path(get,path="/api/v1/runs/{id}/coverage",params(("id"=String,Path),("after"=Option<String>,Query)),responses((status=200,body=Value)))]
pub async fn coverage(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let pool = s.db().await?;
    let summary = db::load_checkpoint(&pool, &id, "coverage:document")
        .await?
        .unwrap_or(json!({}));
    let scope = summary["scope"].as_str().unwrap_or_default();
    let rows = sqlx::query("SELECT step,data FROM checkpoints WHERE run_id=? AND step LIKE 'coverage:item:%' AND step>? AND JSON_UNQUOTE(JSON_EXTRACT(data,'$.scope'))=? ORDER BY step LIMIT 21")
        .bind(&id).bind(page.after.unwrap_or_default()).bind(scope).fetch_all(&pool).await?;
    let more = rows.len() > 20;
    let mut cursor = String::new();
    let mut items = vec![];
    for row in rows.into_iter().take(20) {
        cursor = row.try_get("step")?;
        items.push(serde_json::from_str::<Value>(
            &row.try_get::<String, _>("data")?,
        )?);
    }
    Ok(Json(
        json!({"summary":summary,"items":items,"next_cursor":cursor,"has_more":more}),
    ))
}
#[utoipa::path(get,path="/api/v1/runs/{id}/outline",params(("id"=String,Path)),responses((status=200,body=Value)))]
pub async fn outline(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let pool = s.db().await?;
    let plan = db::load_checkpoint(&pool, &id, "outline_candidate")
        .await?
        .or(db::load_checkpoint(&pool, &id, "outline").await?);
    let revision = plan
        .as_ref()
        .and_then(|v| v["revision"].as_u64())
        .unwrap_or(0);
    let review = db::load_checkpoint(&pool, &id, &format!("outline_review:{revision}")).await?;
    Ok(Json(
        json!({"outline":plan,"review":review,"state":db::load_checkpoint(&pool,&id,"outline_state").await?}),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct Revision {
    base_revision: u32,
    request_id: String,
    outline: Option<Outline>,
    feedback: Option<String>,
}
#[utoipa::path(post,path="/api/v1/runs/{id}/outline/revisions",params(("id"=String,Path)),request_body=Revision,responses((status=200,body=Value),(status=409,description="Stale revision or active run")))]
pub async fn revise(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(edit): Json<Revision>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        tokio::spawn(async move { revise_inner(s, id, edit).await })
            .await
            .context("Outline operation interrupted")??,
    ))
}
async fn revise_inner(s: Arc<AppState>, id: String, edit: Revision) -> Result<Value> {
    let _permit = s
        .lifecycle
        .try_acquire()
        .context("Another lifecycle operation is running")?;
    ensure!(
        !edit.request_id.is_empty()
            && edit.request_id.len() <= 64
            && edit
                .request_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid request ID"
    );
    ensure!(
        edit.feedback.as_ref().is_none_or(|v| v.len() <= 16000),
        "Outline feedback exceeds 16000 bytes"
    );
    let pool = s.db().await?;
    let receipt = format!("outline_edit:{}", edit.request_id);
    if let Some(v) = db::load_checkpoint(&pool, &id, &receipt).await? {
        return Ok(v);
    }
    ensure!(
        !s.controls.lock().await.contains_key(&id),
        "CONFLICT: 실행을 중단한 뒤 구성을 수정하세요"
    );
    let current = db::load_checkpoint(&pool, &id, "outline_candidate")
        .await?
        .or(db::load_checkpoint(&pool, &id, "outline").await?)
        .context("No outline available yet")?;
    let current: Outline = serde_json::from_value(current)?;
    ensure!(
        current.revision == edit.base_revision,
        "CONFLICT: 목차가 변경되었습니다. 최신 버전을 다시 불러오세요"
    );
    let row = sqlx::query("SELECT snapshot FROM runs WHERE id=?")
        .bind(&id)
        .fetch_one(&pool)
        .await?;
    let mut snapshot: RunSnapshot =
        serde_json::from_str(&s.vault.decrypt(&row.try_get::<String, _>("snapshot")?)?)?;
    snapshot.task.preview_outline = true;
    if let Some(hash) = sqlx::query_scalar::<_, String>(
        "SELECT hash FROM artifacts WHERE run_id=? ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&id)
    .fetch_optional(&pool)
    .await?
    {
        snapshot.original_hash = Some(hash);
    }
    let next = current.revision + 1;
    let mut candidate = edit.outline;
    if let Some(plan) = candidate.as_mut() {
        plan.requirements = current.requirements.clone();
        plan.revision = next;
        let discovery: planning::Discovery = serde_json::from_value(
            db::load_checkpoint(&pool, &id, "source_understanding")
                .await?
                .context("원본 근거가 없는 과거 실행은 피드백으로 목차를 다시 생성하세요")?,
        )?;
        planning::validate_outline(plan, &discovery.evidence, snapshot.task.max_diagrams)?;
    } else {
        ensure!(
            edit.feedback.as_ref().is_some_and(|v| !v.trim().is_empty()),
            "Supply an edited outline or feedback"
        );
    }
    let result = json!({"id":id,"revision":next});
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM checkpoints WHERE run_id=? AND (step IN ('outline','outline_candidate','outline_approved','document_validation_version') OR step LIKE 'review:%' OR step LIKE 'repair:%')").bind(&id).execute(&mut *tx).await?;
    for (step, value) in [
        (receipt, result.clone()),
        (
            "outline_state".into(),
            json!({"revision":if candidate.is_some(){next}else{current.revision},"round":0}),
        ),
        (
            "outline_feedback".into(),
            json!({"previous_plan":current,"user_feedback":edit.feedback}),
        ),
    ] {
        sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,?,?) ON DUPLICATE KEY UPDATE data=VALUES(data)").bind(&id).bind(step).bind(value.to_string()).execute(&mut *tx).await?;
    }
    if let Some(plan) = candidate {
        sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,'outline_candidate',?)")
            .bind(&id)
            .bind(serde_json::to_string(&plan)?)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE runs SET status='awaiting_outline',snapshot=?,error=NULL WHERE id=?")
        .bind(s.vault.encrypt(&serde_json::to_string(&snapshot)?)?)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    runner::enqueue(s.clone(), snapshot, Some(id)).await?;
    Ok(result)
}

#[derive(Deserialize, ToSchema)]
pub struct Approval {
    revision: u32,
}
#[utoipa::path(post,path="/api/v1/runs/{id}/outline/continue",params(("id"=String,Path)),request_body=Approval,responses((status=200,body=Value),(status=409,description="Stale revision or active run")))]
pub async fn continue_outline(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(approval): Json<Approval>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        tokio::spawn(async move {
            let _permit = s
                .lifecycle
                .try_acquire()
                .context("Another lifecycle operation is running")?;
            ensure!(
                !s.controls.lock().await.contains_key(&id),
                "CONFLICT: This run is active"
            );
            let pool = s.db().await?;
            let plan: Outline = serde_json::from_value(
                db::load_checkpoint(&pool, &id, "outline_candidate")
                    .await?
                    .context("No pending outline")?,
            )?;
            ensure!(
                plan.revision == approval.revision,
                "CONFLICT: Outline revision changed"
            );
            let review: OutlineReview = serde_json::from_value(
                db::load_checkpoint(&pool, &id, &format!("outline_review:{}", plan.revision))
                    .await?
                    .context("목차 검토를 먼저 완료하세요")?,
            )?;
            ensure!(
                review.issues.iter().all(|i| i.severity != "major"),
                "주요 구성 문제를 먼저 수정하세요"
            );
            let row = sqlx::query("SELECT snapshot,status FROM runs WHERE id=?")
                .bind(&id)
                .fetch_one(&pool)
                .await?;
            ensure!(
                row.try_get::<String, _>("status")? == "awaiting_outline",
                "CONFLICT: Run is not awaiting an outline"
            );
            let snapshot: RunSnapshot =
                serde_json::from_str(&s.vault.decrypt(&row.try_get::<String, _>("snapshot")?)?)?;
            db::checkpoint(&pool, &id, "outline_approved", &json!(plan.revision)).await?;
            let id = runner::enqueue(s.clone(), snapshot, Some(id)).await?;
            Ok::<Value, anyhow::Error>(json!({"id":id}))
        })
        .await
        .context("Outline continuation interrupted")??,
    ))
}
