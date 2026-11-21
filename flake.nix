{
  description = "Persistent per-person AI workspaces: long-lived cloud-hypervisor VMs on NixOS";

  # llm-agents.nix's tools come built from its cache; built here, herdr's
  # Zig dependencies can't be fetched.
  nixConfig = {
    extra-substituters = [ "https://cache.numtide.com" ];
    extra-trusted-public-keys = [ "niks3.numtide.com-1:DTx8wZduET09hRmMtKdQDxNNthLQETkc/yaX7M4qK0g=" ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    vmkit = {
      url = "github:OSSystems/vmkit-nix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.blueprint.follows = "blueprint";
    };
    # The AI tools the apps run. Its own nixpkgs, so its packages are the
    # ones cache.numtide.com has built.
    llm-agents.url = "github:numtide/llm-agents.nix";
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
