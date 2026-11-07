use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::{Component, Path},
};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use wit_parser::{
    Function, FunctionKind, Handle as ResourceHandle, InterfaceId, Resolve, Type, TypeDefKind,
    TypeId, WorldItem, WorldKey,
};

#[cfg(test)]
#[path = "wit_compat_tests.rs"]
mod tests;

pub fn run() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    check(&root)
}

fn check(root: &Path) -> Result<()> {
    let wit = root.join("backend/crates/stravia-vendor-sdk/wit");
    let released = wit.join("released");
    let index: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(released.join("index.json"))?)?;
    ensure!(!index.is_empty(), "released WIT index is empty");
    let mut actual = BTreeSet::new();
    for entry in walkdir::WalkDir::new(&released).min_depth(1) {
        let entry = entry?;
        ensure!(
            !entry.file_type().is_symlink(),
            "symlink in released WIT: {}",
            entry.path().display()
        );
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(&released)?;
        if relative == Path::new("index.json") {
            continue;
        }
        let name = relative
            .to_str()
            .context("non-UTF8 snapshot path")?
            .replace('\\', "/");
        actual.insert(name);
    }
    let expected: BTreeSet<_> = index.keys().cloned().collect();
    ensure!(
        actual == expected,
        "released WIT files differ from index: missing {:?}, unlisted {:?}",
        expected.difference(&actual).collect::<Vec<_>>(),
        actual.difference(&expected).collect::<Vec<_>>()
    );
    let mut snapshots = BTreeSet::new();
    for (name, hash) in &index {
        let mut components = Path::new(name).components();
        let Some(Component::Normal(snapshot)) = components.next() else {
            bail!("invalid snapshot path {name}")
        };
        ensure!(
            components.clone().next().is_some()
                && components.all(|c| matches!(c, Component::Normal(_))),
            "invalid snapshot path {name}"
        );
        let snapshot = snapshot.to_str().context("non-UTF8 snapshot name")?;
        snapshots.insert(snapshot.to_owned());
        let digest = snapshot_hash(&fs::read(released.join(name))?);
        ensure!(&digest == hash, "snapshot hash mismatch: {name}");
    }
    let mut current_vendor = Resolve {
        all_features: true,
        ..Default::default()
    };
    let (vendor_id, _) = current_vendor
        .push_dir(&wit)
        .context("parsing current vendor WIT")?;
    for snapshot in snapshots {
        let path = released.join(&snapshot);
        let mut old = Resolve {
            all_features: true,
            ..Default::default()
        };
        let (old_id, _) = old
            .push_dir(&path)
            .with_context(|| format!("parsing snapshot {snapshot}"))?;
        let old_pkg = &old.packages[old_id];
        let expected_dir = format!(
            "{}-{}@{}",
            old_pkg.name.namespace,
            old_pkg.name.name,
            old_pkg
                .name
                .version
                .as_ref()
                .context("snapshot package has no version")?
        );
        ensure!(
            snapshot == expected_dir,
            "snapshot directory {snapshot} does not match package {expected_dir}"
        );
        let current_id = if old_pkg.name.name == "vendor" && old_pkg.name.namespace == "stravia" {
            vendor_id
        } else {
            current_vendor
                .packages
                .iter()
                .find_map(|(id, pkg)| {
                    (pkg.name.namespace == old_pkg.name.namespace
                        && pkg.name.name == old_pkg.name.name)
                        .then_some(id)
                })
                .with_context(|| format!("current package missing for {snapshot}"))?
        };
        let new_pkg = &current_vendor.packages[current_id];
        let old_version = old_pkg.name.version.as_ref().unwrap();
        let new_version = new_pkg
            .name
            .version
            .as_ref()
            .context("current package has no version")?;
        ensure!(
            new_version >= old_version,
            "current package {} is older than {snapshot}",
            new_pkg.name
        );
        if new_version.major != old_version.major {
            continue;
        }
        let mut cmp = Compare {
            old: &old,
            new: &current_vendor,
            seen: HashSet::new(),
        };
        for (name, old_id) in &old_pkg.interfaces {
            let new_id = new_pkg
                .interfaces
                .get(name)
                .with_context(|| format!("{snapshot}: interface {name} removed"))?;
            cmp.interface(*old_id, *new_id)
                .with_context(|| format!("{snapshot}: interface {name}"))?;
        }
        for (name, old_id) in &old_pkg.worlds {
            let new_id = new_pkg
                .worlds
                .get(name)
                .with_context(|| format!("{snapshot}: world {name} removed"))?;
            let a = &old.worlds[*old_id];
            let b = &current_vendor.worlds[*new_id];
            for (old_items, new_items, exported) in [
                (&a.imports, &b.imports, false),
                (&a.exports, &b.exports, true),
            ] {
                for (key, item) in old_items {
                    let (_, other) = new_items
                        .iter()
                        .find(|(k, _)| cmp.world_key(key, k))
                        .with_context(|| {
                            format!("{snapshot}: world {name} item removed: {key:?}")
                        })?;
                    cmp.world_item(item, other)
                        .with_context(|| format!("{snapshot}: world {name} item {key:?}"))?;
                    if exported
                        && let (
                            WorldItem::Interface { id: old_id, .. },
                            WorldItem::Interface { id: new_id, .. },
                        ) = (item, other)
                    {
                        ensure!(
                            old.interfaces[*old_id].functions.len()
                                == current_vendor.interfaces[*new_id].functions.len(),
                            "{snapshot}: new exports must use a separate optional interface"
                        );
                    }
                }
                if exported {
                    for (key, item) in new_items {
                        if !old_items.keys().any(|old| cmp.world_key(old, key)) {
                            // Optional interfaces are loaded dynamically; the real released
                            // component regression checks that the host does not require them.
                            ensure!(
                                matches!(item, WorldItem::Interface { .. }),
                                "{snapshot}: new exports must use a separate optional interface"
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

struct Compare<'a> {
    old: &'a Resolve,
    new: &'a Resolve,
    seen: HashSet<(TypeId, TypeId)>,
}
impl Compare<'_> {
    fn interface_identity(&self, a: InterfaceId, b: InterfaceId) -> bool {
        let a = &self.old.interfaces[a];
        let b = &self.new.interfaces[b];
        a.name == b.name
            && match (a.package, b.package) {
                (Some(a), Some(b)) => {
                    let a = &self.old.packages[a].name;
                    let b = &self.new.packages[b].name;
                    a.namespace == b.namespace
                        && a.name == b.name
                        && a.version.as_ref().map(|v| v.major)
                            == b.version.as_ref().map(|v| v.major)
                }
                (None, None) => true,
                _ => false,
            }
    }
    fn world_key(&self, a: &WorldKey, b: &WorldKey) -> bool {
        match (a, b) {
            (WorldKey::Name(a), WorldKey::Name(b)) => a == b,
            (WorldKey::Interface(a), WorldKey::Interface(b)) => self.interface_identity(*a, *b),
            _ => false,
        }
    }
    fn world_item(&mut self, a: &WorldItem, b: &WorldItem) -> Result<()> {
        match (a, b) {
            (
                WorldItem::Interface {
                    id: a,
                    external_id: x,
                    ..
                },
                WorldItem::Interface {
                    id: b,
                    external_id: y,
                    ..
                },
            ) => {
                ensure!(
                    x == y && self.interface_identity(*a, *b),
                    "interface identity changed"
                );
                self.interface(*a, *b)
            }
            (WorldItem::Function(a), WorldItem::Function(b)) => self.function(a, b),
            (WorldItem::Type { id: a, .. }, WorldItem::Type { id: b, .. }) => self.type_id(*a, *b),
            _ => bail!("world item kind changed"),
        }
    }
    fn interface(&mut self, a: InterfaceId, b: InterfaceId) -> Result<()> {
        let a = &self.old.interfaces[a];
        let b = &self.new.interfaces[b];
        for (name, id) in &a.types {
            self.type_id(
                *id,
                *b.types
                    .get(name)
                    .with_context(|| format!("type {name} removed"))?,
            )
            .with_context(|| format!("type {name}"))?;
        }
        for (name, f) in &a.functions {
            self.function(
                f,
                b.functions
                    .get(name)
                    .with_context(|| format!("function {name} removed"))?,
            )
            .with_context(|| format!("function {name}"))?;
        }
        Ok(())
    }
    fn function(&mut self, a: &Function, b: &Function) -> Result<()> {
        use FunctionKind::*;
        match (&a.kind, &b.kind) {
            (Freestanding, Freestanding) | (AsyncFreestanding, AsyncFreestanding) => {}
            (Method(a), Method(b))
            | (AsyncMethod(a), AsyncMethod(b))
            | (Static(a), Static(b))
            | (AsyncStatic(a), AsyncStatic(b))
            | (Constructor(a), Constructor(b)) => self.type_id(*a, *b)?,
            _ => bail!("function kind changed"),
        }
        ensure!(
            a.name == b.name && a.external_id == b.external_id && a.params.len() == b.params.len(),
            "function signature changed"
        );
        for (a, b) in a.params.iter().zip(&b.params) {
            ensure!(a.name == b.name, "parameter name changed");
            self.ty(a.ty, b.ty)?;
        }
        self.option_type(a.result, b.result)
    }
    fn option_type(&mut self, a: Option<Type>, b: Option<Type>) -> Result<()> {
        match (a, b) {
            (Some(a), Some(b)) => self.ty(a, b),
            (None, None) => Ok(()),
            _ => bail!("optional type changed"),
        }
    }
    fn ty(&mut self, a: Type, b: Type) -> Result<()> {
        match (a, b) {
            (Type::Id(a), Type::Id(b)) => self.type_id(a, b),
            (a, b) if a == b => Ok(()),
            _ => bail!("type changed"),
        }
    }
    fn type_id(&mut self, a: TypeId, b: TypeId) -> Result<()> {
        if !self.seen.insert((a, b)) {
            return Ok(());
        }
        let a = &self.old.types[a];
        let b = &self.new.types[b];
        ensure!(
            a.name == b.name && a.external_id == b.external_id,
            "type identity changed"
        );
        match (a.owner, b.owner) {
            (wit_parser::TypeOwner::Interface(a), wit_parser::TypeOwner::Interface(b)) => {
                ensure!(self.interface_identity(a, b), "type owner changed")
            }
            (wit_parser::TypeOwner::World(a), wit_parser::TypeOwner::World(b)) => ensure!(
                self.old.worlds[a].name == self.new.worlds[b].name,
                "type owner changed"
            ),
            (wit_parser::TypeOwner::None, wit_parser::TypeOwner::None) => {}
            _ => bail!("type owner changed"),
        }
        use TypeDefKind::*;
        match (&a.kind, &b.kind) {
            (Resource, Resource) => {}
            (Handle(ResourceHandle::Own(a)), Handle(ResourceHandle::Own(b)))
            | (Handle(ResourceHandle::Borrow(a)), Handle(ResourceHandle::Borrow(b))) => {
                self.type_id(*a, *b)?
            }
            (Type(a), Type(b)) | (Option(a), Option(b)) | (List(a), List(b)) => self.ty(*a, *b)?,
            (Map(ak, av), Map(bk, bv)) => {
                self.ty(*ak, *bk)?;
                self.ty(*av, *bv)?;
            }
            (FixedLengthList(a, an), FixedLengthList(b, bn)) => {
                ensure!(an == bn, "list length changed");
                self.ty(*a, *b)?;
            }
            (Future(a), Future(b)) | (Stream(a), Stream(b)) => self.option_type(*a, *b)?,
            (Result(a), Result(b)) => {
                self.option_type(a.ok, b.ok)?;
                self.option_type(a.err, b.err)?;
            }
            (Tuple(a), Tuple(b)) => {
                ensure!(a.types.len() == b.types.len(), "tuple length changed");
                for (a, b) in a.types.iter().zip(&b.types) {
                    self.ty(*a, *b)?;
                }
            }
            (Record(a), Record(b)) => {
                ensure!(a.fields.len() == b.fields.len(), "record fields changed");
                for (a, b) in a.fields.iter().zip(&b.fields) {
                    ensure!(a.name == b.name, "record field changed");
                    self.ty(a.ty, b.ty)?;
                }
            }
            (Variant(a), Variant(b)) => {
                ensure!(a.cases.len() == b.cases.len(), "variant cases changed");
                for (a, b) in a.cases.iter().zip(&b.cases) {
                    ensure!(a.name == b.name, "variant case changed");
                    self.option_type(a.ty, b.ty)?;
                }
            }
            (Enum(a), Enum(b)) => ensure!(
                a.cases
                    .iter()
                    .map(|x| &x.name)
                    .eq(b.cases.iter().map(|x| &x.name)),
                "enum cases changed"
            ),
            (Flags(a), Flags(b)) => ensure!(
                a.flags
                    .iter()
                    .map(|x| &x.name)
                    .eq(b.flags.iter().map(|x| &x.name)),
                "flags changed"
            ),
            _ => bail!("type kind changed"),
        }
        Ok(())
    }
}

fn snapshot_hash(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        hex.push(HEX[(byte >> 4) as usize] as char);
        hex.push(HEX[(byte & 15) as usize] as char);
    }
    hex
}
