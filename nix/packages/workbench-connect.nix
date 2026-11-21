# workbench-connect, which people run on their own computer to reach their
# workspaces over SSH: a static binary for each Linux they may have, apart
# from workbench's crates and the host's dependencies.
{ pkgs, ... }:

let
  inherit (pkgs) lib;
  src = lib.fileset.toSource {
    root = ../../connect;
    fileset = lib.fileset.unions [
      ../../connect/Cargo.toml
      ../../connect/Cargo.lock
      ../../connect/rustfmt.toml
      ../../connect/src
    ];
  };
  # For a computer of `on`, built here.
  build =
    on:
    on.pkgsStatic.rustPlatform.buildRustPackage {
      pname = "workbench-connect";
      version = "0.2.0";
      inherit src;
      cargoLock.lockFile = ../../connect/Cargo.lock;
      # checks.connect runs the tests.
      doCheck = false;
      meta = {
        description = "Reaches a person's workbench workspaces over SSH, from their own computer";
        license = lib.licenses.mit;
        mainProgram = "workbench-connect";
        platforms = lib.platforms.linux;
      };
    };
  x86_64-linux = build (if pkgs.stdenv.hostPlatform.isx86_64 then pkgs else pkgs.pkgsCross.gnu64);
  aarch64-linux = build (
    if pkgs.stdenv.hostPlatform.isAarch64 then pkgs else pkgs.pkgsCross.aarch64-multiplatform
  );
in
x86_64-linux.overrideAttrs (old: {
  passthru = (old.passthru or { }) // {
    inherit x86_64-linux aarch64-linux;
  };
})
