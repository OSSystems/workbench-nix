# The NixOS of this workspace. Change it, then run
#
#   sudo nixos-rebuild switch
#
# Keep workbench.nixosModules.guest in flake.nix: it boots this system as a
# workspace, with its disk, network, the host's shares and the app.
{ pkgs, ... }:
{
  nixpkgs.hostPlatform = "x86_64-linux";

  users.users."@owner@" = {
    isNormalUser = true;
    extraGroups = [ "wheel" ];
  };

  environment.systemPackages = with pkgs; [
    git
    vim
  ];

  system.stateVersion = "@stateVersion@";
}
