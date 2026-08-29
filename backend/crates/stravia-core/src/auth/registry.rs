use std::sync::Arc;

use crate::auth::drivers::{ClaudeOAuthDriver, GrokOAuthDriver, OpenAIOAuthDriver};
use crate::auth::types::{AuthDriver, AuthDriverMetadata};

pub fn normalize_driver_key(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "openai-oauth" | "openai_oauth" | "openai" | "codex-cli" | "codex" => "codex".to_string(),
        "xai" | "x-ai" | "x.ai" | "grok-build" | "grok_build" | "grok" => "grok".to_string(),
        "claude-code" | "claude_code" | "claude-oauth" | "claude_oauth" | "claude"
        | "anthropic" => "claude-code".to_string(),
        other => other.to_string(),
    }
}

pub fn build_driver(key: &str) -> Option<Arc<dyn AuthDriver>> {
    match normalize_driver_key(key).as_str() {
        "codex" => Some(Arc::new(OpenAIOAuthDriver)),
        "grok" => Some(Arc::new(GrokOAuthDriver::default())),
        "claude-code" => Some(Arc::new(ClaudeOAuthDriver)),
        _ => None,
    }
}

pub fn list_driver_metadata() -> Vec<AuthDriverMetadata> {
    [
        build_driver("codex"),
        build_driver("grok"),
        build_driver("claude-code"),
    ]
    .into_iter()
    .flatten()
    .map(|driver| driver.metadata())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::types::OAuthCallbackPort;

    #[test]
    fn oauth_drivers_publish_their_loopback_callback_contracts() {
        let codex = build_driver("codex").unwrap().metadata();
        let codex_callback = codex.callback.expect("Codex callback policy");
        assert_eq!(codex_callback.bind_host, "127.0.0.1");
        assert_eq!(codex_callback.redirect_host, "localhost");
        assert_eq!(codex_callback.path, "/auth/callback");
        assert_eq!(
            codex_callback.port,
            OAuthCallbackPort::Fixed {
                primary: 1455,
                fallback: 1457,
            }
        );
        assert_eq!(codex_callback.cancel_path, Some("/cancel"));

        let grok = build_driver("xai").unwrap().metadata();
        assert_eq!(grok.key, "grok");
        assert_eq!(grok.scheme, crate::auth::types::AuthScheme::OAuthDeviceCode);
        assert!(grok.callback.is_none());

        let claude = build_driver("claude-code").unwrap().metadata();
        let claude_callback = claude.callback.expect("Claude Code callback policy");
        assert_eq!(claude_callback.bind_host, "127.0.0.1");
        assert_eq!(claude_callback.redirect_host, "localhost");
        assert_eq!(claude_callback.path, "/callback");
        assert_eq!(claude_callback.port, OAuthCallbackPort::Dynamic);
        assert_eq!(
            claude_callback.manual_redirect_uri,
            "https://platform.claude.com/oauth/code/callback"
        );
    }
}
