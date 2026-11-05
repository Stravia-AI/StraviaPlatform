import base64
import json
import os
import re
from pathlib import Path


def main() -> None:
    for name in (
        "TAURI_SIGNING_PRIVATE_KEY",
        "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
        "STRAVIA_UPDATER_PUBLIC_KEY",
    ):
        if not os.environ.get(name):
            raise SystemExit(f"{name} must not be empty")

    raw_public_key = (
        os.environ["STRAVIA_UPDATER_PUBLIC_KEY"]
        .strip()
        .replace("\\r\\n", "\n")
        .replace("\\n", "\n")
    )
    payload_match = re.search(r"(?:^|\s)(RW[A-Za-z0-9+/=]+)\s*$", raw_public_key)
    if payload_match is None:
        try:
            decoded_public_key = base64.b64decode(
                "".join(raw_public_key.split()), validate=True
            ).decode("utf-8")
        except (ValueError, UnicodeDecodeError):
            decoded_public_key = ""
        payload_match = re.search(
            r"(?:^|\s)(RW[A-Za-z0-9+/=]+)\s*$", decoded_public_key
        )
    if payload_match is None:
        raise SystemExit("STRAVIA_UPDATER_PUBLIC_KEY must contain a Minisign public key")

    public_key_text = (
        "untrusted comment: minisign public key\n"
        f"{payload_match.group(1)}\n"
    )
    public_key = base64.b64encode(public_key_text.encode("utf-8")).decode("ascii")
    config_path = Path("backend/apps/stravia-desktop/tauri.conf.json")
    config = json.loads(config_path.read_text(encoding="utf-8"))
    config["plugins"]["updater"]["pubkey"] = public_key
    config_path.write_text(
        json.dumps(config, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    with Path(os.environ["GITHUB_ENV"]).open("a", encoding="utf-8") as output:
        output.write(f"STRAVIA_UPDATER_PUBLIC_KEY={public_key}\n")


if __name__ == "__main__":
    main()
