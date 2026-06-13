use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde::Serialize;

use mempal::core::{
    db::Database,
    design_insights::{
        DesignInsight, DesignInsightFilters, DesignInsightScope, DesignInsightSource,
        DesignInsightStatus, DesignInsightTargetArtifact, NewDesignInsight, list_design_insights,
        record_design_insight, resolve_design_insight,
    },
};

#[derive(Debug, Clone, Subcommand)]
pub enum InsightCommands {
    /// Record a content-free, redacted design insight for later draining.
    Record {
        /// user-idea, review-finding, tool-friction, incident, or research.
        #[arg(long)]
        source: String,
        /// project, cross-project, repo, or issue.
        #[arg(long)]
        scope: String,
        /// memory, skill, agents-rule, agents-rules-ref, codex-skill, github-issue, or mempal-knowledge.
        #[arg(long = "target", alias = "target-artifact")]
        target_artifact: String,
        /// Content-free evidence reference: issue URL, session id, review id, or incident id.
        #[arg(long)]
        evidence: String,
        /// Redacted, content-free summary of the reusable design insight.
        #[arg(long)]
        summary: String,
        /// Proposed acceptance criteria or reusable rule text.
        #[arg(long = "rule", alias = "acceptance", alias = "acceptance-criteria")]
        rule_text: Option<String>,
        /// Value/priority from 1 to 5. Values 4-5 surface in status/doctor reminders.
        #[arg(long, default_value_t = 3)]
        priority: u8,
        /// Optional project id for filtering/audit context.
        #[arg(long)]
        project: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List design insights, open by default, for autonomous drain workflows.
    List {
        /// open, resolved, or all.
        #[arg(long, default_value = "open")]
        status: String,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long = "target", alias = "target-artifact")]
        target_artifact: Option<String>,
        /// Minimum priority to show.
        #[arg(long, default_value_t = 1)]
        min_priority: u8,
        /// Maximum number of rows to print.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Mark an insight as drained into its target artifact.
    Resolve {
        /// Insight ID returned by `mempal insight record` or `list`.
        insight_id: String,
        /// Optional resolving actor/tool id.
        #[arg(long)]
        actor: Option<String>,
        /// Optional content-free resolution note.
        #[arg(long)]
        note: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the design-opportunity pass workflow for dev2merge/issue-drain.
    Runbook {
        #[arg(long, default_value = "plain")]
        format: String,
    },
}

#[derive(Debug, Serialize)]
struct ResolveOutput<'a> {
    insight_id: &'a str,
    resolved: bool,
}

pub fn run_command(db: &Database, command: InsightCommands) -> Result<()> {
    match command {
        InsightCommands::Record {
            source,
            scope,
            target_artifact,
            evidence,
            summary,
            rule_text,
            priority,
            project,
            json,
        } => cmd_record(
            db,
            RecordArgs {
                source,
                scope,
                target_artifact,
                evidence,
                summary,
                rule_text,
                priority,
                project,
                json,
            },
        ),
        InsightCommands::List {
            status,
            source,
            scope,
            target_artifact,
            min_priority,
            limit,
            json,
        } => cmd_list(
            db,
            ListArgs {
                status,
                source,
                scope,
                target_artifact,
                min_priority,
                limit,
                json,
            },
        ),
        InsightCommands::Resolve {
            insight_id,
            actor,
            note,
            json,
        } => cmd_resolve(db, &insight_id, actor.as_deref(), note.as_deref(), json),
        InsightCommands::Runbook { format } => cmd_runbook(&format),
    }
}

struct RecordArgs {
    source: String,
    scope: String,
    target_artifact: String,
    evidence: String,
    summary: String,
    rule_text: Option<String>,
    priority: u8,
    project: Option<String>,
    json: bool,
}

fn cmd_record(db: &Database, args: RecordArgs) -> Result<()> {
    let source = parse_source(&args.source)?;
    let scope = parse_scope(&args.scope)?;
    let target_artifact = parse_target(&args.target_artifact)?;
    let insight = record_design_insight(
        db.conn(),
        &NewDesignInsight {
            source,
            scope,
            target_artifact,
            evidence_ref: &args.evidence,
            summary: &args.summary,
            rule_text: args.rule_text.as_deref(),
            priority: args.priority,
            project_id: args.project.as_deref(),
        },
    )
    .context("failed to record design insight")?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&insight)?);
    } else {
        println!("insight_id={}", insight.id);
        println!(
            "status={} priority={} redactions={}",
            insight.status.as_str(),
            insight.priority,
            insight.redaction_count
        );
    }
    Ok(())
}

struct ListArgs {
    status: String,
    source: Option<String>,
    scope: Option<String>,
    target_artifact: Option<String>,
    min_priority: u8,
    limit: usize,
    json: bool,
}

fn cmd_list(db: &Database, args: ListArgs) -> Result<()> {
    let filters = DesignInsightFilters {
        status: parse_status_filter(&args.status)?,
        source: args.source.as_deref().map(parse_source).transpose()?,
        scope: args.scope.as_deref().map(parse_scope).transpose()?,
        target_artifact: args
            .target_artifact
            .as_deref()
            .map(parse_target)
            .transpose()?,
        min_priority: Some(args.min_priority),
        limit: Some(args.limit),
    };
    let insights =
        list_design_insights(db.conn(), &filters).context("failed to list design insights")?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&insights)?);
        return Ok(());
    }
    print_insight_table(&insights);
    Ok(())
}

fn cmd_resolve(
    db: &Database,
    insight_id: &str,
    actor: Option<&str>,
    note: Option<&str>,
    json: bool,
) -> Result<()> {
    let resolved = resolve_design_insight(db.conn(), insight_id, actor, note)
        .context("failed to resolve design insight")?;
    if !resolved {
        bail!("design insight not found or already resolved: {insight_id}");
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ResolveOutput {
                insight_id,
                resolved
            })?
        );
    } else {
        println!("resolved insight_id={insight_id}");
    }
    Ok(())
}

fn cmd_runbook(format: &str) -> Result<()> {
    const TITLE: &str = "Design Insight Learning Loop";
    const STEPS: &[(&str, &str)] = &[
        (
            "Design-opportunity pass",
            "For every non-trivial dev2merge/issue-drain issue, ask whether the user idea, review finding, incident, tool friction, or research result should become a durable artifact.",
        ),
        (
            "Record",
            "mempal insight record --source review-finding --scope issue --target github-issue --evidence <issue-or-session-ref> --summary <content-free-insight> --rule <acceptance-or-reusable-rule> --priority 4",
        ),
        (
            "Drain",
            "mempal insight list --status open --min-priority 4",
        ),
        (
            "Resolve",
            "mempal insight resolve <insight_id> --actor <agent-or-human> --note <target-artifact-ref>",
        ),
    ];
    match format {
        "plain" => {
            println!("{TITLE}");
            println!();
            for (label, detail) in STEPS {
                println!("{label}:");
                println!("  {detail}");
            }
            Ok(())
        }
        "json" => {
            let value = serde_json::json!({
                "title": TITLE,
                "steps": STEPS
                    .iter()
                    .map(|(label, detail)| serde_json::json!({
                        "label": label,
                        "detail": detail,
                    }))
                    .collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        other => bail!("unsupported insight runbook format: {other}"),
    }
}

fn print_insight_table(insights: &[DesignInsight]) {
    if insights.is_empty() {
        println!("no design insights found");
        return;
    }
    println!(
        "{:<25} {:<8} {:<15} {:<14} {:<18} {:>3}  SUMMARY",
        "INSIGHT_ID", "STATUS", "SOURCE", "SCOPE", "TARGET", "PRI"
    );
    println!("{}", "-".repeat(112));
    for insight in insights {
        println!(
            "{:<25} {:<8} {:<15} {:<14} {:<18} {:>3}  {}",
            insight.id,
            insight.status.as_str(),
            insight.source.as_str(),
            insight.scope.as_str(),
            insight.target_artifact.as_str(),
            insight.priority,
            inline_preview(&insight.summary, 96)
        );
    }
}

fn parse_source(value: &str) -> Result<DesignInsightSource> {
    value
        .parse()
        .with_context(|| format!("invalid design insight source: {value}"))
}

fn parse_scope(value: &str) -> Result<DesignInsightScope> {
    value
        .parse()
        .with_context(|| format!("invalid design insight scope: {value}"))
}

fn parse_target(value: &str) -> Result<DesignInsightTargetArtifact> {
    value
        .parse()
        .with_context(|| format!("invalid design insight target artifact: {value}"))
}

fn parse_status_filter(value: &str) -> Result<Option<DesignInsightStatus>> {
    let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
    if normalized == "all" {
        return Ok(None);
    }
    Ok(Some(value.parse().with_context(|| {
        format!("invalid design insight status: {value}")
    })?))
}

fn inline_preview(value: &str, max_chars: usize) -> String {
    let one_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = one_line.chars();
    let mut preview = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}
