{ inputs, ... }:
{
  imports = [
    inputs.vmkit.nixosModules.default
    ./workspaces.nix
    ./web.nix
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
        services.workbench = {
          package = lib.mkDefault t3code;
          herdr.package = lib.mkDefault agents.herdr;
          herdrCore.package = lib.mkDefault agents.herdr-core;
          vmkitSource = inputs.vmkit;
        };
      }
    )
  ];
}
