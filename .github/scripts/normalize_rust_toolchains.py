"""Keep only the active CI toolchain so runner image drift cannot split cache keys."""

from __future__ import annotations

import os
import subprocess


def main() -> None:
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise RuntimeError("Toolchain normalization is only allowed on GitHub Actions runners")
    active = subprocess.check_output(
        ["rustup", "show", "active-toolchain"], text=True
    ).split()[0]
    installed = subprocess.check_output(
        ["rustup", "toolchain", "list", "--quiet"], text=True
    ).splitlines()
    for line in installed:
        toolchain = line.split()[0]
        if toolchain != active:
            subprocess.run(["rustup", "toolchain", "uninstall", toolchain], check=True)


if __name__ == "__main__":
    main()
