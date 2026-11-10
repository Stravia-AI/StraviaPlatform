{ pkgs, lib, config, inputs, ... }:

let
  packageJson = builtins.fromJSON (builtins.readFile ./package.json);
  nativeStdenv =
    if pkgs.stdenv.hostPlatform.isLinux
    then pkgs.stdenvAdapters.useMoldLinker pkgs.clangStdenv
    else pkgs.clangStdenv;
  replaceRustCompiler = oldCompiler: dependencies:
    map (dependency:
      if dependency.outPath == oldCompiler.outPath then nativeStdenv.cc else dependency
    ) dependencies;
  # 只替换聚合包传播的编译器，不改变预编译 Rust 组件及其运行库。
  rustToolchain = ((inputs.rust-overlay.lib.mkRustBin { } pkgs.buildPackages)
    .fromRustupToolchainFile config.languages.rust.toolchainFile).overrideAttrs (old: {
    depsHostHostPropagated = replaceRustCompiler
      pkgs.buildPackages.stdenv.cc old.depsHostHostPropagated;
    propagatedBuildInputs = replaceRustCompiler
      pkgs.pkgsTargetTarget.stdenv.cc old.propagatedBuildInputs;
  });
  # C 模块硬编码 pkgs.stdenv；保留它的开发工具，但由当前环境统一提供编译器。
  cDevelopmentPackages = with pkgs; [
    clang-tools
    gnumake
  ] ++ lib.optional config.languages.c.lsp.enable config.languages.c.lsp.package
    ++ lib.optional (config.languages.c.debugger != null) config.languages.c.debugger
    ++ lib.optional (lib.meta.availableOn pkgs.stdenv.hostPlatform pkgs.valgrind
      && !pkgs.valgrind.meta.broken) pkgs.valgrind;
  desktopPackages = with pkgs; [
    gtk3
    webkitgtk_4_1
    libayatana-appindicator
    librsvg
    gsettings-desktop-schemas
  ];
  patchCrate = pkgs.callPackage ./nix/patch-crate.nix { };
  workspaceInit = pkgs.writeScript "stravia-workspace-init" ''
    #!${lib.getExe pkgs.python3}
    import errno
    import fcntl
    import json
    import os
    from pathlib import Path
    import socket
    import subprocess
    import sys
    import tempfile

    root = Path(os.environ["DEVENV_ROOT"]).resolve(strict=True)
    state = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state")
    registry_directory = state / "stravia/devenv"
    registry_directory.mkdir(parents=True, exist_ok=True)
    registry = registry_directory / "server-ports.json"

    # 跨工作区串行分配；尚未启动的工作区也保留登记，避免其他工作区复用。
    with (registry_directory / "server-ports.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            allocations = json.loads(registry.read_text())
        except FileNotFoundError:
            allocations = {}
        allocations = {
            directory: port for directory, port in allocations.items()
            if Path(directory).is_dir()
        }
        # 复用已有登记，不重新探测可能正由本工作区 Server 使用的端口。
        if str(root) not in allocations:
            reserved = set(allocations.values())
            for port in range(23471, 65536):
                if port in reserved:
                    continue
                with socket.socket() as listener:
                    try:
                        listener.bind(("127.0.0.1", port))
                    except OSError as error:
                        if error.errno == errno.EADDRINUSE:
                            continue
                        raise
                    allocations[str(root)] = port
                    with tempfile.TemporaryDirectory(dir=registry_directory) as temporary:
                        staged = Path(temporary) / registry.name
                        staged.write_text(json.dumps(allocations) + "\n")
                        staged.replace(registry)
                    break
            else:
                raise SystemExit("No free Stravia development server port available")
        print(allocations[str(root)])

    if not (root / ".git").is_file():
        raise SystemExit(0)

    worktrees = subprocess.check_output([
        "${pkgs.git}/bin/git", "-C", str(root), "worktree", "list", "--porcelain", "-z",
    ])
    main = Path(os.fsdecode(worktrees.split(b"\0", 1)[0].removeprefix(b"worktree ")))
    if root.samefile(main):
        raise SystemExit(0)

    print("Checking linked worktree initialization", file=sys.stderr, flush=True)
    # 锁文件保留原 inode，避免等待者与新进程锁住不同文件。
    with (Path(os.environ["DEVENV_STATE"]) / "worktree-init.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for name in (".stravia-dev", "target"):
            source = main / name
            destination = root / name
            if os.path.lexists(destination) or not source.is_dir():
                continue

            # 临时副本与目标同文件系统；失败不发布半成品，也不覆盖并发创建的目标。
            with tempfile.TemporaryDirectory(prefix=".devenv-worktree-init-", dir=root) as temporary:
                staged = Path(temporary) / name
                if name == "target":
                    # reflink 仍逐文件创建元数据；只预热构建依赖与 Wasm 成品。
                    caches = (
                        "patch",
                        "debug/deps", "debug/.fingerprint", "debug/build",
                        "release/deps", "release/.fingerprint", "release/build",
                        "vendor-plugins", "vendor-plugins-all",
                    )
                    staged.mkdir()
                    copies = [(source / cache, staged / cache) for cache in caches
                              if (source / cache).is_dir()]
                else:
                    copies = [(source.resolve(), staged)]
                for original, copied in copies:
                    copied.parent.mkdir(parents=True, exist_ok=True)
                    subprocess.run([
                        "${pkgs.coreutils}/bin/cp", "--archive", "--reflink=always",
                        "--", str(original), str(copied),
                    ], check=True)
                subprocess.run([
                    "${pkgs.coreutils}/bin/mv", "--no-clobber", "--no-target-directory",
                    "--", str(staged), str(destination),
                ], check=True)
  '';
in
{
  stdenv = nativeStdenv;

  assertions = [
    {
      assertion = config.languages.javascript.bun.package.version == packageJson.engines.bun;
      message = "devenv.lock 中的 Bun 与 package.json 不一致，请选择包含所需 Bun 版本的 nixpkgs。";
    }
  ];

  packages = with pkgs; [
    go-task
    uv
    python3
    git
    pkg-config
    cmake
    openssl
    docker-client
  ] ++ [ patchCrate ] ++ cDevelopmentPackages
    ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux ([ pkgs.mold pkgs.perf ] ++ desktopPackages);

  services.postgres = {
    enable = true;
    package = pkgs.postgresql_16;
  };

  languages.c.enable = false;

  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
    toolchainPackage = lib.mkForce rustToolchain;
    lsp.package = rustToolchain;
    # 不再额外注入默认 Clang wrapper；Cargo 直接使用同一套编译器和链接配置。
    clangLinker.enable = false;
  };

  languages.javascript = {
    enable = true;
    bun.enable = true;
  };

  env.LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
  env."CARGO_TARGET_${pkgs.stdenv.hostPlatform.rust.cargoEnvVarTarget}_LINKER" =
    "${nativeStdenv.cc}/bin/clang";

  # 放在实际 shell hook 中，避免 CLI 忽略初始化 task 的失败后继续启动。
  enterShell = ''
    STRAVIA_PORT="$(${workspaceInit})" || exit $?
    export STRAVIA_PORT
    (cd "$DEVENV_ROOT" && task prepare:crates) || exit $?
  '' + lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
    # 未安装到系统目录的 GTK schema 仍须能被桌面进程找到。
    export XDG_DATA_DIRS="${lib.makeSearchPath "share/gsettings-schemas/${pkgs.gtk3.name}" [ pkgs.gtk3 ]}:${lib.makeSearchPath "share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}" [ pkgs.gsettings-desktop-schemas ]}:''${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
  '';
}
