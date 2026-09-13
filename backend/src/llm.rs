use crate::{budget, model::LlmConfig, runner::RunContext, source::hash};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use std::time::{Duration, Instant};

pub fn client(c: &LlmConfig) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(c.timeout_seconds))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none());
    if c.proxy_mode == "none" {
        b = b.no_proxy();
    } else if c.proxy_mode == "custom" {
        let mut proxy = reqwest::Proxy::all(&c.proxy_url)?;
        if !c.proxy_user.is_empty() {
            proxy = proxy.basic_auth(&c.proxy_user, &c.proxy_password);
        }
        b = b.proxy(proxy);
    }
    if !c.ca_path.is_empty() {
        b = b.add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(&c.ca_path)?)?);
    }
    Ok(b.build()?)
}
fn payload(c: &LlmConfig, messages: Value) -> Value {
    let mut p = json!({"model":c.model,"messages":messages,"stream":false});
    p[&c.output_parameter] = json!(c.max_output_tokens);
    if c.reasoning != "default" {
        if c.reasoning_parameter == "enable_thinking" {
            p["chat_template_kwargs"] = json!({"enable_thinking":c.reasoning=="on"});
        } else {
            p["reasoning_effort"] = json!(if c.reasoning == "off" {
                "none"
            } else {
                &c.effort
            });
        }
    }
    p
}
async fn read_response(response: reqwest::Response) -> Result<Value> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(next) = stream.next().await {
        let chunk = next?;
        if bytes.len().saturating_add(chunk.len()) > 4 * 1024 * 1024 {
            bail!("API_RESPONSE_LIMIT: server response exceeded 4 MiB");
        }
        bytes.extend(chunk);
    }
    serde_json::from_slice(&bytes).context("API returned invalid JSON")
}
pub async fn call(ctx: &RunContext, system: &str, mut input: Value) -> Result<String> {
    ctx.check()?;
    let c = &ctx.snapshot.settings.llm;
    crate::editorial::compact_evidence_ids(&mut input);
    let request = payload(
        c,
        json!([{"role":"system","content":system},{"role":"user","content":input.to_string()}]),
    );
    let cache_key = hash(
        serde_json::to_string(&json!({"version":1,"endpoint":c.base_url,"request":request}))?
            .as_bytes(),
    );
    if let Some(row) = sqlx::query("SELECT data FROM llm_cache WHERE hash=?")
        .bind(&cache_key)
        .fetch_optional(&ctx.pool)
        .await?
    {
        ctx.event("cache", json!({"stage":"llm","cache_hit":true}))
            .await?;
        return Ok(row.try_get("data")?);
    }
    let estimated = if c.token_mode == "server" {
        let response = ctx
            .client
            .post(&c.token_count_url)
            .bearer_auth(&c.api_key)
            .json(&request)
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("Token counting endpoint failed");
        }
        read_response(response)
            .await?
            .get("input_tokens")
            .and_then(Value::as_u64)
            .context("Counting endpoint must return input_tokens")?
    } else {
        budget::estimate(&request)?
    };
    let b = budget::check(
        c,
        estimated,
        ctx.extra_margin.load(std::sync::atomic::Ordering::Relaxed),
    )?;
    let reservation = b.input.saturating_add(b.output);
    if reservation > c.tpm as u64 {
        bail!("CONTEXT_BUDGET: request exceeds configured TPM; split input or increase TPM");
    }
    for attempt in 0..=c.retries {
        ctx.check()?;
        let _permit = tokio::select! { _ = ctx.cancel.cancelled() => { bail!("CANCELLED"); }, p=ctx.state.llm_slots.acquire() => p? };
        rate_limit(ctx, reservation).await?;
        ctx.reserve(
            reservation,
            (b.input as f64 * c.input_price + b.output as f64 * c.output_price) / 1_000_000.0,
        )?;
        ctx.event("llm_request",json!({"stage":"llm","attempt":attempt+1,"budget":b,"reserved_total":ctx.reserved_tokens.load(std::sync::atomic::Ordering::Relaxed)})).await?;
        let start = Instant::now();
        let response = tokio::select! {
            _ = ctx.cancel.cancelled() => { bail!("CANCELLED"); },
            r=ctx.client.post(format!("{}/chat/completions",c.base_url.trim_end_matches('/'))).bearer_auth(&c.api_key).json(&request).send()=>r
        };
        match response {
            Ok(response) => {
                let status = response.status();
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|s| s.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0)
                    .min(60);
                let body = match tokio::select! { _=ctx.cancel.cancelled()=>{bail!("CANCELLED");}, r=read_response(response)=>r }
                {
                    Ok(body) => body,
                    Err(e) if e.downcast_ref::<reqwest::Error>().is_some() => {
                        ctx.event("retry", json!({"stage":"retry","attempt":attempt+1,"reason":"response_body_connection_or_timeout","exhausted":attempt==c.retries})).await?;
                        if attempt == c.retries {
                            bail!("API_RETRIES_EXHAUSTED: response body connection or timeout");
                        }
                        cancellable_delay(ctx, Duration::from_secs(1u64 << attempt)).await?;
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                if status.is_success() && body.get("error").is_some() {
                    let message = body
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_lowercase();
                    let code = body
                        .pointer("/error/code")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if provider_quota_exhausted(code, &message) {
                        bail!("PROVIDER_BUDGET: provider daily quota or credits exhausted");
                    }
                    ctx.event("retry",json!({"stage":"retry","attempt":attempt+1,"reason":"provider_error_in_success_response","exhausted":attempt==c.retries})).await?;
                    if attempt == c.retries {
                        bail!(
                            "API_RETRIES_EXHAUSTED: provider returned an error instead of a completion"
                        );
                    }
                    cancellable_delay(ctx, Duration::from_secs(1u64 << attempt)).await?;
                    continue;
                }
                if status.is_success() {
                    let usage = body.get("usage").cloned().unwrap_or(json!({}));
                    let input_tokens = usage
                        .get("prompt_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(b.input);
                    let output_tokens = usage
                        .get("completion_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(b.output);
                    let reasoning = usage
                        .pointer("/completion_tokens_details/reasoning_tokens")
                        .and_then(Value::as_u64);
                    let actual_cost = (input_tokens as f64 * c.input_price
                        + output_tokens as f64 * c.output_price)
                        / 1_000_000.0;
                    let released = if usage.get("prompt_tokens").and_then(Value::as_u64).is_some()
                        && usage
                            .get("completion_tokens")
                            .and_then(Value::as_u64)
                            .is_some()
                    {
                        ctx.reconcile_tokens(
                            reservation,
                            input_tokens.saturating_add(output_tokens),
                        )
                    } else {
                        0
                    };
                    ctx.usage(input_tokens.saturating_add(output_tokens), actual_cost)
                        .await?;
                    ctx.event("llm_response",json!({"stage":"llm","input_tokens":input_tokens,"output_tokens":output_tokens,"reasoning_tokens":reasoning,"reservation_released":released,"usage_estimated":usage.get("prompt_tokens").and_then(Value::as_u64).is_none() || usage.get("completion_tokens").and_then(Value::as_u64).is_none(),"elapsed_ms":start.elapsed().as_millis() as u64,"cost":actual_cost})).await?;
                    if input_tokens > b.input {
                        ctx.extra_margin
                            .fetch_add(5, std::sync::atomic::Ordering::Relaxed);
                    }
                    if output_tokens > b.output {
                        bail!("API_OUTPUT_CONTRACT: server ignored the configured output limit");
                    }
                    let finish = body
                        .pointer("/choices/0/finish_reason")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if finish == "length" {
                        ctx.event("output_limit",json!({"stage":"repairing","output_tokens":output_tokens,"reasoning_tokens":reasoning,"reasoning_dominated":reasoning_dominated(output_tokens,reasoning),"message":if reasoning_dominated(output_tokens,reasoning) { "출력 한도의 대부분을 추론에 사용했습니다. Reasoning 설정 또는 출력 한도를 확인하세요." } else { "출력이 잘려 더 짧은 초안을 생성합니다." }})).await?;
                        bail!("OUTPUT_TRUNCATED: retry a smaller section");
                    }
                    if finish != "stop" {
                        bail!("API response did not finish normally");
                    }
                    let output = body
                        .pointer("/choices/0/message/content")
                        .and_then(Value::as_str)
                        .context("API returned no visible output")?
                        .to_string();
                    if output.trim().is_empty() {
                        bail!("API returned empty output");
                    }
                    sqlx::query("INSERT INTO llm_cache(hash,data) VALUES(?,?) ON DUPLICATE KEY UPDATE data=VALUES(data)").bind(&cache_key).bind(&output).execute(&ctx.pool).await?;
                    return Ok(output);
                }
                let code = body
                    .pointer("/error/code")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let message = body
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_lowercase();
                if provider_quota_exhausted(code, &message) {
                    bail!(
                        "PROVIDER_BUDGET: provider daily quota or credits exhausted; retry after quota reset or update the configured provider account"
                    );
                }
                if code.contains("context")
                    || message.contains("context length")
                    || message.contains("maximum context")
                {
                    ctx.extra_margin
                        .fetch_add(10, std::sync::atomic::Ordering::Relaxed);
                    bail!("CONTEXT_BUDGET: server requires smaller input");
                }
                if status.as_u16() != 429 && !status.is_server_error() {
                    bail!(
                        "API rejected request (HTTP {}); verify authentication, model and reasoning parameter support",
                        status.as_u16()
                    );
                }
                if attempt == c.retries {
                    bail!("API_RETRIES_EXHAUSTED: HTTP {}", status.as_u16());
                }
                ctx.event(
                    "retry",
                    json!({"stage":"retry","attempt":attempt+1,"http_status":status.as_u16()}),
                )
                .await?;
                cancellable_delay(ctx, Duration::from_secs(retry_after.max(1u64 << attempt)))
                    .await?;
            }
            Err(_) => {
                if attempt == c.retries {
                    bail!("API_RETRIES_EXHAUSTED: connection or timeout");
                }
                ctx.event(
                    "retry",
                    json!({"stage":"retry","attempt":attempt+1,"reason":"connection_or_timeout"}),
                )
                .await?;
                cancellable_delay(
                    ctx,
                    Duration::from_millis((1000u64 << attempt) + rand::random::<u8>() as u64),
                )
                .await?;
            }
        }
    }
    bail!("LLM retries exhausted")
}
async fn cancellable_delay(ctx: &RunContext, d: Duration) -> Result<()> {
    tokio::select! {_=ctx.cancel.cancelled()=>bail!("CANCELLED"),_=tokio::time::sleep(d)=>Ok(())}
}
async fn rate_limit(ctx: &RunContext, tokens: u64) -> Result<()> {
    loop {
        let wait = {
            let mut q = ctx.state.rate.lock().await;
            let now = Instant::now();
            while q
                .front()
                .is_some_and(|(t, _)| now.duration_since(*t) >= Duration::from_secs(60))
            {
                q.pop_front();
            }
            let sum: u64 = q.iter().map(|(_, t)| *t).sum();
            if q.len() < ctx.snapshot.settings.llm.rpm as usize
                && sum.saturating_add(tokens) <= ctx.snapshot.settings.llm.tpm as u64
            {
                q.push_back((now, tokens));
                None
            } else {
                Some(
                    q.front()
                        .map(|(t, _)| {
                            Duration::from_secs(60).saturating_sub(now.duration_since(*t))
                        })
                        .unwrap_or(Duration::from_secs(1)),
                )
            }
        };
        if let Some(wait) = wait {
            cancellable_delay(ctx, wait).await?;
        } else {
            return Ok(());
        }
    }
}
pub async fn test(c: &LlmConfig) -> Result<Value> {
    if c.model.is_empty() {
        bail!("Model is required");
    }
    let client = client(c)?;
    let mut probe = c.clone();
    probe.max_output_tokens = c.max_output_tokens.min(256);
    let request = payload(
        &probe,
        json!([{"role":"user","content":"Reply with OK only."}]),
    );
    budget::check(&probe, budget::estimate(&request)?, 0)?;
    let start = Instant::now();
    let response = client
        .post(format!(
            "{}/chat/completions",
            c.base_url.trim_end_matches('/')
        ))
        .bearer_auth(&c.api_key)
        .json(&request)
        .send()
        .await
        .context("LLM connection failed")?;
    let status = response.status();
    let body = read_response(response).await?;
    if !status.is_success() {
        bail!(
            "LLM probe rejected (HTTP {}); check model, key and reasoning mapping",
            status.as_u16()
        );
    }
    if body.pointer("/choices/0/message").is_none() {
        let code = body
            .pointer("/error/code")
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".into());
        bail!(
            "LLM probe returned no completion (provider code {code}); HTTP success alone is not a valid model response"
        );
    }
    Ok(
        json!({"ok":true,"latency_ms":start.elapsed().as_millis() as u64,"finish_reason":body.pointer("/choices/0/finish_reason"),"usage":body.get("usage"),"token_mode":c.token_mode,"note":"Probe validates connectivity and parameter acceptance; it cannot prove server token accounting."}),
    )
}
fn reasoning_dominated(output: u64, reasoning: Option<u64>) -> bool {
    output > 0 && reasoning.is_some_and(|r| r.saturating_mul(100) / output >= 75)
}
fn provider_quota_exhausted(code: &str, message: &str) -> bool {
    code == "insufficient_quota"
        || message.contains("free-models-per-day")
        || message.contains("daily quota")
        || message.contains("insufficient credits")
}
pub fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    let trimmed = text.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    let trimmed = trimmed.trim().strip_suffix("```").unwrap_or(trimmed).trim();
    serde_json::from_str(trimmed).context("LLM output does not match required JSON schema")
}

#[cfg(test)]
mod quota_tests {
    use super::*;
    #[test]
    fn exhausted_quota_is_distinct_from_transient_rate_limits() {
        assert!(reasoning_dominated(8192, Some(7712)));
        assert!(!reasoning_dominated(8192, None));
        assert!(!reasoning_dominated(8192, Some(200)));
        assert!(!reasoning_dominated(0, Some(0)));
        assert!(provider_quota_exhausted(
            "",
            "rate limit exceeded: free-models-per-day"
        ));
        assert!(provider_quota_exhausted("insufficient_quota", ""));
        assert!(!provider_quota_exhausted(
            "429",
            "rate limit exceeded: requests per minute"
        ));
    }
}
