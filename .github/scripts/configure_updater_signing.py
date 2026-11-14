import base64
import json
import os
import re
from pathlib import Path


def main() -> None:
    for name in (
        "TAURI_SIGNING_PRIVATE_KEY",
        "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
    ):
        if not os.environ.get(name):
            raise SystemExit(f"{name} must not be empty")

    config_path = Path("backend/apps/stravia-desktop/tauri.conf.json")
    config = json.loads(config_path.read_text(encoding="utf-8"))
    public_key = config["plugins"]["updater"]["pubkey"]
    try:
        public_key_text = base64.b64decode(public_key, validate=True).decode("utf-8")
    except (ValueError, UnicodeDecodeError):
        raise SystemExit("Configured updater public key must be base64-encoded key text")
    if re.search(r"(?:^|\s)RW[A-Za-z0-9+/=]+\s*$", public_key_text) is None:
        raise SystemExit("Configured updater public key must contain a Minisign public key")


if __name__ == "__main__":
    main()
