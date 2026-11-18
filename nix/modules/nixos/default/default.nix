{ inputs, ... }:
{
  imports = [
    inputs.vmkit.nixosModules.default
    ./workspaces.nix
    (
      { lib, pkgs, ... }:
      {
        services.workbench = {
          package = lib.mkDefault inputs.t3code-nix.packages.${pkgs.stdenv.hostPlatform.system}.t3code;
          vmkitSource = inputs.vmkit;
        };
      }
    )
  ];
}
