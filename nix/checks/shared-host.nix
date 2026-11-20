{
  pkgs,
  flake,
  inputs,
  system,
  ...
}:
let
  inherit (pkgs) lib;
  eval =
    modules:
    (inputs.nixpkgs.lib.nixosSystem {
      inherit system;
      modules = [
        flake.nixosModules.default
        {
          boot.loader.grub.enable = false;
          fileSystems."/" = {
            device = "/dev/vda";
            fsType = "ext4";
          };
          system.stateVersion = "26.05";
          networking.hostName = "ws-host";
          security.acme = {
            acceptTerms = true;
            defaults.email = "admin@example.com";
          };
          services.workbench = {
            enable = true;
            domain = "ws.example.com";
            network.externalInterface = "eth0";
            apps.hello = {
              name = "Hello";
              port = 8080;
              serve = "exec ${pkgs.python3}/bin/python3 -m http.server \"$APP_PORT\"";
            };
            workspaces = {
              alice-main = {
                owner = "alice";
                ip = "10.232.0.10";
              };
              alice-scratch = {
                owner = "alice";
                ip = "10.232.0.11";
                sudo = false;
              };
              alice-bare = {
                owner = "alice";
                ip = "10.232.0.12";
                apps = [ ];
              };
            };
          };
        }
      ]
      ++ modules;
    }).config;
  # Another controller on the host, as vmkit sees it: a guest network and a
  # pool that yields to workspaces.
  neighbour = {
    services.vmkit = {
      networks.neighbour = {
        subnet = "10.231.0";
        interface = "nbr0";
      };
      memory.pools.neighbour = {
        rank = "yielding";
        guaranteedGiB = 8;
      };
    };
  };
  config = eval [ neighbour ];
  alone = eval [ ];
  clash = eval [
    neighbour
    { services.workbench.network.subnet = lib.mkForce "10.231.0"; }
  ];
  failedAssertions = c: map (a: a.message) (lib.filter (a: !a.assertion) c.assertions);
  memoryExec = config.systemd.services.workbench-memory.serviceConfig.ExecStart;
  unit = config.systemd.services."workbench@alice-main";
  fw = config.networking.firewall.extraCommands;
  failures = lib.filter (c: !c.ok) [
    {
      ok = failedAssertions config == [ ];
      msg = "workspaces fit next to another controller: ${toString (failedAssertions config)}";
    }
    {
      ok =
        lib.hasInfix "--behind=/run/vmkit/memory/neighbour.json=8" memoryExec
        && lib.hasInfix "--memory-status=/run/vmkit/memory/workbench.json" memoryExec;
      msg = "workspaces leave a yielding pool its guarantee and publish what they hold";
    }
    {
      ok = !lib.hasInfix "--behind" alone.systemd.services.workbench-memory.serviceConfig.ExecStart;
      msg = "workspaces alone look for no pool behind them";
    }
    {
      ok =
        lib.hasInfix "iptables -I FORWARD -i nbr0 -j neighbour-fwd" fw
        && lib.hasInfix "iptables -I FORWARD -i wb+ -j wb-fwd" fw
        &&
          lib.sort lib.lessThan config.networking.nat.internalInterfaces == [
            "nbr0"
            "wb+"
          ];
      msg = "both guest networks are isolated and NATed";
    }
    {
      ok = lib.elem "services.vmkit.networks: every network needs its own subnet." (
        failedAssertions clash
      );
      msg = "workspaces may not share another network's subnet";
    }
    {
      ok = unit.restartIfChanged == false && lib.elem "multi-user.target" unit.wantedBy;
      msg = "a deploy must not restart a workspace VM";
    }
    {
      ok =
        config.environment.etc ? "workbench/alice-main.json"
        && config.services.nginx.virtualHosts ? "alice-scratch--hello.ws.example.com"
        && lib.all (h: !lib.hasPrefix "alice-bare" h) (lib.attrNames config.services.nginx.virtualHosts);
      msg = "every workspace gets a config, and each of its apps a virtual host";
    }
  ];
in
if failures == [ ] then
  pkgs.writeText "shared-host-eval" (
    builtins.unsafeDiscardStringContext config.system.build.toplevel.drvPath
  )
else
  throw ("shared-host: " + lib.concatMapStringsSep "; " (c: c.msg) failures)
