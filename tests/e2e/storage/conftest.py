from __future__ import annotations

import json
import os
import secrets
import socket
import subprocess
import tempfile
import textwrap
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]


def find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def make_isolated_schema(prefix: str = "stravia_storage_e2e", *, max_len: int = 63) -> str:
    suffix = f"{int(time.time())}_{secrets.token_hex(3)}"
    keep = max(1, max_len - len(suffix) - 1)
    return f"{prefix[:keep]}_{suffix}"


def load_pg_url() -> str | None:
    """Use only the explicitly injected isolated-test PostgreSQL connection."""
    return os.environ.get("DB_URL")


class _MockHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt: str, *args: object) -> None:
        return

    def _write_json(self, status: int, payload: dict[str, object]) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        body = json.loads(raw) if raw else {}
        if self.path.split("?")[0] != "/v1/chat/completions":
            self._write_json(404, {"error": "not found"})
            return
        model = str(body.get("model", "mock"))
        self._write_json(
            200,
            {
                "id": "chatcmpl-storage-e2e",
                "object": "chat.completion",
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4},
            },
        )


def start_mock(port: int) -> ThreadingHTTPServer:
    server = ThreadingHTTPServer(("127.0.0.1", port), _MockHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def build_harness(work_dir: Path) -> None:
    core_path = (REPO_ROOT / "backend" / "crates" / "stravia-core").as_posix()
    server_path = (REPO_ROOT / "backend" / "apps" / "stravia-server").as_posix()
    cargo_toml = textwrap.dedent(
        f"""
        [package]
        name = "stravia-storage-e2e-harness"
        version = "0.1.0"
        edition = "2024"

        [dependencies]
        anyhow = "1"
        stravia-core = {{ path = "{core_path}" }}
        stravia-server = {{ path = "{server_path}", default-features = false }}
        # 依赖版本与特性必须与根 workspace 对齐(reqwest/sqlx 主版本一致、禁用默认
        # 特性),否则 harness 独立解析出的 native-tls/openssl 会与 CI 的 rust-lld
        # 链接冲突,且会重复编译另一套主版本依赖。
        reqwest = {{ version = "0.13", default-features = false, features = ["json"] }}
        serde_json = "1"
        sha2 = "0.10"
        sqlx = {{ version = "0.9", default-features = false, features = ["runtime-tokio", "postgres"] }}
        tokio = {{ version = "1", features = ["macros", "rt-multi-thread", "time"] }}
        """
    ).strip() + "\n"

    main_rs = textwrap.dedent(
        r"""
        use std::env;
        use std::path::PathBuf;
        use std::time::Duration;
        use anyhow::{Context, ensure};
        use stravia_core::admin::identity::AdminAuth;
        use stravia_core::config::{GatewayConfig, SqlStorageConfig, StorageBackendKind};
        use stravia_core::db::models::{
            CreateApiKey, CreateProvider, CreateRoute, CreateTarget, ProviderCredentialInput,
            ProviderSourceInput, PutRoute, UpdateRoute,
        };
        use stravia_core::provider_models::CreateManualProviderModel;
        use stravia_core::Gateway;
        use stravia_server::{AdminMode, HttpAppConfig, build_http_app, start_http_server, standalone_local_origins};
        use reqwest::StatusCode;
        use sha2::{Digest, Sha384};
        use sqlx::postgres::PgPoolOptions;

        #[tokio::main]
        async fn main() -> anyhow::Result<()> {
            if let Ok(action) = env::var("STRAVIA_STORAGE_SCHEMA_ACTION") {
                let pg_url = env::var("STRAVIA_STORAGE_PG_URL").context("STRAVIA_STORAGE_PG_URL")?;
                let schema = env::var("STRAVIA_STORAGE_PG_SCHEMA").context("STRAVIA_STORAGE_PG_SCHEMA")?;
                ensure!(
                    schema.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                    "invalid schema: {schema}"
                );
                let pool = PgPoolOptions::new().max_connections(1).connect(&pg_url).await?;
                match action.as_str() {
                    "create" => {
                        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
                            .execute(&pool)
                            .await?;
                    }
                    "drop" => {
                        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                            "DROP SCHEMA {schema} CASCADE"
                        )))
                        .execute(&pool)
                        .await?;
                    }
                    "prepare_legacy" => {
                        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET search_path TO {schema}")))
                            .execute(&pool)
                            .await?;
                        sqlx::raw_sql(
                            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL, installed_on TIMESTAMPTZ NOT NULL DEFAULT now(), success BOOLEAN NOT NULL, checksum BYTEA NOT NULL, execution_time BIGINT NOT NULL)",
                        )
                        .execute(&pool)
                        .await?;
                        let migration_dir = PathBuf::from(
                            env::var("STRAVIA_STORAGE_MIGRATIONS").context("STRAVIA_STORAGE_MIGRATIONS")?,
                        );
                        let mut migrations = std::fs::read_dir(migration_dir)?
                            .collect::<Result<Vec<_>, _>>()?;
                        migrations.sort_by_key(|entry| entry.file_name());
                        for entry in migrations {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            let Some((version, description)) = name.strip_suffix(".sql").and_then(|name| name.split_once('_')) else { continue; };
                            let version: i64 = version.parse()?;
                            if version >= 34 { continue; }
                            let sql = std::fs::read_to_string(entry.path())?;
                            // SQL 仅来自测试指定的仓库迁移文件，不插入请求或配置数据。
                            sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
                                .execute(&pool)
                                .await?;
                            sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES ($1, $2, TRUE, $3, 0)")
                                .bind(version)
                                .bind(description.replace('_', " "))
                                .bind(Sha384::digest(sql.as_bytes()).to_vec())
                                .execute(&pool)
                                .await?;
                        }
                        sqlx::query("INSERT INTO request_logs (id, created_at, client_request_body) VALUES ($1, $2, $3)")
                            .bind("legacy-log-must-not-survive")
                            .bind(1_i64)
                            .bind(r#"{"secret":"legacy"}"#)
                            .execute(&pool)
                            .await?;
                    }
                    "inspect_observation" => {
                        let tables: i64 = sqlx::query_scalar(
                            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = $1 AND table_name IN ('interaction_observations', 'inference_run_observations', 'model_turn_observations', 'target_attempt_observations', 'observation_events', 'rejected_request_observations', 'debug_trace_manifests')",
                        )
                        .bind(&schema)
                        .fetch_one(&pool)
                        .await?;
                        let legacy: i64 = sqlx::query_scalar(
                            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = $1 AND table_name = 'request_logs'",
                        )
                        .bind(&schema)
                        .fetch_one(&pool)
                        .await?;
                        let indexes: i64 = sqlx::query_scalar(
                            "SELECT COUNT(*) FROM pg_indexes WHERE schemaname = $1 AND indexname IN ('interaction_observations_window_idx', 'model_turns_analytics_idx', 'target_attempts_analytics_idx', 'observation_events_expiry_idx')",
                        )
                        .bind(&schema)
                        .fetch_one(&pool)
                        .await?;
                        println!("observation_tables={tables}");
                        println!("legacy_tables={legacy}");
                        println!("observation_indexes={indexes}");
                    }
                    other => anyhow::bail!("unknown schema action: {other}"),
                }
                pool.close().await;
                return Ok(());
            }

            let backend = env::var("STRAVIA_STORAGE_BACKEND").context("STRAVIA_STORAGE_BACKEND")?;
            let upstream = env::var("STRAVIA_STORAGE_UPSTREAM").context("STRAVIA_STORAGE_UPSTREAM")?;
            let data_dir = PathBuf::from(env::var("STRAVIA_STORAGE_DATA_DIR").context("STRAVIA_STORAGE_DATA_DIR")?);
            let server_port: u16 = env::var("STRAVIA_STORAGE_SERVER_PORT")
                .context("STRAVIA_STORAGE_SERVER_PORT")?
                .parse()
                .context("invalid STRAVIA_STORAGE_SERVER_PORT")?;

            let mut config = GatewayConfig {
                data_dir,
                ..Default::default()
            };

            match backend.as_str() {
                "sqlite" => {
                    config.storage.backend = StorageBackendKind::Sqlite;
                }
                "postgres" => {
                    let pg_url = env::var("STRAVIA_STORAGE_PG_URL").context("STRAVIA_STORAGE_PG_URL")?;
                    let schema = env::var("STRAVIA_STORAGE_PG_SCHEMA").context("STRAVIA_STORAGE_PG_SCHEMA")?;
                    ensure!(
                        schema.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                        "invalid schema: {schema}"
                    );
                    let pool = PgPoolOptions::new().max_connections(1).connect(&pg_url).await?;
                    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA IF NOT EXISTS {schema}"))).execute(&pool).await?;
                    pool.close().await;

                    let url_with_schema = if pg_url.contains('?') {
                        format!("{pg_url}&options=-csearch_path%3D{schema}")
                    } else {
                        format!("{pg_url}?options=-csearch_path%3D{schema}")
                    };
                    config.storage.backend = StorageBackendKind::Postgres;
                    config.storage.postgres = SqlStorageConfig {
                        url: Some(url_with_schema),
                        ..Default::default()
                    };
                }
                other => anyhow::bail!("unknown backend: {other}"),
            }

            let gw = Gateway::new(config).await?;
            let admin = gw.admin();
            let provider = admin.create_provider(CreateProvider {
                name: Some(format!("{backend}-e2e-provider")),
                source: ProviderSourceInput::Custom {
                    vendor: Some("custom".to_string()),
                    protocol: "openai".to_string(),
                    base_url: format!("{upstream}/v1"),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "dummy".to_string(),
                },
                use_proxy: false,
            }).await?;
            admin.create_manual_provider_model(
                &provider.id,
                "gpt-4o-mini",
                CreateManualProviderModel {
                    metadata: serde_json::json!({
                        "id": "gpt-4o-mini",
                        "name": "GPT-4o mini",
                    }),
                },
            ).await?;

            let route = admin.create_model(CreateRoute {
                model_id: format!("{backend}-model"),
                display_name: Some(format!("{backend} Model")),
                balance: None,
                target_provider: provider.id.clone(),
                target_model: "gpt-4o-mini".to_string(),
                targets: vec![],
            }).await?;

            let api_key = admin.create_api_key(CreateApiKey {
                key: None,
                name: format!("{backend}-e2e-key"),
                concurrency_limit: Some(10),
                mcp_access_enabled: false,
                transparent_injection_enabled: false,
                inject_media_understanding: false,
                inject_web_search: false,
                expires_at: None,
                model_ids: vec![route.id.clone()],
            }).await?;

            ensure!(admin.list_providers().await?.len() == 1, "provider count");
            let routes = admin.list_models().await?;
            ensure!(routes.len() == 1, "Route count");
            ensure!(routes[0].model_id == format!("{backend}-model"), "Route Model ID");
            ensure!(routes[0].display_name.as_deref() == Some(format!("{backend} Model").as_str()), "Route display name");
            ensure!(routes[0].targets.len() == 1, "Route aggregate Target count");
            ensure!(routes[0].targets[0].provider_id == provider.id, "Route aggregate Provider");
            let updated = admin.update_model(&route.model_id, UpdateRoute {
                display_name: Some(format!("{backend} Renamed Model")),
                ..Default::default()
            }).await?;
            ensure!(updated.id == route.id, "display-name update preserves Route identity");
            ensure!(updated.model_id == route.model_id, "display-name update preserves Model ID");
            ensure!(updated.display_name.as_deref() == Some(format!("{backend} Renamed Model").as_str()), "updated Route display name");
            let failed_put = gw.storage.routes().put(PutRoute {
                id: Some(route.id.clone()),
                model_id: route.model_id.clone(),
                display_name: updated.display_name.clone(),
                selection_strategy: "latency_preference".to_string(),
                is_enabled: false,
                targets: vec![CreateTarget {
                    provider_id: "missing-provider".to_string(),
                    model: "missing-model".to_string(),
                    enabled: true,
                    priority: Some(1),
                    first_token_timeout_ms: Some(60_000),
                    target_retry_budget: Some(5),
                    target_cooldown_ms: Some(120_000),
                    thinking_level_map: vec![],
                }],
            }).await;
            ensure!(failed_put.is_err(), "invalid Route aggregate put must fail");
            let preserved = gw.storage.routes().get(&route.model_id).await?.context("preserved Route")?;
            ensure!(preserved.balance == route.balance, "failed put preserves Route strategy");
            ensure!(preserved.is_enabled == route.is_enabled, "failed put preserves Route state");
            ensure!(preserved.targets.len() == 1, "failed put preserves Target count");
            ensure!(preserved.targets[0].provider_id == provider.id, "failed put preserves Target");
            ensure!(admin.list_api_keys().await?.len() == 1, "api key count");

            let admin_auth = AdminAuth::new(gw.storage.clone());
            admin_auth.ensure_native_admin().await?;
            let native_session = admin_auth.login_native().await?;
            let cors_origins = standalone_local_origins(server_port);
            let app = build_http_app(gw, HttpAppConfig {
                admin_auth,
                admin_mode: AdminMode::Desktop,
                admin_origin: None,
                admin_cors_origins: cors_origins.clone(),
                proxy_cors_origins: cors_origins,
                serve_embedded_webui: false,
            });
            let server = start_http_server(format!("127.0.0.1:{server_port}"), app).await?;

            let client = reqwest::Client::new();
            let admin_status = client
                .get(format!("http://127.0.0.1:{server_port}/api/v1/status"))
                .bearer_auth(&native_session.access_token)
                .send()
                .await?;
            ensure!(admin_status.status() == StatusCode::OK, "native admin bearer should 200");
            let url = format!("http://127.0.0.1:{server_port}/v1/chat/completions");
            let payload = serde_json::json!({"model": format!("{backend}-model"), "messages": [{"role":"user","content":"hi"}]});

            let no_key = client.post(&url).json(&payload).send().await?;
            ensure!(no_key.status() == StatusCode::UNAUTHORIZED, "missing key should 401");

            let ok = client.post(&url).bearer_auth(&api_key.token).json(&payload).send().await?;
            ensure!(ok.status() == StatusCode::OK, "valid key should 200");
            let body: serde_json::Value = ok.json().await?;
            ensure!(body["choices"][0]["message"]["content"].as_str() == Some("ok"), "content mismatch");

            let mut observation_roots = 0i64;
            let mut stats_requests = 0i64;
            for _ in 0..20 {
                let forest = client
                    .get(format!("http://127.0.0.1:{server_port}/api/v1/observations/interactions?limit=10"))
                    .bearer_auth(&native_session.access_token)
                    .send()
                    .await?;
                ensure!(forest.status() == StatusCode::OK, "Observation forest should 200");
                let forest: serde_json::Value = forest.json().await?;
                observation_roots = forest["data"]["root_total"].as_i64().unwrap_or(0);
                let stats = admin.get_stats_overview(None).await?;
                stats_requests = stats.total_requests;
                if observation_roots >= 1 && stats_requests >= 1 {
                    println!("backend={backend}");
                    println!("observation_roots={observation_roots}");
                    println!("stats_total_requests={stats_requests}");
                    println!("proxy_status_ok=200");
                    println!("proxy_status_no_key=401");
                    admin.delete_provider(&provider.id).await?;
                    ensure!(admin.list_models().await?.is_empty(), "Provider delete removes empty Route");
                    server.shutdown().await?;
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            server.shutdown().await?;
            anyhow::bail!("observation/stat timeout: roots={observation_roots} requests={stats_requests}");
        }
        """
    ).strip() + "\n"

    (work_dir / "Cargo.toml").write_text(cargo_toml, encoding="utf-8")
    src_dir = work_dir / "src"
    src_dir.mkdir(parents=True, exist_ok=True)
    (src_dir / "main.rs").write_text(main_rs, encoding="utf-8")


def run_harness(
    backend: str,
    *,
    upstream_port: int,
    work_dir: Path,
    pg_url: str | None = None,
) -> str:
    env = os.environ.copy()
    env["STRAVIA_STORAGE_BACKEND"] = backend
    env["STRAVIA_STORAGE_UPSTREAM"] = f"http://127.0.0.1:{upstream_port}"
    env["STRAVIA_STORAGE_SERVER_PORT"] = str(find_free_port())
    env["STRAVIA_STORAGE_DATA_DIR"] = str(work_dir / f"{backend}-data")

    if backend == "postgres":
        if not pg_url:
            raise RuntimeError("postgres backend requires DB_URL")
        env["STRAVIA_STORAGE_PG_URL"] = pg_url
        env["STRAVIA_STORAGE_PG_SCHEMA"] = make_isolated_schema()

    proc = subprocess.run(
        ["cargo", "run", "--quiet", "--manifest-path", str(work_dir / "Cargo.toml")],
        env=env,
        cwd=str(REPO_ROOT),
        text=True,
        encoding="utf-8",
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"backend={backend} harness failed\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}"
        )
    return proc.stdout


def postgres_dsn_for_schema(pg_url: str, schema: str) -> str:
    search_path = f"options=-csearch_path%3D{schema}"
    separator = "&" if "?" in pg_url else "?"
    return f"{pg_url}{separator}{search_path}"


def run_schema_action(action: str, *, work_dir: Path, pg_url: str, schema: str) -> str:
    env = os.environ.copy()
    env["STRAVIA_STORAGE_SCHEMA_ACTION"] = action
    env["STRAVIA_STORAGE_PG_URL"] = pg_url
    env["STRAVIA_STORAGE_PG_SCHEMA"] = schema
    env["STRAVIA_STORAGE_MIGRATIONS"] = str(
        REPO_ROOT / "backend" / "crates" / "stravia-core" / "migrations" / "postgres"
    )

    proc = subprocess.run(
        ["cargo", "run", "--quiet", "--manifest-path", str(work_dir / "Cargo.toml")],
        env=env,
        cwd=str(REPO_ROOT),
        text=True,
        encoding="utf-8",
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"schema action={action} failed\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}"
        )
    return proc.stdout


@pytest.fixture(scope="module")
def storage_runtime() -> dict[str, object]:
    upstream_port = find_free_port()
    mock_server = start_mock(upstream_port)

    try:
        with tempfile.TemporaryDirectory(prefix="stravia-storage-e2e-") as tmp:
            tmpdir = Path(tmp)
            build_harness(tmpdir)
            yield {
                "upstream_port": upstream_port,
                "work_dir": tmpdir,
                "pg_url": load_pg_url(),
                "make_isolated_schema": make_isolated_schema,
                "run_harness": run_harness,
                "postgres_dsn_for_schema": postgres_dsn_for_schema,
                "run_schema_action": run_schema_action,
            }
    finally:
        mock_server.shutdown()
        mock_server.server_close()
