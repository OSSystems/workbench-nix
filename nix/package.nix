{
  pkgs,
  inputs ? null,
  vmkit ? inputs.vmkit,
  ...
}:

pkgs.rustPlatform.buildRustPackage {
  pname = "workbench";
  version = "0.2.0";
  src = pkgs.lib.fileset.toSource {
    root = ../.;
    fileset = pkgs.lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };
  # The vmkit crate is the one the flake locks, the same revision as its
  # NixOS module: Cargo.toml takes it from ./vmkit, and its dependencies are
  # in Cargo.lock.
  postPatch = ''
    cp -r --no-preserve=mode ${vmkit} vmkit
  '';
  cargoLock.lockFile = ../Cargo.lock;
  # checks.rust runs the tests, with rustfmt and clippy.
  doCheck = false;
  meta = {
    description = "Persistent per-person workspaces: long-lived cloud-hypervisor VMs on NixOS";
    license = pkgs.lib.licenses.mit;
    mainProgram = "workbench";
  };
}
