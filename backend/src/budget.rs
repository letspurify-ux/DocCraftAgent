use crate::model::LlmConfig;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Budget {
    pub input: u64,
    pub output: u64,
    pub margin: u64,
    pub context: u64,
    pub method: String,
}
pub fn estimate(value: &serde_json::Value) -> Result<u64> {
    // UTF-8 bytes are deliberately more conservative than chars/4; message framing is additional.
    Ok((serde_json::to_vec(value)?.len() as u64).saturating_add(256))
}
pub fn check(c: &LlmConfig, input: u64, extra_margin: u32) -> Result<Budget> {
    let context = c.context_limit.min(c.model_context_limit).min(200_000) as u64;
    let margin = context
        .saturating_mul((c.safety_percent + extra_margin).min(90) as u64)
        .div_ceil(100);
    let output = c.max_output_tokens as u64;
    if output > c.model_max_output as u64
        || input.saturating_add(output).saturating_add(margin) > context
    {
        bail!("CONTEXT_BUDGET: request must be split before sending");
    }
    Ok(Budget {
        input,
        output,
        margin,
        context,
        method: c.token_mode.clone(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn accepted_requests_never_exceed_limit(input in 0u64..500_000, output in 1u32..150_000, context in 1024u32..300_000) {
            let c = LlmConfig { max_output_tokens:output, model_max_output:150_000, context_limit:context, ..Default::default() };
            if let Ok(b) = check(&c, input, 0) { prop_assert!(b.input+b.output+b.margin <= b.context); prop_assert!(b.context <= 200_000); }
        }
        #[test]
        fn unicode_count_is_conservative(s in ".{0,4096}") {
            let v = serde_json::json!({"messages":[{"role":"user","content":s}]});
            prop_assert!(estimate(&v).unwrap_or(0) >= s.len() as u64);
        }
    }
    #[test]
    fn output_reservation_blocks_large_input() {
        assert!(check(&LlmConfig::default(), 199_000, 0).is_err());
    }
}
