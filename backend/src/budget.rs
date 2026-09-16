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
/// A request is serialized twice: once into the input JSON and again into the
/// chat message string that carries it. Measured expansion on real source is
/// about 12%; reserve more for quote-heavy and non-ASCII payloads.
pub const ESCAPE_EXPANSION_PERCENT: u64 = 125;

/// Raw bytes a request may pack alongside `overhead`, mirroring `check` so that
/// callers never build a request the gate then rejects. `overhead` is everything
/// that rides along with the packed bytes: instructions, plans, retained drafts.
pub fn packing_limit(c: &LlmConfig, extra_margin: u32, overhead: usize) -> usize {
    let context = c.context_limit.min(c.model_context_limit).min(200_000) as u64;
    let margin = context
        .saturating_mul((c.safety_percent + extra_margin).min(90) as u64)
        .div_ceil(100);
    let allowed = context
        .saturating_sub(margin)
        .saturating_sub(c.max_output_tokens as u64);
    // The overhead is escaped along with the packed bytes, so the expansion
    // applies to the whole request before the overhead is taken out of it.
    let room = allowed.saturating_mul(100) / ESCAPE_EXPANSION_PERCENT;
    usize::try_from(room.saturating_sub(overhead as u64)).unwrap_or(usize::MAX)
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
    proptest! {
        #[test]
        fn a_request_packed_to_the_limit_is_never_rejected_by_the_gate(
            output in 1u32..60_000, context in 40_000u32..200_000, safety in 5u32..40,
            extra in 0u32..15, overhead in 0usize..40_000,
        ) {
            let c = LlmConfig { max_output_tokens: output, model_max_output: 150_000,
                context_limit: context, safety_percent: safety, ..Default::default() };
            let packed = packing_limit(&c, extra, overhead);
            let serialized = ((packed + overhead) as u64)
                .saturating_mul(ESCAPE_EXPANSION_PERCENT)
                / 100;
            prop_assert!(packed == 0 || check(&c, serialized, extra).is_ok());
        }
    }

    #[test]
    fn output_reservation_blocks_large_input() {
        assert!(check(&LlmConfig::default(), 199_000, 0).is_err());
    }
}
