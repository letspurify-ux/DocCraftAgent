use super::*;
use crate::planning::FindingKind;

fn discovery(topic: &str, observation: &str, path: &str) -> Discovery {
    let content = format!("fn {topic}() {{ /* {observation} */ }}");
    let id = source::hash(format!("{path}:1:{}", source::hash(content.as_bytes())).as_bytes());
    Discovery {
        brief: SourceBrief {
            findings: vec![Finding {
                topic: topic.into(),
                observation: observation.into(),
                kind: FindingKind::Runtime,
                evidence_ids: vec![id.clone()],
            }],
            ..empty_brief()
        },
        evidence: vec![Evidence {
            id,
            path: path.into(),
            start: 1,
            end: 1,
            content,
        }],
        questions: vec![],
    }
}

#[test]
fn detail_matching_restores_question_evidence_missing_from_root() {
    let whole = discovery("startup", "Start server", "main.rs");
    let detail = discovery(
        "cancel",
        "취소 요청은 결과 반환 전에 작업을 종료한다",
        "worker.rs",
    );
    let irrelevant = discovery("options", "서버 설정을 읽는다", "config.rs");
    let mut best = vec![];
    let question = source::search_terms("취소 결과 반환");
    select_details(&mut best, &irrelevant, &question, &[]);
    select_details(&mut best, &detail, &question, &[]);
    let seed = detail_seed(&best, &whole);
    assert_eq!(seed.brief.findings[0].topic, "cancel");
    assert_eq!(seed.evidence[0].path, "worker.rs");
    assert_eq!(whole.brief.findings[0].topic, "startup");
}

#[test]
fn question_archive_survives_overview_compression_and_small_request_packing() -> Result<()> {
    let mut questions = vec![];
    let mut groups = vec![];
    for i in 0..12 {
        let d = discovery(
            &format!("result_{i}"),
            &"결과 분기 설명 ".repeat(120),
            &format!("{i}.rs"),
        );
        let mut second = d.brief.findings[0].clone();
        second.topic = format!("cancel_{i}");
        questions.push(QuestionAnalysis {
            requirement_id: format!("r{i}"),
            question: "분기별 결과?".into(),
            brief: SourceBrief {
                findings: vec![d.brief.findings[0].clone(), second],
                uncertainties: vec![format!("gap {i}")],
                followup_queries: vec![],
            },
            validation_unresolved: false,
        });
        groups.push(d.evidence);
    }
    let d = combine(questions, merge_evidence(&groups));
    assert_eq!(d.brief.findings.len(), 12);
    assert_eq!(
        d.questions
            .iter()
            .map(|q| q.brief.findings.len())
            .sum::<usize>(),
        24
    );
    assert_eq!(d.questions[11].brief.uncertainties[0], "gap 11");
    let saved = serde_json::to_value(&d)?;
    let view = context(&d);
    assert_eq!(view.len(), 12);
    assert!(
        view.iter()
            .all(|q| q["findings"].as_array().is_some_and(|f| !f.is_empty()))
    );
    assert!(serde_json::to_vec(&view)?.len() <= 20_000);
    let _ = pack(&d, 1024);
    assert_eq!(saved, serde_json::to_value(&d)?);
    Ok(())
}

#[test]
fn final_reading_keeps_distinct_first_pass_details_and_replaces_corrected_topics() {
    let mut first = discovery("input", "old input", "input.rs");
    let branch = discovery("cancel", "stop pending work", "cancel.rs");
    first.brief.findings.extend(branch.brief.findings);
    first.evidence.extend(branch.evidence);
    let last = discovery("input", "corrected input", "input.rs");
    let retained = retain_first_pass(
        Reading {
            discovery: first,
            validation_unresolved: false,
        },
        Reading {
            discovery: last,
            validation_unresolved: false,
        },
    );
    assert_eq!(retained.discovery.brief.findings.len(), 2);
    assert_eq!(
        retained.discovery.brief.findings[0].observation,
        "corrected input"
    );
    assert!(
        retained
            .discovery
            .brief
            .findings
            .iter()
            .any(|f| f.topic == "cancel")
    );
}

#[test]
fn packing_reserves_turns_for_other_questions_and_retains_raw_archive() {
    let a = discovery("a", "a", "a.rs");
    let b = discovery("b", "b", "b.rs");
    let c = discovery("c", "c", "c.rs");
    let mut first = a.brief.clone();
    first.findings.extend(b.brief.findings);
    let d = combine(
        vec![
            QuestionAnalysis {
                requirement_id: "r1".into(),
                question: "first".into(),
                brief: first,
                validation_unresolved: false,
            },
            QuestionAnalysis {
                requirement_id: "r2".into(),
                question: "second".into(),
                brief: c.brief,
                validation_unresolved: false,
            },
        ],
        merge_evidence(&[a.evidence, b.evidence, c.evidence]),
    );
    let size = |e: &Evidence| e.content.len() + e.path.len() + 256;
    let packed = pack(&d, size(&d.evidence[0]) + size(&d.evidence[2]));
    assert_eq!(
        packed.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        vec!["a.rs", "c.rs"]
    );
    assert_eq!(d.evidence.len(), 3);
}

#[tokio::test]
#[ignore = "requires DOCCRAFT_TEST_DB_PORT pointing to a disposable MariaDB"]
async fn purpose_pipeline_recovers_fetches_details_resumes_and_reuses_general_reading() -> Result<()>
{
    use axum::{Json, Router, extract::State, routing::post};
    use std::sync::Arc;
    use tokio::sync::Mutex;
    type Requests = Arc<Mutex<Vec<Value>>>;
    async fn respond(State(requests): State<Requests>, Json(payload): Json<Value>) -> Json<Value> {
        let data: Value =
            serde_json::from_str(payload["messages"][1]["content"].as_str().unwrap_or("{}"))
                .unwrap_or(json!({}));
        requests.lock().await.push(data.clone());
        let result = match data["phase"].as_str() {
            Some("document_intent") => {
                json!({"requirements":[{"id":"r1","question":"entry input"},{"id":"r2","question":"cancel refund result"}]})
            }
            Some("outline_review") => json!({"issues":[]}),
            Some("purpose_reading") => {
                if data["required_question"]["id"] == "r1" && data["purpose"] == "First direction" {
                    return Json(
                        json!({"choices":[{"message":{"content":"{bad json"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":10}}),
                    );
                }
                let anchor = data["verified_overview"]["findings"][0]["evidence_ids"][0].clone();
                let final_pass = data["final_pass"] == true;
                json!({"findings":[{"topic":if final_pass {"refund"}else{"cancel"},"observation":if final_pass {"returns refund_result"}else{"cancels pending work"},"kind":"runtime","evidence_ids":[anchor]}],
                    "uncertainties":[],"followup_queries":if final_pass {json!([])}else{json!(["detail.rs refund_result"])} })
            }
            _ => {
                let anchor = data["source_brief"]["findings"][0]["evidence_ids"][0].clone();
                json!({"reader_goal":"Understand cancellation","storyline":"Read input then cancellation","terminology":[],"sections":[
                    {"title":"Input","reader_question":"entry input?","query":"main.rs","handoff":"Continue to cancellation","diagrams":[],"prerequisite_titles":[],"evidence_ids":[anchor],"key_points":["Input"],"out_of_scope":[]},
                    {"title":"Cancel","reader_question":"cancel refund result?","query":"detail.rs","handoff":"","diagrams":[],"prerequisite_titles":["Input"],"evidence_ids":[anchor],"key_points":["Cancellation"],"out_of_scope":[]}],
                    "requirement_owners":[{"requirement_id":"r1","section_title":"Input"},{"requirement_id":"r2","section_title":"Cancel"}]})
            }
        };
        Json(
            json!({"choices":[{"message":{"content":result.to_string()},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":10}}),
        )
    }
    let pool = crate::test_support::pool(4).await?;
    let mut run = crate::test_support::TestRun::new(pool.clone())?;
    let requests: Requests = Arc::new(Mutex::new(vec![]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = Router::new()
        .route("/chat/completions", post(respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    run.ctx.snapshot.settings.llm.base_url = format!("http://{addr}");
    run.ctx.snapshot.settings.llm.model = "purpose-regression".into();
    run.ctx.snapshot.settings.llm.rpm = 1000;
    run.ctx.snapshot.settings.llm.tpm = 20_000_000;
    run.ctx.snapshot.task.direction = "First direction".into();
    let result: Result<()> = async {
        let whole = discovery("startup", "starts server", "main.rs");
        let detail = discovery("cancel", "cancel work and read refund_result", "detail.rs");
        let node = |key: &str, discovery: Discovery| Node {key:key.into(),files:vec![],children:vec![],discovery,
            unresolved_nodes:0,validation_issues:vec![],unverified_brief:None,unverified_output:None};
        let root = serde_json::to_value(node("root",whole))?;
        let leaf = serde_json::to_value(node("understanding:node:leaf",detail.clone()))?;
        for (key,value) in [("understanding:root",root.clone()),("understanding:version",json!(5)),
            ("understanding:coverage",json!({"root":"root","complete":true})),("understanding:leaves",json!(["understanding:node:leaf"])),
            ("understanding:node:leaf",leaf.clone()),("index_fingerprint",json!("stable-source"))] {
            db::checkpoint(&pool,&run.ctx.id,key,&value).await?;
        }
        let e = &detail.evidence[0];
        sqlx::query("INSERT INTO chunks(run_id,file_id,path,start_line,end_line,symbols,content) VALUES(?,1,?,1,1,'refund_result',?)")
            .bind(&run.ctx.id).bind(&e.path).bind(&e.content).execute(&pool).await?;
        let plan = crate::planning::outline(&run.ctx,"Test documentation engine").await?;
        assert_eq!(plan.sections.len(),2);
        let saved: Discovery = serde_json::from_value(db::load_checkpoint(&pool,&run.ctx.id,"source_understanding").await?.unwrap_or(json!({})))?;
        assert!(saved.questions[0].validation_unresolved);
        assert!(!saved.questions[1].validation_unresolved);
        assert!(saved.questions[1].brief.findings.iter().any(|f|f.topic=="refund"));
        assert!(crate::planning::section_evidence(&run.ctx,&plan.sections[1]).await?.iter().any(|e|e.path=="detail.rs"));
        let first_requests = requests.lock().await.clone();
        assert_eq!(first_requests[0]["phase"],"document_intent");
        let reads: Vec<_> = first_requests.iter().filter(|r|r["phase"]=="purpose_reading").collect();
        assert_eq!(reads.len(),5);
        let r2: Vec<_> = reads.iter().filter(|r|r["required_question"]["id"]=="r2").collect();
        assert_eq!(r2[0]["verified_overview"]["findings"][0]["topic"],"cancel");
        assert_eq!(r2[0]["final_pass"],false);
        assert_eq!(r2[1]["final_pass"],true);
        assert!(r2[1]["evidence"].as_array().is_some_and(|list|list.iter().any(|e|e["content"].as_str().is_some_and(|s|s.contains("refund_result")))));
        crate::planning::outline(&run.ctx,"Test documentation engine").await?;
        assert_eq!(requests.lock().await.len(),first_requests.len());
        // Delete only the assembled result to exercise per-question resume.
        sqlx::query("DELETE FROM checkpoints WHERE run_id=? AND step IN ('outline','source_understanding')").bind(&run.ctx.id).execute(&pool).await?;
        crate::planning::outline(&run.ctx,"Test documentation engine").await?;
        assert_eq!(requests.lock().await.len(),first_requests.len());
        run.ctx.snapshot.task.direction = "Changed direction".into();
        crate::planning::outline(&run.ctx,"Test documentation engine").await?;
        let all = requests.lock().await.clone();
        assert!(all[first_requests.len()..].iter().any(|r|r["phase"]=="document_intent" && r["purpose"]=="Changed direction"));
        assert!(all.iter().all(|r|r["phase"]!="understanding_batch" && r["phase"]!="understanding_reduce"));
        assert_eq!(db::load_checkpoint(&pool,&run.ctx.id,"understanding:root").await?,Some(root));
        assert_eq!(db::load_checkpoint(&pool,&run.ctx.id,"understanding:node:leaf").await?,Some(leaf));
        Ok(())
    }.await;
    server.abort();
    crate::test_support::close(pool).await?;
    result
}
