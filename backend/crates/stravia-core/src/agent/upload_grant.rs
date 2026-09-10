use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::artifact::{ArtifactError, ArtifactSettings};

pub(crate) const UPLOAD_PLACEHOLDER: &str = "<stravia-upload-key>";
const UPLOAD_KEY_PREFIX: &str = "stravia_upload_";
const SIGNING_KEY_SETTING: &str = "artifact_upload_signing_key";
const GRANT_LIFETIME_MILLIS: i64 = 15 * 60 * 1000;

// 不实现 Debug：签名密钥与已签发凭据只属于认证和客户端交付。
pub(crate) struct UploadGrantIssuer {
    encoding: EncodingKey,
    decoding: DecodingKey,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

pub(crate) struct UploadGrant {
    pub key: String,
    pub expires_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
struct UploadClaims {
    sub: String,
    expires_at: i64,
    nonce: String,
    purpose: String,
}

impl UploadGrantIssuer {
    pub(crate) async fn load(
        sqlite: Option<&sqlx::SqlitePool>,
        postgres: Option<&sqlx::PgPool>,
    ) -> anyhow::Result<Self> {
        let candidate = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        // 并发初始化只插入一次；重启不能使已交付的短期上传授权失效。
        let secret: String = if let Some(pool) = sqlite {
            sqlx::query(
                "INSERT INTO settings (name, value) VALUES (?, ?) ON CONFLICT(name) DO NOTHING",
            )
            .bind(SIGNING_KEY_SETTING)
            .bind(&candidate)
            .execute(pool)
            .await?;
            sqlx::query_scalar("SELECT value FROM settings WHERE name = ?")
                .bind(SIGNING_KEY_SETTING)
                .fetch_one(pool)
                .await?
        } else if let Some(pool) = postgres {
            sqlx::query(
                "INSERT INTO settings (name, value) VALUES ($1, $2) ON CONFLICT(name) DO NOTHING",
            )
            .bind(SIGNING_KEY_SETTING)
            .bind(&candidate)
            .execute(pool)
            .await?;
            sqlx::query_scalar("SELECT value FROM settings WHERE name = $1")
                .bind(SIGNING_KEY_SETTING)
                .fetch_one(pool)
                .await?
        } else {
            candidate
        };
        let secret = URL_SAFE_NO_PAD.decode(secret)?;
        anyhow::ensure!(secret.len() == 32, "invalid Artifact upload signing key");
        Ok(Self::with_clock(
            &secret,
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
        ))
    }

    pub(crate) fn with_clock(secret: &[u8], clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        Self {
            encoding: EncodingKey::from_secret(secret),
            decoding: DecodingKey::from_secret(secret),
            clock,
        }
    }

    pub(crate) fn issue(&self, principal: &Principal) -> Result<UploadGrant, ArtifactError> {
        let expires_at = (self.clock)().saturating_add(GRANT_LIFETIME_MILLIS);
        let claims = UploadClaims {
            sub: principal.api_key_id().to_owned(),
            expires_at,
            nonce: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 24]>()),
            purpose: "artifact-upload".to_owned(),
        };
        let signed = encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .map_err(|_| ArtifactError::Storage("Artifact upload signing failed".into()))?;
        Ok(UploadGrant {
            key: format!("{UPLOAD_KEY_PREFIX}{signed}"),
            expires_at,
        })
    }

    pub(crate) fn is_valid(&self, grant: &UploadGrant) -> bool {
        grant.expires_at > (self.clock)()
    }

    pub(crate) fn authenticate(&self, key: &str) -> Result<Principal, ArtifactError> {
        let signed = key
            .strip_prefix(UPLOAD_KEY_PREFIX)
            .ok_or(ArtifactError::Unauthorized)?;
        let mut validation = Validation::new(Algorithm::HS256);
        // 私有授权契约用毫秒表达固定到期时间，不使用 JWT 默认秒级宽限。
        validation.required_spec_claims.clear();
        validation.validate_exp = false;
        validation.validate_aud = false;
        let claims = decode::<UploadClaims>(signed, &self.decoding, &validation)
            .map_err(|_| ArtifactError::Unauthorized)?
            .claims;
        if claims.purpose != "artifact-upload"
            || claims.expires_at <= (self.clock)()
            || claims.sub.is_empty()
            || claims.sub == "anonymous"
        {
            return Err(ArtifactError::Unauthorized);
        }
        Ok(Principal::new(claims.sub))
    }
}

pub(crate) fn scrub_upload_grants(input: &str) -> Cow<'_, str> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    if !input.contains(UPLOAD_KEY_PREFIX) {
        return Cow::Borrowed(input);
    }
    // 保留的凭据语法在过期、重启和关闭注入后仍受保护，不依赖有效 token 集合。
    PATTERN
        .get_or_init(|| {
            regex::Regex::new(r"stravia_upload_[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+")
                .expect("upload credential syntax")
        })
        .replace_all(input, UPLOAD_PLACEHOLDER)
}

pub(crate) fn scrub_upload_grant_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            if let Cow::Owned(clean) = scrub_upload_grants(text) {
                *text = clean;
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                scrub_upload_grant_value(value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                scrub_upload_grant_value(value);
            }
            let renamed: Vec<_> = values
                .keys()
                .filter_map(|key| match scrub_upload_grants(key) {
                    Cow::Owned(clean) => Some((key.clone(), clean)),
                    Cow::Borrowed(_) => None,
                })
                .collect();
            for (key, clean) in renamed {
                if let Some(value) = values.remove(&key) {
                    values.insert(clean, value);
                }
            }
        }
        _ => {}
    }
}

async fn settings(gateway: &crate::Gateway) -> Result<ArtifactSettings, ArtifactError> {
    gateway
        .storage
        .settings()
        .get("artifact_settings")
        .await
        .map_err(|error| ArtifactError::Storage(error.to_string()))?
        .map(|value| {
            serde_json::from_str(&value)
                .map_err(|_| ArtifactError::Invalid("invalid Artifact settings".into()))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(crate) async fn upload_prompt_enabled(gateway: &crate::Gateway) -> Result<bool, ArtifactError> {
    Ok(settings(gateway).await?.upload_prompt_injection)
}

pub(crate) async fn upload_instructions(
    gateway: &crate::Gateway,
) -> Result<Option<String>, ArtifactError> {
    let settings = settings(gateway).await?;
    if !settings.upload_prompt_injection {
        return Ok(None);
    }
    if settings.client_base_url.is_empty() {
        return Err(ArtifactError::Invalid(
            "client access address is required for upload instructions".into(),
        ));
    }
    // JSON/shell 参数分别编码；保存的地址含单引号时仍不能逃出 shell 引号。
    let base = settings
        .client_base_url
        .trim_end_matches('/')
        .replace('\'', "'\\''");
    Ok(Some(format!(
        r#"When a client needs to submit a local file, explain or execute this client-side multipart upload workflow. The temporary upload credential below grants only uploads for fifteen minutes and can upload multiple files. Never send a local path for Stravia to fetch. Use the returned reference in a structured attachment or StraviaRead.
Requires curl, jq and a POSIX shell. Set FILE to the local file and MIME to its actual media type.
```sh
FILE='/path/to/file'
MIME='application/octet-stream'
BASE='{base}'
AUTH='Bearer <stravia-upload-key>'
SIZE=$(wc -c < "$FILE" | tr -d '[:space:]')
UPLOAD=$(curl --fail-with-body -sS "$BASE/v1/artifacts/uploads" -H "Authorization: $AUTH" -H 'Content-Type: application/json' --data "$(jq -n --arg mime "$MIME" --argjson size "$SIZE" '{{mime_type:$mime,size:$size}}')") || exit 1
ID=$(printf '%s' "$UPLOAD" | jq -er '.upload_id') || exit 1
TOKEN=$(printf '%s' "$UPLOAD" | jq -er '.upload_token') || exit 1
PART=$(curl --fail-with-body -sS -X PUT "$BASE/v1/artifacts/uploads/$ID/parts/1" -H "Authorization: $AUTH" -H "x-upload-token: $TOKEN" --data-binary "@$FILE") || exit 1
curl --fail-with-body -sS "$BASE/v1/artifacts/uploads/$ID/complete" -H "Authorization: $AUTH" -H 'Content-Type: application/json' --data "$(jq -n --arg token "$TOKEN" --argjson part "$PART" '{{upload_token:$token,parts:[$part]}}')"
```
The completion response includes the stable `reference`. It is not a download credential; only the same Principal may resolve it. Upload at most 100 MiB per file."#
    )))
}
