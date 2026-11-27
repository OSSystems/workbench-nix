# What the fleet, front and node modules render, by evaluation: a front
# alone, a node alone, a machine with both roles and other nodes, a host
# on its own, and each of the fleet's assertions.
{
  pkgs,
  flake,
  inputs,
  system,
  ...
}:
let
  inherit (pkgs) lib;
  machine = {
    boot.loader.grub.enable = false;
    fileSystems."/" = {
      device = "/dev/vda";
      fsType = "ext4";
    };
    system.stateVersion = "26.05";
  };
  fleet = {
    services.workbench.fleet = {
      domain = "ws.example.com";
      front.wireguard = {
        publicKey = "front-public-key";
        endpoint = "front.example.com:51820";
      };
      nodes = {
        a = {
          subnet = "10.232.0";
          wireguard = {
            address = "10.231.0.10";
            publicKey = "a-public-key";
          };
        };
        b = {
          subnet = "10.232.1";
          wireguard = {
            address = "10.231.0.11";
            publicKey = "b-public-key";
          };
        };
      };
      apps.hello = {
        name = "Hello";
        port = 8080;
        serve = "exec sleep infinity";
      };
      environmentFile = "/run/secrets/all.env";
      workspaces = {
        alice = {
          node = "a";
          owner = "alice";
          ip = "10.232.0.10";
          environmentFile = "/run/secrets/alice.env";
        };
        bob = {
          node = "b";
          owner = "bob";
          ip = "10.232.1.10";
          apps = [ "hello" ];
        };
      };
    };
  };
  asFront = {
    services.workbench.front = {
      enable = true;
      virtualHost = {
        forceSSL = true;
        sslCertificate = "/var/lib/certs/ws.pem";
        sslCertificateKey = "/var/lib/certs/ws.key";
      };
      wireguard.privateKeyFile = "/run/secrets/wireguard";
      oidc = {
        issuerUrl = "https://sso.example.com/realms/example";
        clientSecretFile = "/run/secrets/oidc";
        cookieSecretFile = "/run/secrets/cookie";
      };
      selfService = {
        enable = true;
        environmentCommand = "/run/current-system/sw/bin/workspace-env";
      };
    };
  };
  asNode = name: {
    services.workbench.node = {
      enable = true;
      inherit name;
      network.externalInterface = "eth0";
      wireguard.privateKeyFile = "/run/secrets/wireguard";
    };
  };
  eval =
    modules:
    (inputs.nixpkgs.lib.nixosSystem {
      inherit system;
      modules = [
        flake.nixosModules.default
        machine
      ]
      ++ modules;
    }).config;
  failed = c: map (a: a.message) (lib.filter (a: !a.assertion) c.assertions);
  refuses = text: c: lib.any (lib.hasInfix text) (failed c);
  json =
    c: name: builtins.fromJSON (builtins.unsafeDiscardStringContext c.environment.etc.${name}.text);
  # A config's text, without building it.
  fileJson =
    c: name:
    builtins.fromJSON (builtins.unsafeDiscardStringContext c.environment.etc.${name}.source.text);
  netdev = c: c.systemd.network.netdevs."30-wg-workbench" or null;
  firewall = c: c.networking.firewall.extraCommands;

  front = eval [
    fleet
    asFront
  ];
  nodeB = eval [
    fleet
    (asNode "b")
  ];
  both = eval [
    fleet
    asFront
    (asNode "a")
  ];
  # A host on its own: one node, no WireGuard, no keys.
  alone = eval [
    {
      services.workbench.fleet = {
        domain = "ws.example.com";
        nodes.a = {
          subnet = "10.232.0";
          wireguard.address = "10.231.0.10";
        };
        workspaces.alice = {
          node = "a";
          owner = "alice";
          ip = "10.232.0.10";
        };
      };
    }
    (lib.recursiveUpdate asFront { services.workbench.front.wireguard.privateKeyFile = null; })
    (lib.recursiveUpdate (asNode "a") { services.workbench.node.wireguard.privateKeyFile = null; })
  ];
  broken =
    change:
    eval [
      fleet
      asFront
      (asNode "a")
      change
    ];

  frontSettings = json front "workbench-front.json";
  bothFront = json both "workbench-front.json";
  bothNode = json both "workbench-node.json";
  nodeSettings = json nodeB "workbench-node.json";
  template = json nodeB "workbench-self-service.json";
  bothTemplate = json both "workbench-self-service.json";
  alice = json front "workbench-front/alice.json";
  bob = fileJson nodeB "workbench/bob.json";
  unit = nodeB.systemd.services."workbench@";
  web = front.systemd.services.workbench-web;
  webPackage = flake.packages.${system}.web;
  build = flake.packages.${system}.default.build;

  failures = lib.filter (c: !c.ok) [
    {
      ok = lib.all (c: failed c == [ ]) [
        front
        nodeB
        both
        alone
      ];
      msg = "a front, a node, both roles and a host on its own evaluate: ${
        toString (failed front ++ failed nodeB ++ failed both ++ failed alone)
      }";
    }
    {
      ok =
        frontSettings.listen == "10.231.0.1:7911"
        && frontSettings.socket == ""
        && frontSettings.localNode == ""
        &&
          frontSettings.nodes.a == {
            address = "10.231.0.10";
            subnet = "10.232.0";
            api = "10.231.0.10:7910";
          }
        && frontSettings.declared == "/etc/workbench-front"
        && frontSettings.ssh.hostCa == "/var/lib/workbench/ssh/host-ca"
        && frontSettings.selfService.secretsRoot == "/var/lib/workbench-secrets"
        && frontSettings.selfService.environmentFiles == [ "/run/secrets/all.env" ]
        &&
          map (a: a.id) frontSettings.apps == [
            "hello"
            "t3code"
            "herdr"
            "herdrcore"
          ];
      msg = "the front's settings name every node by its WireGuard address and API";
    }
    {
      ok =
        alice == {
          name = "alice";
          owner = "alice";
          node = "a";
          ip = "10.232.0.10";
          os = "ubuntu";
          apps = [
            "hello"
            "t3code"
          ];
          maxMemoryGiB = 16;
          diskGiB = 64;
          sshAuthorizedKeys = [ ];
          secretsDir = "/var/lib/workbench-secrets/alice";
          environmentFiles = [
            "/run/secrets/all.env"
            "/run/secrets/alice.env"
          ];
        }
        && !(front.environment.etc ? "workbench/alice.json")
        && !(front.environment.etc ? "workbench-node.json");
      msg = "a front alone has the declared workspaces' records, and no node's files";
    }
    {
      ok =
        nodeSettings == {
          name = "b";
          listen = "10.231.0.11:7910";
          socket = "";
          socketGroup = "";
          frontAddress = "10.231.0.1";
          front = "10.231.0.1:7911";
          stateDir = "/var/lib/workbench";
          template = "/etc/workbench-self-service.json";
          configDirs = [
            "/etc/workbench"
            "/var/lib/workbench/configs"
          ];
          reservePercent = 10;
          reserveMinGiB = 2;
          pools = [
            {
              name = "workbench";
              status = "/run/vmkit/memory/workbench.json";
            }
          ];
        };
      msg = "a node's settings: its API on WireGuard, the front's address and API";
    }
    {
      ok =
        nodeB.environment.etc ? "workbench/bob.json"
        && !(nodeB.environment.etc ? "workbench/alice.json")
        && !(bob ? ssh)
        && bob.environmentFiles == [ ]
        && !(bob ? socketGroup)
        && bob.gateway == "10.232.1.1"
        && nodeB.users.users ? wb-bob
        && !(nodeB.users.users ? wb-alice)
        && !(nodeB.environment.etc ? "workbench-front.json");
      msg = "a node has its own declared workspaces' configs, without the front's keys and files";
    }
    {
      ok =
        template.subnet == "10.232.1"
        && !(template ? ssh)
        && template.environmentFiles == [ ]
        && !(template ? environmentCommand)
        && !(template ? ownerCommand)
        && !(template ? socketGroup)
        &&
          lib.attrNames template.images == [
            "nixos"
            "ubuntu"
          ]
        && template.limits.bootMemMiB == 2048;
      msg = "a node's template signs nothing and runs no command";
    }
    {
      ok =
        unit.serviceConfig.RestartSec == 10
        && unit.unitConfig.StartLimitIntervalSec == 0
        && lib.hasSuffix "/bin/workbench run %i" unit.serviceConfig.ExecStart
        && nodeB.systemd.services ? workbench-node
        && nodeB.systemd.services ? workbench-node-setup
        && !(nodeB.systemd.services ? workbench-web)
        && !(nodeB.systemd.services ? workbench-front);
      msg = "a node's VMs keep asking the front; it runs no front units";
    }
    {
      ok =
        let
          d = netdev nodeB;
          peer = lib.head d.wireguardPeers;
        in
        d.wireguardConfig.PrivateKeyFile == "/run/secrets/wireguard"
        && lib.length d.wireguardPeers == 1
        && peer.PublicKey == "front-public-key"
        && peer.AllowedIPs == [ "10.231.0.1/32" ]
        && peer.Endpoint == "front.example.com:51820"
        && peer.PersistentKeepalive == 25
        && nodeB.systemd.network.networks."30-wg-workbench".address == [ "10.231.0.11/32" ];
      msg = "a node peers with the front only, and keeps its tunnel up";
    }
    {
      ok =
        let
          d = netdev front;
        in
        lib.sort lib.lessThan (map (p: p.PublicKey) d.wireguardPeers) == [
          "a-public-key"
          "b-public-key"
        ]
        && lib.all (p: !(p ? Endpoint)) d.wireguardPeers
        && lib.elem [
          "10.231.0.11/32"
          "10.232.1.0/24"
        ] (map (p: p.AllowedIPs) d.wireguardPeers)
        && lib.elem 51820 front.networking.firewall.allowedUDPPorts;
      msg = "the front peers with every node, by its address and subnet";
    }
    {
      ok =
        lib.hasInfix "iptables -A workbench-wg-in -s 10.231.0.10 -p tcp --dport 7911 -j ACCEPT" (
          firewall front
        )
        && lib.hasInfix "iptables -I FORWARD -i wg-workbench -j DROP" (firewall front)
        && lib.hasInfix "iptables -I FORWARD -o wg-workbench -j DROP" (firewall front)
        && !lib.hasInfix "workbench-wg-in -s 10.231.0.10 -p tcp --dport 7910" (firewall front);
      msg = "the front answers the nodes' addresses only, and forwards nothing through the tunnel";
    }
    {
      ok =
        let
          out = "iptables -A workbench-wg-out -p tcp --dport";
          new = "-m conntrack --ctstate NEW";
        in
        lib.hasInfix "${out} 7910 ${new} -m owner --uid-owner 0 -j RETURN\n${out} 7910 ${new} -m owner --uid-owner workbench-web -j RETURN\n${out} 7910 ${new} -j REJECT --reject-with tcp-reset\n" (
          firewall front
        )
        && lib.hasInfix "${out} 7911 ${new} -m owner --uid-owner 0 -j RETURN\n${out} 7911 ${new} -j REJECT --reject-with tcp-reset\n" (
          firewall nodeB
        )
        && lib.hasInfix "iptables -I OUTPUT -o wg-workbench -j workbench-wg-out" (firewall nodeB);
      msg = "only root, and on the front the web UI, connect to the other side's API through the tunnel";
    }
    {
      ok =
        let
          rules = nodeB.services.vmkit.networks.workbench.forwardRules;
        in
        lib.hasInfix "iptables -A workbench-wg-in -s 10.231.0.1 -p tcp --dport 7910 -j ACCEPT" (
          firewall nodeB
        )
        && lib.hasInfix "iptables -A workbench-wg-fwd -s 10.231.0.1 -o wb+ -j ACCEPT" (firewall nodeB)
        && lib.hasPrefix "iptables -A wb-fwd -o wg-workbench -d 10.231.0.1 -m conntrack --ctstate ESTABLISHED,RELATED -j RETURN\niptables -A wb-fwd -o wg-workbench -j DROP\n" rules
        && !lib.hasInfix "workbench-wg-in -s 10.231.0.1 -p tcp --dport 7911" (firewall nodeB);
      msg = "a node answers the front only, forwards only its traffic to the guests, and no guest reaches the tunnel";
    }
    {
      ok =
        let
          d = netdev both;
        in
        map (p: p.PublicKey) d.wireguardPeers == [ "b-public-key" ]
        && bothFront.socket == "/run/workbench-front/api.sock"
        && bothFront.localNode == "a"
        && bothFront.nodes.a.api == "/run/workbench-node/api.sock"
        && bothFront.nodes.b.api == "10.231.0.11:7910"
        && bothFront.listen == "10.231.0.1:7911"
        && !(bothNode ? listen)
        && bothNode.socket == "/run/workbench-node/api.sock"
        && bothNode.socketGroup == "workbench"
        && bothNode.front == "/run/workbench-front/api.sock";
      msg = "both roles: sockets to each other, WireGuard to the other nodes only";
    }
    {
      ok =
        let
          bothWeb = both.systemd.services.workbench-web;
          vm = both.systemd.services."workbench@";
          polkit = both.security.polkit.extraConfig;
        in
        lib.all (flag: !lib.hasInfix flag bothWeb.serviceConfig.ExecStart) [
          "--self-service"
          "--config-dir"
          "--state-root"
          "--pool"
          "--mem-reserve"
        ]
        && !lib.any (lib.hasInfix ".stopped") (
          bothWeb.serviceConfig.ReadWritePaths ++ both.systemd.tmpfiles.rules
        )
        && !lib.hasInfix "/^workbench@" polkit
        && lib.hasInfix "/^workbench-move@" polkit
        && !(vm.serviceConfig ? ExecStartPost)
        && !(bothTemplate ? socketGroup)
        && bothTemplate.limits.quota == 0
        && lib.elem "d /var/lib/workbench 0711 root root -" both.systemd.tmpfiles.rules;
      msg = "both roles: the web UI runs on the fleet alone, and reaches its node through the node's API only";
    }
    {
      ok =
        netdev alone == null
        && !((json alone "workbench-front.json") ? listen)
        && !((json alone "workbench-node.json") ? listen);
      msg = "a host on its own runs no WireGuard";
    }
    {
      ok =
        web.environment.WORKBENCH_FRONT == "/etc/workbench-front.json"
        && lib.hasInfix "workbench-move@" front.security.polkit.extraConfig
        && lib.elem "/var/lib/workbench/closed" web.serviceConfig.ReadWritePaths
        && lib.elem "d /var/lib/workbench/closed 0755 workbench-web workbench-web -" front.systemd.tmpfiles.rules
        && lib.elem "d /var/lib/workbench/registry 0755 root root -" front.systemd.tmpfiles.rules
        && front.systemd.services."workbench-move@".restartIfChanged == false
        && front.systemd.services."workbench-move@".serviceConfig.TimeoutStartSec == "infinity"
        && front.systemd.services ? workbench-front-setup;
      msg = "the web UI runs on the fleet, closes nodes and moves workspaces";
    }
    {
      ok =
        builtins.match "[0-9a-f]{32}" build != null
        && webPackage.build == build
        && (flake.packages.${system}.default.drvAttrs.WORKBENCH_BUILD or null) == build
        && (webPackage.drvAttrs.WORKBENCH_BUILD or null) == build;
      msg = "workbench and workbench-web share one build";
    }
    {
      ok = refuses "services.workbench.fleet.workspaces.carol.node: c isn't one of" (broken {
        services.workbench.fleet.workspaces.carol = {
          node = "c";
          owner = "carol";
          ip = "10.232.0.11";
        };
      });
      msg = "a workspace on a node the fleet hasn't";
    }
    {
      ok =
        refuses "services.workbench.fleet.workspaces.carol.ip: an address in 10.232.0.2-127" (broken {
          services.workbench.fleet.workspaces.carol = {
            node = "a";
            owner = "carol";
            ip = "10.232.0.130";
          };
        })
        && refuses "services.workbench.fleet.workspaces.carol.ip" (broken {
          services.workbench.fleet.workspaces.carol = {
            node = "a";
            owner = "carol";
            ip = "10.232.1.11";
          };
        });
      msg = "a declared workspace's address is in the lower half of its node's subnet";
    }
    {
      ok =
        lib.all
          (
            name:
            refuses "services.workbench.fleet.workspaces.${name}: the name" (broken {
              services.workbench.fleet.workspaces.${name} = {
                node = "a";
                owner = "carol";
                ip = "10.232.0.20";
              };
            })
          )
          [
            "closed"
            "configs"
            "registry"
            "self-service"
            "ssh"
          ];
      msg = "the state directory's own names are refused";
    }
    {
      ok =
        refuses "every node needs a subnet of its own" (broken {
          services.workbench.fleet.nodes.b.subnet = lib.mkForce "10.232.0";
        })
        && refuses "need a WireGuard address of their own" (broken {
          services.workbench.fleet.nodes.b.wireguard.address = lib.mkForce "10.231.0.10";
        })
        && refuses "services.workbench.fleet.nodes.b: the name" (broken {
          services.workbench.fleet.nodes.b.wireguard.address = lib.mkForce "10.9.0.11";
        })
        && refuses "services.workbench.fleet.nodes.b: the name" (broken {
          services.workbench.fleet.nodes.b.subnet = lib.mkForce "10.231.0";
        });
      msg = "subnets and WireGuard addresses are distinct, each where it belongs";
    }
    {
      ok = refuses "services.workbench.node.name: c isn't one of" (eval [
        fleet
        (asNode "c")
      ]);
      msg = "a node is one of the fleet's";
    }
    {
      ok =
        refuses "services.workbench.front.wireguard.privateKeyFile" (broken {
          services.workbench.front.wireguard.privateKeyFile = lib.mkForce null;
        })
        && refuses "services.workbench.fleet.nodes.b.wireguard.publicKey" (broken {
          services.workbench.fleet.nodes.b.wireguard.publicKey = lib.mkForce null;
        })
        && refuses "services.workbench.fleet.front.wireguard" (eval [
          fleet
          (asNode "b")
          { services.workbench.fleet.front.wireguard.endpoint = lib.mkForce null; }
        ]);
      msg = "a tunnel needs its keys and the front's endpoint";
    }
    {
      ok = lib.all (o: refuses "It is ${o.now} now" (broken o.set)) [
        {
          set.services.workbench.enable = true;
          now = "services.workbench.front.enable and services.workbench.node.enable";
        }
        {
          set.services.workbench.network.externalInterface = "eth0";
          now = "services.workbench.node.network.externalInterface";
        }
        {
          set.services.workbench.network.subnet = "10.232.0";
          now = "services.workbench.fleet.nodes.<node>.subnet";
        }
        {
          set.services.workbench.memory.reservePercent = 5;
          now = "services.workbench.node.memory.reservePercent";
        }
        {
          set.services.workbench.memory.bootMiB = 1024;
          now = "services.workbench.fleet.memory.bootMiB";
        }
        {
          set.services.workbench.web.enable = true;
          now = "services.workbench.front, which always has the web UI";
        }
      ];
      msg = "an option of a host on its own names the one it became";
    }
    {
      ok = refuses "services.workbench.front.stateDir: on a machine with both roles" (broken {
        services.workbench.node.stateDir = "/srv/workbench";
      });
      msg = "a machine with both roles has one state directory";
    }
  ];
in
if failures == [ ] then
  pkgs.writeText "fleet-eval" (
    builtins.unsafeDiscardStringContext (
      lib.concatMapStringsSep "\n" (c: c.system.build.toplevel.drvPath) [
        front
        nodeB
        both
        alone
      ]
    )
  )
else
  throw ("fleet: " + lib.concatMapStringsSep "; " (c: c.msg) failures)
