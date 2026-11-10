{
  rustPlatform,
  fetchurl,
  pkg-config,
  openssl,
  zlib,
}:
rustPlatform.buildRustPackage {
  pname = "patch-crate";
  version = "0.1.13";
  src = fetchurl {
    name = "patch-crate-0.1.13.tar.gz";
    url = "https://static.crates.io/crates/patch-crate/patch-crate-0.1.13.crate";
    sha256 = "e203d4af5d6fa604bc379356bd8d6ff6bbd5b0a97d59293cc1009020cd900c6b";
  };
  cargoHash = "sha256-H6c+MfXd2RfRmkYIVBmYJDT4RvBlNLT9twEkMhHjA3U=";
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [
    openssl
    zlib
  ];
  doCheck = false;
}
