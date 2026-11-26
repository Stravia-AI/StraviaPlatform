{
  description = "Stravia Server and NixOS service";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/8edc0c72e3a38faf5434e40d1f19431125fe4b30";
    crane.url = "github:ipetkov/crane/692f7e9ef2ece8125b466f66f2af532b3edaed0d";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/a110337465cfb76e3806b81b9db3094c3e6e770a";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    bun2nix = {
      url = "github:nix-community/bun2nix/0f2a1f0b6f42cebe3b149bf62d38754c5e0e9729";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
      bun2nix,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      packagesFor = system: nixpkgs.legacyPackages.${system};
      generatorFor = system: import ./nix/bun2nix.nix bun2nix.packages.${system}.bun2nix;
      serverFor =
        system:
        let
          pkgs = packagesFor system;
          rustToolchain =
            (rust-overlay.lib.mkRustBin { } pkgs.buildPackages).fromRustupToolchainFile
              ./rust-toolchain.toml;
        in
        pkgs.callPackage ./nix/package.nix {
          source = self.outPath;
          inherit rustToolchain;
          craneLib = crane.mkLib pkgs;
          bun2nix = generatorFor system;
          bun = pkgs.bun;
        };
      winVmFor = system: import ./nix/win-vm { pkgs = packagesFor system; };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = packagesFor system;
          server = serverFor system;
          winVm = winVmFor system;
        in
        {
          stravia-server = server;
          default = server;
        }
        // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
          windows-e2e-image = winVm.image;
          windows-e2e-runner = winVm.runner;
        }
      );

      nixosModules.default = import ./nix/nixos-module.nix { inherit self; };

      apps = forAllSystems (
        system:
        let
          pkgs = packagesFor system;
          updateBunDeps = pkgs.writeShellApplication {
            name = "stravia-update-bun-deps";
            runtimeInputs = [ (generatorFor system) ];
            text = ''
              bun2nix -l bun.lock -c ../ -o nix/bun.nix
            '';
          };
        in
        {
          update-bun-deps = {
            type = "app";
            program = pkgs.lib.getExe updateBunDeps;
            meta.description = "Regenerate Stravia's fixed Bun dependency sources";
          };
        }
        // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
          windows-e2e = {
            type = "app";
            program = pkgs.lib.getExe (winVmFor system).runner;
            meta.description = "Run Stravia Windows Desktop E2E in an ephemeral KVM overlay";
          };
        }
      );

      checks = forAllSystems (
        system:
        let
          pkgs = packagesFor system;
          generator = generatorFor system;
        in
        {
          server = self.packages.${system}.stravia-server;
          bun-lock = pkgs.runCommand "stravia-bun-lock" { nativeBuildInputs = [ generator ]; } ''
            cp ${./bun.lock} bun.lock
            bun2nix -l bun.lock -c ../ -o bun.nix
            diff -u ${./nix/bun.nix} bun.nix
            touch "$out"
          '';
          service = import ./nix/tests/service.nix {
            inherit pkgs self;
            serverPackage = self.packages.${system}.stravia-server;
          };
        }
      );

      formatter = forAllSystems (system: (packagesFor system).nixfmt);
    };
}
