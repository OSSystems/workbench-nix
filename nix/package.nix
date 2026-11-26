{
  pkgs,
  inputs ? null,
  vmkit ? inputs.vmkit,
  ...
}:

let
  src = pkgs.lib.fileset.toSource {
    root = ../.;
    fileset = pkgs.lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../model
      # A member of the workspace, though not built here.
      ../web
    ];
  };
  # The build a front and its nodes must share, `Workbench-Version`: the
  # source's and vmkit's, so it changes with the code, and only with it.
  # workbench-web has it too, built from the same source.
  build = builtins.substring 0 32 (
    builtins.hashString "sha256" (builtins.unsafeDiscardStringContext "${src} ${vmkit}")
  );
in
pkgs.rustPlatform.buildRustPackage {
  pname = "workbench";
  version = "0.2.0";
  inherit src;
  # The vmkit crate is the one the flake locks, the same revision as its
  # NixOS module: Cargo.toml takes it from ./vmkit, and its dependencies are
  # in Cargo.lock.
  postPatch = ''
    cp -r --no-preserve=mode ${vmkit} vmkit
  '';
  cargoLock.lockFile = ../Cargo.lock;
  env.WORKBENCH_BUILD = build;
  # checks.rust runs the tests, with rustfmt and clippy.
  doCheck = false;
  passthru = { inherit build; };
  meta = {
    description = "Persistent per-person workspaces: long-lived cloud-hypervisor VMs on NixOS";
    license = pkgs.lib.licenses.mit;
    mainProgram = "workbench";
  };
}
