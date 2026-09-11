"""Generate the bundled offline catalog from an unpacked Kingfisher v1.109.0.

Run with: uv run --with pyyaml==6.0.3 python <this script> <upstream directory>
This is a development-time importer; the Rust runtime never runs Python or YAML.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re

import yaml

VERSION = "v1.109.0"
COMMIT = "9ffb8969c4ad5a5c6c24e686cb252c434bc8adce"
FIELDS = {
    "name", "id", "pattern", "min_entropy", "confidence", "visible",
    "pattern_requirements", "description", "references", "categories",
    "examples", "negative_examples", "validation", "revocation", "tls_mode",
    "depends_on_rule",
}
OFFLINE_FIELDS = {
    "name", "id", "pattern", "min_entropy", "confidence", "visible",
    "pattern_requirements",
}


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("--check", action="store_true", help="Verify generated assets without writing")
    args = parser.parse_args()
    output = Path(__file__).resolve().parents[1] / "src/detection"
    files = {}
    rules = []
    for path in sorted((args.source / "crates/kingfisher-rules/data/rules").glob("*.yml")):
        data = path.read_bytes()
        files[path.relative_to(args.source).as_posix()] = digest(data)
        document = yaml.safe_load(data)
        if set(document) != {"rules"}:
            raise ValueError(f"Unexpected document fields in {path.name}")
        for rule in document["rules"]:
            if set(rule) - FIELDS:
                raise ValueError(f"Unexpected fields for {rule['id']}")
            rules.append({key: value for key, value in rule.items() if key in OFFLINE_FIELDS})
    if len(rules) != 1013 or len({rule["id"] for rule in rules}) != 1013:
        raise ValueError("Expected all 1,013 unique v1.109.0 rules")
    safe_path = args.source / "src/safe_list.rs"
    safe_text = safe_path.read_text(encoding="utf-8")
    safe_list = re.findall(r'compile\(\s*r"((?:\\.|[^"\\])*)"', safe_text.split('// User-supplied')[0])
    if len(safe_list) != 18:
        raise ValueError("Expected all 18 upstream built-in safe-list patterns")
    files["src/safe_list.rs"] = digest(safe_path.read_bytes())
    snapshot = {"version": VERSION, "commit": COMMIT, "rules": rules, "safe_list": safe_list}
    content = (json.dumps(snapshot, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    manifest = {
        "repository": "https://github.com/mongodb/kingfisher", "version": VERSION,
        "commit": COMMIT, "rules": len(rules),
        "visibleRules": sum(rule.get("visible", True) for rule in rules),
        "checksumRules": sum("checksum" in rule.get("pattern_requirements", {}) for rule in rules),
        "snapshot": "kingfisher.json", "snapshotSha256": digest(content),
        "sourceSha256": files,
        "license": "Apache-2.0; see LICENSE.kingfisher and NOTICE.kingfisher",
        "transform": "Offline fields only; patterns and pattern requirements retained verbatim. No validation, revocation, example credentials, or network templates are bundled.",
        "policy": {
            "visibility": "Hidden helper rules are cataloged but never emit protection findings.",
            "confidence": "All confidence levels are retained, as with Betterleaks; no CLI medium-confidence cutoff.",
            "dependencies": "depends_on_rule binds validation variables upstream; not an offline matching prerequisite.",
            "scope": "Text regexes, capture selection, byte entropy, pattern requirements, checksums, built-in safe-list. No file discovery, decoding, tree-sitter, URI validation, user allowlists, or inline ignore directives.",
        },
    }
    # A subsequent import must reproduce the pinned source, not silently replace it.
    manifest_path = output / "UPSTREAM.kingfisher.json"
    if manifest_path.exists():
        previous = json.loads(manifest_path.read_text(encoding="utf-8"))
        if previous["sourceSha256"] != files:
            raise ValueError("Source differs from pinned v1.109.0; review the upstream pin before updating")
    assets = {
        "kingfisher.json": content,
        "UPSTREAM.kingfisher.json": (json.dumps(manifest, indent=2) + "\n").encode("utf-8"),
    }
    for source, target in [("LICENSE", "LICENSE.kingfisher"), ("NOTICE", "NOTICE.kingfisher")]:
        assets[target] = (args.source / source).read_bytes()
    for name, data in assets.items():
        if args.check:
            if (output / name).read_bytes() != data:
                raise ValueError(f"Generated asset differs: {name}")
        else:
            (output / name).write_bytes(data)
    print(f"{'Verified' if args.check else 'Imported'} {len(rules)} rules ({manifest['visibleRules']} visible, {manifest['checksumRules']} checksummed)")


if __name__ == "__main__":
    main()
