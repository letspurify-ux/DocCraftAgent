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

/// Bytes per token, in hundredths, that a request is assumed to carry before the
/// provider has reported any usage: one byte one token, which no tokenizer
/// undercuts on source or prose.
pub const UNCALIBRATED_DENSITY: u32 = 100;
/// Samples needed before measured usage replaces the uncalibrated assumption.
pub const DENSITY_SAMPLES: u32 = 3;
/// The densest assumption calibration may reach, however sparse the samples.
pub const DENSITY_CEILING: u32 = 300;

/// Estimated input tokens for a serialized request at `density`.
pub fn estimate_at(value: &serde_json::Value, density: u32) -> Result<u64> {
    let bytes = estimate(value)?;
    Ok(bytes
        .saturating_mul(100)
        .div_ceil(density.max(UNCALIBRATED_DENSITY) as u64))
}

/// One usage report as a density sample, or nothing when it cannot be one.
///
/// A provider that reports the same prompt_tokens for every request, or a
/// template that dwarfs a tiny request, says nothing about how source bytes
/// become tokens. Real tokenizers put one to six bytes in a token.
pub fn density_sample(request_bytes: u64, prompt_tokens: u64) -> Option<u32> {
    if prompt_tokens == 0 || request_bytes < 4_096 {
        return None;
    }
    let density = request_bytes.saturating_mul(100) / prompt_tokens;
    (UNCALIBRATED_DENSITY as u64..=600)
        .contains(&density)
        .then_some(density as u32)
}

/// The density a run packs requests at, from the densest sample it has seen.
///
/// The densest sample is the one that would overrun first, and the discount
/// covers a request denser than any seen yet. Every byte counted as one token
/// meant a request used about a third of the context it was allowed.
pub fn calibrated_density(samples: u32, densest: u32) -> u32 {
    if samples < DENSITY_SAMPLES || densest == 0 {
        return UNCALIBRATED_DENSITY;
    }
    (densest.saturating_mul(85) / 100).clamp(UNCALIBRATED_DENSITY, DENSITY_CEILING)
}

/// Raw bytes a request may pack alongside `overhead`, mirroring `check` so that
/// callers never build a request the gate then rejects. `overhead` is everything
/// that rides along with the packed bytes: instructions, plans, retained drafts.
pub fn packing_limit(c: &LlmConfig, extra_margin: u32, overhead: usize) -> usize {
    packing_limit_at(c, extra_margin, UNCALIBRATED_DENSITY, overhead)
}

/// `packing_limit` for a run whose provider has reported how dense its
/// requests are.
pub fn packing_limit_at(c: &LlmConfig, extra_margin: u32, density: u32, overhead: usize) -> usize {
    let context = c.context_limit.min(c.model_context_limit).min(200_000) as u64;
    let margin = context
        .saturating_mul((c.safety_percent + extra_margin).min(90) as u64)
        .div_ceil(100);
    let allowed = context
        .saturating_sub(margin)
        .saturating_sub(c.max_output_tokens as u64);
    // The overhead is escaped along with the packed bytes, so the expansion
    // applies to the whole request before the overhead is taken out of it.
    let bytes = allowed.saturating_mul(density.max(UNCALIBRATED_DENSITY) as u64) / 100;
    let room = bytes.saturating_mul(100) / ESCAPE_EXPANSION_PERCENT;
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

    proptest! {
        #[test]
        fn a_request_packed_at_a_calibrated_density_passes_the_gate_at_that_density(
            output in 1u32..60_000, context in 40_000u32..200_000, safety in 5u32..40,
            extra in 0u32..15, overhead in 0usize..40_000, density in 100u32..=300,
        ) {
            let c = LlmConfig { max_output_tokens: output, model_max_output: 150_000,
                context_limit: context, safety_percent: safety, ..Default::default() };
            let packed = packing_limit_at(&c, extra, density, overhead);
            let bytes = ((packed + overhead) as u64).saturating_mul(ESCAPE_EXPANSION_PERCENT) / 100;
            let tokens = bytes.saturating_mul(100) / density as u64;
            prop_assert!(packed == 0 || check(&c, tokens, extra).is_ok());
        }
    }

    #[test]
    fn calibration_needs_real_samples_and_stays_conservative() {
        // A mock that reports the same count for every request is not a sample.
        assert_eq!(density_sample(90_000, 100), None);
        assert_eq!(density_sample(100, 100), None);
        assert_eq!(density_sample(90_000, 0), None);
        assert_eq!(density_sample(90_000, 30_000), Some(300));
        // Until enough samples arrive nothing changes.
        assert_eq!(calibrated_density(2, 300), UNCALIBRATED_DENSITY);
        // Then the densest sample, discounted, and never past the ceiling or
        // below one byte a token.
        assert_eq!(calibrated_density(3, 300), 255);
        assert_eq!(calibrated_density(9, 110), UNCALIBRATED_DENSITY);
        assert_eq!(calibrated_density(9, 600), DENSITY_CEILING);
        // The uncalibrated estimate is exactly the byte count it always was.
        let request = serde_json::json!({"a":"x".repeat(1000)});
        assert_eq!(
            estimate_at(&request, UNCALIBRATED_DENSITY).ok(),
            estimate(&request).ok()
        );
        assert!(estimate_at(&request, 250).ok() < estimate(&request).ok());
        let c = LlmConfig::default();
        assert!(packing_limit_at(&c, 0, 250, 0) > 2 * packing_limit(&c, 0, 0));
    }

    #[test]
    fn output_reservation_blocks_large_input() {
        assert!(check(&LlmConfig::default(), 199_000, 0).is_err());
    }
}
