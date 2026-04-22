use clap::{Args, ValueEnum};
use mempal::core::priming::{PrimingReport, format_stars, format_top_wings};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PrimeFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Args)]
pub struct PrimeArgs {
    #[arg(long, value_enum, default_value_t = PrimeFormat::Text)]
    pub format: PrimeFormat,

    #[arg(
        long,
        default_value_t = mempal::core::priming::DEFAULT_PRIME_TOKEN_BUDGET,
        value_parser = parse_token_budget
    )]
    pub token_budget: usize,

    #[arg(long, default_value = mempal::core::priming::DEFAULT_PRIME_SINCE)]
    pub since: String,

    #[arg(long)]
    pub project_id: Option<String>,

    #[arg(long, default_value_t = false, conflicts_with = "no_stats")]
    pub include_stats: bool,

    #[arg(long, default_value_t = false, conflicts_with = "include_stats")]
    pub no_stats: bool,
}

impl PrimeArgs {
    pub fn want_stats(&self) -> bool {
        if self.include_stats {
            true
        } else {
            !self.no_stats
        }
    }
}

fn parse_token_budget(raw: &str) -> Result<usize, String> {
    let value = raw
        .parse::<usize>()
        .map_err(|_| format!("invalid token budget: {raw}"))?;
    if !(mempal::core::priming::MIN_PRIME_TOKEN_BUDGET
        ..=mempal::core::priming::MAX_PRIME_TOKEN_BUDGET)
        .contains(&value)
    {
        return Err(format!(
            "token budget must be between {} and {}",
            mempal::core::priming::MIN_PRIME_TOKEN_BUDGET,
            mempal::core::priming::MAX_PRIME_TOKEN_BUDGET
        ));
    }
    Ok(value)
}

pub fn render_text(report: &PrimingReport) -> String {
    let mut lines = Vec::new();
    lines.push(report.legend.clone());
    lines.push(String::new());
    lines.push("Timeline:".to_string());
    lines.extend(report.drawers.iter().map(|drawer| {
        format!(
            "{} {} {}/{} {} — {}",
            drawer.added_at,
            format_stars(drawer.importance_stars),
            drawer.wing,
            drawer.room,
            drawer.id,
            drawer.preview
        )
    }));

    if let Some(stats) = report.stats.as_ref() {
        lines.push(String::new());
        lines.push("Stats:".to_string());
        lines.push(format!("total={}", stats.total));
        lines.push(format!("recent_7d={}", stats.recent_7d));
        lines.push(format!("top_wings={}", format_top_wings(&stats.top_wings)));
        lines.push(format!("embedder_status={}", stats.embedder_status));
    }

    lines.join("\n")
}
