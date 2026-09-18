use super::*;
use crate::planning::FindingKind;

fn empty_brief() -> SourceBrief {
    SourceBrief {
        findings: vec![],
        uncertainties: vec![],
        followup_queries: vec![],
    }
}
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
        details: vec![],
        validation_unresolved: false,
    }
}

#[test]
fn relevant_details_survive_root_omission_without_a_question_list() {
    let detail = discovery(
        "cancel",
        "취소 요청은 결과 반환 전에 작업을 종료한다",
        "worker.rs",
    );
    let irrelevant = discovery("options", "서버 설정을 읽는다", "config.rs");
    let mut best = vec![];
    let terms = source::search_terms("취소 결과 반환");
    select_details(&mut best, &irrelevant, &terms);
    select_details(&mut best, &detail, &terms);
    assert_eq!(best[0].finding.topic, "cancel");
    assert_eq!(best[0].evidence[0].path, "worker.rs");
}

#[test]
fn summary_compression_keeps_distinct_details_and_replaces_corrected_topics() -> Result<()> {
    let mut prior = discovery("input", "old input", "input.rs");
    let branch = discovery("cancel", "stop pending work", "cancel.rs");
    prior.details = branch.brief.findings;
    prior.evidence.extend(branch.evidence);
    let latest = discovery("input", "corrected input", "input.rs");
    let retained = retain_details(
        &prior,
        latest.brief,
        merge_evidence(&[prior.evidence.clone(), latest.evidence]),
    );
    assert_eq!(retained.brief.findings[0].observation, "corrected input");
    assert_eq!(retained.details.len(), 1);
    assert_eq!(retained.details[0].topic, "cancel");
    let stored = serde_json::to_value(&retained)?;
    let _ = context(&retained, true);
    let _ = pack(&retained, 1024);
    assert_eq!(stored, serde_json::to_value(&retained)?);
    Ok(())
}

#[test]
fn evidence_packing_balances_files_without_dropping_stored_originals() {
    let a = discovery("a", "a", "a.rs");
    let b = discovery("b", "b", "a.rs");
    let c = discovery("c", "c", "c.rs");
    let mut d = a.clone();
    d.evidence.extend(b.evidence);
    d.evidence.extend(c.evidence);
    let size = |e: &Evidence| e.content.len() + e.path.len() + 256;
    let packed = pack(&d, size(&d.evidence[0]) + size(&d.evidence[2]));
    assert_eq!(
        packed.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        vec!["a.rs", "c.rs"]
    );
    assert_eq!(d.evidence.len(), 3);
}

#[test]
fn sections_sharing_a_branch_split_its_leaves_instead_of_repeating_the_first() -> Result<()> {
    use crate::understanding::{TreeIndex, TreeNode};
    let leaf = |topic: &str| TreeNode {
        files: vec![],
        children: vec![],
        findings: vec![Finding {
            topic: topic.into(),
            observation: format!("{topic} 처리를 설명한다"),
            kind: FindingKind::Runtime,
            evidence_ids: vec![],
        }],
        details: vec![],
        spans: vec![],
    };
    let mut nodes = std::collections::HashMap::new();
    let topics = ["취소", "재시도", "캐시", "인증", "취소 복구", "로그"];
    let leaves: Vec<String> = (0..topics.len()).map(|i| format!("leaf{i}")).collect();
    for (key, topic) in leaves.iter().zip(topics) {
        nodes.insert(key.clone(), leaf(topic));
    }
    nodes.insert(
        "branch".into(),
        TreeNode {
            children: leaves.clone(),
            ..leaf("branch")
        },
    );
    let tree = TreeIndex {
        root: Some("branch".into()),
        leaves: leaves.clone(),
        nodes,
    };
    let outline: crate::model::Outline = serde_json::from_value(json!({"sections":[
        {"title":"취소 흐름","query":"cancel","key_points":["취소와 복구"],"branches":["branch"]},
        {"title":"인증과 캐시","query":"auth","key_points":["인증","캐시"],"branches":["branch"]},
        {"title":"다른 절","query":"x","key_points":["x"],"branches":[]}
    ]}))?;
    let mine = tree.leaves_under(&["branch".to_string()]);
    let (first, left_first) = assign_leaves(&tree, &outline, 0, &mine);
    let (second, left_second) = assign_leaves(&tree, &outline, 1, &mine);
    // Each leaf has exactly one owner among the sections that share the branch.
    let mut all: Vec<&String> = first.iter().chain(&second).collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), leaves.len());
    assert_eq!(first.len() + second.len(), leaves.len());
    assert_eq!(left_first, second.len());
    assert_eq!(left_second, first.len());
    // Matching leaves go to the section they match, most relevant first.
    assert!(first.contains(&"leaf0".to_string()) && first.contains(&"leaf4".to_string()));
    assert!(second.contains(&"leaf2".to_string()) && second.contains(&"leaf3".to_string()));
    // Leaves neither matches are spread rather than all given to one section.
    let unmatched = ["leaf1", "leaf5"];
    assert!(
        unmatched.iter().any(|l| first.contains(&l.to_string()))
            && unmatched.iter().any(|l| second.contains(&l.to_string()))
    );
    // Asking for every section at once gives what asking one at a time gave.
    let all = assign_all(&tree, &outline);
    assert_eq!(all[0], (first.clone(), left_first));
    assert_eq!(all[1], (second.clone(), left_second));
    assert_eq!(all[2], (vec![], 0));
    // A branch nobody else names keeps all of its leaves.
    let alone: crate::model::Outline = serde_json::from_value(
        json!({"sections":[{"title":"전부","query":"q","key_points":[],"branches":["branch"]}]}),
    )?;
    assert_eq!(assign_leaves(&tree, &alone, 0, &mine), (mine.clone(), 0));
    Ok(())
}

#[tokio::test]
#[ignore = "requires DOCCRAFT_TEST_DB_PORT pointing to a disposable MariaDB"]
async fn summary_pipeline_skips_questions_recovers_and_reuses_general_reading() -> Result<()> {
    use axum::{Json, Router, extract::State, routing::post};
    use std::sync::Arc;
    use tokio::sync::Mutex;
    type Requests = Arc<Mutex<Vec<Value>>>;
    async fn respond(State(requests): State<Requests>, Json(payload): Json<Value>) -> Json<Value> {
        let data: Value =
            crate::llm::restore_input(payload["messages"][1]["content"].as_str().unwrap_or("{}"))
                .unwrap_or(json!({}));
        requests.lock().await.push(data.clone());
        let result = match data["phase"].as_str() {
            Some("outline_review") => json!({"issues":[]}),
            Some("purpose_reading") => {
                if data["purpose"] == "Failure direction" {
                    return Json(
                        json!({"choices":[{"message":{"content":"{bad json"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":10}}),
                    );
                }
                let anchor = data["verified_overview"]["findings"][0]["evidence_ids"][0].clone();
                let final_pass = data["final_pass"] == true;
                json!({"findings":[{"topic":if final_pass {"refund"}else{"cancel"},"observation":if final_pass {"returns refund_result"}else{"cancels pending work"},"kind":"runtime","evidence_ids":[anchor]}],
                    "uncertainties":[],"followup_queries":if final_pass || data["purpose"] == "Concise direction" {json!([])}else{json!(["detail.rs refund_result"])} })
            }
            _ => {
                let anchor = data["source_brief"]["findings"][0]["evidence_ids"][0].clone();
                json!({"reader_goal":"Understand cancellation","storyline":"Read input then cancellation","terminology":[],"sections":[
                {"title":"Input","query":"main.rs","diagrams":[],"evidence_ids":[anchor],"key_points":["Input"],"out_of_scope":[]},
                {"title":"Cancel","query":"detail.rs","diagrams":[],"evidence_ids":[anchor],"key_points":["Cancellation"],"out_of_scope":[]}]
                })
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
    run.ctx.snapshot.task.direction = "cancel refund result".into();
    let result: Result<()> = async {
        let whole = discovery("startup", "starts server", "main.rs");
        let detail = discovery("cancel", "cancel work and read refund_result", "detail.rs");
        let node = |key: &str, discovery: Discovery| Node {key:key.into(),files:vec![],children:vec![],discovery,
            unresolved_nodes:0,validation_issues:vec![],unverified_brief:None,unverified_output:None};
        let root = serde_json::to_value(node("root",whole))?;
        let leaf = serde_json::to_value(node("understanding:node:leaf",detail.clone()))?;
        for (key,value) in [("understanding:root",root.clone()),("understanding:version",json!(7)),
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
        assert!(!saved.validation_unresolved);
        assert!(saved.brief.findings.iter().any(|f|f.topic=="refund"));
        assert!(saved.details.iter().any(|f|f.topic=="cancel"));
        assert!(plan.requirements.is_empty());
        assert!(plan.sections.iter().all(|s|s.reader_question.is_empty() && s.owns_requirement_ids.is_empty()));
        let first_requests = requests.lock().await.clone();
        assert_eq!(first_requests.len(),4); // Summary, optional followup, outline, review.
        assert_eq!(first_requests[0]["phase"],"purpose_reading");
        assert!(first_requests.iter().all(|r|r.get("required_question").is_none() && r.get("requirements").is_none()));
        assert!(first_requests[0]["supporting_findings"].as_array().is_some_and(|list|list.iter().any(|f|f["topic"]=="cancel")));
        assert_eq!(first_requests[0]["final_pass"],false);
        assert_eq!(first_requests[1]["final_pass"],true);
        assert!(first_requests[1]["evidence"].as_array().is_some_and(|list|list.iter().any(|e|e["content"].as_str().is_some_and(|s|s.contains("refund_result")))));
        crate::planning::outline(&run.ctx,"Test documentation engine").await?;
        assert_eq!(requests.lock().await.len(),first_requests.len());
        // Delete only the assembled result to exercise summary checkpoint resume.
        sqlx::query("DELETE FROM checkpoints WHERE run_id=? AND step IN ('outline','source_understanding')").bind(&run.ctx.id).execute(&pool).await?;
        crate::planning::outline(&run.ctx,"Test documentation engine").await?;
        assert_eq!(requests.lock().await.len(),first_requests.len());
        run.ctx.snapshot.task.direction = "Changed direction".into();
        crate::planning::outline(&run.ctx,"Test documentation engine").await?;
        let all = requests.lock().await.clone();
        assert!(all[first_requests.len()..].iter().any(|r|r["phase"]=="purpose_reading" && r["purpose"]=="Changed direction"));
        assert!(all.iter().all(|r|r["phase"]!="understanding_batch" && r["phase"]!="understanding_reduce"));
        assert_eq!(db::load_checkpoint(&pool,&run.ctx.id,"understanding:root").await?,Some(root));
        assert_eq!(db::load_checkpoint(&pool,&run.ctx.id,"understanding:node:leaf").await?,Some(leaf));
        let before = requests.lock().await.len();
        run.ctx.snapshot.task.direction = "Concise direction".into();
        crate::planning::outline(&run.ctx,"Test documentation engine").await?;
        assert_eq!(requests.lock().await.len()-before,3); // One summary, one plan, one review.
        run.ctx.snapshot.task.direction = "Failure direction".into();
        let recovered = crate::planning::outline(&run.ctx,"Test documentation engine").await?;
        assert_eq!(recovered.sections.len(),2);
        let saved: Discovery = serde_json::from_value(db::load_checkpoint(&pool,&run.ctx.id,"source_understanding").await?.unwrap_or(json!({})))?;
        assert!(saved.validation_unresolved);
        assert!(requests.lock().await.iter().all(|r|r["phase"]!="document_intent"));
        Ok(())
    }.await;
    server.abort();
    crate::test_support::close(pool).await?;
    result
}
