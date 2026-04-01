use claude_core::types::message::TokenUsage;

#[derive(Debug, Clone, Copy)]
pub struct ModelCosts {
    /// USD per 1M input tokens.
    pub input_per_mtok: f64,
    /// USD per 1M output tokens.
    pub output_per_mtok: f64,
    /// USD per 1M prompt cache write tokens.
    pub cache_write_per_mtok: f64,
    /// USD per 1M prompt cache read tokens.
    pub cache_read_per_mtok: f64,
}

pub fn calculate_usd_cost(model: &str, usage: &TokenUsage) -> Option<f64> {
    let costs = model_costs(model)?;
    let mtok = 1_000_000.0;
    Some(
        (usage.input_tokens as f64 / mtok) * costs.input_per_mtok
            + (usage.output_tokens as f64 / mtok) * costs.output_per_mtok
            + (usage.cache_creation_input_tokens as f64 / mtok) * costs.cache_write_per_mtok
            + (usage.cache_read_input_tokens as f64 / mtok) * costs.cache_read_per_mtok,
    )
}

pub fn model_costs(model: &str) -> Option<ModelCosts> {
    // @see src/utils/modelCost.ts (TypeScript implementation in this repo)
    //
    // If we can't confidently map the model, return None rather than guessing.
    match model {
        // Sonnet tier: $3 input / $15 output per Mtok.
        "claude-3-5-sonnet-20241022"
        | "claude-3-7-sonnet-20250219"
        | "claude-sonnet-4-20250514"
        | "claude-sonnet-4-5-20250929"
        | "claude-sonnet-4-6" => Some(ModelCosts {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_write_per_mtok: 3.75,
            cache_read_per_mtok: 0.3,
        }),

        // Opus 4/4.1 tier: $15 input / $75 output per Mtok.
        "claude-opus-4-20250514" | "claude-opus-4-1-20250805" => Some(ModelCosts {
            input_per_mtok: 15.0,
            output_per_mtok: 75.0,
            cache_write_per_mtok: 18.75,
            cache_read_per_mtok: 1.5,
        }),

        // Opus 4.5/4.6 tier: $5 input / $25 output per Mtok (non-fast).
        "claude-opus-4-5-20251101" | "claude-opus-4-6" => Some(ModelCosts {
            input_per_mtok: 5.0,
            output_per_mtok: 25.0,
            cache_write_per_mtok: 6.25,
            cache_read_per_mtok: 0.5,
        }),

        // Haiku 3.5: $0.80 input / $4 output per Mtok.
        "claude-3-5-haiku-20241022" => Some(ModelCosts {
            input_per_mtok: 0.8,
            output_per_mtok: 4.0,
            cache_write_per_mtok: 1.0,
            cache_read_per_mtok: 0.08,
        }),

        // Haiku 4.5: $1 input / $5 output per Mtok.
        "claude-haiku-4-5-20251001" => Some(ModelCosts {
            input_per_mtok: 1.0,
            output_per_mtok: 5.0,
            cache_write_per_mtok: 1.25,
            cache_read_per_mtok: 0.1,
        }),

        _ => None,
    }
}
