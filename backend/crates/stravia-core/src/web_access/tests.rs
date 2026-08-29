use super::*;

#[test]
fn search_contract_normalizes_domains_and_rejects_overlap() {
    let normalized = validate_search_request(SearchRequest {
        query: "Rust 1.90".into(),
        max_results: 5,
        allowed_domains: vec!["Docs.RS".into()],
        blocked_domains: vec![],
    })
    .expect("valid request");
    assert_eq!(normalized.allowed_domains, ["docs.rs"]);

    let conflict = validate_search_request(SearchRequest {
        query: "Rust".into(),
        max_results: 5,
        allowed_domains: vec!["docs.rs".into()],
        blocked_domains: vec!["DOCS.RS".into()],
    });
    assert!(matches!(
        conflict,
        Err(WebAccessError {
            code: WebAccessErrorCode::InvalidInput,
            ..
        })
    ));
}
#[test]
fn request_schemas_reject_unknown_fields() {
    assert!(
        serde_json::from_value::<SearchRequest>(serde_json::json!({
            "query": "Rust",
            "unexpected": true,
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<FetchRequest>(serde_json::json!({
            "urls": ["https://8.8.8.8/"],
            "unexpected": true,
        }))
        .is_err()
    );
}

#[tokio::test]
async fn fetch_contract_rejects_local_and_reserved_network_targets() {
    for url in [
        "http://192.0.0.1/",
        "http://192.0.0.8/",
        "http://192.0.0.11/",
        "http://192.88.99.2/",
        "http://localhost/",
        "http://service.local/",
        "http://127.0.0.1/",
        "http://192.168.1.1/",
        "http://198.18.0.1/",
        "http://[::ffff:127.0.0.1]/",
        "http://[::192.168.1.1]/",
        "http://[fec0::1]/",
        "http://[100::1]/",
        "http://[2001:2::1]/",
        "http://[64:ff9b:1::1]/",
        "http://[64:ff9b::a00:101]/",
        "http://[2002:a00:100::1]/",
        "http://[100:0:0:1::1]/",
        "http://[3fff::1]/",
        "http://[5f00::1]/",
        "http://[2001:5::1]/",
        "http://[1234::1]/",
        "http://[4000::1]/",
        "http://[8000::1]/",
        "http://[2001:1::4]/",
    ] {
        let result = validate_fetch_request(FetchRequest {
            urls: vec![url.into()],
            max_characters: 1_000,
        })
        .await;
        assert!(
            matches!(
                result,
                Err(WebAccessError {
                    code: WebAccessErrorCode::InvalidInput,
                    ..
                })
            ),
            "expected invalid_input for {url}, got {result:?}"
        );
    }
    assert!(
        validate_fetch_request(FetchRequest {
            urls: vec!["https://8.8.8.8/".into()],
            max_characters: 1_000,
        })
        .await
        .is_ok()
    );
    for url in ["https://192.0.0.9/", "https://192.0.0.10/"] {
        assert!(
            validate_fetch_request(FetchRequest {
                urls: vec![url.into()],
                max_characters: 1_000,
            })
            .await
            .is_ok(),
            "IANA globally reachable IPv4 address should pass: {url}"
        );
    }
    for url in [
        "https://[2001:20::1]/",
        "https://[2001:2f:ffff::1]/",
        "https://[2001:30::1]/",
        "https://[2001:3f:ffff::1]/",
    ] {
        assert!(
            validate_fetch_request(FetchRequest {
                urls: vec![url.into()],
                max_characters: 1_000,
            })
            .await
            .is_ok(),
            "IANA globally reachable IPv6 range should pass: {url}"
        );
    }
    let unresolved = validate_fetch_request(FetchRequest {
        urls: vec!["https://does-not-exist.invalid/".into()],
        max_characters: 1_000,
    })
    .await;
    assert!(matches!(
        unresolved,
        Err(WebAccessError {
            code: WebAccessErrorCode::Unavailable,
            ..
        })
    ));
}

#[test]
fn fetch_result_serializes_false_truncated() {
    let result = FetchResult {
        url: "https://8.8.8.8/".into(),
        status: FetchStatus::Success,
        content: Some("ok".into()),
        format: Some("text".into()),
        title: None,
        truncated: false,
        error: None,
    };
    let value = serde_json::to_value(result).expect("fetch result encoding");
    assert_eq!(value["truncated"], false);
}

#[test]
fn provider_usage_keeps_only_whitelisted_numeric_fields() {
    let usage = ProviderUsage::from_payload(&serde_json::json!({
        "usage": {
            "input_tokens": 12,
            "output_tokens": 8,
            "total_tokens": 20,
            "credits": 1.5,
            "cost": 0.125,
            "query": "do not retain",
            "url": "https://secret.example/",
            "content": "do not retain",
            "metadata": { "input_tokens": 999 }
        }
    }))
    .expect("numeric usage");

    assert_eq!(usage.input_tokens, Some(12));
    assert_eq!(usage.output_tokens, Some(8));
    assert_eq!(usage.total_tokens, Some(20));
    assert_eq!(usage.credits, Some(1.5));
    assert_eq!(usage.cost, Some(0.125));
    let debug = format!("{usage:?}");
    assert!(!debug.contains("do not retain"));
    assert!(!debug.contains("secret.example"));
}

struct FakeSearchProvider {
    response: Result<SearchResponse, ProviderFailure>,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl WebProviderAdapter for FakeSearchProvider {
    fn supports_search(&self) -> bool {
        true
    }

    fn supports_fetch(&self) -> bool {
        false
    }

    async fn search(
        &self,
        _request: &SearchRequest,
    ) -> Result<AdapterSuccess<SearchResponse>, ProviderFailure> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.response
            .clone()
            .map(|response| AdapterSuccess::new(response, None))
    }

    async fn fetch(
        &self,
        _request: &FetchRequest,
    ) -> Result<AdapterSuccess<Vec<FetchResult>>, ProviderFailure> {
        unreachable!("search-only fake")
    }
}

#[tokio::test]
async fn search_fails_over_once_and_empty_results_stop_the_chain() {
    let first = std::sync::Arc::new(FakeSearchProvider {
        response: Err(ProviderFailure::unavailable("upstream failed")),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let second = std::sync::Arc::new(FakeSearchProvider {
        response: Ok(SearchResponse {
            mode: SearchMode::Index,
            query: "Rust".into(),
            results: vec![],
            answer: None,
            citations: None,
        }),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let third = std::sync::Arc::new(FakeSearchProvider {
        response: Err(ProviderFailure::unavailable("must not run")),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let engine = WebAccessEngine::new(vec![first.clone(), second.clone(), third.clone()], vec![]);

    let response = engine
        .search(SearchRequest {
            query: "Rust".into(),
            max_results: 5,
            allowed_domains: vec![],
            blocked_domains: vec![],
        })
        .await
        .expect("second provider succeeds");

    assert!(response.results.is_empty());
    assert_eq!(first.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(third.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn search_exhaustion_returns_provider_neutral_error_message() {
    let provider = Arc::new(FakeSearchProvider {
        response: Err(ProviderFailure::unavailable(
            "Exa private response must not reach callers",
        )),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let error = WebAccessEngine::new(vec![provider], vec![])
        .search(SearchRequest {
            query: "Rust".into(),
            max_results: 5,
            allowed_domains: vec![],
            blocked_domains: vec![],
        })
        .await
        .expect_err("search should fail");

    assert_eq!(error.code, WebAccessErrorCode::Unavailable);
    assert_eq!(error.message, "Web Search is unavailable");
    assert!(!error.message.contains("Exa"));
}

#[tokio::test]
async fn search_strictly_filters_allowed_and_blocked_subdomains() {
    let provider = Arc::new(FakeSearchProvider {
        response: Ok(SearchResponse {
            mode: SearchMode::Index,
            query: "Rust".into(),
            results: vec![
                SearchResult {
                    url: "https://guide.docs.rs/start".into(),

                    title: None,
                    snippet: None,
                },
                SearchResult {
                    url: "https://blocked.docs.rs/".into(),
                    title: None,
                    snippet: None,
                },
                SearchResult {
                    url: "https://example.com/".into(),
                    title: None,
                    snippet: None,
                },
            ],
            answer: None,
            citations: None,
        }),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let engine = WebAccessEngine::new(vec![provider], vec![]);

    let response = engine
        .search(SearchRequest {
            query: "Rust".into(),
            max_results: 5,
            allowed_domains: vec!["docs.rs".into()],
            blocked_domains: vec!["blocked.docs.rs".into()],
        })
        .await
        .expect("provider succeeds");

    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].url, "https://guide.docs.rs/start");
}
#[tokio::test]
async fn configuration_changes_do_not_replace_an_inference_run_snapshot() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (gateway, _logs) = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("gateway");
    let admin = gateway.admin();
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Web key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            inject_web_search: true,
            model_ids: vec![],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let provider = admin
        .create_web_provider(crate::db::models::CreateWebProvider {
            name: "Exa".into(),
            kind: "exa".into(),
            api_key: Some("test-exa-key".into()),
        })
        .await
        .expect("Web Provider");
    admin
        .update_web_access_settings(WebAccessSettings {
            enabled: true,
            search_provider_ids: vec![provider.id.clone()],
            fetch_provider_ids: vec![provider.id],
        })
        .await
        .expect("enabled settings");
    let service = gateway.web_access();
    let old_availability = service
        .capture_run_snapshot("run-old", &key.id)
        .await
        .expect("old snapshot");
    assert_eq!(
        old_availability,
        WebAccessAvailability {
            search: true,
            fetch: true
        }
    );

    admin
        .update_web_access_settings(WebAccessSettings {
            enabled: false,
            search_provider_ids: vec![],
            fetch_provider_ids: vec![],
        })
        .await
        .expect("disabled settings");

    assert!(service.run_snapshot("run-old", &key.id).is_ok());
    assert_eq!(
        service
            .capture_run_snapshot("run-new", &key.id)
            .await
            .expect("new snapshot"),
        WebAccessAvailability::default()
    );
    assert!(service.run_snapshot("run-new", &key.id).is_err());
}

struct FakeFetchProvider {
    fail_url: Option<String>,
    content_characters: usize,
    calls: std::sync::Mutex<Vec<Vec<String>>>,
}

#[async_trait::async_trait]
impl WebProviderAdapter for FakeFetchProvider {
    fn supports_search(&self) -> bool {
        false
    }

    fn supports_fetch(&self) -> bool {
        true
    }

    async fn search(
        &self,
        _request: &SearchRequest,
    ) -> Result<AdapterSuccess<SearchResponse>, ProviderFailure> {
        unreachable!("fetch-only fake")
    }

    async fn fetch(
        &self,
        request: &FetchRequest,
    ) -> Result<AdapterSuccess<Vec<FetchResult>>, ProviderFailure> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(request.urls.clone());
        Ok(AdapterSuccess::new(
            request
                .urls
                .iter()
                .map(|url| {
                    if self.fail_url.as_deref() == Some(url) {
                        FetchResult {
                            url: url.clone(),
                            status: FetchStatus::Error,
                            content: None,
                            format: None,
                            title: None,
                            truncated: false,
                            error: Some(WebAccessPublicError {
                                code: WebAccessErrorCode::Unavailable,
                                message: None,
                            }),
                        }
                    } else {
                        FetchResult {
                            url: url.clone(),
                            status: FetchStatus::Success,
                            content: Some("x".repeat(self.content_characters)),
                            format: Some("markdown".into()),
                            title: None,
                            truncated: false,
                            error: None,
                        }
                    }
                })
                .collect(),
            None,
        ))
    }
}

#[tokio::test]
async fn fetch_retries_only_failed_urls_and_preserves_input_order() {
    let first = Arc::new(FakeFetchProvider {
        fail_url: Some("https://8.8.8.8/b".into()),
        content_characters: 7,
        calls: std::sync::Mutex::new(vec![]),
    });
    let second = Arc::new(FakeFetchProvider {
        fail_url: None,
        content_characters: 7,
        calls: std::sync::Mutex::new(vec![]),
    });
    let engine = WebAccessEngine::new(vec![], vec![first.clone(), second.clone()]);

    let response = engine
        .fetch(FetchRequest {
            urls: vec!["https://8.8.8.8/a".into(), "https://8.8.8.8/b".into()],
            max_characters: 8_000,
        })
        .await
        .expect("partial failure is recovered");

    assert_eq!(
        response
            .results
            .iter()
            .map(|result| result.url.as_str())
            .collect::<Vec<_>>(),
        ["https://8.8.8.8/a", "https://8.8.8.8/b"]
    );
    assert_eq!(
        second.calls.lock().expect("calls lock").as_slice(),
        &[vec!["https://8.8.8.8/b".to_string()]]
    );
}

#[tokio::test]
async fn fetch_applies_a_fair_total_limit_and_marks_all_failures() {
    let large = Arc::new(FakeFetchProvider {
        fail_url: None,
        content_characters: 30_000,
        calls: std::sync::Mutex::new(vec![]),
    });
    let engine = WebAccessEngine::new(vec![], vec![large]);
    let response = engine
        .fetch(FetchRequest {
            urls: vec![
                "https://8.8.8.8/a".into(),
                "https://8.8.8.8/b".into(),
                "https://8.8.8.8/c".into(),
            ],
            max_characters: 50_000,
        })
        .await
        .expect("large Fetch response");
    let fair_limit = MAX_FETCH_TOTAL_CHARACTERS / 3;
    assert!(response.results.iter().all(|result| {
        result.truncated
            && result.content.as_ref().map(|value| value.chars().count()) == Some(fair_limit)
    }));

    let first = Arc::new(FakeFetchProvider {
        fail_url: Some("https://8.8.8.8/failed".into()),
        content_characters: 7,
        calls: std::sync::Mutex::new(vec![]),
    });
    let second = Arc::new(FakeFetchProvider {
        fail_url: Some("https://8.8.8.8/failed".into()),
        content_characters: 7,
        calls: std::sync::Mutex::new(vec![]),
    });
    let response = WebAccessEngine::new(vec![], vec![first, second])
        .fetch(FetchRequest {
            urls: vec!["https://8.8.8.8/failed".into()],
            max_characters: 8_000,
        })
        .await
        .expect("per-URL failure output");
    assert!(response.is_execution_error());
    assert_eq!(response.results[0].status, FetchStatus::Error);
}
