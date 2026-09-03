//! Pure planning for incremental Connect Client Global Config updates.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::ser::{SerializeMap as _, SerializeSeq as _};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use toml::{Table as TomlTable, Value as TomlValue};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectClientId {
    CodexCli,
    ClaudeCode,
    Opencode,
    Openclaw,
    HermesAgent,
    Trae,
    Workbuddy,
    Zcode,
    DeepseekHarness,
    Pi,
    Omp,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectClientModel {
    pub model_id: String,
    pub display_name: String,
    #[serde(default)]
    pub supported_thinking_levels: Vec<String>,
    #[serde(default)]
    pub supports_image_input: bool,
    pub context_window: Option<u64>,
    pub output_max_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeModelMappings {
    pub default_model: String,
    pub haiku_model: String,
    pub sonnet_model: String,
    pub opus_model: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectClientApplyInput {
    pub tool: ConnectClientId,
    pub host: String,
    pub api_key: String,
    pub models: Vec<ConnectClientModel>,
    #[serde(default)]
    pub transparent_image_input_enabled: bool,
    pub mappings: Option<ClaudeModelMappings>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedConnectClientFile {
    pub path: String,
    #[serde(skip)]
    pub bytes: Vec<u8>,
    #[serde(skip)]
    pub root: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectClientApplyPlan {
    pub paths: Vec<String>,
    pub preview: String,
    #[serde(skip)]
    pub files: Vec<PlannedConnectClientFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectClientApplyError {
    pub code: &'static str,
    pub message: String,
    pub path: Option<String>,
}

pub fn plan_connect_client_apply(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
    existing_files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    if input.models.is_empty() {
        return Err(error(
            "invalid_input",
            "At least one authorized model is required",
            None,
        ));
    }

    match input.tool {
        ConnectClientId::CodexCli => plan_codex(input, environment, existing_files),
        ConnectClientId::ClaudeCode => plan_claude(input, environment, existing_files),
        ConnectClientId::Opencode => plan_opencode(input, environment, existing_files),
        ConnectClientId::Openclaw => plan_openclaw(input, environment, existing_files),
        ConnectClientId::HermesAgent => plan_hermes(input, environment, existing_files),
        ConnectClientId::Trae => plan_trae(input, environment, existing_files),
        ConnectClientId::Workbuddy => plan_workbuddy(input, environment, existing_files),
        ConnectClientId::Zcode => plan_zcode(input, environment, existing_files),
        ConnectClientId::DeepseekHarness => {
            plan_deepseek_harness(input, environment, existing_files)
        }
        ConnectClientId::Pi => plan_pi(input, environment, existing_files),
        ConnectClientId::Omp => plan_omp(input, environment, existing_files),
    }
}

pub fn preview_connect_client_apply(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let mut plan = plan_connect_client_apply(input, environment, &BTreeMap::new())?;
    let portable_paths = match input.tool {
        ConnectClientId::CodexCli => vec!["~/.codex/config.toml", "~/.codex/stravia-models.json"],
        ConnectClientId::ClaudeCode => vec!["~/.claude/settings.json"],
        ConnectClientId::Opencode => vec!["~/.config/opencode/opencode.json"],
        ConnectClientId::Openclaw => vec!["~/.openclaw/openclaw.json"],
        ConnectClientId::HermesAgent => vec!["~/.hermes/.env", "~/.hermes/config.yaml"],
        ConnectClientId::Trae => vec!["~/.config/trae/trae_config.yaml"],
        ConnectClientId::Workbuddy => vec!["~/.workbuddy/models.json"],
        ConnectClientId::Zcode => vec!["~/.zcode/v2/config.json"],
        ConnectClientId::DeepseekHarness => {
            vec!["$DSH_HOME/settings.yaml", "$DSH_HOME/.credentials.yaml"]
        }
        ConnectClientId::Pi => vec!["~/.pi/agent/models.json"],
        ConnectClientId::Omp => vec!["~/.omp/agent/models.yml"],
    };
    debug_assert_eq!(plan.paths.len(), portable_paths.len());
    for (absolute, portable) in plan.paths.iter().zip(&portable_paths) {
        plan.preview = plan.preview.replace(absolute, portable);
    }
    plan.paths = portable_paths.into_iter().map(str::to_owned).collect();
    plan.files.clear();
    Ok(plan)
}

fn plan_opencode(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
    existing_files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let path = xdg_config_root(environment)?
        .join("opencode")
        .join("opencode.json");
    let mut document = parse_json(&path, existing_files.get(&path), false)?;
    let provider = json!({
        "npm": "@ai-sdk/open-responses",
        "models": open_responses_models(input),
        "options": {
            "name": "stravia",
            "url": format!("{}/v1/responses", input.host.trim_end_matches('/')),
            "apiKey": input.api_key,
        },
    });
    upsert_json_path(
        &mut document,
        &["provider", "stravia"],
        provider.clone(),
        &path,
    )?;
    json_plan(
        path,
        document,
        json!({ "provider": { "stravia": provider } }),
    )
}

fn plan_openclaw(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
    existing_files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let root = configured_root(environment, "OPENCLAW_STATE_DIR", ".openclaw")?;
    let path = root.join("openclaw.json");
    let mut document = parse_json(&path, existing_files.get(&path), true)?;
    let provider = json!({
        "baseUrl": format!("{}/v1", input.host.trim_end_matches('/')),
        "apiKey": input.api_key,
        "api": "openai-completions",
        "models": input.models.iter().map(|model| {
            let mut value = JsonMap::from_iter([
                ("id".to_owned(), json!(model.model_id)),
                ("name".to_owned(), json!(model.display_name)),
                (
                    "input".to_owned(),
                    json!(input_modalities(model, input.transparent_image_input_enabled)),
                ),
            ]);
            insert_optional_limits(&mut value, model, "contextWindow", "maxTokens");
            JsonValue::Object(value)
        }).collect::<Vec<_>>(),
    });
    upsert_json_path(
        &mut document,
        &["models", "providers", "stravia"],
        provider.clone(),
        &path,
    )?;
    json_plan(
        path,
        document,
        json!({ "models": { "providers": { "stravia": provider } } }),
    )
}

fn plan_hermes(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
    existing_files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let root = configured_root(environment, "HERMES_HOME", ".hermes")?;
    let config_path = root.join("config.yaml");
    let environment_path = root.join(".env");
    let mut document = parse_yaml(&config_path, existing_files.get(&config_path))?;
    let model_entries = JsonMap::from_iter(input.models.iter().map(|model| {
        let mut value = JsonMap::from_iter([(
            "supports_vision".to_owned(),
            json!(input.transparent_image_input_enabled || model.supports_image_input),
        )]);
        if let Some(context_window) = model.context_window {
            value.insert("context_length".to_owned(), json!(context_window));
        }
        (model.model_id.clone(), JsonValue::Object(value))
    }));
    let provider = json!({
        "api": format!("{}/v1", input.host.trim_end_matches('/')),
        "key_env": "STRAVIA_API_KEY",
        "transport": "chat_completions",
        "discover_models": false,
        "models": model_entries,
    });
    upsert_json_path(
        &mut document,
        &["providers", "stravia"],
        provider.clone(),
        &config_path,
    )?;
    upsert_json_path(
        &mut document,
        &["model", "provider"],
        json!("stravia"),
        &config_path,
    )?;
    let config_bytes = serialize_yaml(&config_path, &document)?;
    let environment_bytes = merge_dotenv(
        &environment_path,
        existing_files.get(&environment_path),
        "STRAVIA_API_KEY",
        &input.api_key,
    )?;
    let config_preview = serialize_yaml(
        &config_path,
        &json!({
            "providers": { "stravia": provider },
            "model": { "provider": "stravia" },
        }),
    )?;
    let paths = vec![
        environment_path.display().to_string(),
        config_path.display().to_string(),
    ];
    let root = root.display().to_string();
    Ok(ConnectClientApplyPlan {
        preview: format!(
            "# {}\nSTRAVIA_API_KEY={}\n\n# {}\n{}",
            paths[0],
            input.api_key,
            paths[1],
            String::from_utf8(config_preview).expect("YAML serializer emits UTF-8")
        ),
        files: vec![
            PlannedConnectClientFile {
                path: paths[0].clone(),
                bytes: environment_bytes,
                root: root.clone(),
            },
            PlannedConnectClientFile {
                path: paths[1].clone(),
                bytes: config_bytes,
                root,
            },
        ],
        paths,
    })
}

fn plan_trae(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
    existing_files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let path = xdg_config_root(environment)?
        .join("trae")
        .join("trae_config.yaml");
    let mut document = parse_yaml(&path, existing_files.get(&path))?;
    let provider = json!({
        "provider": "openai",
        "api_key": input.api_key,
        "base_url": format!("{}/v1", input.host.trim_end_matches('/')),
    });
    upsert_json_path(
        &mut document,
        &["model_providers", "stravia"],
        provider.clone(),
        &path,
    )?;
    yaml_plan(
        path,
        document,
        json!({ "model_providers": { "stravia": provider } }),
    )
}

fn plan_workbuddy(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
    existing_files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let path = user_home(environment)?
        .join(".workbuddy")
        .join("models.json");
    let mut models = match existing_files.get(&path) {
        None => Vec::new(),
        Some(bytes) => {
            let text = std::str::from_utf8(bytes)
                .map_err(|cause| error("parse_error", cause.to_string(), Some(&path)))?;
            serde_json::from_str::<Vec<JsonValue>>(text)
                .map_err(|cause| error("parse_error", cause.to_string(), Some(&path)))?
        }
    };
    models.retain(|model| model.get("vendor").and_then(JsonValue::as_str) != Some("Stravia"));
    let owned_models = workbuddy_models(input);
    models.extend(owned_models.clone());
    json_plan(
        path,
        JsonValue::Array(models),
        JsonValue::Array(owned_models),
    )
}

fn plan_zcode(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
    existing_files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let path = user_home(environment)?
        .join(".zcode")
        .join("v2")
        .join("config.json");
    let mut document = parse_json(&path, existing_files.get(&path), false)?;
    let provider = json!({
        "name": "Stravia",
        "kind": "openai-compatible",
        "options": {
            "apiKey": input.api_key,
            "baseURL": format!("{}/v1", input.host.trim_end_matches('/')),
            "apiKeyRequired": true,
        },
        "source": "custom",
        "models": zcode_models(input),
    });
    upsert_json_path(
        &mut document,
        &["provider", "custom:stravia"],
        provider.clone(),
        &path,
    )?;
    json_plan(
        path,
        document,
        json!({ "provider": { "custom:stravia": provider } }),
    )
}

fn plan_deepseek_harness(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
    existing_files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let root = configured_root(environment, "DSH_HOME", ".dsh")?;
    let path = root.join("settings.yaml");
    let credentials_path = root.join(".credentials.yaml");
    let mut document = parse_yaml(&path, existing_files.get(&path))?;
    let mut credentials = parse_yaml(&credentials_path, existing_files.get(&credentials_path))?;
    let provider = json!({
        "displayName": "Stravia Gateway",
        "apiKeyEnv": "STRAVIA_API_KEY",
        "api": "openai-completions",
        "baseURL": format!("{}/v1", input.host.trim_end_matches('/')),
        "models": input.models.iter().map(|model| {
            let mut value = JsonMap::from_iter([
                ("id".to_owned(), json!(model.model_id)),
                (
                    "input".to_owned(),
                    json!(input_modalities(model, input.transparent_image_input_enabled)),
                ),
            ]);
            insert_optional_limits(&mut value, model, "contextWindow", "maxTokens");
            JsonValue::Object(value)
        }).collect::<Vec<_>>(),
    });
    upsert_json_path(
        &mut document,
        &["llm-pi-ai", "providers", "stravia"],
        provider.clone(),
        &path,
    )?;
    upsert_json_path(&mut credentials, &["version"], json!(1), &credentials_path)?;
    upsert_json_path(
        &mut credentials,
        &["refs", "STRAVIA_API_KEY"],
        json!(input.api_key),
        &credentials_path,
    )?;
    let settings_bytes = serialize_yaml(&path, &document)?;
    let credentials_bytes = serialize_yaml(&credentials_path, &credentials)?;
    let settings_preview = serialize_yaml(
        &path,
        &json!({ "llm-pi-ai": { "providers": { "stravia": provider } } }),
    )?;
    let credentials_preview = serialize_yaml(
        &credentials_path,
        &json!({ "version": 1, "refs": { "STRAVIA_API_KEY": input.api_key } }),
    )?;
    let paths = vec![
        path.display().to_string(),
        credentials_path.display().to_string(),
    ];
    let root = root.display().to_string();
    Ok(ConnectClientApplyPlan {
        preview: format!(
            "# {}\n{}\n# {}\n{}",
            paths[0],
            String::from_utf8(settings_preview).expect("YAML serializer emits UTF-8"),
            paths[1],
            String::from_utf8(credentials_preview).expect("YAML serializer emits UTF-8"),
        ),
        files: vec![
            PlannedConnectClientFile {
                path: paths[0].clone(),
                bytes: settings_bytes,
                root: root.clone(),
            },
            PlannedConnectClientFile {
                path: paths[1].clone(),
                bytes: credentials_bytes,
                root,
            },
        ],
        paths,
    })
}

fn plan_pi(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
    existing_files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let root = environment
        .get("PI_CODING_AGENT_DIR")
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or(user_home(environment)?.join(".pi").join("agent"));
    let root = require_absolute_root(root, "PI_CODING_AGENT_DIR")?;
    let path = root.join("models.json");
    let mut document = parse_json(&path, existing_files.get(&path), false)?;
    let provider = responses_provider(input, "baseUrl");
    upsert_json_path(
        &mut document,
        &["providers", "stravia"],
        provider.clone(),
        &path,
    )?;
    json_plan(
        path,
        document,
        json!({ "providers": { "stravia": provider } }),
    )
}

fn plan_omp(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
    existing_files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let root = user_home(environment)?.join(".omp").join("agent");
    let path = root.join("models.yml");
    let mut document = parse_yaml(&path, existing_files.get(&path))?;
    let provider = responses_provider(input, "baseUrl");
    upsert_json_path(
        &mut document,
        &["providers", "stravia"],
        provider.clone(),
        &path,
    )?;
    yaml_plan(
        path,
        document,
        json!({ "providers": { "stravia": provider } }),
    )
}

fn plan_claude(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
    existing_files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let root = configured_root(environment, "CLAUDE_CONFIG_DIR", ".claude")?;
    let path = root.join("settings.json");
    let mappings = input.mappings.as_ref().ok_or_else(|| {
        error(
            "invalid_input",
            "Claude Code requires all four model mappings",
            None,
        )
    })?;
    let allowed_models = input
        .models
        .iter()
        .map(|model| model.model_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if [
        &mappings.default_model,
        &mappings.haiku_model,
        &mappings.sonnet_model,
        &mappings.opus_model,
    ]
    .iter()
    .any(|model| !allowed_models.contains(model.as_str()))
    {
        return Err(error(
            "invalid_input",
            "Claude Code model mappings must use models authorized for the API Key",
            None,
        ));
    }

    let owned_environment = JsonMap::from_iter([
        ("ANTHROPIC_AUTH_TOKEN".to_owned(), json!(input.api_key)),
        (
            "ANTHROPIC_BASE_URL".to_owned(),
            json!(input.host.trim_end_matches('/')),
        ),
        ("ANTHROPIC_MODEL".to_owned(), json!(mappings.default_model)),
        (
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_owned(),
            json!(mappings.haiku_model),
        ),
        (
            "ANTHROPIC_DEFAULT_SONNET_MODEL".to_owned(),
            json!(mappings.sonnet_model),
        ),
        (
            "ANTHROPIC_DEFAULT_OPUS_MODEL".to_owned(),
            json!(mappings.opus_model),
        ),
    ]);
    let mut document = parse_json(&path, existing_files.get(&path), false)?;
    let root_object = document.as_object_mut().ok_or_else(|| {
        error(
            "merge_error",
            "Claude Code settings must contain a JSON object",
            Some(&path),
        )
    })?;
    let environment_value = root_object
        .entry("env")
        .or_insert_with(|| JsonValue::Object(JsonMap::new()));
    let environment_object = environment_value.as_object_mut().ok_or_else(|| {
        error(
            "merge_error",
            "Claude Code `env` must contain a JSON object",
            Some(&path),
        )
    })?;
    environment_object.extend(owned_environment.clone());

    json_plan(
        path,
        document,
        JsonValue::Object(JsonMap::from_iter([(
            "env".to_owned(),
            JsonValue::Object(owned_environment),
        )])),
    )
}

fn plan_codex(
    input: &ConnectClientApplyInput,
    environment: &BTreeMap<String, String>,
    existing_files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let root = configured_root(environment, "CODEX_HOME", ".codex")?;
    let config_path = root.join("config.toml");
    let catalog_path = root.join("stravia-models.json");
    let mut document = parse_toml(&config_path, existing_files.get(&config_path))?;
    let catalog_pointer = catalog_path.display().to_string();
    let provider = TomlValue::Table(TomlTable::from_iter([
        (
            "name".to_owned(),
            TomlValue::String("Stravia Gateway".to_owned()),
        ),
        (
            "base_url".to_owned(),
            TomlValue::String(format!("{}/v1", input.host.trim_end_matches('/'))),
        ),
        (
            "wire_api".to_owned(),
            TomlValue::String("responses".to_owned()),
        ),
        (
            "experimental_bearer_token".to_owned(),
            TomlValue::String(input.api_key.clone()),
        ),
    ]));

    let table = document
        .as_table_mut()
        .expect("parse_toml always returns a TOML table");
    table.insert(
        "model_provider".to_owned(),
        TomlValue::String("stravia".to_owned()),
    );
    table.insert(
        "model_catalog_json".to_owned(),
        TomlValue::String(catalog_pointer.clone()),
    );
    let providers = ensure_toml_table(table, "model_providers")?;
    providers.insert("stravia".to_owned(), provider.clone());

    let config_bytes = toml::to_string_pretty(&document)
        .map_err(|cause| error("serialize_error", cause.to_string(), Some(&config_path)))?
        .into_bytes();
    let catalog_bytes = serde_json::to_vec_pretty(&codex_catalog(input))
        .map_err(|cause| error("serialize_error", cause.to_string(), Some(&catalog_path)))?;

    let preview_document = TomlValue::Table(TomlTable::from_iter([
        (
            "model_provider".to_owned(),
            TomlValue::String("stravia".to_owned()),
        ),
        (
            "model_catalog_json".to_owned(),
            TomlValue::String(catalog_pointer),
        ),
        (
            "model_providers".to_owned(),
            TomlValue::Table(TomlTable::from_iter([("stravia".to_owned(), provider)])),
        ),
    ]));
    let preview_toml = toml::to_string_pretty(&preview_document)
        .map_err(|cause| error("serialize_error", cause.to_string(), Some(&config_path)))?;
    let preview_catalog =
        String::from_utf8(catalog_bytes.clone()).expect("serde_json always emits UTF-8");
    let paths = vec![
        config_path.display().to_string(),
        catalog_path.display().to_string(),
    ];
    Ok(ConnectClientApplyPlan {
        preview: format!(
            "# {}\n{}\n# {}\n{}",
            paths[0], preview_toml, paths[1], preview_catalog
        ),
        files: vec![
            PlannedConnectClientFile {
                path: paths[0].clone(),
                bytes: config_bytes,
                root: root.display().to_string(),
            },
            PlannedConnectClientFile {
                path: paths[1].clone(),
                bytes: catalog_bytes,
                root: root.display().to_string(),
            },
        ],
        paths,
    })
}

fn open_responses_models(input: &ConnectClientApplyInput) -> JsonValue {
    JsonValue::Object(JsonMap::from_iter(input.models.iter().map(|model| {
        let variants = JsonMap::from_iter(model.supported_thinking_levels.iter().map(|level| {
            let effort = if level == "off" {
                "none".to_owned()
            } else {
                level.clone()
            };
            (effort.clone(), json!({ "reasoningEffort": effort }))
        }));
        let mut value = JsonMap::from_iter([
            (
                "reasoning".to_owned(),
                json!(
                    model
                        .supported_thinking_levels
                        .iter()
                        .any(|level| level != "off")
                ),
            ),
            ("variants".to_owned(), JsonValue::Object(variants)),
            (
                "modalities".to_owned(),
                json!({
                    "input": input_modalities(model, input.transparent_image_input_enabled),
                    "output": ["text"],
                }),
            ),
        ]);
        if let (Some(context), Some(output)) = (model.context_window, model.output_max_tokens) {
            value.insert(
                "limit".to_owned(),
                json!({ "context": context, "output": output }),
            );
        }
        (model.model_id.clone(), JsonValue::Object(value))
    })))
}

fn responses_provider(input: &ConnectClientApplyInput, base_url_key: &str) -> JsonValue {
    let models = input
        .models
        .iter()
        .map(|model| {
            let levels = model
                .supported_thinking_levels
                .iter()
                .filter(|level| level.as_str() != "off")
                .cloned()
                .collect::<Vec<_>>();
            let mut value = JsonMap::from_iter([
                ("id".to_owned(), json!(model.model_id)),
                ("name".to_owned(), json!(model.display_name)),
                ("reasoning".to_owned(), json!(!levels.is_empty())),
                (
                    "input".to_owned(),
                    json!(input_modalities(
                        model,
                        input.transparent_image_input_enabled
                    )),
                ),
            ]);
            if !levels.is_empty() {
                let mut thinking = JsonMap::from_iter([
                    ("mode".to_owned(), json!("effort")),
                    ("efforts".to_owned(), json!(levels)),
                ]);
                if model
                    .supported_thinking_levels
                    .iter()
                    .any(|level| level == "medium")
                {
                    thinking.insert("defaultLevel".to_owned(), json!("medium"));
                }
                value.insert("thinking".to_owned(), JsonValue::Object(thinking));
            }
            insert_optional_limits(&mut value, model, "contextWindow", "maxTokens");
            JsonValue::Object(value)
        })
        .collect::<Vec<_>>();
    JsonValue::Object(JsonMap::from_iter([
        (
            base_url_key.to_owned(),
            json!(format!("{}/v1", input.host.trim_end_matches('/'))),
        ),
        ("apiKey".to_owned(), json!(input.api_key)),
        ("api".to_owned(), json!("openai-responses")),
        ("authHeader".to_owned(), json!(true)),
        ("models".to_owned(), JsonValue::Array(models)),
    ]))
}

fn workbuddy_models(input: &ConnectClientApplyInput) -> Vec<JsonValue> {
    input
        .models
        .iter()
        .map(|model| {
            let efforts = model
                .supported_thinking_levels
                .iter()
                .filter(|level| level.as_str() != "off")
                .cloned()
                .collect::<Vec<_>>();
            let mut value = JsonMap::from_iter([
                ("id".to_owned(), json!(model.model_id)),
                ("name".to_owned(), json!(model.display_name)),
                ("vendor".to_owned(), json!("Stravia")),
                (
                    "url".to_owned(),
                    json!(format!(
                        "{}/v1/chat/completions",
                        input.host.trim_end_matches('/')
                    )),
                ),
                ("apiKey".to_owned(), json!(input.api_key)),
                ("supportsToolCall".to_owned(), json!(true)),
                (
                    "supportsImages".to_owned(),
                    json!(input.transparent_image_input_enabled || model.supports_image_input),
                ),
                ("supportsReasoning".to_owned(), json!(!efforts.is_empty())),
                ("useCustomProtocol".to_owned(), json!(false)),
            ]);
            insert_optional_limits(&mut value, model, "maxInputTokens", "maxOutputTokens");
            if !efforts.is_empty() {
                value.insert(
                    "reasoning".to_owned(),
                    json!({ "supportedEfforts": efforts }),
                );
            }
            JsonValue::Object(value)
        })
        .collect()
}

fn zcode_models(input: &ConnectClientApplyInput) -> JsonValue {
    JsonValue::Object(JsonMap::from_iter(input.models.iter().map(|model| {
        let mut value = JsonMap::from_iter([
            (
                "modalities".to_owned(),
                json!({
                    "input": input_modalities(model, input.transparent_image_input_enabled),
                    "output": ["text"],
                }),
            ),
            (
                "zcode".to_owned(),
                json!({ "modalitiesConfigured": true, "modified": true }),
            ),
        ]);
        let mut limit = JsonMap::new();
        if let Some(context) = model.context_window {
            limit.insert("context".to_owned(), json!(context));
        }
        if let Some(output) = model.output_max_tokens {
            limit.insert("output".to_owned(), json!(output));
        }
        if !limit.is_empty() {
            value.insert("limit".to_owned(), JsonValue::Object(limit));
        }
        (model.model_id.clone(), JsonValue::Object(value))
    })))
}

fn insert_optional_limits(
    value: &mut JsonMap<String, JsonValue>,
    model: &ConnectClientModel,
    context_key: &str,
    output_key: &str,
) {
    if let Some(context) = model.context_window {
        value.insert(context_key.to_owned(), json!(context));
    }
    if let Some(output) = model.output_max_tokens {
        value.insert(output_key.to_owned(), json!(output));
    }
}

fn upsert_json_path(
    document: &mut JsonValue,
    path: &[&str],
    value: JsonValue,
    config_path: &Path,
) -> Result<(), ConnectClientApplyError> {
    let Some((leaf, parents)) = path.split_last() else {
        return Err(error(
            "merge_error",
            "An owned config path is empty",
            Some(config_path),
        ));
    };
    let mut cursor = document;
    for key in parents {
        let object = cursor.as_object_mut().ok_or_else(|| {
            error(
                "merge_error",
                format!("`{}` must contain an object", key),
                Some(config_path),
            )
        })?;
        cursor = object
            .entry((*key).to_owned())
            .or_insert_with(|| JsonValue::Object(JsonMap::new()));
    }
    let object = cursor.as_object_mut().ok_or_else(|| {
        error(
            "merge_error",
            format!("`{leaf}` must be nested in an object"),
            Some(config_path),
        )
    })?;
    object.insert((*leaf).to_owned(), value);
    Ok(())
}

fn parse_yaml(
    path: &Path,
    existing: Option<&Vec<u8>>,
) -> Result<JsonValue, ConnectClientApplyError> {
    let Some(bytes) = existing else {
        return Ok(JsonValue::Object(JsonMap::new()));
    };
    let text = std::str::from_utf8(bytes)
        .map_err(|cause| error("parse_error", cause.to_string(), Some(path)))?;
    let document = serde_saphyr::from_str::<JsonValue>(text)
        .map_err(|cause| error("parse_error", cause.to_string(), Some(path)))?;
    if !document.is_object() {
        return Err(error(
            "parse_error",
            "The global config must contain a YAML mapping",
            Some(path),
        ));
    }
    Ok(document)
}

fn serialize_yaml(path: &Path, document: &JsonValue) -> Result<Vec<u8>, ConnectClientApplyError> {
    serde_saphyr::to_string(&YamlJsonValue(document))
        .map(String::into_bytes)
        .map_err(|cause| error("serialize_error", cause.to_string(), Some(path)))
}

// `serde_json/arbitrary_precision` exposes numbers to non-JSON serializers as a
// private tagged struct. Adapt them explicitly so YAML receives scalar values.
struct YamlJsonValue<'a>(&'a JsonValue);

impl Serialize for YamlJsonValue<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            JsonValue::Null => serializer.serialize_unit(),
            JsonValue::Bool(value) => serializer.serialize_bool(*value),
            JsonValue::Number(value) => {
                if let Some(value) = value.as_i64() {
                    serializer.serialize_i64(value)
                } else if let Some(value) = value.as_u64() {
                    serializer.serialize_u64(value)
                } else if let Some(value) = value.as_i128() {
                    serializer.serialize_i128(value)
                } else if let Some(value) = value.as_u128() {
                    serializer.serialize_u128(value)
                } else if let Some(value) = value.as_f64() {
                    serializer.serialize_f64(value)
                } else {
                    Err(serde::ser::Error::custom(format!(
                        "JSON number cannot be represented as a YAML scalar: {value}"
                    )))
                }
            }
            JsonValue::String(value) => serializer.serialize_str(value),
            JsonValue::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&YamlJsonValue(value))?;
                }
                sequence.end()
            }
            JsonValue::Object(values) => {
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    map.serialize_entry(key, &YamlJsonValue(value))?;
                }
                map.end()
            }
        }
    }
}

fn yaml_plan(
    path: PathBuf,
    document: JsonValue,
    preview: JsonValue,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let bytes = serialize_yaml(&path, &document)?;
    let preview =
        String::from_utf8(serialize_yaml(&path, &preview)?).expect("YAML serializer emits UTF-8");
    let path_string = path.display().to_string();
    Ok(ConnectClientApplyPlan {
        paths: vec![path_string.clone()],
        preview: format!("# {path_string}\n{preview}"),
        files: vec![PlannedConnectClientFile {
            path: path_string,
            bytes,
            root: path
                .parent()
                .expect("global config path has a parent")
                .display()
                .to_string(),
        }],
    })
}

fn merge_dotenv(
    path: &Path,
    existing: Option<&Vec<u8>>,
    key: &str,
    value: &str,
) -> Result<Vec<u8>, ConnectClientApplyError> {
    let text = match existing {
        Some(bytes) => std::str::from_utf8(bytes)
            .map_err(|cause| error("parse_error", cause.to_string(), Some(path)))?,
        None => "",
    };
    let mut next = Vec::new();
    let mut replaced = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            next.push(line.to_owned());
            continue;
        }
        let Some((line_key, _)) = line.split_once('=') else {
            return Err(error(
                "parse_error",
                format!("Invalid environment entry `{line}`"),
                Some(path),
            ));
        };
        if line_key.trim() == key {
            if !replaced {
                next.push(format!("{key}={value}"));
                replaced = true;
            }
        } else {
            next.push(line.to_owned());
        }
    }
    if !replaced {
        next.push(format!("{key}={value}"));
    }
    Ok(format!("{}\n", next.join("\n")).into_bytes())
}

fn parse_json(
    path: &Path,
    existing: Option<&Vec<u8>>,
    allow_json5: bool,
) -> Result<JsonValue, ConnectClientApplyError> {
    let Some(bytes) = existing else {
        return Ok(JsonValue::Object(JsonMap::new()));
    };
    let text = std::str::from_utf8(bytes)
        .map_err(|cause| error("parse_error", cause.to_string(), Some(path)))?;
    if allow_json5 {
        json5::from_str(text).map_err(|cause| error("parse_error", cause.to_string(), Some(path)))
    } else {
        serde_json::from_str(text)
            .map_err(|cause| error("parse_error", cause.to_string(), Some(path)))
    }
}

fn json_plan(
    path: PathBuf,
    document: JsonValue,
    preview: JsonValue,
) -> Result<ConnectClientApplyPlan, ConnectClientApplyError> {
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|cause| error("serialize_error", cause.to_string(), Some(&path)))?;
    let preview = serde_json::to_string_pretty(&preview)
        .map_err(|cause| error("serialize_error", cause.to_string(), Some(&path)))?;
    let path_string = path.display().to_string();
    Ok(ConnectClientApplyPlan {
        paths: vec![path_string.clone()],
        preview: format!("# {path_string}\n{preview}"),
        files: vec![PlannedConnectClientFile {
            path: path_string,
            bytes,
            root: path
                .parent()
                .expect("global config path has a parent")
                .display()
                .to_string(),
        }],
    })
}

fn xdg_config_root(
    environment: &BTreeMap<String, String>,
) -> Result<PathBuf, ConnectClientApplyError> {
    if let Some(configured) = environment
        .get("XDG_CONFIG_HOME")
        .filter(|value| !value.trim().is_empty())
    {
        let root = PathBuf::from(configured);
        if root.is_absolute() {
            return Ok(root);
        }
        return Err(error(
            "invalid_global_path",
            "XDG_CONFIG_HOME must contain an absolute directory",
            Some(&root),
        ));
    }
    if cfg!(windows)
        && let Some(configured) = environment
            .get("APPDATA")
            .filter(|value| !value.trim().is_empty())
    {
        let root = PathBuf::from(configured);
        if root.is_absolute() {
            return Ok(root);
        }
    }
    Ok(user_home(environment)?.join(".config"))
}

fn configured_root(
    environment: &BTreeMap<String, String>,
    variable: &str,
    default_directory: &str,
) -> Result<PathBuf, ConnectClientApplyError> {
    let root = if let Some(configured) = environment
        .get(variable)
        .filter(|value| !value.trim().is_empty())
    {
        PathBuf::from(configured)
    } else {
        user_home(environment)?.join(default_directory)
    };
    require_absolute_root(root, variable)
}

fn require_absolute_root(
    root: PathBuf,
    variable: &str,
) -> Result<PathBuf, ConnectClientApplyError> {
    if root.is_absolute() {
        Ok(root)
    } else {
        Err(error(
            "invalid_global_path",
            format!("{variable} must contain an absolute directory"),
            Some(&root),
        ))
    }
}

fn user_home(environment: &BTreeMap<String, String>) -> Result<PathBuf, ConnectClientApplyError> {
    let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = environment
        .get(variable)
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            error(
                "global_path_unavailable",
                format!("{variable} is not available in the current OS user environment"),
                None,
            )
        })?;
    if !home.is_absolute() {
        return Err(error(
            "invalid_global_path",
            format!("{variable} must contain an absolute directory"),
            Some(&home),
        ));
    }
    Ok(home)
}

fn parse_toml(
    path: &Path,
    existing: Option<&Vec<u8>>,
) -> Result<TomlValue, ConnectClientApplyError> {
    let Some(bytes) = existing else {
        return Ok(TomlValue::Table(TomlTable::new()));
    };
    let text = std::str::from_utf8(bytes)
        .map_err(|cause| error("parse_error", cause.to_string(), Some(path)))?;
    let value = toml::from_str::<TomlValue>(text)
        .map_err(|cause| error("parse_error", cause.to_string(), Some(path)))?;
    if !value.is_table() {
        return Err(error(
            "parse_error",
            "The global config must contain a TOML table",
            Some(path),
        ));
    }
    Ok(value)
}

fn ensure_toml_table<'a>(
    parent: &'a mut TomlTable,
    key: &str,
) -> Result<&'a mut TomlTable, ConnectClientApplyError> {
    let value = parent
        .entry(key.to_owned())
        .or_insert_with(|| TomlValue::Table(TomlTable::new()));
    value
        .as_table_mut()
        .ok_or_else(|| error("merge_error", format!("`{key}` must be a table"), None))
}

fn codex_catalog(input: &ConnectClientApplyInput) -> JsonValue {
    let models = input
        .models
        .iter()
        .enumerate()
        .map(|(priority, model)| {
            let levels = model
                .supported_thinking_levels
                .iter()
                .map(|level| {
                    let effort = if level == "off" { "none" } else { level };
                    json!({
                        "effort": effort,
                        "description": thinking_level_description(level),
                    })
                })
                .collect::<Vec<_>>();
            let default_level = if model
                .supported_thinking_levels
                .iter()
                .any(|level| level == "medium")
            {
                Some("medium")
            } else {
                model
                    .supported_thinking_levels
                    .first()
                    .map(|level| if level == "off" { "none" } else { level.as_str() })
            };
            let mut entry = JsonMap::from_iter([
                ("slug".to_owned(), json!(model.model_id)),
                ("display_name".to_owned(), json!(model.display_name)),
                ("description".to_owned(), JsonValue::Null),
                ("default_reasoning_level".to_owned(), json!(default_level)),
                ("supported_reasoning_levels".to_owned(), json!(levels)),
                ("shell_type".to_owned(), json!("unified_exec")),
                ("visibility".to_owned(), json!("list")),
                ("supported_in_api".to_owned(), json!(true)),
                ("priority".to_owned(), json!(priority)),
                ("availability_nux".to_owned(), JsonValue::Null),
                ("upgrade".to_owned(), JsonValue::Null),
                (
                    "base_instructions".to_owned(),
                    json!(
                        "You are a coding agent running in Codex. Follow the user's instructions and use the available tools to complete the task."
                    ),
                ),
                ("support_verbosity".to_owned(), json!(false)),
                ("default_verbosity".to_owned(), JsonValue::Null),
                ("apply_patch_tool_type".to_owned(), JsonValue::Null),
                (
                    "truncation_policy".to_owned(),
                    json!({ "mode": "bytes", "limit": 10_000 }),
                ),
                ("supports_parallel_tool_calls".to_owned(), json!(false)),
                ("experimental_supported_tools".to_owned(), json!([])),
                (
                    "input_modalities".to_owned(),
                    json!(input_modalities(
                        model,
                        input.transparent_image_input_enabled
                    )),
                ),
            ]);
            if let Some(context_window) = model.context_window {
                entry.insert("context_window".to_owned(), json!(context_window));
            }
            JsonValue::Object(entry)
        })
        .collect::<Vec<_>>();
    json!({ "models": models })
}

fn thinking_level_description(level: &str) -> &str {
    match level {
        "off" => "No reasoning",
        "minimal" => "Minimal reasoning effort",
        "low" => "Low reasoning effort",
        "medium" => "Medium reasoning effort",
        "high" => "High reasoning effort",
        "xhigh" => "Extra high reasoning effort",
        "max" => "Maximum reasoning effort",
        _ => "Reasoning effort",
    }
}

fn input_modalities(
    model: &ConnectClientModel,
    transparent_image_input_enabled: bool,
) -> Vec<&'static str> {
    if transparent_image_input_enabled || model.supports_image_input {
        vec!["text", "image"]
    } else {
        vec!["text"]
    }
}

fn error(
    code: &'static str,
    message: impl Into<String>,
    path: Option<&Path>,
) -> ConnectClientApplyError {
    ConnectClientApplyError {
        code,
        message: message.into(),
        path: path.map(|path| path.display().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use toml::Value as TomlValue;

    use super::{
        ClaudeModelMappings, ConnectClientApplyInput, ConnectClientId, ConnectClientModel,
        plan_connect_client_apply, preview_connect_client_apply,
    };

    fn model(id: &str, display_name: &str) -> ConnectClientModel {
        ConnectClientModel {
            model_id: id.to_owned(),
            display_name: display_name.to_owned(),
            supported_thinking_levels: vec!["off".to_owned(), "medium".to_owned()],
            supports_image_input: true,
            context_window: Some(200_000),
            output_max_tokens: Some(32_000),
        }
    }

    fn environment(home: &std::path::Path) -> BTreeMap<String, String> {
        let mut environment = BTreeMap::new();
        environment.insert(
            if cfg!(windows) { "USERPROFILE" } else { "HOME" }.to_owned(),
            home.display().to_string(),
        );
        environment
    }

    #[test]
    fn codex_plan_uses_codex_home_and_preserves_current_model() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let codex_home = temporary.path().join("relocated-codex");
        let mut environment = environment(temporary.path());
        environment.insert("CODEX_HOME".to_owned(), codex_home.display().to_string());
        let config_path = codex_home.join("config.toml");
        let existing = BTreeMap::from([(
            config_path.clone(),
            br#"model = "existing-model"
model_catalog_json = "other-catalog.json"

[history]
persistence = "save-all"

[model_providers.other]
name = "Other"
"#
            .to_vec(),
        )]);
        let input = ConnectClientApplyInput {
            tool: ConnectClientId::CodexCli,
            host: "http://127.0.0.1:5174".to_owned(),
            api_key: "sk-client".to_owned(),
            models: vec![
                model("route-one", "Route One"),
                model("route-two", "Friendly Two"),
            ],
            transparent_image_input_enabled: false,
            mappings: None,
        };

        let plan = plan_connect_client_apply(&input, &environment, &existing).expect("valid plan");

        assert_eq!(
            plan.paths,
            vec![
                config_path.display().to_string(),
                codex_home.join("stravia-models.json").display().to_string(),
            ]
        );
        let config = plan
            .files
            .iter()
            .find(|file| file.path == config_path.display().to_string())
            .expect("Codex config file");
        let document =
            toml::from_str::<TomlValue>(std::str::from_utf8(&config.bytes).expect("UTF-8 TOML"))
                .expect("valid TOML");
        assert_eq!(document["model"].as_str(), Some("existing-model"));
        assert_eq!(document["model_provider"].as_str(), Some("stravia"));
        assert_eq!(
            document["model_catalog_json"].as_str(),
            Some(
                codex_home
                    .join("stravia-models.json")
                    .display()
                    .to_string()
                    .as_str()
            )
        );
        assert_eq!(
            document["history"]["persistence"].as_str(),
            Some("save-all")
        );
        assert_eq!(
            document["model_providers"]["other"]["name"].as_str(),
            Some("Other")
        );
        let provider = &document["model_providers"]["stravia"];
        assert_eq!(provider["name"].as_str(), Some("Stravia Gateway"));
        assert_eq!(
            provider["base_url"].as_str(),
            Some("http://127.0.0.1:5174/v1")
        );
        assert_eq!(provider["wire_api"].as_str(), Some("responses"));
        assert_eq!(
            provider["experimental_bearer_token"].as_str(),
            Some("sk-client")
        );
        assert!(provider.get("env_key").is_none());
        assert!(!plan.preview.contains("model ="));
        assert!(plan.preview.contains("model_provider = \"stravia\""));
        assert!(
            plan.preview
                .contains("experimental_bearer_token = \"sk-client\"")
        );
    }

    #[test]
    fn claude_plan_merges_only_anthropic_environment_mappings() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let claude_home = temporary.path().join("claude-global");
        let mut environment = environment(temporary.path());
        environment.insert(
            "CLAUDE_CONFIG_DIR".to_owned(),
            claude_home.display().to_string(),
        );
        let settings_path = claude_home.join("settings.json");
        let existing = BTreeMap::from([(
            settings_path.clone(),
            br#"{
  "env": { "KEEP_ME": "yes", "ANTHROPIC_MODEL": "old" },
  "permissions": { "allow": ["Bash(git status)"] },
  "effortLevel": "low",
  "autoCompactWindow": 64000
}"#
            .to_vec(),
        )]);
        let input = ConnectClientApplyInput {
            tool: ConnectClientId::ClaudeCode,
            host: "http://127.0.0.1:6188".to_owned(),
            api_key: "sk-claude".to_owned(),
            models: vec![
                model("route-default", "Default"),
                model("route-haiku", "Haiku"),
                model("route-sonnet", "Sonnet"),
                model("route-opus", "Opus"),
            ],
            transparent_image_input_enabled: false,
            mappings: Some(ClaudeModelMappings {
                default_model: "route-default".to_owned(),
                haiku_model: "route-haiku".to_owned(),
                sonnet_model: "route-sonnet".to_owned(),
                opus_model: "route-opus".to_owned(),
            }),
        };

        let plan = plan_connect_client_apply(&input, &environment, &existing).expect("valid plan");

        assert_eq!(plan.paths, vec![settings_path.display().to_string()]);
        let settings: serde_json::Value =
            serde_json::from_slice(&plan.files[0].bytes).expect("valid JSON");
        assert_eq!(settings["env"]["KEEP_ME"], "yes");
        assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], "sk-claude");
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:6188"
        );
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "route-default");
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"],
            "route-haiku"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
            "route-sonnet"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
            "route-opus"
        );
        assert_eq!(settings["permissions"]["allow"][0], "Bash(git status)");
        assert_eq!(settings["effortLevel"], "low");
        assert_eq!(settings["autoCompactWindow"], 64_000);
        assert!(
            plan.preview
                .contains("\"ANTHROPIC_MODEL\": \"route-default\"")
        );
        assert!(!plan.preview.contains("effortLevel"));
        assert!(!plan.preview.contains("autoCompactWindow"));
        assert!(!plan.preview.contains("KEEP_ME"));
    }

    fn standard_input(tool: ConnectClientId) -> ConnectClientApplyInput {
        ConnectClientApplyInput {
            tool,
            host: "http://127.0.0.1:7331".to_owned(),
            api_key: "sk-standard".to_owned(),
            models: vec![
                model("route-one", "Route One"),
                model("route-two", "Friendly Two"),
            ],
            transparent_image_input_enabled: false,
            mappings: None,
        }
    }

    fn planned_text(plan: &super::ConnectClientApplyPlan, suffix: &str) -> String {
        let file = plan
            .files
            .iter()
            .find(|file| file.path.ends_with(suffix))
            .unwrap_or_else(|| panic!("planned file ending with {suffix}"));
        String::from_utf8(file.bytes.clone()).expect("UTF-8 config")
    }

    #[test]
    fn omp_plan_serializes_model_limits_as_yaml_numbers() {
        #[derive(serde::Deserialize)]
        struct Config {
            providers: BTreeMap<String, Provider>,
        }

        #[derive(serde::Deserialize)]
        struct Provider {
            models: Vec<Model>,
        }

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Model {
            context_window: u64,
            max_tokens: u64,
        }

        let temporary = tempfile::tempdir().expect("temporary directory");
        let input = standard_input(ConnectClientId::Omp);

        let plan =
            plan_connect_client_apply(&input, &environment(temporary.path()), &BTreeMap::new())
                .expect("OMP plan");
        let text = planned_text(&plan, "models.yml");
        let document =
            serde_saphyr::from_str::<Config>(&text).expect("generated OMP config matches schema");
        let model = &document.providers["stravia"].models[0];

        assert_eq!(model.context_window, 200_000);
        assert_eq!(model.max_tokens, 32_000);
        assert!(!text.contains("$serde_json::private::Number"));
    }

    #[test]
    fn every_non_claude_client_upserts_owned_configuration_without_selecting_a_model() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let mut environment = environment(temporary.path());
        environment.insert(
            "XDG_CONFIG_HOME".to_owned(),
            temporary.path().join("xdg").display().to_string(),
        );
        environment.insert(
            "OPENCLAW_STATE_DIR".to_owned(),
            temporary.path().join("openclaw").display().to_string(),
        );
        environment.insert(
            "HERMES_HOME".to_owned(),
            temporary.path().join("hermes").display().to_string(),
        );
        environment.insert(
            "DSH_HOME".to_owned(),
            temporary.path().join("dsh").display().to_string(),
        );
        environment.insert(
            "PI_CODING_AGENT_DIR".to_owned(),
            temporary.path().join("pi").display().to_string(),
        );

        let cases = [
            (
                ConnectClientId::Opencode,
                "opencode.json",
                br#"{"model":"other/current","provider":{"other":{"name":"Other"}}}"#.as_slice(),
            ),
            (
                ConnectClientId::Openclaw,
                "openclaw.json",
                br#"{ // valid JSON5
                    agents: { defaults: { model: { primary: 'other/current', }, }, },
                    models: { providers: { other: { name: 'Other' }, }, },
                }"#
                .as_slice(),
            ),
            (
                ConnectClientId::HermesAgent,
                "config.yaml",
                b"providers:\n  other:\n    api: https://other.invalid\nmodel:\n  provider: other\n  default: current\nhooks:\n  enabled: true\n"
                    .as_slice(),
            ),
            (
                ConnectClientId::Trae,
                "trae_config.yaml",
                b"agents:\n  trae_agent:\n    model: existing\nmodel_providers:\n  other:\n    provider: openai\n"
                    .as_slice(),
            ),
            (
                ConnectClientId::Workbuddy,
                "models.json",
                br#"[{"id":"other","vendor":"Custom","url":"https://other.invalid"},{"id":"stale","vendor":"Stravia"}]"#
                    .as_slice(),
            ),
            (
                ConnectClientId::Zcode,
                "config.json",
                br#"{"provider":{"custom:other":{"name":"Other"}},"model":"custom:other/current"}"#.as_slice(),
            ),
            (
                ConnectClientId::DeepseekHarness,
                "settings.yaml",
                b"defaultModel: other/current\nllm-pi-ai:\n  providers:\n    other:\n      displayName: Other\n"
                    .as_slice(),
            ),
            (
                ConnectClientId::Pi,
                "models.json",
                br#"{"providers":{"other":{"name":"Other"}},"defaultModel":"other/current"}"#.as_slice(),
            ),
            (
                ConnectClientId::Omp,
                "models.yml",
                b"providers:\n  other:\n    name: Other\nmodelRoles:\n  default: other/current\n".as_slice(),
            ),
        ];

        for (tool, suffix, bytes) in cases {
            let empty_plan =
                plan_connect_client_apply(&standard_input(tool), &environment, &BTreeMap::new())
                    .unwrap_or_else(|error| panic!("missing {tool:?} plan: {error:?}"));
            let path = empty_plan
                .files
                .iter()
                .find(|file| file.path.ends_with(suffix))
                .expect("target path")
                .path
                .clone();
            let existing = BTreeMap::from([(std::path::PathBuf::from(&path), bytes.to_vec())]);
            let plan = plan_connect_client_apply(&standard_input(tool), &environment, &existing)
                .unwrap_or_else(|error| panic!("valid {tool:?} plan: {error:?}"));
            let next = planned_text(&plan, suffix);

            assert!(
                next.contains("stravia") || next.contains("Stravia"),
                "{tool:?} must contain Stravia-owned configuration: {next}"
            );
            assert!(
                !plan.preview.contains("other/current"),
                "{tool:?} preview must not write a current model: {}",
                plan.preview
            );
            assert!(
                !plan.preview.contains("stravia/route-"),
                "{tool:?} preview must not write a fused provider/model key: {}",
                plan.preview
            );
        }

        let opencode = plan_connect_client_apply(
            &standard_input(ConnectClientId::Opencode),
            &environment,
            &BTreeMap::from([(
                temporary
                    .path()
                    .join("xdg")
                    .join("opencode")
                    .join("opencode.json"),
                br#"{"model":"other/current","provider":{"other":{"name":"Other"}}}"#.to_vec(),
            )]),
        )
        .expect("OpenCode plan");
        let opencode: serde_json::Value =
            serde_json::from_str(&planned_text(&opencode, "opencode.json")).expect("OpenCode JSON");
        assert_eq!(opencode["model"], "other/current");
        assert_eq!(opencode["provider"]["other"]["name"], "Other");

        let workbuddy = plan_connect_client_apply(
            &standard_input(ConnectClientId::Workbuddy),
            &environment,
            &BTreeMap::new(),
        )
        .expect("WorkBuddy plan");
        let workbuddy: serde_json::Value =
            serde_json::from_str(&planned_text(&workbuddy, "models.json")).expect("WorkBuddy JSON");
        assert_eq!(workbuddy.as_array().expect("model list").len(), 2);
        assert_eq!(workbuddy[1]["id"], "route-two");
        assert_eq!(workbuddy[1]["name"], "Friendly Two");
    }

    #[test]
    fn malformed_global_config_returns_a_structured_error_without_next_bytes() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let environment = environment(temporary.path());
        let path = temporary.path().join(".codex").join("config.toml");
        let existing = BTreeMap::from([(path.clone(), b"[broken".to_vec())]);

        let error = plan_connect_client_apply(
            &standard_input(ConnectClientId::CodexCli),
            &environment,
            &existing,
        )
        .expect_err("malformed TOML must be refused");

        assert_eq!(error.code, "parse_error");
        assert_eq!(error.path, Some(path.display().to_string()));
        assert!(!error.message.is_empty());
    }

    #[test]
    fn default_resolution_ignores_project_local_and_per_file_overrides() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let mut environment = environment(temporary.path());
        environment.insert(
            "OPENCODE_CONFIG".to_owned(),
            temporary
                .path()
                .join("project")
                .join("opencode.json")
                .display()
                .to_string(),
        );
        environment.insert(
            "OPENCLAW_CONFIG_PATH".to_owned(),
            temporary
                .path()
                .join("project")
                .join("openclaw.json")
                .display()
                .to_string(),
        );

        let opencode = plan_connect_client_apply(
            &standard_input(ConnectClientId::Opencode),
            &environment,
            &BTreeMap::new(),
        )
        .expect("OpenCode plan");
        let openclaw = plan_connect_client_apply(
            &standard_input(ConnectClientId::Openclaw),
            &environment,
            &BTreeMap::new(),
        )
        .expect("OpenClaw plan");

        assert_eq!(
            opencode.paths[0],
            temporary
                .path()
                .join(".config")
                .join("opencode")
                .join("opencode.json")
                .display()
                .to_string()
        );
        assert_eq!(
            openclaw.paths[0],
            temporary
                .path()
                .join(".openclaw")
                .join("openclaw.json")
                .display()
                .to_string()
        );
        assert!(opencode.paths[0].contains(temporary.path().to_string_lossy().as_ref()));
        assert!(openclaw.paths[0].contains(temporary.path().to_string_lossy().as_ref()));
        assert!(!opencode.paths[0].contains("project"));
        assert!(!openclaw.paths[0].contains("project"));
    }

    #[test]
    fn portable_preview_uses_the_same_planner_payload_without_machine_paths() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let environment = environment(temporary.path());
        let input = standard_input(ConnectClientId::CodexCli);
        let native =
            plan_connect_client_apply(&input, &environment, &BTreeMap::new()).expect("native plan");
        let portable =
            preview_connect_client_apply(&input, &environment).expect("portable preview");
        let expected = native
            .preview
            .replace(&native.paths[0], "~/.codex/config.toml")
            .replace(&native.paths[1], "~/.codex/stravia-models.json");

        assert_eq!(portable.preview, expected);
        assert_eq!(
            portable.paths,
            ["~/.codex/config.toml", "~/.codex/stravia-models.json"]
        );
        assert!(portable.files.is_empty());
        assert!(
            !portable
                .preview
                .contains(&temporary.path().display().to_string())
        );
    }
}
