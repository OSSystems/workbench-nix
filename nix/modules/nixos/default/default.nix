{ inputs, ... }:
let
  # What a host on its own had, before the fleet, and where each went: an
  # old option fails the evaluation and names its new one.
  moved = {
    enable = "services.workbench.front.enable and services.workbench.node.enable";
    domain = "services.workbench.fleet.domain";
    publicScheme = "services.workbench.fleet.publicScheme";
    package = "services.workbench.fleet.package";
    vmkitSource = "services.workbench.fleet.vmkitSource";
    apps = "services.workbench.fleet.apps";
    herdr = "services.workbench.fleet.herdr";
    herdrCore = "services.workbench.fleet.herdrCore";
    extraPackages = "services.workbench.fleet.extraPackages";
    environment = "services.workbench.fleet.environment";
    environmentFile = "services.workbench.fleet.environmentFile, a path on the front";
    allowedEndpoints = "services.workbench.fleet.allowedEndpoints";
    image = "services.workbench.fleet.image";
    imageFormat = "services.workbench.fleet.imageFormat";
    nixos = "services.workbench.fleet.nixos";
    "memory.bootMiB" = "services.workbench.fleet.memory.bootMiB";
    "memory.maxGiB" = "services.workbench.fleet.memory.maxGiB";
    "memory.reservePercent" = "services.workbench.node.memory.reservePercent";
    "network.subnet" = "services.workbench.fleet.nodes.<node>.subnet";
    "network.externalInterface" = "services.workbench.node.network.externalInterface";
    "network.nameservers" = "services.workbench.fleet.network.nameservers";
    workspaces = "services.workbench.fleet.workspaces, each with its `node`";
    virtualHost = "services.workbench.front.virtualHost";
    web = "services.workbench.front, which always has the web UI";
    stateDir = "services.workbench.front.stateDir and services.workbench.node.stateDir";
  };
  removed =
    path: now:
    { lib, ... }:
    {
      imports = [
        (lib.mkRemovedOptionModule
          (
            [
              "services"
              "workbench"
            ]
            ++ lib.splitString "." path
          )
          ''
            It is ${now} now: workbench runs as a fleet. See the README's "From one host to a front and a node".
          ''
        )
      ];
    };
in
{
  imports = [
    inputs.vmkit.nixosModules.default
    ./fleet.nix
    ./front.nix
    ./node.nix
    ./wireguard.nix
    (
      { lib, pkgs, ... }:
      let
        system = pkgs.stdenv.hostPlatform.system;
        # The AI tools, from llm-agents.nix.
        agents = inputs.llm-agents.packages.${system};
        # T3 Code from t3code-nix, its server alone: a NixOS guest copies
        # the whole closure at every boot. With the agents it runs on its
        # PATH, as llm-agents.nix's T3 Code has them; its own wrapper has
        # codex already.
        t3 = inputs.t3code-nix.packages.${system}.t3code-server;
        t3code =
          pkgs.runCommand "t3code-${t3.version}"
            {
              inherit (t3) version;
              nativeBuildInputs = [ pkgs.makeBinaryWrapper ];
              meta = t3.meta // {
                mainProgram = "t3";
              };
            }
            ''
              makeWrapper ${lib.getExe' t3 "t3"} $out/bin/t3 --prefix PATH : ${
                lib.makeBinPath (
                  with agents;
                  [
                    claude-code
                    cursor-agent
                    grok
                    opencode
                  ]
                )
              }
            '';
      in
      {
        services.workbench.fleet = {
          package = lib.mkDefault t3code;
          herdr.package = lib.mkDefault agents.herdr;
          herdrCore.package = lib.mkDefault agents.herdr-core;
          vmkitSource = inputs.vmkit;
        };
      }
    )
  ]
  ++ builtins.attrValues (builtins.mapAttrs removed moved);
}
