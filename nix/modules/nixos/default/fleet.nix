# `services.workbench.fleet`: what the front and every node share, which
# every machine of a fleet imports alike: the domain, the apps, the
# guests' images and tools, the nodes, the WireGuard addresses and the
# declared workspaces.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib) mkOption types;

  fleet = config.services.workbench.fleet;
  front = config.services.workbench.front;
  node = config.services.workbench.node;
  wb = import ./lib.nix { inherit config lib pkgs; };

  # GiB, or `auto`: the node's memory, which the launcher reads.
  maxGiB = types.either types.ints.positive (types.enum [ "auto" ]);

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

  # T3 Code, from `package`.
  t3 = lib.getExe' fleet.package "t3";
  t3code = {
    name = lib.mkDefault "T3 Code";
    port = lib.mkDefault 3773;
    packages = [ fleet.package ];
    # The node sizes the VM, which grows on demand from its boot memory:
    # caps T3 Code took from the RAM of the moment would stop agents far
    # below the VM's own limit.
    serve = ''
      export T3CODE_HOME=$HOME/.t3
      export T3CODE_AGENT_SLICE_MEMORY_POLICY=host-managed
      exec ${t3} serve --host 0.0.0.0 --port "$APP_PORT"
    '';
    openLink = ''
      set -eo pipefail
      export T3CODE_HOME=$HOME/.t3
      ${t3} auth pairing create --base-url "$APP_URL" --label workbench --json | ${lib.getExe pkgs.jq} -er .pairUrl
    '';
  };

  # A tool that updates itself, refusing to: the workspace has the host's
  # version, and a newer one comes with its next restart.
  noSelfUpdate =
    pkg: bins:
    pkgs.runCommand "${pkg.pname}-workspace-${pkg.version}" { } ''
      mkdir -p $out/bin
      for f in ${pkg}/*; do
        [ "''${f##*/}" = bin ] || ln -s "$f" $out/
      done
      ln -s ${pkg}/bin/* $out/bin/
      for b in ${lib.escapeShellArgs bins}; do
        rm $out/bin/$b
        cat > $out/bin/$b <<EOF
      #!${pkgs.runtimeShell}
      if [ "\''${1-}" = update ]; then
        echo "$b comes from the workspace's host: a newer one comes with the workspace's next restart, once the host has it." >&2
        exit 1
      fi
      exec ${pkg}/bin/$b "\$@"
      EOF
        chmod +x $out/bin/$b
      done
    '';

  # herdr's two runtimes, which people reach from their terminal over SSH:
  # the TUI's server, and herdr-core, which herdr-app and `hc` drive.
  herdr = {
    name = lib.mkDefault "Herdr";
    port = lib.mkDefault null;
    default = lib.mkDefault false;
    packages = [ (noSelfUpdate fleet.herdr.package [ "herdr" ]) ];
    connect = [ "In a terminal: `herdr --remote @host@`" ];
    # In the foreground: its panes get the workspace's environment.
    serve = ''
      exec herdr server
    '';
  };
  # The core and the holders of its terminals are user services, which
  # take the user manager's environment and the caller's: the workspace's
  # goes to the manager first, so a core started later over SSH has it
  # too. From then on `hc` keeps the core up.
  herdrCore = {
    name = lib.mkDefault "Herdr app";
    port = lib.mkDefault null;
    default = lib.mkDefault false;
    oneshot = lib.mkDefault true;
    connect = [
      "In Herdr app: Add machine, with `@host@`."
      "Or in a terminal: `hc machine add @host@`, then `hc run --on @name@ <command>`."
    ];
    packages = [
      (noSelfUpdate fleet.herdrCore.package [
        "hc"
        "herdr-core"
      ])
    ];
    serve = ''
      set -euo pipefail
      names=$(${pkgs.gnused}/bin/sed -nE 's/^[[:space:]]*(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*)=.*/\2/p' \
          /run/workspace/config/environment /run/workspace/secrets/environment 2>/dev/null \
        | ${pkgs.gnugrep}/bin/grep -vxE 'PATH|HOME|USER|LOGNAME|SHELL|XDG_[A-Z_]*|HC_[A-Z_]*' \
        | ${pkgs.coreutils}/bin/sort -u || true)
      if [ -n "$names" ]; then
        systemctl --user import-environment $names
      fi
      exec hc server start
    '';
  };

  appModule = {
    options = {
      name = mkOption {
        type = types.str;
        example = "T3 Code";
        description = "What people call it.";
      };
      port = mkOption {
        type = types.nullOr types.port;
        example = 3773;
        description = ''
          The port it listens on in the guest, which `serve` gets in
          `APP_PORT`, and the app is at `<workspace>--<id>.<domain>`. Null
          for an app without an address, which people reach from a
          terminal instead.
        '';
      };
      connect = mkOption {
        type = types.listOf types.str;
        default = [ "In a terminal: `ssh @host@`" ];
        example = [ "In a terminal: `herdr --remote @host@`" ];
        description = ''
          For an app without a port: how people reach it from a computer
          set up on its Connect page, one step each, with `@host@` for
          `<workspace>.<domain>`, `@name@` for the workspace, and commands
          between backticks, each closed in its step.
        '';
      };
      default = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Whether workspaces have it unless they say otherwise: the
          default of `workspaces.<name>.apps`, and what a new workspace
          made in the web UI starts with. False for an app people opt into.
        '';
      };
      serve = mkOption {
        type = types.lines;
        description = ''
          Shell script run at every boot as the owner, in their home, that
          serves the app, on `$APP_PORT` if it has a port; systemd restarts
          it when it ends, unless the app is `oneshot`. It has the
          workspace's environment and secrets, the workspace's packages on
          its PATH, the app's address in `APP_URL`, and the owner's user
          manager, which lingers, at `XDG_RUNTIME_DIR`.
        '';
      };
      oneshot = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Whether `serve` starts what the app runs and ends, such as user
          services of the owner's, rather than serving until it is
          stopped. Its unit stays active once `serve` is done, and is
          never restarted.
        '';
      };
      openLink = mkOption {
        type = types.nullOr types.lines;
        default = null;
        description = ''
          Shell script run as the owner that prints one URL on `$APP_URL`,
          the app's address, that signs a browser in to it, and nothing
          else. Null for an app without a sign-in of its own: opening it is
          all it takes.
        '';
      };
      packages = mkOption {
        type = types.listOf types.package;
        default = [ ];
        description = "Packages on the PATH of the workspaces that have the app.";
      };
    };
  };

  workspaceModule =
    { name, ... }:
    {
      options = {
        node = mkOption {
          type = types.str;
          example = "a";
          description = ''
            The node it runs on, one of `nodes`, where its disk is. To move
            it, change `node` and `ip` and deploy: the front moves its disk.
          '';
        };
        apps = mkOption {
          type = types.listOf types.str;
          default = wb.defaultApps;
          defaultText = lib.literalMD "the apps whose `default` is true";
          example = [
            "t3code"
            "herdr"
          ];
          description = ''
            The apps it runs, by id, each with an address at
            `<name>--<id>.<domain>`. A change takes effect when the VM next
            starts.
          '';
        };
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
            runs the app from the node's store. `nixos` is a NixOS the owner
            rebuilds from /etc/nixos, with its own store the app's closure is
            copied into; it needs `sudo`.
          '';
        };
        ip = mkOption {
          type = types.str;
          example = "10.232.0.10";
          description = ''
            Static IPv4 address in the lower half of its node's subnet,
            `.2` to `.127`: the upper half is for workspaces made at run
            time. The front proxies the workspace's apps to it.
          '';
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
          description = "vCPUs; 0 gives the VM every CPU of its node.";
        };
        memory = {
          bootMiB = mkOption {
            type = types.nullOr types.ints.positive;
            default = null;
            description = "Boot memory; null uses `memory.bootMiB`.";
          };
          maxGiB = mkOption {
            type = types.nullOr maxGiB;
            default = null;
            description = "Most memory the VM grows to, or `auto`; null uses `memory.maxGiB`.";
          };
        };
        sshAuthorizedKeys = mkOption {
          type = types.listOf types.str;
          default = [ ];
          description = "SSH keys of the owner in the guest. Only its node and the front reach the guest's SSH server.";
        };
        environment = mkOption {
          type = types.attrsOf types.str;
          default = { };
          description = "Non-secret environment for this workspace's app and the processes it starts.";
        };
        environmentFile = mkOption {
          type = types.nullOr types.path;
          default = null;
          description = "File of `KEY=value` lines on the front, such as an API key, appended to the guest's secret environment at each start. Kept out of the Nix store.";
        };
        extraPackages = mkOption {
          type = types.listOf types.package;
          default = [ ];
          description = "Packages on this workspace's PATH, shared read-only from its node's store with their closure.";
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
            Directory on the front, filled by sops-nix or similar, copied
            read-only to /run/workspace/secrets in the guest at each start.
            Its `environment` file, plus the environment files, is the app's
            secret environment. Null uses
            /var/lib/workbench-secrets/<workspace>.
          '';
        };
      };
    };

  nodeModule = {
    options = {
      subnet = mkOption {
        type = types.str;
        example = "10.232.0";
        description = ''
          First three octets of the /24 its workspaces' addresses come
          from; the node is `.1`. Declared workspaces take `.2` to `.127`,
          those made at run time from `.128`.
        '';
      };
      wireguard = {
        address = mkOption {
          type = types.str;
          example = "10.231.0.10";
          description = "Its address in `wireguard.subnet`: the only one the front takes its requests from.";
        };
        publicKey = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "Its WireGuard public key. Needed unless it is on the front's machine, which reaches it without WireGuard.";
        };
      };
    };
  };
in
{
  options.services.workbench.fleet = {
    vmkitSource = mkOption {
      type = types.path;
      internal = true;
      description = "vmkit's source, which workbench builds against; `nixosModules.default` sets it.";
    };
    package = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = "T3 Code: when set, the app `t3code`, which every workspace has unless it says otherwise.";
    };
    herdr.package = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = ''
        herdr: when set, the app `herdr`, the TUI's server, which people
        reach with `herdr --remote <workspace>.<domain>`. Workspaces have it
        when they name it.
      '';
    };
    herdrCore.package = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = ''
        herdr-core: when set, the app `herdrcore`, which herdr-app and `hc`
        reach as the machine `<workspace>.<domain>`. Workspaces have it when
        they name it.
      '';
    };
    apps = mkOption {
      type = types.attrsOf (types.submodule appModule);
      default = { };
      description = ''
        What workspaces run and people open from outside them, by id: a
        lowercase letter, then lowercase letters and digits, at most 16.
        An app with a port is at `<name>--<id>.<domain>`, an origin of its
        own, in the browser; one without is reached from a terminal.
        `package` adds `t3code`.
      '';
    };
    domain = mkOption {
      type = types.str;
      example = "ws.example.com";
      description = "The web UI is at `<domain>`, a workspace's apps at `<workspace>--<app>.<domain>`; one wildcard certificate covers them all.";
    };
    publicScheme = mkOption {
      type = types.enum [
        "https"
        "http"
      ];
      default = "https";
      description = "Scheme of the workspace URLs pairing links point to.";
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
        default = "${wb.nixosImage}/nixos.qcow2";
        defaultText = lib.literalMD "a NixOS of the node's nixpkgs with `nixosModules.guest`";
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
    environment = mkOption {
      type = types.attrsOf types.str;
      default = { };
      description = "Non-secret environment for every workspace.";
    };
    environmentFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "File of `KEY=value` lines on the front for every workspace; see the per-workspace option.";
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
        type = maxGiB;
        default = 16;
        description = ''
          Most memory a workspace grows to. `auto` is its node's memory, read
          at every start: growth still stops at the node's
          `memory.reservePercent`.
        '';
      };
    };
    network.nameservers = mkOption {
      type = types.listOf types.str;
      default = [
        "1.1.1.1"
        "8.8.8.8"
      ];
      description = "DNS servers of the guests; private ones are allowed on port 53.";
    };
    wireguard.subnet = mkOption {
      type = types.strMatching "[0-9]{1,3}(\\.[0-9]{1,3}){3}/[0-9]{1,2}";
      default = "10.231.0.0/24";
      description = ''
        WireGuard's own network, which the front's and the nodes'
        addresses are in. Private, so no guest reaches it.
      '';
    };
    front.wireguard = {
      address = mkOption {
        type = types.str;
        default = wb.firstAddress fleet.wireguard.subnet;
        defaultText = lib.literalMD "the first address of `wireguard.subnet`";
        description = "The front's address in `wireguard.subnet`: the only one the nodes' APIs answer.";
      };
      publicKey = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "The front's WireGuard public key. Needed by every node on another machine.";
      };
      endpoint = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "front.example.com:51820";
        description = "Where nodes reach the front's WireGuard, UDP port 51820; the front needs no node's.";
      };
    };
    nodes = mkOption {
      type = types.attrsOf (types.submodule nodeModule);
      default = { };
      example = lib.literalExpression ''
        {
          a = { subnet = "10.232.0"; wireguard = { address = "10.231.0.10"; publicKey = "…"; }; };
          b = { subnet = "10.232.1"; wireguard = { address = "10.231.0.11"; publicKey = "…"; }; };
        }
      '';
      description = ''
        The machines that run workspaces, by name, which their own
        `node.name` is. A node is plugged in by declaring it here and
        deploying the fleet.
      '';
    };
    workspaces = mkOption {
      type = types.attrsOf (types.submodule workspaceModule);
      default = { };
      description = ''
        Declared workspaces, by name; `<name>.<domain>` is their address.
        Removing one stops its VM and keeps its disk on its node.
      '';
    };
  };

  config = lib.mkMerge [
    {
      services.workbench.fleet.apps = {
        t3code = lib.mkIf (fleet.package != null) t3code;
        herdr = lib.mkIf (fleet.herdr.package != null) herdr;
        herdrcore = lib.mkIf (fleet.herdrCore.package != null) herdrCore;
      };
    }
    (lib.mkIf (front.enable || node.enable) {
      environment.systemPackages = [ wb.package ];

      assertions =
        let
          ips = lib.mapAttrsToList (_: ws: ws.ip) fleet.workspaces;
          subnets = lib.mapAttrsToList (_: n: n.subnet) fleet.nodes;
          addresses = [
            fleet.front.wireguard.address
          ]
          ++ lib.mapAttrsToList (_: n: n.wireguard.address) fleet.nodes;
          # The lower half of a node's subnet, but its own `.1`.
          declaredAt =
            ws:
            let
              o = wb.octets ws.ip;
              subnet = fleet.nodes.${ws.node}.subnet;
            in
            wb.isAddress ws.ip
            && lib.concatStringsSep "." (map toString (lib.take 3 o)) == subnet
            && lib.elemAt o 3 >= 2
            && lib.elemAt o 3 <= 127;
        in
        [
          {
            assertion = pkgs.stdenv.hostPlatform.system == "x86_64-linux";
            message = "services.workbench: only x86_64-linux machines are supported.";
          }
          {
            assertion = fleet.nodes != { };
            message = "services.workbench.fleet.nodes: a fleet needs at least one node.";
          }
          {
            assertion = lib.length subnets == lib.length (lib.unique subnets);
            message = "services.workbench.fleet.nodes: every node needs a subnet of its own.";
          }
          {
            assertion = lib.length addresses == lib.length (lib.unique addresses);
            message = "services.workbench.fleet: the front and every node need a WireGuard address of their own.";
          }
          {
            assertion = wb.isRange fleet.wireguard.subnet;
            message = "services.workbench.fleet.wireguard.subnet: an IPv4 range such as 10.231.0.0/24.";
          }
          {
            assertion = lib.length ips == lib.length (lib.unique ips);
            message = "services.workbench.fleet.workspaces: every workspace needs a distinct ip.";
          }
        ]
        ++ lib.optionals (wb.isRange fleet.wireguard.subnet) (
          [
            {
              assertion =
                wb.isAddress fleet.front.wireguard.address
                && wb.inRange fleet.wireguard.subnet fleet.front.wireguard.address;
              message = "services.workbench.fleet.front.wireguard.address: an address in ${fleet.wireguard.subnet}.";
            }
          ]
          ++ lib.mapAttrsToList (name: n: {
            assertion =
              builtins.match "[a-z]([a-z0-9-]{0,26}[a-z0-9])?" name != null
              && !lib.hasInfix "--" name
              && wb.isSubnet n.subnet
              && !wb.overlaps "${n.subnet}.0/24" fleet.wireguard.subnet
              && wb.isAddress n.wireguard.address
              && wb.inRange fleet.wireguard.subnet n.wireguard.address;
            message = "services.workbench.fleet.nodes.${name}: the name must be a DNS label of at most 28 characters without `--`, the subnet three octets outside ${fleet.wireguard.subnet}, and the WireGuard address one in ${fleet.wireguard.subnet}.";
          }) fleet.nodes
        )
        ++ lib.mapAttrsToList (id: _: {
          assertion = builtins.match "[a-z][a-z0-9]{0,15}" id != null;
          message = "services.workbench.fleet.apps.${id}: an app's id is a lowercase letter, then lowercase letters and digits, at most 16.";
        }) fleet.apps
        ++ lib.concatLists (
          lib.mapAttrsToList (
            id: app:
            map (step: {
              assertion = lib.mod (lib.length (lib.splitString "`" step) - 1) 2 == 0;
              message = "services.workbench.fleet.apps.${id}.connect: the step \"${step}\" opens a command with a backtick it never closes.";
            }) app.connect
          ) fleet.apps
        )
        ++ lib.mapAttrsToList (name: ws: {
          assertion =
            lib.all (id: fleet.apps ? ${id}) ws.apps
            && lib.allUnique ws.apps
            && lib.allUnique (wb.portsOf ws.apps);
          message = "services.workbench.fleet.workspaces.${name}.apps: each must be one of services.workbench.fleet.apps, once, on a port of its own.";
        }) fleet.workspaces
        ++ lib.mapAttrsToList (name: ws: {
          assertion =
            builtins.match "[a-z]([a-z0-9-]{0,26}[a-z0-9])?" name != null
            && !lib.hasInfix "--" name
            && !lib.elem name wb.reserved
            && builtins.match "[a-z_][a-z0-9_.-]{0,31}" ws.owner != null
            && ws.owner != "root";
          message = "services.workbench.fleet.workspaces.${name}: the name must be a DNS label of at most 28 characters without `--`, other than ${lib.concatStringsSep ", " wb.reserved}, which name the state directory's own, and the owner a user name other than root.";
        }) fleet.workspaces
        ++ lib.mapAttrsToList (name: ws: {
          assertion = fleet.nodes ? ${ws.node} && declaredAt ws;
          message =
            if fleet.nodes ? ${ws.node} then
              "services.workbench.fleet.workspaces.${name}.ip: an address in ${fleet.nodes.${ws.node}.subnet}.2-127, the lower half of node ${ws.node}'s subnet; the rest are for workspaces made at run time."
            else
              "services.workbench.fleet.workspaces.${name}.node: ${ws.node} isn't one of services.workbench.fleet.nodes.";
        }) fleet.workspaces
        ++ lib.mapAttrsToList (name: ws: {
          assertion = ws.os == "nixos" -> ws.sudo;
          message = "services.workbench.fleet.workspaces.${name}: a NixOS workspace needs sudo; its owner rebuilds the system.";
        }) fleet.workspaces;
    })
  ];
}
