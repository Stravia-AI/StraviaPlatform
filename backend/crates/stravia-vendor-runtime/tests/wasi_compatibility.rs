use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_runtime::{LoadError, VendorRuntime};
use stravia_vendor_sdk::{AiRequest, Operation, ProviderSnapshot};

#[tokio::test]
async fn rejects_forbidden_wasi_interfaces_and_versions() {
    let runtime = VendorRuntime::new().expect("runtime");
    let mut names = Vec::new();
    for version in ["0.2.0", "0.2.9", "0.2.12", "0.2.13"] {
        for interface in [
            "filesystem/types",
            "filesystem/preopens",
            "sockets/tcp",
            "sockets/udp",
            "sockets/network",
            "sockets/instance-network",
            "http/outgoing-handler",
            "io/streams-impostor",
            "random/random-extra",
            "cli/environment-extra",
        ] {
            names.push(format!("wasi:{interface}@{version}"));
        }
    }
    for version in ["0.1.12", "0.3.0", "0.20.0", "1.2.0", "0.2.13-rc.1"] {
        names.push(format!("wasi:random/random@{version}"));
    }
    names.extend([
        "wasi:random/random".into(),
        "impostor:random/random@0.2.12".into(),
        "wasi:random/random@0.2.12-extra".into(),
        "wasi:random/random@0.2.12-rc.1+build".into(),
    ]);
    for name in names {
        // A valid component with no guest exports: import admission must reject
        // it before descriptor instantiation can fail for a different reason.
        let bytes = wat::parse_str(format!("(component (import \"{name}\" (instance)))"))
            .expect("valid minimal component");
        match runtime.load(&bytes).await {
            Err(LoadError::ForbiddenImport(actual)) => assert_eq!(actual, name),
            Err(other) => panic!("{name}: expected ForbiddenImport, got {other:?}"),
            Ok(_) => panic!("{name}: forbidden import was admitted"),
        }
    }
}

#[tokio::test]
async fn rejects_malformed_interface_versions_before_admission() {
    let runtime = VendorRuntime::new().expect("runtime");
    for version in ["0.2.", "0.2.01", "0.2.18446744073709551616"] {
        let bytes = wat::parse_str(format!(
            "(component (import \"wasi:random/random@{version}\" (instance)))"
        ))
        .expect("component text");
        assert!(
            matches!(
                runtime.load(&bytes).await,
                Err(LoadError::InvalidComponent(_))
            ),
            "malformed interface version must fail component validation: {version}"
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum WasiImports {
    Uniform(&'static str),
    Mixed,
}

fn base_component() -> Vec<u8> {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("target/vendor-plugins");
    let manifest_path = directory.join("manifest.json");
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", manifest_path.display())),
    )
    .expect("valid vendor manifest");
    let entry = manifest
        .as_array()
        .expect("manifest entries")
        .iter()
        .find(|entry| entry["vendor_id"] == "base")
        .expect("manifest must contain the real base component");
    let path = directory.join(entry["file"].as_str().expect("base component filename"));
    std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn redirect_imports(text: &str, mode: WasiImports) -> Vec<u8> {
    let mut redirected = String::with_capacity(text.len());
    let mut old = 0;
    let mut new = 0;
    for line in text.split_inclusive('\n') {
        // wasmprinter emits outer component imports with exactly this indent.
        // Change only the quoted external name, not identifiers, inner core
        // imports, data strings, or embedded custom-section metadata.
        if let Some(rest) = line.strip_prefix("  (import \"wasi:") {
            let end = rest.find('"').expect("WASI import name terminator");
            let name = &rest[..end];
            let (interface, version) = name.rsplit_once('@').expect("versioned WASI import");
            assert!(
                matches!(version, "0.2.9" | "0.2.12"),
                "unexpected base WASI import version: {name}"
            );
            let target = match mode {
                WasiImports::Uniform(version) => version,
                // io, clocks, and stdio share resource identities. Keep the
                // whole resource-connected family together; random has no
                // resources and can safely use the other host version.
                WasiImports::Mixed if interface.starts_with("random/") => "0.2.13",
                WasiImports::Mixed => "0.2.0",
            };
            if target == "0.2.0" {
                old += 1;
            } else {
                new += 1;
            }
            redirected.push_str("  (import \"wasi:");
            redirected.push_str(interface);
            redirected.push('@');
            redirected.push_str(target);
            redirected.push_str(&rest[end..]);
        } else {
            redirected.push_str(line);
        }
    }
    match mode {
        WasiImports::Uniform("0.2.0") => assert!(old > 0 && new == 0, "0.2.0 imports required"),
        WasiImports::Uniform(_) => assert!(new > 0 && old == 0, "requested patch imports required"),
        WasiImports::Mixed => assert!(old > 0 && new > 0, "both WASI versions required"),
    }
    wat::parse_str(&redirected).expect("round-trip real base component")
}

#[tokio::test]
#[ignore = "opt-in: first build the real target/vendor-plugins base component"]
async fn real_base_executes_with_stable_wasi_patches_and_mixed_imports() {
    let text = wasmprinter::print_bytes(base_component()).expect("print real base component");
    let runtime = VendorRuntime::new().expect("runtime");
    let provider = ProviderSnapshot {
        provider_id: "custom".into(),
        channel: "default".into(),
        base_url: "https://upstream.invalid/v1".into(),
        protocol: "openai-compatible".into(),
        options: BTreeMap::new(),
        credentials: BTreeMap::new(),
        model: None,
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata: BTreeMap::new(),
    };
    let mut request = AiRequest::new("model", Vec::new());
    request.embedding = Some(stravia_runtime_contract::protocol::ir::EmbeddingRequest {
        input: stravia_runtime_contract::protocol::ir::EmbeddingInput::Text("input".into()),
        dimensions: None,
        encoding_format: None,
        user: None,
    });
    for mode in [
        WasiImports::Uniform("0.2.0"),
        WasiImports::Uniform("0.2.9"),
        WasiImports::Uniform("0.2.10"),
        WasiImports::Uniform("0.2.11"),
        WasiImports::Uniform("0.2.12"),
        WasiImports::Uniform("0.2.13"),
        WasiImports::Uniform("0.2.12+vendor-build"),
        WasiImports::Mixed,
    ] {
        let bytes = redirect_imports(&text, mode);
        let plugin = runtime
            .load(&bytes)
            .await
            .unwrap_or_else(|error| panic!("{mode:?}: real base load failed: {error:?}"));
        let protocol = runtime
            .select_protocol(
                &plugin,
                Operation::Infer,
                &provider,
                &request,
                CancellationToken::new(),
                Deadline::from_now(Duration::from_secs(60)),
            )
            .await
            .unwrap_or_else(|error| panic!("{mode:?}: real protocol selection failed: {error}"));
        assert_eq!(protocol, "openai-compatible/embeddings/v1", "{mode:?}");
    }

    // Keep all real guest exports intact. A newer patch name alone is allowed,
    // but a function absent from this host must still prevent instantiation.
    let end = text.rfind(')').expect("outer component terminator");
    let mut incompatible = String::with_capacity(text.len() + 150);
    incompatible.push_str(&text[..end]);
    incompatible.push_str(
        "\n  (import \"wasi:random/insecure@0.2.13\" \
         (instance (export \"not-yet-implemented\" (func (result u64)))))\n",
    );
    incompatible.push_str(&text[end..]);
    let bytes = wat::parse_str(&incompatible).expect("component requiring an unavailable function");
    match runtime.load(&bytes).await {
        Err(LoadError::DescriptorExecution(_)) => {}
        Err(other) => panic!("expected incompatible interface, got {other:?}"),
        Ok(_) => panic!("an unavailable WASI function was admitted"),
    }
}
