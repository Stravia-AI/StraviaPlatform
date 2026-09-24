//! Shared build-time compiler for embedded guest locale catalogs.
//! Each guest build script includes this file and depends on serde_json.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

// 本文件同时被 guest build.rs 独立编译和 stravia-vendor-sdk 测试模块引入；
// 独立编译时 crate 根没有 language_tag，只能按路径重复装载，属有意共享。
#[allow(clippy::duplicate_mod)]
#[path = "../src/language_tag.rs"]
mod language_tag;

pub fn generate() -> Result<(), Box<dyn Error>> {
    let manifest = env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is missing")?;
    let out = env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?;
    generate_at(Path::new(&manifest), Path::new(&out))
}

fn generate_at(manifest: &Path, out: &Path) -> Result<(), Box<dyn Error>> {
    let directory = manifest.join("messages");
    println!("cargo:rerun-if-changed={}", directory.display());
    let mut catalogs = BTreeMap::new();
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        println!("cargo:rerun-if-changed={}", path.display());
        if !entry.file_type()?.is_file() {
            return Err(invalid(format!("{} is not a catalog file", path.display())));
        }
        let locale = path
            .file_stem()
            .and_then(OsStr::to_str)
            .ok_or_else(|| invalid(format!("{} has a non-UTF-8 locale name", path.display())))?;
        if !language_tag::valid_language_tag(locale) {
            return Err(invalid(format!(
                "{} has an invalid ordinary BCP 47 language tag",
                path.display()
            )));
        }
        let bytes = fs::read(&path)?;
        let FlatCatalog(messages) = serde_json::from_slice(&bytes)
            .map_err(|error| invalid(format!("{}: {error}", path.display())))?;
        for (key, text) in &messages {
            if !valid_identifier(key) {
                return Err(invalid(format!(
                    "{}: unsafe message identifier {key:?}",
                    path.display()
                )));
            }
            if text.trim().is_empty() {
                return Err(invalid(format!(
                    "{}: blank translation for {key:?}",
                    path.display()
                )));
            }
        }
        catalogs.insert(locale.to_owned(), messages);
    }

    let english = catalogs.get("en-US").ok_or_else(|| {
        invalid(format!(
            "{}: missing en-US.json catalog",
            directory.display()
        ))
    })?;
    let mut parameters = BTreeMap::new();
    for (key, text) in english {
        let (names, _) =
            parse_template(text).map_err(|error| invalid(format!("en-US/{key}: {error}")))?;
        parameters.insert(key, names);
    }
    for (locale, catalog) in &catalogs {
        for (key, text) in catalog {
            let Some(expected) = parameters.get(key) else {
                return Err(invalid(format!(
                    "{locale}/{key}: translation key is absent from en-US"
                )));
            };
            let (actual, _) = parse_template(text)
                .map_err(|error| invalid(format!("{locale}/{key}: {error}")))?;
            if actual != *expected {
                return Err(invalid(format!(
                    "{locale}/{key}: placeholders {actual:?} differ from en-US {expected:?}"
                )));
            }
        }
    }

    let mut generated =
        String::from("// Generated at build time from messages/*.json. Do not edit.\n");
    for (key, names) in parameters {
        write!(&mut generated, "pub(crate) fn {key}(")?;
        for (index, name) in names.iter().enumerate() {
            if index != 0 {
                generated.push_str(", ");
            }
            write!(&mut generated, "{name}: &str")?;
        }
        generated.push_str(") -> stravia_vendor_sdk::LocalizedText {\n");
        if names.is_empty() {
            generated.push_str("    stravia_vendor_sdk::LocalizedText::from_static(&[\n");
        } else {
            generated.push_str("    stravia_vendor_sdk::LocalizedText::from_translations([\n");
        }
        for (locale, catalog) in &catalogs {
            let Some(text) = catalog.get(key) else {
                continue; // English is present; this locale falls back via the host.
            };
            if names.is_empty() {
                let (_, literal) = parse_template(text).expect("validated above");
                writeln!(&mut generated, "        ({locale:?}, {literal:?}),")?;
            } else {
                write!(&mut generated, "        ({locale:?}, format!({text:?}")?;
                for name in &names {
                    write!(&mut generated, ", {name} = {name}")?;
                }
                generated.push_str(")),\n");
            }
        }
        generated.push_str("    ])\n}\n");
    }
    fs::write(out.join("messages.rs"), generated)?;
    Ok(())
}

/// Deserialize a flat JSON object without silently overwriting duplicate keys.
struct FlatCatalog(BTreeMap<String, String>);

impl<'de> Deserialize<'de> for FlatCatalog {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FlatVisitor;

        impl<'de> Visitor<'de> for FlatVisitor {
            type Value = FlatCatalog;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a flat object of message keys and string translations")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut messages = BTreeMap::new();
                while let Some((key, text)) = map.next_entry::<String, String>()? {
                    if messages.insert(key.clone(), text).is_some() {
                        return Err(M::Error::custom(format!("duplicate message key {key:?}")));
                    }
                }
                Ok(FlatCatalog(messages))
            }
        }

        deserializer.deserialize_map(FlatVisitor)
    }
}

fn valid_identifier(name: &str) -> bool {
    let bytes = name.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_lowercase)
        || !bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        || bytes.windows(2).any(|pair| pair == b"__")
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
    {
        return false;
    }
    // Includes reserved and edition-specific keywords; never emit raw identifiers.
    !matches!(
        name,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "gen"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "macro"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "override"
            | "priv"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "try"
            | "type"
            | "typeof"
            | "union"
            | "unsafe"
            | "unsized"
            | "use"
            | "virtual"
            | "where"
            | "while"
            | "yield"
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
    )
}

/// Return placeholder names and the literal with doubled braces collapsed.
/// Format templates with placeholders retain their original escaped braces.
fn parse_template(text: &str) -> Result<(BTreeSet<String>, String), String> {
    let mut names = BTreeSet::new();
    let mut literal = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '{' => {
                let mut name = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == '}' {
                        closed = true;
                        break;
                    }
                    if next == '{' {
                        return Err("nested '{' in placeholder".into());
                    }
                    name.push(next);
                }
                if !closed {
                    return Err("unclosed '{' in placeholder".into());
                }
                if !valid_identifier(&name) {
                    return Err(format!(
                        "invalid placeholder {{{name}}}; use a snake_case name without format specifiers"
                    ));
                }
                names.insert(name);
            }
            '}' => return Err("unmatched '}' in translation".into()),
            _ => literal.push(ch),
        }
    }
    Ok((names, literal))
}

fn invalid(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::new(io::ErrorKind::InvalidData, message.into()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct CatalogDirectory(std::path::PathBuf);

    impl CatalogDirectory {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "stravia-message-compiler-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("messages")).unwrap();
            Self(path)
        }

        fn catalog(&self, locale: &str, text: &str) {
            fs::write(self.0.join("messages").join(format!("{locale}.json")), text).unwrap();
        }

        fn compile(&self) -> Result<(), Box<dyn Error>> {
            generate_at(&self.0, &self.0)
        }
    }

    impl Drop for CatalogDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn rejects_duplicate_decoded_keys_and_unknown_translation_keys() {
        let directory = CatalogDirectory::new();
        directory.catalog("en-US", r#"{"label":"Name","\u006cabel":"Other"}"#);
        assert!(directory.compile().is_err());

        directory.catalog("en-US", r#"{"label":"Name"}"#);
        directory.catalog("zh-CN", r#"{"other":"别名"}"#);
        assert!(directory.compile().is_err());
    }

    #[test]
    fn requires_english_catalog_and_matching_placeholders() {
        let directory = CatalogDirectory::new();
        directory.catalog("zh-CN", r#"{"label":"名称"}"#);
        assert!(directory.compile().is_err());

        directory.catalog("en-US", r#"{"label":"{field}: {limit}"}"#);
        assert!(directory.compile().is_err());
    }

    #[test]
    fn accepts_named_placeholders_and_unescapes_literal_braces() {
        let directory = CatalogDirectory::new();
        directory.catalog(
            "en-US",
            r#"{"hint":"Use {{value}}", "problem":"{{{field}}}: {limit}"}"#,
        );
        directory.catalog("zh-CN", r#"{"problem":"{limit}: {{{field}}}"}"#);
        directory.compile().unwrap();
        assert_eq!(
            parse_template("Use {{value}}").unwrap(),
            (BTreeSet::new(), "Use {value}".into())
        );
        assert_eq!(
            parse_template("{{{field}}}: {limit}").unwrap().0,
            BTreeSet::from(["field".into(), "limit".into()])
        );
        assert!(parse_template("{field:>4}").is_err());
        assert!(parse_template("{field").is_err());
    }

    #[test]
    fn rejects_invalid_catalog_language_and_unsafe_identifiers() {
        let directory = CatalogDirectory::new();
        directory.catalog("en-US", r#"{"self":"Name"}"#);
        assert!(directory.compile().is_err());
        directory.catalog("en-US", r#"{"label":"Name"}"#);
        directory.catalog("en--US", r#"{"label":"Name"}"#);
        assert!(directory.compile().is_err());
    }
}
