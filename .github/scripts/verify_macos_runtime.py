import argparse
import plistlib
import re
import subprocess
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Verify and exercise the signed macOS application's Wasm runtime"
    )
    parser.add_argument("app", type=Path)
    args = parser.parse_args()
    app = args.app.resolve(strict=True)
    executable = app / "Contents/MacOS/stravia-desktop"
    subprocess.run(
        ["codesign", "--verify", "--deep", "--strict", str(app)], check=True
    )
    signature = subprocess.run(
        ["codesign", "-dvvv", str(executable)],
        check=True, capture_output=True, text=True,
    )
    flags = re.search(r"\bflags=0x([0-9a-fA-F]+)", signature.stderr)
    if flags is None or not int(flags[1], 16) & 0x10000:
        raise SystemExit("The desktop executable must retain Hardened Runtime")
    entitlements = subprocess.run(
        ["codesign", "-d", "--entitlements", ":-", str(executable)],
        check=True, capture_output=True,
    )
    if plistlib.loads(entitlements.stdout) != {
        "com.apple.security.cs.allow-unsigned-executable-memory": True
    }:
        raise SystemExit("Unexpected desktop runtime entitlements")

    # 必须运行最终签名宿主；未签名的 cargo test 无法覆盖 macOS 可执行页策略。
    # 由子进程创建隔离目录并执行真实内置组件，不启动 WebView 或访问生产服务。
    subprocess.run([str(executable), "--smoke-test-runtime"], check=True, timeout=180)


if __name__ == "__main__":
    main()
