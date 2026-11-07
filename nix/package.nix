{
  lib,
  stdenv,
  bun,
  bun2nix,
  rustPlatform,
  rustToolchain,
  source,
  autoPatchelfHook,
  clang,
  cmake,
  fetchurl,
  gitMinimal,
  ninja,
  pkg-config,
  python3,
}:
let
  rootPackage = lib.importJSON (source + "/package.json");
  workspace = builtins.fromTOML (builtins.readFile (source + "/Cargo.toml"));
  toolchain = builtins.fromTOML (builtins.readFile (source + "/rust-toolchain.toml"));
  version = workspace.workspace.package.version;
  rustlsCrate = fetchurl {
    url = "https://static.crates.io/crates/rustls/rustls-0.23.46.crate";
    hash = "sha256-SOE72MDpNlxDz6XJ6PmtSdPIREkmyarIGeDk3FA8j98=";
  };
  ramaHttpCoreCrate = fetchurl {
    url = "https://static.crates.io/crates/rama-http-core/rama-http-core-0.4.0.crate";
    hash = "sha256-vYMlYwApeXVOYZU3oNWGGx8cjftQW3HDFMzXgbJpcyQ=";
  };

  # 用白名单界定源码，避免把开发数据库、凭据和已有产物送入 store。
  cleanSource =
    include:
    lib.cleanSourceWith {
      name = "stravia-source";
      src = source;
      filter =
        path: type:
        let
          relative = lib.removePrefix (toString source + "/") (toString path);
          name = baseNameOf path;
        in
        (toString path == toString source || include relative)
        && !(builtins.elem name [
          "target"
          "dist"
          "dist-desktop-e2e"
          "node_modules"
          ".svelte-kit"
          ".git"
          ".devenv"
          ".direnv"
          ".stravia-dev"
          "paraglide"
        ])
        && !(lib.hasPrefix ".env" name)
        && type != "symlink";
    };
  inTree = root: path: path == root || lib.hasPrefix (root + "/") path;
  frontendInput =
    path:
    builtins.elem path [
      "package.json"
      "bun.lock"
      "frontend"
    ]
    || (inTree "patches" path && !(inTree "patches/cargo" path))
    || inTree "frontend/stravia-webui" path;
  webSource = cleanSource frontendInput;
  serverSource = cleanSource (
    path:
    builtins.elem path [
      "Cargo.toml"
      "Cargo.lock"
      "rust-toolchain.toml"
      "LICENSE"
      "patches"
      "frontend"
      "frontend/stravia-webui"
      "frontend/stravia-webui/src"
      "frontend/stravia-webui/src/assets"
      "frontend/stravia-webui/static"
    ]
    || inTree ".cargo" path
    || inTree "patches/cargo" path
    || inTree "backend" path
    || inTree "frontend/stravia-webui/src/assets/icons" path
    || path == "frontend/stravia-webui/static/stravia-logo.svg"
  );
  patchedDependencies = lib.mapAttrs (_: patch: source + "/${patch}") rootPackage.patchedDependencies;

  webui = stdenv.mkDerivation {
    pname = "stravia-webui";
    inherit version;
    src = webSource;
    bunDeps = bun2nix.fetchBunDeps {
      bunNix = ./bun.nix;
      overrides = bun2nix.patchedDependenciesToOverrides { inherit patchedDependencies; };
    };
    nativeBuildInputs = [
      bun
      bun2nix.hook
      autoPatchelfHook
    ];
    buildInputs = [ stdenv.cc.cc.lib ];
    strictDeps = true;
    dontConfigure = true;
    dontUseBunBuild = true;
    dontUseBunCheck = true;
    dontUseBunInstall = true;
    bunInstallFlags = [
      "--frozen-lockfile"
      "--linker=isolated"
      "--backend=copyfile"
    ];

    # Bun 同时安装 GNU 和 musl optional packages；glibc 构建只修补 GNU bindings。
    # 在 lifecycle scripts 前完成，Tailwind/LightningCSS 的 dlopen 才能找到运行库。
    postBunNodeModulesInstallPhase = ''
      chmod -R u+w node_modules
      mkdir -p frontend/stravia-webui/node_modules
      chmod u+w frontend/stravia-webui/node_modules
      autoPatchelf node_modules/.bun/*-linux-*-gnu@*
    '';
    buildPhase = ''
      runHook preBuild
      bun run build:web
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      mkdir -p "$out"
      cp -r frontend/stravia-webui/dist/. "$out/"
      runHook postInstall
    '';
  };
in
assert lib.assertMsg (
  bun.version == rootPackage.engines.bun
) "Stravia requires the Bun version in package.json engines.bun";
assert lib.assertMsg (
  rootPackage.packageManager == "bun@${bun.version}"
) "Stravia packageManager and Bun versions must agree";
assert lib.assertMsg (
  rustToolchain.version == toolchain.toolchain.channel
) "Stravia requires the Rust version in rust-toolchain.toml";
assert lib.assertMsg (builtins.elem "wasm32-wasip2" toolchain.toolchain.targets)
  "Stravia requires the wasm32-wasip2 Rust target";
stdenv.mkDerivation {
  pname = "stravia-server";
  inherit version;
  src = serverSource;
  cargoDeps = rustPlatform.importCargoLock {
    lockFile = builtins.path {
      path = source + "/Cargo.lock";
      name = "Cargo.lock";
    };
    outputHashes = {
      "tokio-tungstenite-0.28.0" = "sha256-V1xmnrfRWOcZZogelZEA4vvyMj2awCfHVA5/glQ6KAI=";
      "tungstenite-0.27.0" = "sha256-VVHhk7l9J/sEmG3q/UuV/sQ3f+fGsmq5vumSy8vbMvw=";
    };
  };
  nativeBuildInputs = [
    rustToolchain
    rustPlatform.cargoSetupHook
    rustPlatform.bindgenHook
    clang
    cmake
    gitMinimal
    ninja
    pkg-config
    python3
  ];
  strictDeps = true;
  dontConfigure = true;

  # Cargo 的子进程继承同一离线配置，guest 的显式 --target 不受宿主默认 target 干扰。
  env = {
    CARGO_NET_OFFLINE = "true";
    CMAKE_POLICY_VERSION_MINIMUM = "3.5";
  };
  buildPhase = ''
    runHook preBuild
    export CARGO_HOME="$TMPDIR/cargo-home"
    export CARGO_BUILD_JOBS="$NIX_BUILD_CORES"
    mkdir -p "$CARGO_HOME" frontend/stravia-webui/dist
    cp -r ${webui}/. frontend/stravia-webui/dist/
    python3 patches/cargo/prepare.py --offline \
      --crate-file rustls=${rustlsCrate} \
      --crate-file rama-http-core=${ramaHttpCoreCrate}
    cargo run --locked --offline -p stravia-vendor-base --bin stravia-build-vendors
    cargo build --locked --offline --release -p stravia-server --bin stravia-server
    runHook postBuild
  '';
  installPhase = ''
    runHook preInstall
    install -Dm755 target/release/stravia-server "$out/bin/stravia-server"
    install -Dm644 LICENSE "$out/share/licenses/stravia-server/LICENSE"
    runHook postInstall
  '';
  meta = {
    description = "Stravia Server with embedded production WebUI and base Wasm vendor";
    homepage = "https://github.com/Stravia-AI/StraviaPlatform";
    license = lib.licenses.agpl3Only;
    mainProgram = "stravia-server";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
  };
}
