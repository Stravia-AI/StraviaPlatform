{
  description = "Stravia AI protocol gateway";

  nixConfig = {
    extra-substituters = [
      "https://stravia-platform.cachix.org"
    ];
    extra-trusted-public-keys = [
      "stravia-platform.cachix.org-1:hL3Z7P4yIu42OshQ0TzlLRj23+lhujpq8l4gIH4C144="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      nixosModules.default = import ./nix/nixos-module.nix { inherit self; };

      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          inherit (pkgs) lib;
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

          source = lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              let
                name = baseNameOf path;
              in
              !builtins.elem name [
                ".git"
                ".scratch"
                "node_modules"
                "result"
                "target"
              ];
          };

          bun = pkgs.bun.overrideAttrs {
            version = "1.4.0";
            src = pkgs.fetchurl (
              if system == "x86_64-linux" then
                {
                  url = "https://github.com/oven-sh/bun/releases/download/bun-v1.4.0/bun-linux-x64-baseline.zip";
                  hash = "sha256-GE+0WV8NQBohfPfHjBvEMLqDMU2reouUgFurv3+nCX8=";
                }
              else
                {
                  url = "https://github.com/oven-sh/bun/releases/download/bun-v1.4.0/bun-linux-aarch64.zip";
                  hash = "sha256-SxozLuhhmD65O8/m93D/+U4+MbLDiL2uo8jtNeWO7Q4=";
                }
            );
          };

          bunCache = pkgs.stdenvNoCC.mkDerivation {
            pname = "stravia-bun-cache";
            inherit version;
            src = source;

            nativeBuildInputs = [ bun ];
            dontConfigure = true;
            dontPatchELF = true;
            dontPatchShebangs = true;
            dontStrip = true;

            buildPhase = ''
              runHook preBuild
              export HOME=$(mktemp -d)
              export BUN_INSTALL_CACHE_DIR="$out"
              mkdir -p "$out"
              bun install --frozen-lockfile --ignore-scripts --cpu='*' --os=linux
              runHook postBuild
            '';

            installPhase = "true";
            outputHashMode = "recursive";
            outputHashAlgo = "sha256";
            outputHash = "sha256-ABbbeswuVdhqdhcnLghQZ0KkWRAsicd//1Iea49JedQ=";
          };

          webui = pkgs.stdenvNoCC.mkDerivation {
            pname = "stravia-webui";
            inherit version;
            src = source;

            nativeBuildInputs = [
              bun
              pkgs.nodejs
            ];

            buildPhase = ''
              runHook preBuild
              export HOME=$(mktemp -d)
              export BUN_INSTALL_CACHE_DIR=$(mktemp -d)
              cp -R ${bunCache}/. "$BUN_INSTALL_CACHE_DIR"
              chmod -R u+w "$BUN_INSTALL_CACHE_DIR"
              bun install --frozen-lockfile --offline
              patchShebangs node_modules/.bun
              bun run build:web
              runHook postBuild
            '';

            installPhase = ''
              runHook preInstall
              mkdir -p "$out"
              cp -R frontend/stravia-webui/dist/. "$out/"
              runHook postInstall
            '';
          };

          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };

          server = rustPlatform.buildRustPackage {
            pname = "stravia-server";
            inherit version;
            src = source;

            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [
              "-p"
              "stravia-server"
            ];
            doCheck = false;
            nativeBuildInputs = [
              pkgs.cmake
              pkgs.git
            ];

            preBuild = ''
              mkdir -p frontend/stravia-webui/dist
              cp -R ${webui}/. frontend/stravia-webui/dist/
            '';

            meta = {
              description = "Local AI protocol gateway";
              homepage = "https://github.com/Stravia-AI/StraviaPlatform";
              license = lib.licenses.agpl3Only;
              mainProgram = "stravia-server";
              platforms = systems;
            };
          };
        in
        {
          default = server;
          inherit server webui;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.server}/bin/stravia-server";
          meta.description = "Run the Stravia server";
        };
      });

      checks = forAllSystems (
        system:
        let
          nixos = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              {
                boot.isContainer = true;
                system.stateVersion = "25.11";
                services.stravia = {
                  enable = true;
                  host = "0.0.0.0";
                  port = 24567;
                  openFirewall = true;
                  environmentFile = "/run/secrets/stravia.env";
                };
              }
            ];
          };
          disabledNixos = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              {
                boot.isContainer = true;
                system.stateVersion = "25.11";
              }
            ];
          };
          service = nixos.config.systemd.services.stravia;
          moduleCheck =
            assert !(disabledNixos.config.systemd.services ? stravia);
            assert
              builtins.match ".* --host 0.0.0.0 --port 24567 --data-dir /var/lib/stravia" service.serviceConfig.ExecStart
              != null;
            assert service.serviceConfig.DynamicUser;
            assert service.serviceConfig.StateDirectory == "stravia";
            assert service.serviceConfig.EnvironmentFile == "/run/secrets/stravia.env";
            assert builtins.elem 24567 nixos.config.networking.firewall.allowedTCPPorts;
            nixpkgs.legacyPackages.${system}.runCommand "stravia-nixos-module-check" { } "touch $out";
        in
        {
          default = self.packages.${system}.server;
          nixos-module = moduleCheck;
        }
      );
    };
}
