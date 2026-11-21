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
      ../../web
    ];
  };
  nativeBuildInputs = old.nativeBuildInputs ++ [
    pkgs.clippy
    pkgs.rustfmt
    # The tests sign SSH keys as the host does.
    pkgs.openssh
  ];
  doCheck = true;
  # The web UI builds twice: its server, and its WebAssembly.
  checkPhase = ''
    runHook preCheck
    cargo fmt --all --check
    cargo clippy --offline --workspace --all-targets -- -D warnings
    cargo clippy --offline -p workbench-web --features ssr --all-targets -- -D warnings
    cargo clippy --offline -p workbench-web --lib --target wasm32-unknown-unknown --features hydrate -- -D warnings
    cargo test --offline --workspace
    cargo test --offline -p workbench-web --features ssr
    runHook postCheck
  '';
  installPhase = "touch $out";
})
