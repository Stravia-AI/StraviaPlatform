# Reproducible, lockfile-independent Windows 10 Enterprise LTSC 2021 Eval golden qcow2 image
# built inside Nix's KVM sandbox (`sandbox = true`, `requiredSystemFeatures = [ "kvm" ]`).
#
# Important: Project dependency caches (bun-cache / uv-cache) are intentionally excluded from
# this derivation so updates to bun.lock / uv.lock / Cargo.lock never rebuild the OS image.
# Do NOT publish the resulting store path to a public binary cache (Microsoft Eval license).
{
  lib,
  stdenvNoCC,
  assets,
  qemu_kvm,
  _7zz,
  cdrkit,
  openssh,
  python3,
  coreutils,
  gawk,
  gnugrep,
  gnused,
  findutils,
  diskSize ? "24G",
  buildRam ? "8G",
  maxBuildCpus ? 8,
  buildTimeout ? 2700,
  windowsUser ? "stravia",
  buildScript ? ../../.github/scripts/build_windows_kvm_image.sh,
}:
stdenvNoCC.mkDerivation {
  pname = "stravia-windows-e2e-image";
  version = "win10-ltsc-2021-disk${diskSize}";

  requiredSystemFeatures = [
    "kvm"
    "big-parallel"
  ];

  dontUnpack = true;
  dontConfigure = true;
  dontFixup = true;

  nativeBuildInputs = [
    qemu_kvm
    _7zz
    cdrkit
    openssh
    python3
    coreutils
    gawk
    gnugrep
    gnused
    findutils
  ];

  buildPhase = ''
    runHook preBuild

    export HOME="$TMPDIR"
    build_cpus="''${NIX_BUILD_CORES:-4}"
    if (( build_cpus > ${toString maxBuildCpus} )); then
      build_cpus=${toString maxBuildCpus}
    elif (( build_cpus < 2 )); then
      build_cpus=2
    fi

    export OFFLINE_BUILD=1
    export SKIP_ISO_SHA256=1
    export KEEP_BUILD_WORKDIR=0
    export DISK_SIZE=${lib.escapeShellArg diskSize}
    export BUILD_RAM=${lib.escapeShellArg buildRam}
    export BUILD_CPUS="$build_cpus"
    export BUILD_TIMEOUT=${toString buildTimeout}
    export WINDOWS_SSH_USER=${lib.escapeShellArg windowsUser}

    export QEMU_BIN="${qemu_kvm}/bin/qemu-system-x86_64"
    export QEMU_IMG_BIN="${qemu_kvm}/bin/qemu-img"
    export SEVENZ_BIN="${_7zz}/bin/7zz"
    export MKISOFS_BIN="${cdrkit}/bin/genisoimage"
    export PYTHON3_BIN="${python3}/bin/python3"

    export VIRTIO_VERSION=${lib.escapeShellArg assets.versions.virtio}
    export OPENSSH_VERSION=${lib.escapeShellArg assets.versions.openssh}
    export NODE_VERSION=${lib.escapeShellArg assets.versions.node}
    export BUN_VERSION=${lib.escapeShellArg assets.versions.bun}
    export UV_VERSION=${lib.escapeShellArg assets.versions.uv}
    export PYTHON_VERSION=${lib.escapeShellArg assets.versions.python}
    export PYTHON_BUILD_TAG=${lib.escapeShellArg assets.versions.pythonBuildTag}

    export WIN10_LTSC_ISO_PATH="${assets.win10Iso}"
    export VIRTIO_ISO_PATH="${assets.virtioIso}"
    export WEBVIEW2_INSTALLER_PATH="${assets.webview2Installer}"
    export OPENSSH_ZIP_PATH="${assets.opensshZip}"
    export NODE_ZIP_PATH="${assets.nodeZip}"
    export BUN_ZIP_PATH="${assets.bunZip}"
    export UV_ZIP_PATH="${assets.uvZip}"
    export PYTHON_STANDALONE_TAR_PATH="${assets.pythonStandaloneTar}"

    export OUTPUT_IMAGE="$out/share/stravia-win-vm/win10-ltsc-2021-e2e-golden.qcow2"
    export METADATA_OUTPUT="$out/share/stravia-win-vm/metadata.json"

    bash ${buildScript}

    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    test -f "$out/share/stravia-win-vm/win10-ltsc-2021-e2e-golden.qcow2"
    test -f "$out/share/stravia-win-vm/metadata.json"
    runHook postInstall
  '';

  passthru = {
    inherit assets diskSize;
    imagePath = "share/stravia-win-vm/win10-ltsc-2021-e2e-golden.qcow2";
    metadataPath = "share/stravia-win-vm/metadata.json";
  };

  meta = {
    description = "Read-only Windows 10 Enterprise LTSC 2021 Eval golden qcow2 for Stravia KVM Desktop E2E";
    platforms = [ "x86_64-linux" ];
  };
}
