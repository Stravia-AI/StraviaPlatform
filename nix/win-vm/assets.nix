# Pinned immutable Windows guest assets for the Layer-2 KVM Desktop E2E golden image.
# All entries are fixed-output derivations (FODs) so `image.nix` builds with `sandbox = true`
# and zero network access. Callers can override any source (e.g. pointing `win10Iso` at an
# internal mirror or `pkgs.requireFile`).
{
  fetchurl,
  win10Iso ? fetchurl {
    name = "win10-ltsc-2021-eval-x64.iso";
    url = "https://software-download.microsoft.com/download/pr/19044.1288.211006-0501.21h2_release_svc_refresh_CLIENT_LTSC_EVAL_x64FRE_en-us.iso";
    hash = "sha256-5KsuNTW+V0glKo1dV1Oablm+jWcmNF7hDnr9LLif77U=";
  },
  virtioIso ? fetchurl {
    name = "virtio-win-0.1.302-1.iso";
    url = "https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/archive-virtio/virtio-win-0.1.302-1/virtio-win.iso";
    hash = "sha256-MD965A2tSV1q5HT9xXHfWJWKTbxcN6Ui2A+aIDhnlJ0=";
  },
  webview2Installer ? fetchurl {
    name = "MicrosoftEdgeWebView2RuntimeInstallerX64.exe";
    url = "https://msedge.sf.dl.delivery.mp.microsoft.com/filestreamingservice/files/58f85fae-b5f5-4443-9b8d-6d4cf627d6f6/MicrosoftEdgeWebView2RuntimeInstallerX64.exe";
    hash = "sha256-RXadZoCallIFD4qRq1MpiBtlzEEj/nxzOEHe7775G/Y=";
  },
  opensshZip ? fetchurl {
    name = "OpenSSH-Win64-v9.5.0.0p1-Beta.zip";
    url = "https://github.com/PowerShell/Win32-OpenSSH/releases/download/v9.5.0.0p1-Beta/OpenSSH-Win64.zip";
    hash = "sha256-vUj+mF1ABALCeMSF2yDmqCvEx/fY4O9agRKPUjCWUww=";
  },
  nodeZip ? fetchurl {
    name = "node-v22.14.0-win-x64.zip";
    url = "https://nodejs.org/dist/v22.14.0/node-v22.14.0-win-x64.zip";
    hash = "sha256-VbY5KVkgshm7KsvPoA+QOToniQlbcyP3lHXJ80eV8hc=";
  },
  bunZip ? fetchurl {
    name = "bun-windows-x64-1.4.2.zip";
    url = "https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/bun-windows-x64.zip";
    hash = "sha256-zkwXSXsvKXEqmdPVPwKN4ozULjusuFiVmefwAOSbZAU=";
  },
  uvZip ? fetchurl {
    name = "uv-0.13.10-x86_64-pc-windows-msvc.zip";
    url = "https://github.com/astral-sh/uv/releases/download/0.13.10/uv-x86_64-pc-windows-msvc.zip";
    hash = "sha256-KXs4gf4a37kivswsFhMgKrtB3IPqX/Q9Ryqgh8ofhoM=";
  },
  pythonStandaloneTar ? fetchurl {
    name = "cpython-3.12.16-20261124-windows-x64.tar.gz";
    url = "https://github.com/astral-sh/python-build-standalone/releases/download/20261124/cpython-3.12.16%2B20261124-x86_64-pc-windows-msvc-install_only_stripped.tar.gz";
    hash = "sha256-lmlLUNz94T1W5ANG3apVKpzWUCkOm5M87Pg8HYIiaWA=";
  },
}:
{
  versions = {
    virtio = "0.1.302-1";
    openssh = "v9.5.0.0p1-Beta";
    node = "22.14.0";
    bun = "1.4.2";
    uv = "0.13.10";
    python = "3.12.16";
    pythonBuildTag = "20261124";
  };

  inherit
    win10Iso
    virtioIso
    webview2Installer
    opensshZip
    nodeZip
    bunZip
    uvZip
    pythonStandaloneTar
    ;
}
