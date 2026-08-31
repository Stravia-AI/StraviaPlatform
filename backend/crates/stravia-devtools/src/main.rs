use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

mod fixture;
mod protocol;
mod record;
mod replay;
mod scenarios;

const POSTGRES_SCHEMA_SQL: &str = concat!(
    include_str!("../../stravia-core/migrations/postgres/0001_initial.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0002_provider_models.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0003_web_access.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0004_oauth_connection_generation.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0005_split_web_access_permissions.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0006_zhipu_web_provider.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0007_turn_chain.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0008_agent_definitions.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0009_artifacts.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0010_web_research.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0011_media_understanding.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0012_media_derivatives.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0013_image_generation.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0014_require_api_key.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0015_reusable_response_prefix.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0016_remove_image_generation.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0017_principal_concurrency_limit.sql"),
    "\n",
    include_str!(
        "../../stravia-core/migrations/postgres/0018_advanced_capabilities_web_search.sql"
    ),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0019_always_record_payloads.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0020_revisioned_catalog_source.sql"),
    "\n",
    include_str!(
        "../../stravia-core/migrations/postgres/0021_adapter_credentials_and_vendor_npm.sql"
    ),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0022_thinking_level_mapping.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0023_history_markers.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0024_derive_route_thinking_levels.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0025_request_log_usage_details.sql"),
    "\n",
    include_str!("../../stravia-core/migrations/postgres/0026_agent_definition_thinking_level.sql")
);
const POSTGRES_SCHEMA_HEADER: &str = "\
-- Stravia AI Gateway - PostgreSQL Final Schema
--
-- This file represents the authoritative final-state schema after all migrations.
-- It is a DBA review artifact only. Do not execute it to initialize a Stravia
-- database: direct execution does not record SQLx migration history.
-- Start stravia-server with a blank database so it can apply the migrations.
--
-- Generated from: backend/crates/stravia-core/migrations/postgres/
-- Regenerate  : stravia-tools dump-schema --backend postgres
--
";

fn postgres_schema() -> String {
    format!("{POSTGRES_SCHEMA_HEADER}{POSTGRES_SCHEMA_SQL}")
}

#[derive(Parser)]
#[command(
    name = "stravia-tools",
    version,
    about = "CLI suite for Stravia development and E2E testing",
    long_about = "Commands for protocol-conversion testing and schema review:\n\
                  - record: scenario-driven recording against real LLM endpoints\n\
                  - replay: persistent stub upstream that replays fixtures by replay_model\n\
                  - dump-schema: print the final-state DDL for a storage backend"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scenario-driven recorder: replays fixed scenarios against a real LLM and writes .jsonl fixtures
    Record(record::RecordArgs),
    /// Persistent stub upstream: serves recorded fixtures via in-memory replay_model HashMap
    Replay(replay::ReplayArgs),
    /// Print scenario metadata (anchor + expected_fields per protocol) as JSON — consumed by pytest
    PrintScenarios,
    /// Print the final-state PostgreSQL DDL schema.
    /// Useful for DBAs to review schema changes.
    /// The output matches deploy/schema/postgres.sql in the repository.
    DumpSchema(DumpSchemaArgs),
}

#[derive(Parser)]
struct DumpSchemaArgs {
    /// PostgreSQL is Stravia's only reference-schema backend.
    #[arg(long, value_enum, default_value = "postgres")]
    backend: SchemaBackend,
}

#[derive(ValueEnum, Clone)]
enum SchemaBackend {
    Postgres,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Record(args) => record::run(args).await,
        Command::Replay(args) => replay::run(args).await,
        Command::PrintScenarios => print_scenarios(),
        Command::DumpSchema(args) => {
            match args.backend {
                SchemaBackend::Postgres => print!("{}", postgres_schema()),
            }
            Ok(())
        }
    }
}

fn print_scenarios() -> Result<()> {
    use protocol::ProtocolKind;
    let protocols = [
        ProtocolKind::OpenAiChat,
        ProtocolKind::OpenResponses,
        ProtocolKind::AnthropicMessages,
        ProtocolKind::GoogleContent,
    ];
    let entries: Vec<serde_json::Value> = scenarios::SCENARIOS
        .iter()
        .map(|s| {
            let expected: serde_json::Map<String, serde_json::Value> = protocols
                .iter()
                .map(|p| {
                    (
                        p.as_short_name().to_string(),
                        serde_json::json!(s.expected_fields_for(*p)),
                    )
                })
                .collect();
            serde_json::json!({
                "name": s.name,
                "anchor": s.anchor,
                "stream": s.stream,
                "uses_reasoning_model": s.uses_reasoning_model,
                "expected_fields": expected,
            })
        })
        .collect();
    let body = serde_json::json!({
        "version": fixture::FIXTURE_VERSION,
        "scenarios": entries,
        "protocols": protocols.iter().map(|p| p.as_short_name()).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_postgres_schema_matches_generator() {
        assert_eq!(
            include_str!("../../../../deploy/schema/postgres.sql"),
            postgres_schema()
        );
    }
}
