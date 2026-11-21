{ inputs, ... }:
{
  imports = [
    inputs.vmkit.nixosModules.default
    ./workspaces.nix
    ./web.nix
    (
      { lib, pkgs, ... }:
      let
        # The AI tools, from llm-agents.nix.
        agents = inputs.llm-agents.packages.${pkgs.stdenv.hostPlatform.system};
      in
      {
        services.workbench = {
          package = lib.mkDefault agents.t3code;
          herdr.package = lib.mkDefault agents.herdr;
          herdrCore.package = lib.mkDefault agents.herdr-core;
          vmkitSource = inputs.vmkit;
        };
      }
    )
  ];
}
