# workbench-connect's rustfmt, clippy and tests: a crate of its own.
{ pkgs, ... }:

pkgs.rustPlatform.buildRustPackage {
  pname = "workbench-connect-tests";
  version = "0.2.0";
  src = pkgs.lib.fileset.toSource {
    root = ../../connect;
    fileset = pkgs.lib.fileset.unions [
      ../../connect/Cargo.toml
      ../../connect/Cargo.lock
      ../../connect/rustfmt.toml
      ../../connect/src
    ];
  };
  cargoLock.lockFile = ../../connect/Cargo.lock;
  nativeBuildInputs = [
    pkgs.clippy
    pkgs.rustfmt
  ];
  doCheck = true;
  checkPhase = ''
    runHook preCheck
    cargo fmt --check
    cargo clippy --offline --all-targets -- -D warnings
    cargo test --offline
    runHook postCheck
  '';
  installPhase = "touch $out";
}
