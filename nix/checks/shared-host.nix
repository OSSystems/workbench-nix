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
          # A host on its own: the front, and its one node.
          services.workbench = {
            fleet = {
              domain = "ws.example.com";
              nodes.host = {
                subnet = "10.232.0";
                wireguard.address = "10.231.1.10";
              };
              # Not the default: the other controller below has its guests there.
              wireguard.subnet = "10.231.1.0/24";
              apps.hello = {
                name = "Hello";
                port = 8080;
                serve = "exec ${pkgs.python3}/bin/python3 -m http.server \"$APP_PORT\"";
              };
              # Reached from a terminal, and only by the workspaces that ask.
              apps.term = {
                name = "Term";
                port = null;
                default = false;
                serve = "exec sleep infinity";
              };
              workspaces = lib.mapAttrs (_: ws: ws // { node = "host"; }) {
                alice-main = {
                  owner = "alice";
                  ip = "10.232.0.10";
                };
                alice-scratch = {
                  owner = "alice";
                  ip = "10.232.0.11";
                  sudo = false;
                  apps = [
                    "hello"
                    "term"
                  ];
                };
                alice-bare = {
                  owner = "alice";
                  ip = "10.232.0.12";
                  apps = [ ];
                };
              };
            };
            front = {
              enable = true;
              virtualHost = {
                forceSSL = true;
                useACMEHost = "ws.example.com";
              };
              oidc = {
                issuerUrl = "https://sso.example.com/realms/example";
                clientSecretFile = "/run/secrets/oidc";
                cookieSecretFile = "/run/secrets/cookie";
              };
            };
            node = {
              enable = true;
              name = "host";
              network.externalInterface = "eth0";
            };
          };
          security.acme.certs."ws.example.com" = {
            domain = "ws.example.com";
            extraDomainNames = [ "*.ws.example.com" ];
            dnsProvider = "rfc2136";
            environmentFile = "/run/secrets/acme";
            group = "nginx";
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
    { services.workbench.fleet.nodes.host.subnet = lib.mkForce "10.231.0"; }
  ];
  failedAssertions = c: map (a: a.message) (lib.filter (a: !a.assertion) c.assertions);
  memoryExec = config.systemd.services.workbench-memory.serviceConfig.ExecStart;
  unit = config.systemd.services."workbench@alice-main";
  fw = config.networking.firewall.extraCommands;
  appsOf =
    name:
    (builtins.fromJSON (
      builtins.unsafeDiscardStringContext config.environment.etc."workbench/${name}.json".source.text
    )).apps;
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
        && config.environment.etc ? "workbench-front/alice-main.json"
        &&
          lib.attrNames config.services.nginx.virtualHosts == [
            "workbench-workspaces"
            "ws.example.com"
          ];
      msg = "every workspace gets a config and a record, and one virtual host serves every app";
    }
    {
      ok =
        map (a: a.id) (appsOf "alice-main") == [
          "hello"
          "t3code"
        ]
        &&
          map (a: a.port) (appsOf "alice-scratch") == [
            8080
            null
          ];
      msg = "an app people opt into, without an address, is only where asked";
    }
  ];
in
if failures == [ ] then
  pkgs.writeText "shared-host-eval" (
    builtins.unsafeDiscardStringContext config.system.build.toplevel.drvPath
  )
else
  throw ("shared-host: " + lib.concatMapStringsSep "; " (c: c.msg) failures)
