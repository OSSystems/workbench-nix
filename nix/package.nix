{
  pkgs,
  inputs ? null,
  vmkit ? inputs.vmkit,
  ...
}:

pkgs.buildGoModule {
  pname = "workbench";
  version = "0.1.0";
  src = pkgs.lib.fileset.toSource {
    root = ../.;
    fileset = pkgs.lib.fileset.unions [
      ../go.mod
      ../cmd
    ];
  };
  # The Go packages come from the vmkit the flake locks, the same revision
  # as its NixOS module.
  postPatch = ''
    cp -r --no-preserve=mode ${vmkit} vmkit
    go mod edit -replace github.com/OSSystems/vmkit-nix=./vmkit
  '';
  subPackages = [ "cmd/workbench" ];
  vendorHash = "sha256-16drYKxO+GWFsyTPdpF7ZaSlQZgdrYgN4PXobhPV8oQ=";
  env.CGO_ENABLED = 0;
  meta = {
    description = "Persistent per-person AI workspaces: long-lived cloud-hypervisor VMs on NixOS";
    license = pkgs.lib.licenses.mit;
    mainProgram = "workbench";
  };
}
