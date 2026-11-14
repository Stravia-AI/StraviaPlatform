#!/usr/bin/env python3
"""Select dedicated plugins against a completed selective-release ancestor."""
import json
import os
import posixpath
import re
import subprocess
import tomllib
import urllib.request
from pathlib import Path

PLUGINS = {
    "openai-codex": ("stravia-vendor-codex", "openai.svg"),
    "xai-grok": ("stravia-vendor-grok", "xai.svg"),
    "command-code": ("stravia-vendor-command-code", None),
    "devin": ("stravia-vendor-devin", None),
    "opencode-free": ("stravia-vendor-opencode-free", "opencode-logo-light.svg"),
    "claude-code": ("stravia-vendor-claudecode", "anthropic.svg"),
    "antigravity": ("stravia-vendor-antigravity", None),
}
MARKER = "stravia-plugin-selection-v1.json"


def git(*args):
    return subprocess.check_output(["git", *args]).decode()


def blob(rev, path):
    return subprocess.check_output(["git", "show", f"{rev}:{path}"])


def snapshot(rev, package, icon):
    """Fingerprint only source inputs and the plugin's locked dependency closure."""
    files = git("ls-tree", "-r", "--name-only", rev).splitlines()
    lock = tomllib.loads(blob(rev, "Cargo.lock").decode())
    root = tomllib.loads(blob(rev, "Cargo.toml").decode())
    workspace = root["workspace"]
    members = {}
    inherited = set()
    for path in workspace.get("members", []):
        manifest = tomllib.loads(blob(rev, f"{path}/Cargo.toml").decode())
        metadata = manifest["package"]
        members[metadata["name"]] = path
        if metadata.get("version") == {"workspace": True}:
            inherited.add(metadata["name"])
    by_name = {}
    for item in lock["package"]:
        by_name.setdefault(item["name"], []).append(item)

    def resolve(dependency):
        parts = dependency.split(" ", 2)
        matches = by_name.get(parts[0], [])
        if len(parts) > 1:
            matches = [item for item in matches if item["version"] == parts[1]]
        if len(parts) > 2:
            source = parts[2].removeprefix("(").removesuffix(")")
            matches = [item for item in matches if item.get("source") == source]
        if len(matches) != 1:
            raise ValueError(f"Cannot resolve locked dependency {dependency!r}")
        return matches[0]

    pending = [resolve(package)]
    seen_ids = set()
    seen = set()
    records = []
    while pending:
        item = pending.pop()
        identity = (item["name"], item["version"], item.get("source"))
        if identity in seen_ids:
            continue
        seen_ids.add(identity)
        name = item["name"]
        seen.add(name)
        record = dict(item)
        if name in inherited and "source" not in item:
            record.pop("version", None)
        dependencies = record.get("dependencies", [])
        resolved = [(dep, resolve(dep)) for dep in dependencies]
        # Only inherited workspace versions are host release metadata.
        record["dependencies"] = sorted(
            target["name"] if target["name"] in inherited and "source" not in target else dep
            for dep, target in resolved
        )
        pending.extend(target for _, target in resolved)
        records.append(record)
    config = {
        "package": {k: v for k, v in workspace.get("package", {}).items() if k != "version"},
        "dependencies": {k: v for k, v in workspace.get("dependencies", {}).items()
                         if k in seen or (isinstance(v, dict) and v.get("package") in seen)},
        "patch": {registry: {name: value for name, value in entries.items() if name in seen}
                  for registry, entries in root.get("patch", {}).items()},
        "profile": root.get("profile", {}),
        "locked": sorted(records, key=lambda x: (x["name"], x.get("version", ""), x.get("source", ""))),
    }
    prefixes = [f"{members[name]}/" for name in seen if name in members]
    def is_crate_input(path, prefix):
        if not path.startswith(prefix):
            return False
        relative = path[len(prefix):]
        return relative in ("Cargo.toml", "build.rs") or relative.startswith(
            ("src/", "messages/", "wit/", "build-support/")
        )
    selected = [path for path in files if any(is_crate_input(path, p) for p in prefixes)]
    selected += [p for p in files if p.startswith("patches/")
                 and p.rsplit("/", 1)[-1].split("+", 1)[0] in seen]
    taskfile = blob(rev, "Taskfile.yml").decode()
    config["patch_tool"] = taskfile.split("  brand:generate:", 1)[0]
    selected += [p for p in ("rust-toolchain.toml", ".cargo/config.toml",
                 "backend/crates/stravia-vendor-base/src/bin/stravia-build-vendors.rs") if p in files]
    # Rust's literal includes can reference assets outside the crate (e.g. UI icons).
    # Traverse them so newly added includes remain tracked after their first release.
    queue = list(selected)
    known = set(selected)
    while queue:
        path = queue.pop()
        if not path.endswith(".rs"):
            continue
        text = blob(rev, path).decode("utf-8")
        for target in re.findall(r'include_(?:str|bytes)!\s*\(\s*"([^"\n]+)"', text):
            resource = posixpath.normpath(posixpath.join(posixpath.dirname(path), target))
            if resource in files and resource not in known:
                known.add(resource)
                selected.append(resource)
                queue.append(resource)
    if icon:
        selected.append("frontend/stravia-webui/src/assets/icons/" + icon)
    workflow = blob(rev, ".github/workflows/release.yml").decode()
    config["tool_versions"] = re.findall(r'^  (?:RUST_VERSION|TASK_VERSION):.*$', workflow, re.M)
    return json.dumps(config, sort_keys=True), [(p, blob(rev, p)) for p in sorted(set(selected))]


def choose_baseline(releases, head="HEAD"):
    candidates = []
    for release in releases:
        if release.get("draft") or not release.get("published_at"):
            continue
        tag = release["tag_name"]
        try:
            commit = git("rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}").strip()
        except subprocess.CalledProcessError:
            # An unresolved published tag cannot safely be ruled out as the baseline.
            return None
        if subprocess.run(["git", "merge-base", "--is-ancestor", commit, head]).returncode:
            continue
        count = int(git("rev-list", "--count", f"{commit}..{head}"))
        if count:
            marked = any(a.get("name") == MARKER for a in release.get("assets", []))
            candidates.append((count, commit, marked))
    if not candidates:
        return None
    _, commit, marked = min(candidates)
    return commit if marked else None


def select(baseline, head="HEAD"):
    if baseline is None:
        return list(PLUGINS)
    return [name for name, (package, icon) in PLUGINS.items()
            if snapshot(baseline, package, icon) != snapshot(head, package, icon)]


def releases():
    result = []
    page = 1
    while True:
        url = f"https://api.github.com/repos/{os.environ['GITHUB_REPOSITORY']}/releases?per_page=100&page={page}"
        request = urllib.request.Request(url, headers={"Authorization": f"Bearer {os.environ['GH_TOKEN']}",
                                                       "Accept": "application/vnd.github+json"})
        with urllib.request.urlopen(request) as response:
            batch = json.load(response)
        result.extend(batch)
        if len(batch) < 100:
            return result
        page += 1


def main():
    baseline = choose_baseline(releases()) if os.environ.get("PUBLISH") == "true" else None
    selected = select(baseline)
    print(f"Plugin baseline: {baseline or 'none'}; selected: {', '.join(selected) or 'none'}")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
        output.write(f"selected={','.join(selected)}\nhas_plugins={str(bool(selected)).lower()}\n")
    Path(MARKER).write_text(json.dumps({"schema": 1}) + "\n")


if __name__ == "__main__":
    main()
