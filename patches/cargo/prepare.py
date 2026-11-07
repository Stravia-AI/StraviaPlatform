#!/usr/bin/env python3
"""Prepare the workspace's pinned, locally patched Cargo dependencies."""

import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
OUTPUT = ROOT / '.stravia-patched-crates'
STAMP = '.prepare-stamp.json'


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def tree_digest(root):
    h = hashlib.sha256()
    for path in sorted(root.rglob('*')):
        if path.name == STAMP and path.parent == root:
            continue
        if path.is_symlink() or not (path.is_file() or path.is_dir()):
            raise ValueError(f'unexpected file in prepared crate: {path}')
        name = path.relative_to(root).as_posix().encode()
        h.update(len(name).to_bytes(8, 'big'))
        h.update(name)
        if path.is_file():
            h.update(b'f')
            h.update(bytes.fromhex(digest(path)))
        else:
            h.update(b'd')
    return h.hexdigest()


@contextlib.contextmanager
def lock():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    with (OUTPUT / '.lock').open('a+b') as f:
        if os.name == 'nt':
            import msvcrt
            f.seek(0, os.SEEK_END)
            if f.tell() == 0:
                f.write(b'0')
                f.flush()
            while True:
                try:
                    f.seek(0)
                    msvcrt.locking(f.fileno(), msvcrt.LK_NBLCK, 1)
                    break
                except OSError:
                    time.sleep(0.1)
        else:
            import fcntl
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            if os.name == 'nt':
                f.seek(0)
                msvcrt.locking(f.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)


def expected(entry):
    return {'crate': entry['crate'], 'version': entry['version'],
            'tarball': entry['sha256'], 'patch': digest(HERE / entry['patch']),
            'script': digest(Path(__file__))}


def current(path, wanted):
    try:
        if path.is_symlink():
            return False
        stamp = json.loads((path / STAMP).read_text(encoding='utf-8'))
        return (all(stamp.get(k) == v for k, v in wanted.items())
                and stamp.get('tree') == tree_digest(path))
    except (OSError, ValueError, TypeError):
        return False


def acquire(entry, args, temp):
    name = f"{entry['crate']}-{entry['version']}.crate"
    if entry['crate'] in args.crate_file:
        candidates = [args.crate_file[entry['crate']]]
    else:
        cargo_home = Path(os.environ.get('CARGO_HOME', Path.home() / '.cargo'))
        candidates = sorted((cargo_home / 'registry' / 'cache').glob(f'*/{name}'))
    for candidate in candidates:
        if digest(candidate) != entry['sha256']:
            raise ValueError(f'SHA-256 mismatch for {candidate}; remove the corrupt archive or supply --crate-file')
        return candidate
    if args.offline:
        raise ValueError(f'{name} not found in Cargo cache; supply --crate-file in offline mode')
    url = f"https://static.crates.io/crates/{entry['crate']}/{name}"
    target = temp / name
    with urllib.request.urlopen(url, timeout=60) as response, target.open('wb') as dest:
        shutil.copyfileobj(response, dest)
    if digest(target) != entry['sha256']:
        raise ValueError(f'SHA-256 mismatch downloading {url}')
    return target


def extract(archive, temp, entry):
    prefix = f"{entry['crate']}-{entry['version']}"
    root = temp / prefix
    with tarfile.open(archive, 'r:gz') as tar:
        for member in tar:
            p = PurePosixPath(member.name)
            if (p.is_absolute() or not p.parts or p.parts[0] != prefix
                    or any(part in ('.', '..') or '\\' in part or ':' in part for part in p.parts)
                    or not (member.isdir() or member.isfile())):
                raise ValueError(f'unsafe crate archive member: {member.name}')
            target = temp.joinpath(*p.parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                source = tar.extractfile(member)
                if source is None:
                    raise ValueError(f'cannot read crate archive member: {member.name}')
                with source, target.open('xb') as dest:
                    shutil.copyfileobj(source, dest)
                os.chmod(target, member.mode & 0o777)
    if not root.is_dir():
        raise ValueError(f'archive has no {prefix} directory')
    return root


def apply_patch(root, patch):
    # Prevent Git from discovering the enclosing checkout; otherwise git apply can
    # silently skip all paths when invoked below the worktree root.
    env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
    env['GIT_CEILING_DIRECTORIES'] = str(OUTPUT.resolve())
    for options in (['--check'], []):
        subprocess.run(['git', 'apply', *options, str(patch)], cwd=root,
                       env=env, check=True)


def prepare(entry, args):
    directory = entry['directory']
    if not directory or Path(directory).name != directory or directory in ('.', '..'):
        raise ValueError(f'invalid crate directory: {directory!r}')
    dest = OUTPUT / directory
    wanted = expected(entry)
    if current(dest, wanted):
        return
    if args.check:
        raise ValueError(f'{dest} is missing or stale; run task prepare:crates')
    if dest.exists() or dest.is_symlink():
        raise ValueError(f'{dest} is stale or corrupt; bump the patch revision and Cargo path instead of replacing an existing crate directory')
    with tempfile.TemporaryDirectory(prefix='.prepare-', dir=OUTPUT) as tmp:
        tmp = Path(tmp)
        archive = acquire(entry, args, tmp)
        root = extract(archive, tmp, entry)
        apply_patch(root, (HERE / entry['patch']).resolve())
        wanted['tree'] = tree_digest(root)
        (root / STAMP).write_text(json.dumps(wanted, sort_keys=True) + '\n', encoding='utf-8')
        # A revision is immutable once published. Rename into its previously
        # absent destination on the same filesystem while holding the lock.
        root.rename(dest)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--offline', action='store_true', help='never download a crate')
    parser.add_argument('--crate-file', action='append', default=[], metavar='CRATE=PATH',
                        help='use this archive for CRATE instead of the Cargo cache (repeatable)')
    parser.add_argument('--check', action='store_true', help='check prepared output without writing')
    args = parser.parse_args()
    entries = json.loads((HERE / 'crates.json').read_text(encoding='utf-8'))
    entries_by_name = {entry['crate']: entry for entry in entries}
    mappings = {}
    for mapping in args.crate_file:
        name, separator, filename = mapping.partition('=')
        if not separator or not name or not filename:
            parser.error('--crate-file must be CRATE=PATH')
        if name not in entries_by_name:
            parser.error(f'unknown crate in --crate-file: {name}')
        if name in mappings:
            parser.error(f'duplicate --crate-file for {name}')
        path = Path(filename)
        if digest(path) != entries_by_name[name]['sha256']:
            raise ValueError(f'SHA-256 mismatch for {path}')
        mappings[name] = path
    args.crate_file = mappings
    if args.check:
        for entry in entries:
            prepare(entry, args)
    else:
        with lock():
            for entry in entries:
                prepare(entry, args)


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, KeyError, json.JSONDecodeError, tarfile.TarError,
            subprocess.CalledProcessError) as exc:
        print(f'prepare:crates: {exc}', file=sys.stderr)
        sys.exit(1)
