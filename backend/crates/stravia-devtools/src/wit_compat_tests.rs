use std::{fs, path::Path};
use sha2::{Digest, Sha256};

fn fixture(root: &Path, current: &str) {
    let wit = root.join("backend/crates/stravia-vendor-sdk/wit");
    let released = wit.join("released/stravia-vendor@1.0.0");
    fs::create_dir_all(&released).unwrap();
    let old = "package stravia:vendor@1.0.0; interface api { record item { value: string } resource client { send: func(x: item); } ping: func(); } world plugin { import api; }";
    fs::write(released.join("vendor.wit"), old).unwrap();
    fs::write(wit.join("vendor.wit"), current).unwrap();
    let hash = format!("{:x}", Sha256::digest(old.as_bytes()));
    fs::write(wit.join("released/index.json"), serde_json::json!({"stravia-vendor@1.0.0/vendor.wit": hash}).to_string()).unwrap();
}

#[test]
fn checks_immutable_snapshots_and_backward_compatibility() {
    let dir = tempfile::tempdir().unwrap();
    let current = "package stravia:vendor@1.1.0; interface api { record item { value: string } resource client { send: func(x: item); } ping: func(); added: func(); } world plugin { import api; }";
    fixture(dir.path(), current);
    super::check(dir.path()).unwrap();
    let wit = dir.path().join("backend/crates/stravia-vendor-sdk/wit");
    fs::write(wit.join("vendor.wit"), current.replace("import api;", "import api; export extra: func();")).unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(wit.join("vendor.wit"), current.replace("import api;", "import api; import extra: func();")).unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(wit.join("vendor.wit"), current.replace("ping: func();", "")).unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(wit.join("vendor.wit"), current.replace("value: string", "value: u32")).unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(wit.join("vendor.wit"), current.replace("send: func(x: item);", "")).unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(wit.join("vendor.wit"), current.replace("@1.1.0", "@1.0.0")).unwrap();
    super::check(dir.path()).unwrap();
    fs::write(wit.join("vendor.wit"), current.replace("@1.1.0", "@0.9.0")).unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::write(wit.join("vendor.wit"), current).unwrap();
    fs::write(wit.join("released/stravia-vendor@1.0.0/unlisted.wit"), "extra").unwrap();
    assert!(super::check(dir.path()).is_err());
    fs::remove_file(wit.join("released/stravia-vendor@1.0.0/unlisted.wit")).unwrap();
    fs::write(wit.join("released/stravia-vendor@1.0.0/vendor.wit"), "modified").unwrap();
    assert!(super::check(dir.path()).is_err());
}
