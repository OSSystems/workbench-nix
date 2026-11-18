{
  description = "NixOS of workspace @name@";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-@release@";
    workbench = {
      url = "github:OSSystems/workbench-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, workbench, ... }:
    {
      nixosConfigurations."@name@" = nixpkgs.lib.nixosSystem {
        modules = [
          workbench.nixosModules.guest
          ./configuration.nix
        ];
      };
    };
}
