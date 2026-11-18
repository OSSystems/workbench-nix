{
  description = "Persistent per-person AI workspaces: long-lived cloud-hypervisor VMs on NixOS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    vmkit = {
      url = "github:OSSystems/vmkit-nix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.blueprint.follows = "blueprint";
    };
    t3code-nix.url = "github:OSSystems/t3code-nix";
    blueprint = {
      url = "github:numtide/blueprint";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs:
    inputs.blueprint {
      inherit inputs;
      prefix = "nix/";
      systems = [ "x86_64-linux" ];
    };
}
