{
  pkgs,
  flake,
  inputs,
  ...
}:
let
  inherit (pkgs) lib;
  template = ../modules/nixos/guest/template;
  fill =
    file:
    builtins.replaceStrings
      [ "@name@" "@owner@" "@release@" "@stateVersion@" ]
      [ "alice-dev" "alice" lib.trivial.release lib.trivial.release ]
      (builtins.readFile "${template}/${file}");

  # The flake a new NixOS workspace writes, and the system it builds.
  generated = import (builtins.toFile "flake.nix" (fill "flake.nix"));
  system = inputs.nixpkgs.lib.nixosSystem {
    modules = [
      flake.nixosModules.guest
      (import (builtins.toFile "configuration.nix" (fill "configuration.nix")))
    ];
  };
  inherit (system) config;
  units = config.systemd.services;
in
assert generated.inputs.workbench.url == "github:OSSystems/workbench-nix";
assert generated.inputs.nixpkgs.url == "github:NixOS/nixpkgs/nixos-${lib.trivial.release}";
assert config.users.users.alice.isNormalUser;
assert lib.elem "wheel" config.users.users.alice.extraGroups;
assert config.users.users.alice.linger;
assert config.fileSystems."/nix/store" or null == null;
assert config.fileSystems."/run/workspace/host/nix/store".fsType == "virtiofs";
assert
  config.fileSystems."/run/workspace/config".options == [
    "ro"
    "nodev"
    "nosuid"
  ];
assert lib.all (u: units ? ${u}) [
  "workspace-network"
  "workspace-store"
  "workspace-init"
  "workspace-agent"
];
assert !(units ? workspace-app);
assert lib.elem "/run/workspace/config/authorized_keys/%u"
  config.services.openssh.authorizedKeysFiles;
assert
  config.services.openssh.settings.TrustedUserCAKeys == "/run/workspace/config/ssh/user-ca.pub";
assert
  config.services.openssh.settings.AuthorizedPrincipalsFile
  == "/run/workspace/config/ssh/principals/%u";
assert lib.hasInfix "HostCertificate /run/workspace/secrets/ssh-host/"
  config.services.openssh.extraConfig;
pkgs.writeText "guest" (builtins.unsafeDiscardStringContext config.system.build.toplevel.drvPath)
