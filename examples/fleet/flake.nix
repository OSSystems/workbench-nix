# A fleet of three machines, each a nixosConfiguration of its own: the
# front, and nodes `a` and `b`. Each imports the module, the one fleet and
# its own role; docs/fleet.md walks through it.
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    workbench.url = "github:OSSystems/workbench-nix";
  };

  outputs =
    { nixpkgs, workbench, ... }:
    let
      machine =
        name:
        nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            workbench.nixosModules.default
            # The same on every machine.
            ./fleet.nix
            # What is this machine's: its role.
            ./${name}.nix
            # And its hardware, as nixos-generate-config writes it.
            ./hardware/${name}.nix
          ];
        };
    in
    {
      nixosConfigurations = {
        front = machine "front";
        a = machine "node-a";
        b = machine "node-b";
      };
    };
}
