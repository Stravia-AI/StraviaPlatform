from __future__ import annotations

from pathlib import Path

import pytest

from tests.common.helpers import resolve_stravia_binary


@pytest.fixture(scope="session")
def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


@pytest.fixture(scope="session")
def stravia_binary(repo_root: Path) -> Path:
    binary = resolve_stravia_binary(repo_root)
    if not binary.exists():
        pytest.skip(f"stravia-server not found at {binary}; build it first or set STRAVIA_BINARY")
    return binary
