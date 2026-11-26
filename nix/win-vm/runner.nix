# Wrapper around `.github/scripts/kvm_windows_e2e.sh` with pinned Nix host tools.
# When `baseImage` is null (default `devenv shell`), the golden image is resolved lazily via
# `nix build .#windows-e2e-image` on demand so entering `devenv shell` never downloads or
# builds a multi-gigabyte Windows image.
{
  writeShellApplication,
  qemu_kvm,
  openssh,
  cdrkit,
  gnutar,
  gzip,
  coreutils,
  python3,
  gitMinimal,
  baseImage ? null,
}:
writeShellApplication {
  name = "stravia-win-e2e";
  runtimeInputs = [
    qemu_kvm
    openssh
    cdrkit
    gnutar
    gzip
    coreutils
    python3
    gitMinimal
  ];
  text = ''
    repo_root="''${REPO_ROOT:-''${DEVENV_ROOT:-}}"
    if [[ -z "$repo_root" ]]; then
      repo_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
    fi

    if [[ -z "''${WINDOWS_BASE_IMAGE:-}" ]]; then
      ${
        if baseImage != null then
          ''
            WINDOWS_BASE_IMAGE="${baseImage}/${baseImage.imagePath}"
          ''
        else
          ''
            echo "==> Resolving immutable Windows E2E golden image via Nix (.#windows-e2e-image)..." >&2
            img_out="$(nix build "''${repo_root}#windows-e2e-image" --no-link --print-out-paths)"
            WINDOWS_BASE_IMAGE="''${img_out}/share/stravia-win-vm/win10-ltsc-2021-e2e-golden.qcow2"
          ''
      }
      export WINDOWS_BASE_IMAGE
    fi

    export REPO_ROOT="$repo_root"
    export QEMU_BIN="${qemu_kvm}/bin/qemu-system-x86_64"
    export QEMU_IMG_BIN="${qemu_kvm}/bin/qemu-img"
    export MKISOFS_BIN="${cdrkit}/bin/genisoimage"
    export PYTHON3_BIN="${python3}/bin/python3"

    exec bash "''${repo_root}/.github/scripts/kvm_windows_e2e.sh" "$@"
  '';
  meta = {
    description = "Run Stravia Windows Desktop E2E in an ephemeral KVM overlay over the read-only Nix golden image";
    platforms = [ "x86_64-linux" ];
  };
}
