# `services.workbench.node`: a machine that runs workspaces' VMs and keeps
# their disks. The front gives it the rest at every start: a declared
# workspace's secrets and host key, and a record for one made at run
# time. `workbench-node`, the node's API, answers the front only.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib) mkOption types;

  fleet = config.services.workbench.fleet;
  node = config.services.workbench.node;
  vmkit = config.services.vmkit;
  wb = import ./lib.nix { inherit config lib pkgs; };
  inherit (wb) exe tapOf userOf;

  poolName = "workbench";
  # Pools that yield to workspaces, and what they keep.
  behind = lib.filterAttrs (_: p: p.rank == "yielding") vmkit.memory.pools;

  # This node in the fleet; an assertion says when it isn't one.
  entry =
    fleet.nodes.${node.name} or {
      subnet = "0.0.0";
      wireguard.address = "0.0.0.0";
    };
  inherit (entry) subnet;
  # Its declared workspaces.
  workspaces = lib.filterAttrs (_: ws: ws.node == node.name) fleet.workspaces;
  # The web UI's group may use the VMs' sockets, where it runs.
  socketGroup = if wb.bothRoles then "workbench" else "";
  configDirs = [
    "/etc/workbench"
    "${node.stateDir}/configs"
  ];
  withConfigs = lib.concatMapStringsSep " " (d: "--config-dir=${d}") configDirs;

  configFile =
    name: ws:
    pkgs.writeText "workbench-${name}.json" (
      builtins.toJSON (
        wb.workspaceConfig {
          inherit subnet socketGroup;
          inherit (node) stateDir;
        } name ws
      )
    );

  # A node offers nothing itself: of the limits, it reads the boot memory
  # only. On a machine with both roles they are the front's, which today's
  # web UI reads from the template.
  limits =
    if wb.bothRoles && config.services.workbench.front.selfService.enable then
      wb.frontLimits
    else
      {
        os = [
          "ubuntu"
          "nixos"
        ];
        bootMemMiB = fleet.memory.bootMiB;
        maxMemoryGiB = fleet.memory.maxGiB;
        maxDiskGiB = 0;
        quota = 0;
      };
  template = builtins.toJSON (
    wb.templateFor {
      inherit subnet socketGroup limits;
      inherit (node) stateDir;
    }
  );

  settings = builtins.toJSON (
    {
      inherit (node) name stateDir;
      socket = if wb.bothRoles then wb.nodeSocket else "";
      socketGroup = if wb.bothRoles then "workbench" else "";
      frontAddress = fleet.front.wireguard.address;
      front =
        if wb.bothRoles then
          wb.frontSocket
        else
          "${fleet.front.wireguard.address}:${toString wb.frontPort}";
      inherit (wb) template;
      inherit configDirs;
      inherit (node.memory) reservePercent;
      reserveMinGiB = vmkit.memory.reserveMinGiB;
      pools = [
        {
          name = poolName;
          status = vmkit.memory.pools.${poolName}.statusFile;
        }
      ]
      ++ lib.mapAttrsToList (name: p: {
        inherit name;
        status = p.statusFile;
      }) (lib.filterAttrs (name: _: name != poolName) vmkit.memory.pools);
    }
    # Its API on WireGuard, unless the front is on this machine.
    // lib.optionalAttrs (!wb.bothRoles) {
      listen = "${entry.wireguard.address}:${toString wb.nodePort}";
    }
  );

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
  ) fleet.network.nameservers;

  forwardRules =
    lib.concatStrings (
      lib.mapAttrsToList (
        name: ws:
        "iptables -A wb-fwd -i ${tapOf ws} ! -s ${ws.ip} -j DROP\n"
        + lib.concatMapStrings (allowRule "-i ${tapOf ws} ") ws.allowedEndpoints
      ) workspaces
    )
    + lib.concatMapStrings (allowRule "") (fleet.allowedEndpoints ++ nameserverEndpoints);

in
{
  options.services.workbench.node = {
    enable = lib.mkEnableOption "a node of the fleet: it runs workspaces' VMs and keeps their disks";
    name = mkOption {
      type = types.str;
      example = "a";
      description = "This node's name, one of `services.workbench.fleet.nodes`.";
    };
    stateDir = mkOption {
      type = types.str;
      default = "/var/lib/workbench";
      description = ''
        Where its workspaces' disks live, with the configs of those made
        at run time in `configs/`. A workspace removed from the
        configuration keeps its disk here until `workbench destroy`.
      '';
    };
    network.externalInterface = mkOption {
      type = types.str;
      description = "Interface workspaces are NATed out of.";
    };
    memory.reservePercent = mkOption {
      type = types.ints.between 0 99;
      default = vmkit.memory.reservePercent;
      defaultText = lib.literalExpression "config.services.vmkit.memory.reservePercent";
      description = "Share of the node's memory, at least 2 GiB, no workspace grows into.";
    };
    wireguard.privateKeyFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/run/secrets/wireguard";
      description = ''
        The node's WireGuard private key, kept out of the Nix store and
        readable by `systemd-network`, such as a sops secret. Needed unless
        the front is on this machine.
      '';
    };
  };

  config = lib.mkIf node.enable {
    assertions = [
      {
        assertion = fleet.nodes ? ${node.name};
        message = "services.workbench.node.name: ${node.name} isn't one of services.workbench.fleet.nodes.";
      }
      {
        # The front may give any node workspaces made at run time.
        assertion = lib.elem config.networking.firewall.checkReversePath [
          true
          "strict"
        ];
        message = "services.workbench.node: workspaces made at run time rely on the firewall's strict reverse path check to keep a guest to its own address.";
      }
    ];

    environment.etc = {
      "workbench-node.json".text = settings;
      "workbench-self-service.json".text = template;
    }
    // lib.mapAttrs' (
      name: ws: lib.nameValuePair "workbench/${name}.json" { source = configFile name ws; }
    ) workspaces;

    users.users = lib.mapAttrs' (
      name: _:
      lib.nameValuePair (userOf name) {
        isSystemUser = true;
        group = "kvm";
        description = "cloud-hypervisor of workspace ${name}";
      }
    ) workspaces;

    # The VMs' users of workspaces made at run time are userdb records.
    services.userdbd = {
      enable = true;
      # It warns for systemd-homed's sake, which workspaces don't use.
      silenceHighSystemUsers = lib.mkDefault true;
    };

    systemd = {
      tmpfiles.rules = [
        # The web UI's group lists it, for the disks of removed workspaces.
        "d ${node.stateDir} ${if wb.bothRoles then "0751 root workbench" else "0711 root root"} -"
        "d ${node.stateDir}/configs 0755 root root -"
        # The VMs' users reach their own seed, by its name.
        "d ${node.stateDir}/.seeds 0711 root root -"
      ];

      services = {
        "workbench@" = {
          description = "Workspace VM %i";
          after = [
            "systemd-networkd.service"
          ]
          # Its start asks the front, which has the declared workspaces'
          # records once it is set up.
          ++ lib.optionals wb.bothRoles [
            "workbench-front.service"
            "workbench-front-setup.service"
          ];
          restartIfChanged = false;
          # `workbench stop` leaves this marker so a stopped workspace stays
          # stopped across reboots; `workbench start` removes it.
          unitConfig = {
            ConditionPathExists = "!${node.stateDir}/.stopped/%i";
            # While the front can't be reached or refuses, it keeps trying.
            StartLimitIntervalSec = 0;
          };
          serviceConfig = {
            ExecStart = "${exe} run %i";
            ExecStartPost = "${exe} ${withConfigs} share-sockets %i";
            # A first start makes the disk from the image, which may take
            # minutes; one cut short starts it over.
            TimeoutStartSec = "15min";
            ExecStop = "${exe} ${withConfigs} shutdown %i";
            RuntimeDirectory = "workbench/%i";
            RuntimeDirectoryMode = "0711";
            PrivateMounts = true;
            Restart = "always";
            RestartSec = 10;
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
              [ exe ]
              ++ map (d: "--config-dir=${d}") configDirs
              ++ [
                "memory"
                "--mem-reserve-percent=${toString node.memory.reservePercent}"
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

        # The node's API, which the front asks, and the shared consoles.
        workbench-node = {
          description = "The node's API, which the front asks";
          wantedBy = [ "multi-user.target" ];
          after = [
            "systemd-networkd.service"
            "systemd-userdbd.socket"
          ];
          restartTriggers = [ settings ];
          path = [ config.systemd.package ];
          serviceConfig = {
            ExecStart = "${exe} node serve";
            RuntimeDirectory = "workbench-node";
            # Its socket is the web UI's too, on a machine with both roles.
            RuntimeDirectoryMode = "0755";
            # Its WireGuard address may come a moment after it starts.
            Restart = "always";
            RestartSec = 5;
          };
        };

        workbench-node-setup = {
          description = "Users, taps and starts of workspaces made at run time";
          wantedBy = [ "multi-user.target" ];
          after = [
            "systemd-networkd.service"
            "systemd-userdbd.socket"
          ];
          # A new template makes every config again.
          restartTriggers = [ template ];
          path = [ config.systemd.package ];
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
            TimeoutStartSec = "10min";
            ExecStart = "${exe} node setup";
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
      ) workspaces;

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
        ) workspaces;
        networks = lib.mapAttrs' (
          _: ws:
          lib.nameValuePair "40-${tapOf ws}" {
            matchConfig.Name = tapOf ws;
            address = [ "${subnet}.1/32" ];
            routes = [ { Destination = "${ws.ip}/32"; } ];
            networkConfig = {
              ConfigureWithoutCarrier = true;
              IgnoreCarrierLoss = true;
              LinkLocalAddressing = "no";
              IPv6AcceptRA = false;
            };
            linkConfig.RequiredForOnline = "no";
          }
        ) workspaces;
      };
    };

    # Workspaces reach the internet and allowed endpoints only, and come
    # first for memory on a shared host.
    services.vmkit = {
      inherit (node.network) externalInterface;
      networks.${poolName} = {
        inherit subnet;
        interface = "wb+";
        chain = "wb";
        inherit forwardRules;
        blockIPv6 = true;
      };
      memory.pools.${poolName}.rank = "first";
    };

    networking.networkmanager.unmanaged = lib.mkIf config.networking.networkmanager.enable [
      "interface-name:wb*"
    ];
  };
}
