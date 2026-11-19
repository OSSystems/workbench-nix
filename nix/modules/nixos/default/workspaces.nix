{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib) mkOption types;

  cfg = config.services.workbench;
  vmkit = config.services.vmkit;
  poolName = "workbench";
  # Pools that yield to workspaces, and what they keep.
  behind = lib.filterAttrs (_: p: p.rank == "yielding") vmkit.memory.pools;
  package = pkgs.callPackage ../../../package.nix { vmkit = cfg.vmkitSource; };
  exe = lib.getExe' package "workbench";

  hostAddress = "${cfg.network.subnet}.1";
  controlPort = 5001;
  agentPort = 5002;

  octets = ip: map lib.toInt (lib.splitString "." ip);
  hex2 = n: lib.toLower (lib.fixedWidthString 2 "0" (lib.toHexString n));
  tapOf = ws: "wb" + lib.concatMapStrings hex2 (octets ws.ip);
  macOf = ws: "02:00:" + lib.concatMapStringsSep ":" hex2 (octets ws.ip);
  userOf = name: "wb-${name}";
  hostFor = name: "${name}.${cfg.domain}";
  secretsDirOf =
    name: ws: if ws.secretsDir != null then ws.secretsDir else "/var/lib/workbench-secrets/${name}";

  endpointType = types.submodule {
    options = {
      address = mkOption {
        type = types.str;
        example = "10.0.5.20/32";
        description = "IPv4 address or CIDR range.";
      };
      port = mkOption {
        type = types.nullOr types.port;
        default = null;
        description = "Destination port; null allows every port.";
      };
      protocol = mkOption {
        type = types.enum [
          "tcp"
          "udp"
        ];
        default = "tcp";
      };
    };
  };

  allowRule =
    match: e:
    "iptables -A wb-fwd ${match}-d ${e.address}"
    + lib.optionalString (e.port != null) " -p ${e.protocol} --dport ${toString e.port}"
    + " -j RETURN\n";

  nameserverEndpoints = lib.concatMap (
    address:
    map
      (protocol: {
        inherit address protocol;
        port = 53;
      })
      [
        "udp"
        "tcp"
      ]
  ) cfg.network.nameservers;

  forwardRules =
    lib.concatStrings (
      lib.mapAttrsToList (
        name: ws:
        "iptables -A wb-fwd -i ${tapOf ws} ! -s ${ws.ip} -j DROP\n"
        + lib.concatMapStrings (allowRule "-i ${tapOf ws} ") ws.allowedEndpoints
      ) cfg.workspaces
    )
    + lib.concatMapStrings (allowRule "") (cfg.allowedEndpoints ++ nameserverEndpoints);

  memMiB = ws: {
    boot = if ws.memory.bootMiB != null then ws.memory.bootMiB else cfg.memory.bootMiB;
    max = (if ws.memory.maxGiB != null then ws.memory.maxGiB else cfg.memory.maxGiB) * 1024;
  };

  guestApp =
    name: ws:
    let
      sw = pkgs.buildEnv {
        name = "workbench-${name}-sw";
        paths = [ cfg.package ] ++ cfg.extraPackages ++ ws.extraPackages;
      };
      t3 = lib.getExe' cfg.package "t3";
    in
    pkgs.runCommand "workbench-${name}-app" { } ''
      mkdir -p $out/bin
      ln -s ${sw} $out/sw
      ln -s ${t3} $out/bin/t3
      cat > $out/bin/serve <<EOF
      #!${pkgs.runtimeShell}
      export PATH=${sw}/bin:\$PATH
      export T3CODE_HOME=\$HOME/.t3
      exec ${t3} serve --host 0.0.0.0 --port ${toString ws.appPort}
      EOF
      cat > $out/bin/workspace-agent <<EOF
      #!${pkgs.runtimeShell}
      exec ${exe} guest-agent --config=/run/workspace/config/workspace.json
      EOF
      # The sign-in hook: one link that signs a browser in to T3 Code.
      cat > $out/bin/sign-in-link <<EOF
      #!${pkgs.runtimeShell}
      set -eo pipefail
      export T3CODE_HOME=\$HOME/.t3
      ${t3} auth pairing create --base-url "\$WORKSPACE_URL" --label workbench --json | ${lib.getExe pkgs.jq} -er .pairUrl
      EOF
      chmod +x $out/bin/serve $out/bin/workspace-agent $out/bin/sign-in-link
    '';

  mountUnit = what: where: ''
    [Unit]
    Description=Workspace ${what} share
    DefaultDependencies=no
    After=systemd-modules-load.service
    Before=local-fs.target

    [Mount]
    What=${what}
    Where=${where}
    Type=virtiofs
    Options=ro,nodev,nosuid

    [Install]
    WantedBy=local-fs.target
  '';

  userData =
    name: ws:
    let
      home = "/home/${ws.owner}";
      shares = "nix-store.mount run-workspace-config.mount run-workspace-secrets.mount";
    in
    "#cloud-config\n"
    + builtins.toJSON {
      hostname = name;
      users = [
        (
          {
            name = ws.owner;
            shell = "/bin/bash";
            lock_passwd = true;
            ssh_authorized_keys = ws.sshAuthorizedKeys;
          }
          // lib.optionalAttrs ws.sudo { sudo = "ALL=(ALL) NOPASSWD:ALL"; }
        )
      ];
      ssh_pwauth = false;
      ssh_deletekeys = false;
      package_update = false;
      write_files = [
        {
          path = "/etc/systemd/system/nix-store.mount";
          content = mountUnit "store" "/nix/store";
        }
        {
          path = "/etc/systemd/system/run-workspace-config.mount";
          content = mountUnit "config" "/run/workspace/config";
        }
        {
          path = "/etc/systemd/system/run-workspace-secrets.mount";
          content = mountUnit "secrets" "/run/workspace/secrets";
        }
        {
          path = "/etc/modules-load.d/workbench.conf";
          content = "virtiofs\nvirtio_mem\n";
        }
        {
          path = "/etc/profile.d/workbench.sh";
          content = ''
            PATH=/run/workspace/config/app/sw/bin:$PATH
          '';
        }
        {
          path = "/etc/systemd/system/workspace-agent.service";
          content = ''
            [Unit]
            Description=Workspace agent: memory reports and host requests
            Requires=${shares}
            After=${shares}

            [Service]
            ExecStart=/run/workspace/config/app/bin/workspace-agent
            Restart=always
            RestartSec=2

            [Install]
            WantedBy=multi-user.target
          '';
        }
        {
          path = "/etc/systemd/system/workspace-app.service";
          content = ''
            [Unit]
            Description=Workspace app
            Requires=${shares}
            After=${shares} network-online.target
            Wants=network-online.target

            [Service]
            User=${ws.owner}
            WorkingDirectory=${home}
            Environment=HOME=${home}
            EnvironmentFile=/run/workspace/config/environment
            EnvironmentFile=-/run/workspace/secrets/environment
            ExecStart=/run/workspace/config/app/bin/serve
            Restart=always
            RestartSec=5
            KillMode=mixed

            [Install]
            WantedBy=multi-user.target
          '';
        }
      ];
      runcmd = [
        [
          "systemctl"
          "daemon-reload"
        ]
        [
          "systemctl"
          "enable"
          "--now"
          "nix-store.mount"
          "run-workspace-config.mount"
          "run-workspace-secrets.mount"
          "workspace-agent.service"
          "workspace-app.service"
        ]
      ];
    };

  networkConfig = ws: {
    version = 2;
    ethernets.uplink = {
      match.macaddress = macOf ws;
      addresses = [ "${ws.ip}/32" ];
      routes = [
        {
          to = "0.0.0.0/0";
          via = hostAddress;
          on-link = true;
        }
      ];
      nameservers.addresses = cfg.network.nameservers;
    };
  };

  seed =
    name: ws:
    let
      user = userData name ws;
      network = builtins.toJSON (networkConfig ws);
      meta = builtins.toJSON {
        instance-id = "${name}-${builtins.substring 0 12 (builtins.hashString "sha256" (user + network))}";
        local-hostname = name;
      };
    in
    pkgs.runCommand "workbench-${name}-seed.img"
      {
        nativeBuildInputs = [
          pkgs.dosfstools
          pkgs.mtools
        ];
      }
      ''
        cp ${pkgs.writeText "user-data" user} user-data
        cp ${pkgs.writeText "network-config" network} network-config
        cp ${pkgs.writeText "meta-data" meta} meta-data
        touch -d @0 user-data network-config meta-data
        mkfs.vfat -n CIDATA -i 12345678 -C $out 1024
        mcopy -m -i $out user-data network-config meta-data ::
      '';

  workspaceConfig =
    name: ws:
    let
      app = guestApp name ws;
      closure = pkgs.closureInfo { rootPaths = [ app ]; };
      mem = memMiB ws;
    in
    {
      inherit name;
      inherit (ws) owner ip;
      user = userOf name;
      url = "${cfg.publicScheme}://${hostFor name}";
      stateDir = "${cfg.stateDir}/${name}";
      runtimeDir = "/run/workbench/${name}";
      cloudHypervisor = lib.getExe pkgs.cloud-hypervisor;
      virtiofsd = lib.getExe pkgs.virtiofsd;
      qemuImg = lib.getExe' pkgs.qemu-utils "qemu-img";
      firmware = "${pkgs.OVMF-cloud-hypervisor.fd}/FV/CLOUDHV.fd";
      inherit (ws) os diskGiB cpus;
      bootMemMiB = mem.boot;
      maxMemMiB = mem.max;
      tap = tapOf ws;
      mac = macOf ws;
      gateway = hostAddress;
      inherit (cfg.network) nameservers;
      inherit (ws) appPort;
      sshAuthorizedKeys = map lib.strings.trim ws.sshAuthorizedKeys;
      closure = "${closure}/store-paths";
      registration = "${closure}/registration";
      app = "${app}";
      environment = cfg.environment // ws.environment;
      environmentFiles = lib.filter (f: f != null) [
        cfg.environmentFile
        ws.environmentFile
      ];
      secretsDir = secretsDirOf name ws;
      inherit controlPort agentPort;
    }
    // lib.optionalAttrs cfg.web.enable { socketGroup = "workbench"; }
    // (
      if ws.os == "ubuntu" then
        {
          inherit (cfg) image imageFormat;
          seed = seed name ws;
        }
      else
        {
          inherit (cfg.nixos) image imageFormat;
        }
    );

  # The system a new NixOS disk starts with: the guest module alone, until
  # the owner rebuilds from the flake it writes to /etc/nixos.
  nixosImage =
    let
      system = import "${pkgs.path}/nixos/lib/eval-config.nix" {
        system = null;
        modules = [
          ../guest
          {
            nixpkgs.pkgs = pkgs;
            system.stateVersion = lib.trivial.release;
          }
        ];
      };
    in
    import "${pkgs.path}/nixos/lib/make-disk-image.nix" {
      inherit pkgs lib;
      inherit (system) config;
      name = "workbench-nixos-image";
      format = "qcow2-compressed";
      partitionTableType = "efi";
      copyChannel = false;
    };

  configFile =
    name: ws: pkgs.writeText "workbench-${name}.json" (builtins.toJSON (workspaceConfig name ws));

  # What every workspace made at run time shares: `workbench self-service`
  # fills in the rest, as `workspaceConfig` would.
  ss = cfg.web.selfService;
  selfServiceTemplate =
    let
      app = guestApp "self-service" {
        extraPackages = [ ];
        appPort = 3773;
      };
      closure = pkgs.closureInfo { rootPaths = [ app ]; };
      images = {
        ubuntu = {
          inherit (cfg) image imageFormat;
          userData = userData "@name@" {
            owner = "@owner@";
            sshAuthorizedKeys = [ ];
            sudo = true;
          };
        };
        nixos = { inherit (cfg.nixos) image imageFormat; };
      };
    in
    {
      limits = {
        inherit (ss)
          os
          quota
          maxMemoryGiB
          maxDiskGiB
          ;
        bootMemMiB = cfg.memory.bootMiB;
      };
      inherit (cfg) domain stateDir;
      scheme = cfg.publicScheme;
      secretsRoot = "/var/lib/workbench-secrets";
      inherit (cfg.network) subnet nameservers;
      appPort = 3773;
      inherit controlPort agentPort;
      cpus = 0;
      cloudHypervisor = lib.getExe pkgs.cloud-hypervisor;
      virtiofsd = lib.getExe pkgs.virtiofsd;
      qemuImg = lib.getExe' pkgs.qemu-utils "qemu-img";
      firmware = "${pkgs.OVMF-cloud-hypervisor.fd}/FV/CLOUDHV.fd";
      closure = "${closure}/store-paths";
      registration = "${closure}/registration";
      app = "${app}";
      inherit (cfg) environment;
      environmentFiles = lib.optional (cfg.environmentFile != null) cfg.environmentFile;
      socketGroup = "workbench";
      images = lib.getAttrs ss.os images;
      mkfsVfat = "${pkgs.dosfstools}/bin/mkfs.vfat";
      mcopy = "${pkgs.mtools}/bin/mcopy";
      inherit (ss) environmentCommand;
      ownerCommand = if ss.ownerCommand == null then "" else ss.ownerCommand;
    };
  selfServiceUnit = description: args: {
    inherit description;
    after = [
      "systemd-networkd.service"
      "systemd-userdbd.socket"
    ];
    path = [ config.systemd.package ];
    serviceConfig = {
      Type = "oneshot";
      # The web UI waits a little longer for the outcome.
      TimeoutStartSec = "10min";
      ExecStart = lib.escapeShellArgs (
        [
          exe
          "self-service"
        ]
        ++ args
      );
    };
  };

  workspaceModule =
    { name, ... }:
    {
      options = {
        owner = mkOption {
          type = types.str;
          example = "alice";
          description = "The person the workspace is for: the guest user, named after their directory (LDAP) uid. The app runs as this user.";
        };
        os = mkOption {
          type = types.enum [
            "ubuntu"
            "nixos"
          ];
          default = "ubuntu";
          description = ''
            Guest operating system. `ubuntu` is set up by cloud-init and
            runs the app from the host's store. `nixos` is a NixOS the owner
            rebuilds from /etc/nixos, with its own store the app's closure is
            copied into; it needs `sudo`.
          '';
        };
        ip = mkOption {
          type = types.str;
          example = "10.232.0.10";
          description = "Static IPv4 address in `network.subnet`; the host proxies the workspace URL to it.";
        };
        sudo = mkOption {
          type = types.bool;
          default = true;
          description = "Passwordless sudo for the owner inside the guest.";
        };
        diskGiB = mkOption {
          type = types.ints.positive;
          default = 64;
          description = "Size of the persistent disk; growing it takes effect at the next start, it never shrinks.";
        };
        cpus = mkOption {
          type = types.ints.unsigned;
          default = 0;
          description = "vCPUs; 0 gives the VM every host CPU.";
        };
        memory = {
          bootMiB = mkOption {
            type = types.nullOr types.ints.positive;
            default = null;
            description = "Boot memory; null uses `memory.bootMiB`.";
          };
          maxGiB = mkOption {
            type = types.nullOr types.ints.positive;
            default = null;
            description = "Most memory the VM grows to; null uses `memory.maxGiB`.";
          };
        };
        sshAuthorizedKeys = mkOption {
          type = types.listOf types.str;
          default = [ ];
          description = "SSH keys of the owner in the guest. Only the host reaches the guest's SSH server.";
        };
        appPort = mkOption {
          type = types.port;
          default = 3773;
          description = "Port the app listens on in the guest.";
        };
        environment = mkOption {
          type = types.attrsOf types.str;
          default = { };
          description = "Non-secret environment for this workspace's app and the processes it starts.";
        };
        environmentFile = mkOption {
          type = types.nullOr types.path;
          default = null;
          description = "Host file of `KEY=value` lines, such as an API key, appended to the guest's secret environment at each start. Kept out of the Nix store.";
        };
        extraPackages = mkOption {
          type = types.listOf types.package;
          default = [ ];
          description = "Packages on this workspace's PATH, shared read-only from the host store with their closure.";
        };
        allowedEndpoints = mkOption {
          type = types.listOf endpointType;
          default = [ ];
          description = "Internal endpoints this workspace may reach despite the private-range block.";
        };
        secretsDir = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = ''
            Host directory, filled by sops-nix or similar, copied read-only to
            /run/workspace/secrets in the guest at each start. Its
            `environment` file, plus the environment files, is the app's secret
            environment. Null uses
            /var/lib/workbench-secrets/<workspace>.
          '';
        };
      };
    };
in
{
  options.services.workbench = {
    enable = lib.mkEnableOption "persistent per-person AI workspace VMs";

    vmkitSource = mkOption {
      type = types.path;
      internal = true;
      description = "vmkit's source, which workbench builds against; `nixosModules.default` sets it.";
    };
    package = mkOption {
      type = types.package;
      description = "T3 Code package run in every workspace.";
    };
    domain = mkOption {
      type = types.str;
      example = "ws.example.com";
      description = "Each workspace is served at `<workspace>.<domain>`; one wildcard certificate covers them all.";
    };
    publicScheme = mkOption {
      type = types.enum [
        "https"
        "http"
      ];
      default = "https";
      description = "Scheme of the workspace URLs pairing links point to.";
    };
    virtualHost = mkOption {
      type = types.attrsOf types.anything;
      default = {
        forceSSL = true;
        enableACME = true;
      };
      example = {
        forceSSL = true;
        useACMEHost = "ws.example.com";
      };
      description = "nginx virtual host settings, such as TLS, applied to every workspace.";
    };
    image = mkOption {
      type = types.path;
      default = pkgs.fetchurl {
        url = "https://cloud-images.ubuntu.com/releases/noble/release-20261017/ubuntu-24.04-server-cloudimg-amd64.img";
        hash = "sha256-BOjyRhoWpE54o3AoRHHylhbN2brxiT3yWAe3H5hbbJQ=";
      };
      defaultText = lib.literalMD "the Ubuntu 24.04 cloud image of 2026-10-17";
      description = "OS image a new Ubuntu workspace disk is made from.";
    };
    imageFormat = mkOption {
      type = types.enum [
        "qcow2"
        "raw"
      ];
      default = "qcow2";
    };
    nixos = {
      image = mkOption {
        type = types.path;
        default = "${nixosImage}/nixos.qcow2";
        defaultText = lib.literalMD "a NixOS of the host's nixpkgs with `nixosModules.guest`";
        description = ''
          Image a new NixOS workspace disk is made from. It must boot
          `nixosModules.guest`; the owner's flake replaces its system after.
        '';
      };
      imageFormat = mkOption {
        type = types.enum [
          "qcow2"
          "raw"
        ];
        default = "qcow2";
      };
    };
    stateDir = mkOption {
      type = types.str;
      default = "/var/lib/workbench";
      description = "Where workspace disks live. A workspace removed from the configuration keeps its disk here until `workbench destroy`.";
    };
    environment = mkOption {
      type = types.attrsOf types.str;
      default = { };
      description = "Non-secret environment for every workspace.";
    };
    environmentFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Host file of `KEY=value` lines for every workspace; see the per-workspace option.";
    };
    extraPackages = mkOption {
      type = types.listOf types.package;
      default = [ ];
      description = "Packages on every workspace's PATH.";
    };
    allowedEndpoints = mkOption {
      type = types.listOf endpointType;
      default = [ ];
      example = [
        {
          address = "10.0.5.20";
          port = 443;
        }
      ];
      description = "Internal endpoints (LLM proxy, git, SSO) every workspace may reach despite the private-range block.";
    };
    memory = {
      bootMiB = mkOption {
        type = types.ints.positive;
        default = 2048;
        description = "Memory a workspace boots with; it grows through virtio-mem under pressure.";
      };
      maxGiB = mkOption {
        type = types.ints.positive;
        default = 16;
        description = "Most memory a workspace grows to.";
      };
      reservePercent = mkOption {
        type = types.ints.between 0 99;
        default = vmkit.memory.reservePercent;
        defaultText = lib.literalExpression "config.services.vmkit.memory.reservePercent";
        description = "Share of host memory, at least 2 GiB, no workspace grows into.";
      };
    };
    network = {
      subnet = mkOption {
        type = types.str;
        default = "10.232.0";
        description = "First three octets of the /24 workspace addresses come from; the host is .1.";
      };
      externalInterface = mkOption {
        type = types.str;
        description = "Interface workspaces are NATed out of.";
      };
      nameservers = mkOption {
        type = types.listOf types.str;
        default = [
          "1.1.1.1"
          "8.8.8.8"
        ];
        description = "DNS servers of the guests; private ones are allowed on port 53.";
      };
    };
    workspaces = mkOption {
      type = types.attrsOf (types.submodule workspaceModule);
      default = { };
      description = "Workspaces, keyed by name; `<name>.<domain>` is the URL. Removing one stops its VM and keeps its disk.";
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        assertions =
          let
            ips = lib.mapAttrsToList (_: ws: ws.ip) cfg.workspaces;
            inSubnet =
              ip:
              let
                o = octets ip;
              in
              lib.length o == 4
              && lib.concatStringsSep "." (map toString (lib.take 3 o)) == cfg.network.subnet
              && lib.elemAt o 3 >= 2
              && lib.elemAt o 3 <= 254;
          in
          [
            {
              assertion = pkgs.stdenv.hostPlatform.system == "x86_64-linux";
              message = "services.workbench: only x86_64-linux hosts are supported.";
            }
            {
              assertion = lib.length ips == lib.length (lib.unique ips);
              message = "services.workbench.workspaces: every workspace needs a distinct ip.";
            }
          ]
          ++ lib.mapAttrsToList (name: ws: {
            assertion =
              builtins.match "[a-z]([a-z0-9-]{0,26}[a-z0-9])?" name != null
              && builtins.match "[a-z_][a-z0-9_.-]{0,31}" ws.owner != null
              && ws.owner != "root"
              && inSubnet ws.ip;
            message = "services.workbench.workspaces.${name}: the name must be a DNS label of at most 28 characters, the owner a user name other than root, and ip an address in ${cfg.network.subnet}.2-254.";
          }) cfg.workspaces
          ++ lib.mapAttrsToList (name: ws: {
            assertion = ws.os == "nixos" -> ws.sudo;
            message = "services.workbench.workspaces.${name}: a NixOS workspace needs sudo; its owner rebuilds the system.";
          }) cfg.workspaces;

        environment.systemPackages = [ package ];
        environment.etc = lib.mapAttrs' (
          name: ws: lib.nameValuePair "workbench/${name}.json" { source = configFile name ws; }
        ) cfg.workspaces;

        users.users = lib.mapAttrs' (
          name: _:
          lib.nameValuePair (userOf name) {
            isSystemUser = true;
            group = "kvm";
            description = "cloud-hypervisor of workspace ${name}";
          }
        ) cfg.workspaces;

        systemd = {
          tmpfiles.rules = [
            # The web UI's group lists it, for the disks of removed workspaces.
            "d ${cfg.stateDir} ${if cfg.web.enable then "0751 root workbench" else "0711 root root"} -"
            "d /var/lib/workbench-secrets 0700 root root -"
          ]
          ++ lib.mapAttrsToList (name: ws: "d ${secretsDirOf name ws} 0700 root root -") cfg.workspaces;

          services = {
            "workbench@" = {
              description = "Workspace VM %i";
              after = [ "systemd-networkd.service" ];
              restartIfChanged = false;
              # `workbench stop` leaves this marker so a stopped workspace stays
              # stopped across reboots; `workbench start` removes it.
              unitConfig.ConditionPathExists = "!${cfg.stateDir}/.stopped/%i";
              serviceConfig = {
                ExecStart = "${exe} run %i";
                ExecStartPost = "${exe} share-sockets %i";
                # A first start makes the disk from the image, which may take
                # minutes; one cut short starts it over.
                TimeoutStartSec = "15min";
                ExecStop = "${exe} shutdown %i";
                RuntimeDirectory = "workbench/%i";
                RuntimeDirectoryMode = "0711";
                PrivateMounts = true;
                Restart = "always";
                RestartSec = 5;
                TimeoutStopSec = 120;
                CPUAccounting = true;
                MemoryAccounting = true;
              };
            };

            workbench-memory = {
              description = "Grow and shrink workspace memory";
              wantedBy = [ "multi-user.target" ];
              serviceConfig = {
                ExecStart = lib.escapeShellArgs (
                  [
                    exe
                    "memory"
                    "--mem-reserve-percent=${toString cfg.memory.reservePercent}"
                    "--mem-reserve-min-gib=${toString vmkit.memory.reserveMinGiB}"
                    "--memory-status=${vmkit.memory.pools.${poolName}.statusFile}"
                  ]
                  ++ lib.mapAttrsToList (_: p: "--behind=${p.statusFile}=${toString p.guaranteedGiB}") behind
                );
                RuntimeDirectory = "workbench-memory";
                RuntimeDirectoryPreserve = "restart";
                Restart = "always";
                RestartSec = 5;
              };
            };
          }
          // lib.mapAttrs' (
            name: ws:
            let
              device = "sys-subsystem-net-devices-${tapOf ws}.device";
            in
            lib.nameValuePair "workbench@${name}" {
              overrideStrategy = "asDropin";
              wantedBy = [ "multi-user.target" ];
              requires = [ device ];
              after = [ device ];
              restartIfChanged = false;
            }
          ) cfg.workspaces;

          network = {
            enable = true;
            wait-online.enable = false;
            config.networkConfig = {
              ManageForeignRoutes = false;
              ManageForeignRoutingPolicyRules = false;
            };
            netdevs = lib.mapAttrs' (
              name: ws:
              lib.nameValuePair "40-${tapOf ws}" {
                netdevConfig = {
                  Kind = "tap";
                  Name = tapOf ws;
                };
                tapConfig = {
                  User = userOf name;
                  Group = "kvm";
                  VNetHeader = true;
                };
              }
            ) cfg.workspaces;
            networks = lib.mapAttrs' (
              name: ws:
              lib.nameValuePair "40-${tapOf ws}" {
                matchConfig.Name = tapOf ws;
                address = [ "${hostAddress}/32" ];
                routes = [ { Destination = "${ws.ip}/32"; } ];
                networkConfig = {
                  ConfigureWithoutCarrier = true;
                  IgnoreCarrierLoss = true;
                  LinkLocalAddressing = "no";
                  IPv6AcceptRA = false;
                };
                linkConfig.RequiredForOnline = "no";
              }
            ) cfg.workspaces;
          };
        };

        # Workspaces reach the internet and allowed endpoints only, and come
        # first for memory on a shared host.
        services.vmkit = {
          inherit (cfg.network) externalInterface;
          networks.${poolName} = {
            inherit (cfg.network) subnet;
            interface = "wb+";
            chain = "wb";
            inherit forwardRules;
            blockIPv6 = true;
          };
          memory.pools.${poolName}.rank = "first";
        };

        networking = {
          networkmanager.unmanaged = lib.mkIf config.networking.networkmanager.enable [
            "interface-name:wb*"
          ];
        };

        # With the web UI, one virtual host of its serves every workspace.
        services.nginx = {
          enable = true;
          recommendedProxySettings = true;
          virtualHosts = lib.mkIf (!cfg.web.enable) (
            lib.mapAttrs' (
              name: ws:
              lib.nameValuePair (hostFor name) (
                cfg.virtualHost
                // {
                  locations."/" = {
                    proxyPass = "http://${ws.ip}:${toString ws.appPort}";
                    proxyWebsockets = true;
                    extraConfig = ''
                      proxy_read_timeout 1d;
                      proxy_send_timeout 1d;
                      client_max_body_size 64m;
                    '';
                  };
                }
              )
            ) cfg.workspaces
          );
        };
      }
      (lib.mkIf (cfg.web.enable && ss.enable) {
        environment.etc."workbench-self-service.json".text = builtins.toJSON selfServiceTemplate;

        # The VMs' users of workspaces made at run time are userdb records.
        services.userdbd = {
          enable = true;
          # It warns for systemd-homed's sake, which workspaces don't use.
          silenceHighSystemUsers = lib.mkDefault true;
        };

        systemd.services = {
          "workbench-create@" = selfServiceUnit "Make workspace %i" [
            "create"
            "%i"
          ];
          "workbench-destroy@" = selfServiceUnit "Destroy workspace %i" [
            "destroy"
            "%i"
          ];
          workbench-self-service =
            lib.recursiveUpdate
              (selfServiceUnit "Users, taps and starts of workspaces made at run time" [ "setup" ])
              {
                wantedBy = [ "multi-user.target" ];
                serviceConfig.RemainAfterExit = true;
              };
          workbench-self-service-reconcile = selfServiceUnit "Stop the workspaces of owners who left" [
            "reconcile"
          ];
        };
        systemd.timers.workbench-self-service-reconcile = {
          wantedBy = [ "timers.target" ];
          timerConfig = {
            OnBootSec = "5min";
            OnUnitActiveSec = ss.reconcileInterval;
          };
        };
      })
    ]
  );
}
