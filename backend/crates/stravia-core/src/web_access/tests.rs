use super::*;

#[test]
fn search_contract_normalizes_allowed_domains() {
    let normalized = validate_search_request(SearchRequest {
        query: "Rust 1.90".into(),
        max_results: 5,
        allowed_domains: vec!["Docs.RS".into()],
    })
    .expect("valid request");
    assert_eq!(normalized.allowed_domains, ["docs.rs"]);
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
        serde_json::from_value::<SearchRequest>(serde_json::json!({
            "query": "Rust",
            "blocked_domains": [],
        }))
        .is_err(),
        "removed blocked_domains input must be rejected as an unknown field"
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
        "http://127.0.0.1../",
        "http://192.168.1.1/",
        "http://192.168.1.1../",
        "http://LOCALHOST../",
        "http://service.local../",
        "http://home.arpa../",
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
        "https://[2001:1::1]/",
        "https://[2001:1::2]/",
        "https://[2001:1::3]/",
        "https://[2001:3::1]/",
        "https://[2001:4:112::1]/",
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

#[tokio::test]
async fn fetch_contract_preserves_unicode_trimming_and_input_error_codes() {
    let normalized = validate_fetch_request(FetchRequest {
        urls: vec!["\u{2003}https://8.8.8.8/\u{3000}".into()],
        max_characters: 1_000,
    })
    .await
    .expect("Unicode whitespace should be trimmed before URL validation");
    assert_eq!(normalized.urls, ["https://8.8.8.8/"]);

    for url in ["http://[", "ftp://8.8.8.8/", "https://user:pass@8.8.8.8/"] {
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
            "parse failures and static policy rejections must be invalid_input: {url}: {result:?}"
        );
    }
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
        limitations: Vec::new(),
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
async fn configured_adapters_observe_proxy_snapshot_empty_success_and_failover() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let primary_factory = Arc::new(
        crate::test_support::StubWebProviderFactory::new("worker-web", true, false)
            .with_requires_api_key(false),
    );
    let secondary_factory = Arc::new(crate::test_support::StubWebProviderFactory::new(
        "remote-web",
        true,
        false,
    ));
    let gateway = crate::GatewayBuilder::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .web_provider_factory(primary_factory.clone())
    .web_provider_factory(secondary_factory.clone())
    .build()
    .await
    .expect("gateway");
    let admin = gateway.admin();
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Web key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            transparent_injections: crate::db::models::TransparentInjectionSet::from_items([
                crate::db::models::TransparentInjection::WebSearch,
            ]),
            model_ids: vec![],
            spend_limit: None,
        })
        .await
        .expect("API key");
    let primary = admin
        .create_web_provider(crate::db::models::CreateWebProvider {
            name: "Worker".into(),
            kind: "worker-web".into(),
            api_key: None,
            use_proxy: true,
            config: None,
        })
        .await
        .expect("Primary Web Provider");
    let secondary = admin
        .create_web_provider(crate::db::models::CreateWebProvider {
            name: "Remote".into(),
            kind: "remote-web".into(),
            api_key: Some("test-remote-key".into()),
            use_proxy: false,
            config: None,
        })
        .await
        .expect("Secondary Web Provider");
    admin
        .update_web_access_settings(WebAccessSettings {
            search_provider_ids: vec![primary.id.clone(), secondary.id.clone()],
            fetch_provider_ids: vec![],
        })
        .await
        .expect("selected sources");
    admin
        .set_setting(
            "outbound_proxy",
            r#"{"url":"http://old-proxy.example:8080","bypass":".old.internal","force_http1":false}"#,
        )
        .await
        .expect("old proxy");

    let failing_primary = Arc::new(FakeSearchProvider {
        response: Err(ProviderFailure::unavailable("Worker unavailable")),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let rescuing_secondary = Arc::new(FakeSearchProvider {
        response: Ok(SearchResponse {
            mode: SearchMode::Index,
            query: "Rust".into(),
            results: vec![SearchResult {
                url: "https://docs.rs/".into(),
                title: Some("docs.rs".into()),
                snippet: None,
            }],
            answer: None,
            citations: None,
        }),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    primary_factory.set_adapter(primary.id.clone(), failing_primary.clone());
    secondary_factory.set_adapter(secondary.id.clone(), rescuing_secondary.clone());

    let service = gateway.web_access();
    assert_eq!(
        service
            .capture_run_snapshot("run-old", &key.id)
            .await
            .expect("old snapshot"),
        WebAccessAvailability {
            search: true,
            fetch: false,
        }
    );

    admin
        .set_setting(
            "outbound_proxy",
            r#"{"url":"http://new-proxy.example:8080","bypass":".new.internal","force_http1":false}"#,
        )
        .await
        .expect("new proxy");
    let rescued = service
        .search_in_run(
            "run-old",
            &key.id,
            SearchRequest {
                query: "Rust".into(),
                max_results: 5,
                allowed_domains: vec![],
            },
        )
        .await
        .expect("Secondary rescues Primary hard failure");
    assert_eq!(rescued.results[0].url, "https://docs.rs/");
    assert_eq!(
        primary_factory.recorded_outbounds(),
        [(
            primary.id.clone(),
            stravia_web_access::OutboundProxyMode::Explicit {
                url: "http://old-proxy.example:8080".into(),
                bypass: ".old.internal".into(),
            },
        )]
    );
    assert_eq!(
        secondary_factory.recorded_outbounds(),
        [(
            secondary.id.clone(),
            stravia_web_access::OutboundProxyMode::Direct,
        )]
    );

    let empty_primary = Arc::new(FakeSearchProvider {
        response: Ok(SearchResponse {
            mode: SearchMode::Index,
            query: "Rust".into(),
            results: vec![],
            answer: None,
            citations: None,
        }),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let skipped_secondary = Arc::new(FakeSearchProvider {
        response: Err(ProviderFailure::unavailable("must not run")),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    primary_factory.set_adapter(primary.id.clone(), empty_primary.clone());
    secondary_factory.set_adapter(secondary.id.clone(), skipped_secondary.clone());

    service
        .capture_run_snapshot("run-new", &key.id)
        .await
        .expect("new snapshot");
    let empty = service
        .search_in_run(
            "run-new",
            &key.id,
            SearchRequest {
                query: "Rust".into(),
                max_results: 5,
                allowed_domains: vec![],
            },
        )
        .await
        .expect("Primary empty result succeeds");
    assert!(empty.results.is_empty());
    assert_eq!(
        empty_primary.calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        skipped_secondary
            .calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        primary_factory.recorded_outbounds().last().unwrap(),
        &(
            primary.id,
            stravia_web_access::OutboundProxyMode::Explicit {
                url: "http://new-proxy.example:8080".into(),
                bypass: ".new.internal".into(),
            },
        )
    );
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
        })
        .await
        .expect_err("search should fail");

    assert_eq!(error.code, WebAccessErrorCode::Unavailable);
    assert_eq!(error.message, "Web Search is unavailable");
    assert!(!error.message.contains("Exa"));
}

#[tokio::test]
async fn search_strictly_filters_sources_outside_allowed_domains() {
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
                    url: "https://docs.rs/".into(),
                    title: None,
                    snippet: None,
                },
                SearchResult {
                    url: "https://evil-docs.rs/".into(),
                    title: None,
                    snippet: None,
                },
                SearchResult {
                    url: "https://docs.rs.evil.com/".into(),
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
        })
        .await
        .expect("provider succeeds");

    assert_eq!(response.results.len(), 2);
    assert_eq!(response.results[0].url, "https://guide.docs.rs/start");
    assert_eq!(response.results[1].url, "https://docs.rs/");
}
#[tokio::test]
async fn unavailable_factory_skips_offline_provider_while_retaining_available_sources() {
    let directory = tempfile::tempdir().unwrap();
    let worker_available = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gateway = crate::GatewayBuilder::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .web_provider_factory(Arc::new(
        crate::test_support::StubWebProviderFactory::new("worker-web", true, true)
            .with_requires_api_key(false)
            .with_availability(worker_available.clone(), "Worker is offline"),
    ))
    .web_provider_factory(Arc::new(crate::test_support::StubWebProviderFactory::new(
        "remote-web",
        true,
        true,
    )))
    .build()
    .await
    .unwrap();
    let service = gateway.web_access();
    let store = gateway.storage.web_providers().unwrap();
    let worker = gateway
        .admin()
        .create_web_provider(crate::db::models::CreateWebProvider {
            name: "Worker".into(),
            kind: "worker-web".into(),
            api_key: None,
            use_proxy: false,
            config: None,
        })
        .await
        .unwrap();
    let settings = WebAccessSettings {
        search_provider_ids: vec![worker.id.clone()],
        fetch_provider_ids: vec![worker.id.clone()],
    };
    store.save_settings(&settings).await.unwrap();
    let remote = gateway
        .admin()
        .create_web_provider(crate::db::models::CreateWebProvider {
            name: "Remote".into(),
            kind: "remote-web".into(),
            api_key: Some("secret".into()),
            use_proxy: false,
            config: None,
        })
        .await
        .unwrap();
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Web key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            transparent_injections: crate::db::models::TransparentInjectionSet::from_items([
                crate::db::models::TransparentInjection::WebSearch,
            ]),
            model_ids: vec![],
            spend_limit: None,
        })
        .await
        .unwrap();
    assert_eq!(
        service
            .capture_run_snapshot("offline-worker", &key.id)
            .await
            .unwrap(),
        WebAccessAvailability::default()
    );
    assert!(service.run_snapshot("offline-worker", &key.id).is_err());
    worker_available.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        service
            .capture_run_snapshot("online-worker", &key.id)
            .await
            .unwrap(),
        WebAccessAvailability {
            search: true,
            fetch: true
        }
    );
    service.run_snapshot("online-worker", &key.id).unwrap();
    assert_eq!(store.load_settings().await.unwrap(), settings);
    store
        .save_settings(&WebAccessSettings::default())
        .await
        .unwrap();
    assert_eq!(
        service
            .capture_run_snapshot("disabled-worker", &key.id)
            .await
            .unwrap(),
        WebAccessAvailability::default()
    );
    assert!(service.run_snapshot("disabled-worker", &key.id).is_err());
    service.run_snapshot("online-worker", &key.id).unwrap();
    let remote_settings = WebAccessSettings {
        search_provider_ids: vec![remote.id.clone()],
        fetch_provider_ids: vec![remote.id],
    };
    store.save_settings(&remote_settings).await.unwrap();
    assert_eq!(
        service
            .capture_run_snapshot("remote-fallback", &key.id)
            .await
            .unwrap(),
        WebAccessAvailability {
            search: true,
            fetch: true
        }
    );
}

#[tokio::test]
async fn legacy_switch_does_not_block_snapshots_and_source_changes_preserve_active_runs() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let gateway = crate::GatewayBuilder::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .web_provider_factory(Arc::new(crate::test_support::StubWebProviderFactory::new(
        "remote-web",
        true,
        true,
    )))
    .build()
    .await
    .expect("gateway");
    gateway
        .storage
        .settings()
        .set("web_access_enabled", "false")
        .await
        .expect("legacy disabled setting");
    let admin = gateway.admin();
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Web key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            transparent_injections: crate::db::models::TransparentInjectionSet::from_items([
                crate::db::models::TransparentInjection::WebSearch,
            ]),
            model_ids: vec![],
            spend_limit: None,
        })
        .await
        .expect("API key");
    let provider = admin
        .create_web_provider(crate::db::models::CreateWebProvider {
            name: "Remote".into(),
            kind: "remote-web".into(),
            api_key: Some("test-remote-key".into()),
            use_proxy: false,
            config: None,
        })
        .await
        .expect("Web Provider");
    admin
        .update_web_access_settings(WebAccessSettings {
            search_provider_ids: vec![provider.id.clone()],
            fetch_provider_ids: vec![provider.id],
        })
        .await
        .expect("selected sources");
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
            search_provider_ids: vec![],
            fetch_provider_ids: vec![],
        })
        .await
        .expect("cleared sources");

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
    calls: parking_lot::Mutex<Vec<Vec<String>>>,
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
        self.calls.lock().push(request.urls.clone());
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
                            limitations: Vec::new(),
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
                            limitations: Vec::new(),
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
        calls: parking_lot::Mutex::new(vec![]),
    });
    let second = Arc::new(FakeFetchProvider {
        fail_url: None,
        content_characters: 7,
        calls: parking_lot::Mutex::new(vec![]),
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
        second.calls.lock().as_slice(),
        &[vec!["https://8.8.8.8/b".to_string()]]
    );
}

#[tokio::test]
async fn configured_fetch_retries_only_failed_urls_on_secondary_provider() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let primary_factory = Arc::new(
        crate::test_support::StubWebProviderFactory::new("worker-web", false, true)
            .with_requires_api_key(false),
    );
    let secondary_factory = Arc::new(crate::test_support::StubWebProviderFactory::new(
        "remote-web",
        false,
        true,
    ));
    let gateway = crate::GatewayBuilder::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .web_provider_factory(primary_factory.clone())
    .web_provider_factory(secondary_factory.clone())
    .build()
    .await
    .expect("gateway");
    let admin = gateway.admin();
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Web key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            transparent_injections: crate::db::models::TransparentInjectionSet::from_items([
                crate::db::models::TransparentInjection::WebSearch,
            ]),
            model_ids: vec![],
            spend_limit: None,
        })
        .await
        .expect("API key");
    let primary = admin
        .create_web_provider(crate::db::models::CreateWebProvider {
            name: "Worker".into(),
            kind: "worker-web".into(),
            api_key: None,
            use_proxy: false,
            config: None,
        })
        .await
        .expect("Primary Web Provider");
    let secondary = admin
        .create_web_provider(crate::db::models::CreateWebProvider {
            name: "Remote".into(),
            kind: "remote-web".into(),
            api_key: Some("test-remote-key".into()),
            use_proxy: false,
            config: None,
        })
        .await
        .expect("Secondary Web Provider");
    admin
        .update_web_access_settings(WebAccessSettings {
            search_provider_ids: vec![],
            fetch_provider_ids: vec![primary.id.clone(), secondary.id.clone()],
        })
        .await
        .expect("selected sources");

    let primary_fetch = Arc::new(FakeFetchProvider {
        fail_url: Some("https://8.8.8.8/b".into()),
        content_characters: 7,
        calls: parking_lot::Mutex::new(vec![]),
    });
    let secondary_fetch = Arc::new(FakeFetchProvider {
        fail_url: None,
        content_characters: 7,
        calls: parking_lot::Mutex::new(vec![]),
    });
    primary_factory.set_adapter(primary.id, primary_fetch.clone());
    secondary_factory.set_adapter(secondary.id, secondary_fetch.clone());
    let service = gateway.web_access();
    assert_eq!(
        service
            .capture_run_snapshot("run-fetch", &key.id)
            .await
            .expect("Fetch snapshot"),
        WebAccessAvailability {
            search: false,
            fetch: true,
        }
    );
    let response = service
        .fetch_in_run(
            "run-fetch",
            &key.id,
            FetchRequest {
                urls: vec!["https://8.8.8.8/a".into(), "https://8.8.8.8/b".into()],
                max_characters: 8_000,
            },
        )
        .await
        .expect("Secondary recovers Primary per-URL failure");

    assert_eq!(
        response
            .results
            .iter()
            .map(|result| result.url.as_str())
            .collect::<Vec<_>>(),
        ["https://8.8.8.8/a", "https://8.8.8.8/b"]
    );
    assert_eq!(
        secondary_fetch.calls.lock().as_slice(),
        &[vec!["https://8.8.8.8/b".to_string()]]
    );
}

#[tokio::test]
async fn fetch_read_budget_preserves_content_above_the_default_batch_ceiling() {
    let provider = Arc::new(FakeFetchProvider {
        fail_url: None,
        content_characters: 120_000,
        calls: parking_lot::Mutex::new(vec![]),
    });
    let response = WebAccessEngine::new(vec![], vec![provider])
        .fetch(FetchRequest {
            urls: vec!["https://8.8.8.8/article".into()],
            max_characters: 500_000,
        })
        .await
        .expect("full read budget");
    assert_eq!(
        response.results[0].content.as_deref(),
        Some("x".repeat(120_000).as_str())
    );
    assert!(!response.results[0].truncated);
}

#[tokio::test]
async fn fetch_applies_a_fair_total_limit_and_marks_all_failures() {
    let large = Arc::new(FakeFetchProvider {
        fail_url: None,
        content_characters: 30_000,
        calls: parking_lot::Mutex::new(vec![]),
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
        calls: parking_lot::Mutex::new(vec![]),
    });
    let second = Arc::new(FakeFetchProvider {
        fail_url: Some("https://8.8.8.8/failed".into()),
        content_characters: 7,
        calls: parking_lot::Mutex::new(vec![]),
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
