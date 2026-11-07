use std::{fs, path::Path};

fn fixture(root: &Path, current: &str) {
    let old = "package stravia:vendor@1.0.0; interface api { record item { value: string } resource client { send: func(x: item); } ping: func(); } world plugin { import api; }";
    fixture_from(root, old, current);
}

fn fixture_from(root: &Path, old: &str, current: &str) {
    let wit = root.join("backend/crates/stravia-vendor-sdk/wit");
    let released = wit.join("released/stravia-vendor@1.0.0");
    fs::create_dir_all(&released).unwrap();
    fs::write(released.join("vendor.wit"), old).unwrap();
    fs::write(wit.join("vendor.wit"), current).unwrap();
    let hash = super::snapshot_hash(old.as_bytes());
    fs::write(
        wit.join("released/index.json"),
        serde_json::json!({"stravia-vendor@1.0.0/vendor.wit": hash}).to_string(),
    )
    .unwrap();
}

#[test]
fn old_guests_do_not_gain_required_export_functions() {
    let dir = tempfile::tempdir().unwrap();
    let old = "package stravia:vendor@1.0.0; interface api { ping: func(); } world plugin { export api; }";
    let current = old.replace("@1.0.0", "@1.1.0");
    fixture_from(
        dir.path(),
        old,
        &current.replace("ping: func();", "ping: func(); required: func();"),
    );
    assert!(super::check(dir.path()).is_err());
    let optional = current
        .replace(
            "world plugin",
            "interface optional-api { added: func(); } world plugin",
        )
        .replace("export api;", "export api; export optional-api;");
    fixture_from(dir.path(), old, &optional);
    super::check(dir.path()).unwrap();
}

#[test]
fn independent_state_snapshot_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    let vendor = "package stravia:vendor@1.0.0; interface api { ping: func(); } world plugin { import api; }";
    let current_vendor = "package stravia:vendor@1.0.0; interface api { ping: func(); } world plugin { import api; } world plugin-with-state { include plugin; import stravia:state/store@1.0.0; }";
    fixture_from(dir.path(), vendor, current_vendor);
    let wit = dir.path().join("backend/crates/stravia-vendor-sdk/wit");
    let state = "package stravia:state@1.0.0; interface store { enum scope { provider, conversation } record snapshot { revision: u64, payload: option<list<u8>> } record error { code: string, message: string } resource cell { load: func() -> result<snapshot, error>; compare-exchange: func(expected-revision: u64, payload: option<list<u8>>) -> result<bool, error>; } open: func(scope: scope, key: string) -> result<cell, error>; }";
    let snapshot = wit.join("released/stravia-state@1.0.0");
    let current = wit.join("deps/stravia-state");
    fs::create_dir_all(&snapshot).unwrap();
    fs::create_dir_all(&current).unwrap();
    fs::write(snapshot.join("state.wit"), state).unwrap();
    fs::write(current.join("state.wit"), state).unwrap();
    let index_path = wit.join("released/index.json");
    let mut index: std::collections::BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    index.insert(
        "stravia-state@1.0.0/state.wit".into(),
        super::snapshot_hash(state.as_bytes()),
    );
    fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
    super::check(dir.path()).unwrap();

    fs::write(
        current.join("state.wit"),
        state.replace("revision: u64", "revision: u32"),
    )
    .unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(
        current.join("state.wit"),
        state.replace("compare-exchange:", "exchange:"),
    )
    .unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(
        wit.join("vendor.wit"),
        current_vendor.replace("state/store@1.0.0", "state/store@0.9.0"),
    )
    .unwrap();
    fs::write(current.join("state.wit"), state.replace("@1.0.0", "@0.9.0")).unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(
        wit.join("vendor.wit"),
        current_vendor.replace("state/store@1.0.0", "state/store@1.1.0"),
    )
    .unwrap();
    fs::write(current.join("state.wit"), state.replace("@1.0.0", "@1.1.0")).unwrap();
    super::check(dir.path()).unwrap();
    fs::write(
        snapshot.join("state.wit"),
        state.replace("revision: u64", "revision: u32"),
    )
    .unwrap();
    assert!(super::check(dir.path()).is_err());
}

#[test]
fn checks_immutable_snapshots_and_backward_compatibility() {
    let dir = tempfile::tempdir().unwrap();
    let current = "package stravia:vendor@1.1.0; interface api { record item { value: string } resource client { send: func(x: item); } ping: func(); added: func(); } world plugin { import api; }";
    fixture(dir.path(), current);
    super::check(dir.path()).unwrap();
    let wit = dir.path().join("backend/crates/stravia-vendor-sdk/wit");
    fs::write(
        wit.join("vendor.wit"),
        current.replace("import api;", "import api; export extra: func();"),
    )
    .unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(
        wit.join("vendor.wit"),
        current.replace("import api;", "import api; import extra: func();"),
    )
    .unwrap();
    super::check(dir.path()).unwrap();
    let optional = current
        .replace(
            "world plugin",
            "interface optional-api { added: func(); } world plugin",
        )
        .replace("import api;", "import api; export optional-api;");
    fs::write(wit.join("vendor.wit"), optional).unwrap();
    super::check(dir.path()).unwrap();
    fs::write(wit.join("vendor.wit"), current.replace("ping: func();", "")).unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(
        wit.join("vendor.wit"),
        current.replace("value: string", "value: u32"),
    )
    .unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(
        wit.join("vendor.wit"),
        current.replace("send: func(x: item);", ""),
    )
    .unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(wit.join("vendor.wit"), current.replace("@1.1.0", "@1.0.0")).unwrap();
    super::check(dir.path()).unwrap();
    fs::write(wit.join("vendor.wit"), current.replace("@1.1.0", "@0.9.0")).unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(wit.join("vendor.wit"), current).unwrap();
    fs::write(
        wit.join("released/stravia-vendor@1.0.0/unlisted.wit"),
        "extra",
    )
    .unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::remove_file(wit.join("released/stravia-vendor@1.0.0/unlisted.wit")).unwrap();
    fs::write(
        wit.join("released/stravia-vendor@1.0.0/vendor.wit"),
        "modified",
    )
    .unwrap();
    assert!(super::check(dir.path()).is_err());
}
