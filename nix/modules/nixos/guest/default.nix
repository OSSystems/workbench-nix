# What makes a NixOS system a workspace. The host shares the closure of
# its packages and apps, the workspace's settings and its secrets over
# virtiofs; at every boot this module mounts them, copies the closure into
# the guest's own store, and sets the network, the owner, SSH and the agent
# up from them. The agent runs the apps. Everything else is the owner's: it is imported by the system the
# host builds for a new disk, and by the flake the owner rebuilds after.
{
  config,
  lib,
  pkgs,
  modulesPath,
  ...
}:

let
  configDir = "/run/workspace/config";
  secretsDir = "/run/workspace/secrets";
  hostStore = "/run/workspace/host/nix/store";
  workspaceJSON = "${configDir}/workspace.json";
  # Who the system's configuration says lingers, or not: the owner's choice.
  lingerJSON = pkgs.writeText "workspace-linger.json" (
    builtins.toJSON (
      lib.mapAttrs (_: u: u.linger) (lib.filterAttrs (_: u: u.linger != null) config.users.users)
    )
  );
  app = "${configDir}/app";
  jq = lib.getExe pkgs.jq;

  share = tag: {
    device = tag;
    fsType = "virtiofs";
    options = [
      "ro"
      "nodev"
      "nosuid"
    ];
  };

  template = pkgs.runCommand "workspace-nixos-template" { } ''
    mkdir $out
    substitute ${./template/flake.nix} $out/flake.nix \
      --replace-fail @release@ ${config.system.nixos.release}
    substitute ${./template/configuration.nix} $out/configuration.nix \
      --replace-fail @stateVersion@ ${config.system.stateVersion}
  '';

  # Copies store paths from the host's share into /nix/store, which NixOS
  # mounts read-only; the remount stays in this unit's mount namespace.
  copyPaths = pkgs.writeShellScript "workspace-copy-paths" ''
    set -eu
    if ${pkgs.util-linux}/bin/mountpoint -q /nix/store; then
      ${pkgs.util-linux}/bin/mount -o remount,bind,rw /nix/store
    fi
    for path in "$@"; do
      echo "copying $path"
      rm -rf "$path"
      cp -a "${hostStore}/''${path#/nix/store/}" "$path"
    done
  '';

in
{
  imports = [ (modulesPath + "/profiles/qemu-guest.nix") ];

  boot = {
    loader = {
      systemd-boot.enable = lib.mkDefault true;
      efi.canTouchEfiVariables = lib.mkDefault false;
      timeout = lib.mkDefault 1;
    };
    growPartition = true;
    kernelParams = [ "console=ttyS0" ];
    kernelModules = [
      "virtiofs"
      "virtio_mem"
    ];
  };

  fileSystems = {
    "/" = {
      device = "/dev/disk/by-label/nixos";
      fsType = "ext4";
      autoResize = true;
    };
    "/boot" = {
      device = "/dev/disk/by-label/ESP";
      fsType = "vfat";
      options = [
        "fmask=0077"
        "dmask=0077"
      ];
    };
    ${configDir} = share "config";
    ${secretsDir} = share "secrets";
    ${hostStore} = share "store";
  };

  networking = {
    hostName = lib.mkDefault "";
    useDHCP = false;
    useNetworkd = true;
    # The host decides what reaches a workspace and what it reaches.
    firewall.enable = lib.mkDefault false;
  };

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];
  security.sudo.wheelNeedsPassword = lib.mkDefault false;
  documentation.nixos.enable = lib.mkDefault false;

  services.openssh = {
    enable = true;
    authorizedKeysFiles = [ "${configDir}/authorized_keys/%u" ];
    settings = {
      PasswordAuthentication = lib.mkDefault false;
      KbdInteractiveAuthentication = lib.mkDefault false;
    };
  };

  environment.extraInit = ''
    PATH="${app}/sw/bin:$PATH"
  '';

  systemd.services = {
    workspace-network = {
      description = "Workspace address, route, DNS and hostname";
      wantedBy = [ "systemd-networkd.service" ];
      before = [ "systemd-networkd.service" ];
      unitConfig = {
        DefaultDependencies = false;
        RequiresMountsFor = [ configDir ];
      };
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        mkdir -p /run/systemd/network
        {
          printf '[Match]\nType=ether\nKind=!*\n\n[Network]\nAddress=%s/32\n' "$(${jq} -r .ip ${workspaceJSON})"
          printf 'LinkLocalAddressing=no\nIPv6AcceptRA=no\n'
          ${jq} -r '.nameservers[] | "DNS=" + .' ${workspaceJSON}
          printf '\n[Route]\nGateway=%s\nGatewayOnLink=yes\n' "$(${jq} -r .gateway ${workspaceJSON})"
        } > /run/systemd/network/10-workspace.network
        ${jq} -j .name ${workspaceJSON} > /proc/sys/kernel/hostname
      '';
    };

    # The host's closure is copied, not mounted at /nix/store: the store is
    # the owner's, and a path the next closure drops must not stay
    # registered without its files.
    workspace-store = {
      description = "Copy the app's closure into the store";
      unitConfig.RequiresMountsFor = [
        configDir
        hostStore
      ];
      path = [ config.nix.package ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        invalid=$(xargs nix-store --check-validity --print-invalid < ${configDir}/store-paths)
        if [ -n "$invalid" ]; then
          ${pkgs.util-linux}/bin/unshare --mount ${copyPaths} $invalid
        fi
        nix-store --load-db < ${configDir}/registration
        ln -sfn "$(readlink ${app})" /nix/var/nix/gcroots/workspace-app
        rm -f /nix/var/nix/gcroots/workspace-agent
        if [ -e ${configDir}/agent ]; then
          ln -sfn "$(readlink ${configDir}/agent)" /nix/var/nix/gcroots/workspace-agent
        fi
        rm -f /nix/var/nix/gcroots/workspace-app-*
        for bundle in ${configDir}/apps/*; do
          if [ -e "$bundle" ]; then
            ln -sfn "$(readlink "$bundle")" "/nix/var/nix/gcroots/workspace-app-''${bundle##*/}"
          fi
        done
      '';
    };

    # The owner's account and, on a new disk, the flake they rebuild from.
    # The owner lingers, so their user manager runs from boot for the
    # apps, unless the owner's configuration says otherwise.
    workspace-init = {
      description = "Workspace owner and NixOS configuration";
      wantedBy = [ "multi-user.target" ];
      after = [ "systemd-logind.service" ];
      wants = [ "systemd-logind.service" ];
      unitConfig.RequiresMountsFor = [ configDir ];
      path = [
        pkgs.shadow
        config.systemd.package
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        owner=$(${jq} -r .owner ${workspaceJSON})
        name=$(${jq} -r .name ${workspaceJSON})
        if ! id "$owner" >/dev/null 2>&1; then
          useradd --create-home --groups wheel --shell /run/current-system/sw/bin/bash "$owner"
        fi
        if ${jq} -e --arg o "$owner" 'has($o) | not' ${lingerJSON} >/dev/null; then
          loginctl enable-linger "$owner"
        fi
        if [ ! -e /etc/nixos/flake.nix ]; then
          mkdir -p /etc/nixos
          sed "s/@name@/$name/g" ${template}/flake.nix > /etc/nixos/flake.nix
          sed "s/@owner@/$owner/g" ${template}/configuration.nix > /etc/nixos/configuration.nix
        fi
      '';
    };

    # It runs each app as a unit of its own, workspace-app-<id>.
    workspace-agent = {
      description = "Workspace agent: memory reports, apps and host requests";
      wantedBy = [ "multi-user.target" ];
      requires = [ "workspace-store.service" ];
      after = [ "workspace-store.service" ];
      serviceConfig = {
        ExecStart = "${app}/bin/workspace-agent";
        Restart = "always";
        RestartSec = 2;
      };
    };
  };
}
