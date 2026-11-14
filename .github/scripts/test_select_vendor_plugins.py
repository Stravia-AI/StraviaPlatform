import importlib.util
import json
import subprocess
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("selector", Path(__file__).with_name("select_vendor_plugins.py"))
selector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(selector)


def tree(version="1.0.0"):
    names = [package for package, _ in selector.PLUGINS.values()] + ["stravia-vendor-common"]
    paths = [f"backend/crates/{name}" for name in names]
    files = {
        "Cargo.toml": f'[workspace]\nmembers = {json.dumps(paths)}\n[workspace.package]\nversion = "{version}"\nedition = "2024"\n[workspace.dependencies]\nserde = "1"\n',
        "Taskfile.yml": "tasks:\n  brand:generate:\n",
        ".github/workflows/release.yml": '  RUST_VERSION: "1"\n',
        "frontend/stravia-webui/src/unrelated.ts": "ui",
    }
    lock = ["version = 4\n"]
    for name, path in zip(names, paths):
        files[f"{path}/Cargo.toml"] = f'[package]\nname = "{name}"\nversion.workspace = true\n'
        files[f"{path}/src/lib.rs"] = name
        deps = 'dependencies = ["stravia-vendor-common", "serde 1.0.0"]\n' if name != "stravia-vendor-common" else ""
        lock.append(f'[[package]]\nname = "{name}"\nversion = "{version}"\n{deps}')
    for number in ("1.0.0", "2.0.0"):
        lock.append(f'[[package]]\nname = "serde"\nversion = "{number}"\nsource = "registry+https://example.com"\n')
    files["Cargo.lock"] = "".join(lock)
    for _, icon in selector.PLUGINS.values():
        if icon:
            files[f"frontend/stravia-webui/src/assets/icons/{icon}"] = "icon"
    return {k: v.encode() for k, v in files.items()}


class SelectionTests(unittest.TestCase):
    def select(self, old, new):
        trees = {"old": old, "new": new}
        def git(*args):
            return "\n".join(trees[args[-1]])
        def blob(rev, path):
            return trees[rev][path]
        with patch.object(selector, "git", side_effect=git), patch.object(selector, "blob", side_effect=blob):
            return selector.select("old", "new")

    def test_first_release_and_host_only_change(self):
        self.assertEqual(selector.select(None), list(selector.PLUGINS))
        old, new = tree(), tree("3.0.0")
        new["frontend/stravia-webui/src/unrelated.ts"] = b"changed"
        self.assertEqual(self.select(old, new), [])

    def test_single_plugin_and_shared_changes(self):
        old, new = tree(), tree()
        new["backend/crates/stravia-vendor-codex/src/lib.rs"] += b"changed"
        self.assertEqual(self.select(old, new), ["openai-codex"])
        new = tree()
        new["backend/crates/stravia-vendor-common/src/lib.rs"] += b"changed"
        self.assertEqual(self.select(old, new), list(selector.PLUGINS))
        new = tree()
        new["frontend/stravia-webui/src/assets/icons/openai.svg"] += b"changed"
        self.assertEqual(self.select(old, new), ["openai-codex"])

    def test_nested_fixtures_and_tests_are_not_inputs(self):
        old, new = tree(), tree()
        base = "backend/crates/stravia-vendor-common/"
        for path in ("fixtures/example/src/lib.rs", "fixtures/example/Cargo.toml",
                     "fixtures/example/build.rs", "tests/src/helper.rs"):
            old[base + path] = b"before"
            new[base + path] = b"after"
        self.assertEqual(self.select(old, new), [])
        new[base + "build-support/messages.rs"] = b"real build input"
        self.assertEqual(self.select(old, new), list(selector.PLUGINS))

    def test_exact_lock_version_closure(self):
        old, new = tree(), tree()
        new["Cargo.lock"] = new["Cargo.lock"].replace(b'version = "2.0.0"\nsource', b'version = "2.1.0"\nsource')
        self.assertEqual(self.select(old, new), [])
        new = tree()
        new["Cargo.lock"] = new["Cargo.lock"].replace(b'"serde 1.0.0"', b'"serde 2.0.0"')
        self.assertEqual(self.select(old, new), list(selector.PLUGINS))

    def test_release_baseline_selection(self):
        def release(tag, **kwargs):
            return {"tag_name": tag, "published_at": "today", "assets": [{"name": selector.MARKER}], **kwargs}
        releases = [release("old"), release("draft", draft=True), release("fork"), release("near")]
        def git(*args):
            if args[0] == "rev-parse":
                return args[-1].split("/")[-1].split("^")[0]
            return {"old..HEAD": "10", "near..HEAD": "2"}[args[-1]]
        def run(args):
            return subprocess.CompletedProcess(args, 1 if args[-2] == "fork" else 0)
        with patch.object(selector, "git", side_effect=git), patch.object(selector.subprocess, "run", side_effect=run):
            self.assertEqual(selector.choose_baseline(releases), "near")
            self.assertIsNone(selector.choose_baseline([release("near", assets=[]), release("old")]))
            self.assertIsNone(selector.choose_baseline([release("draft", draft=True), release("fork")]))
        with patch.object(selector, "git", side_effect=subprocess.CalledProcessError(1, "git")):
            self.assertIsNone(selector.choose_baseline([release("unknown")]))


if __name__ == "__main__":
    unittest.main()
