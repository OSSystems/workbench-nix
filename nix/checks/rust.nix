{ pkgs, flake, ... }:

flake.packages.${pkgs.stdenv.hostPlatform.system}.default.overrideAttrs (old: {
  pname = "workbench-rust-tests";
  src = pkgs.lib.fileset.toSource {
    root = ../..;
    fileset = pkgs.lib.fileset.unions [
      ../../Cargo.toml
      ../../Cargo.lock
      ../../rustfmt.toml
      ../../src
      ../../model
    ];
  };
  nativeBuildInputs = old.nativeBuildInputs ++ [
    pkgs.clippy
    pkgs.rustfmt
  ];
  doCheck = true;
  cargoTestFlags = [ "--workspace" ];
  preCheck = ''
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --offline -- -D warnings
  '';
  installPhase = "touch $out";
})
