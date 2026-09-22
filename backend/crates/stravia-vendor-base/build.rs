use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let catalog_path = manifest_dir.join("../stravia-core/assets/providers.stravia.json");
    println!("cargo:rerun-if-changed={}", catalog_path.display());

    let source = std::fs::read_to_string(&catalog_path).expect("read bundled Provider Catalog");
    let entries = serde_json::from_str::<BTreeMap<String, serde_json::Value>>(&source)
        .expect("parse bundled Provider Catalog");
    let mut generated =
        String::from("pub(crate) static BUNDLED_CATALOG_PROFILES: &[BundledCatalogProfile] = &[\n");
    for (key, value) in entries {
        let object = value.as_object().expect("Provider Catalog entry object");
        let field = |name: &str| {
            object
                .get(name)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| panic!("Provider Catalog {key} missing {name}"))
        };
        let id = field("id");
        assert_eq!(key, id, "bundled Provider Catalog key/id mismatch");
        let npm = field("npm");
        let name = field("name");
        let api = object.get("api").and_then(serde_json::Value::as_str);
        writeln!(
            generated,
            "    BundledCatalogProfile {{ id: {id:?}, npm: {npm:?}, name: {name:?}, api: {} }},",
            api.map_or_else(|| "None".to_owned(), |api| format!("Some({api:?})")),
        )
        .expect("write generated catalog profile");
    }
    generated.push_str("];\n");

    let output =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("catalog_profiles.rs");
    std::fs::write(output, generated).expect("write generated catalog profiles");
}
