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
  # The host's SSH certificate authorities: one signs the guests' host
  # keys, at every start, the other people's keys, in the web UI; each
  # private key is readable by its signer only.
  sshDir = "${cfg.stateDir}/ssh";
  sshCa = {
    keygen = "${pkgs.openssh}/bin/ssh-keygen";
    hostCa = "${sshDir}/host-ca";
    userCa = "${sshDir}/user-ca.pub";
    hostKeys = "${sshDir}/hosts";
  };
  userCaSigner = if cfg.web.enable then "workbench-web" else "root";
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

  # A workspace's packages, those of its apps among them: what the guest
  # finds at config/app. Its `bin/workspace-agent` starts the agent at
  # config/agent, so the bundle stays the same when only workbench is
  # newer. Guests set up before apps were apps run its `bin/serve` as the
  # owner; it only waits, as the agent runs the apps now.
  packagesOf =
    name: extraPackages: apps:
    let
      sw = pkgs.buildEnv {
        name = "workbench-${name}-sw";
        paths = cfg.extraPackages ++ extraPackages ++ lib.concatMap (id: cfg.apps.${id}.packages) apps;
      };
    in
    pkgs.runCommand "workbench-${name}-packages" { } ''
      mkdir -p $out/bin
      ln -s ${sw} $out/sw
      cat > $out/bin/workspace-agent <<EOF
      #!${pkgs.runtimeShell}
      exec /run/workspace/config/agent/bin/workspace-agent
      EOF
      cat > $out/bin/serve <<EOF
      #!${pkgs.runtimeShell}
      exec ${pkgs.coreutils}/bin/sleep infinity
      EOF
      chmod +x $out/bin/workspace-agent $out/bin/serve
    '';

  # The agent, the same for every workspace: what the guest finds at
  # config/agent.
  agent = pkgs.runCommand "workbench-agent" { } ''
    mkdir -p $out/bin
    cat > $out/bin/workspace-agent <<EOF
    #!${pkgs.runtimeShell}
    exec ${exe} guest-agent --config=/run/workspace/config/workspace.json
    EOF
    chmod +x $out/bin/workspace-agent
  '';

  # An app's bundle, which the guest finds at config/apps/<id>: its
  # `bin/serve`, and `bin/open-link` if it signs browsers in.
  appBundles = lib.mapAttrs (
    id: app:
    pkgs.runCommand "workbench-app-${id}" { } ''
      mkdir -p $out/bin
      ln -s ${pkgs.writeShellScript "${id}-serve" app.serve} $out/bin/serve
      ${lib.optionalString (app.openLink != null) ''
        ln -s ${pkgs.writeShellScript "${id}-open-link" app.openLink} $out/bin/open-link
      ''}
    ''
  ) cfg.apps;
  appConfig = id: {
    inherit id;
    inherit (cfg.apps.${id}) name port oneshot;
    connect = lib.optionals (cfg.apps.${id}.port == null) cfg.apps.${id}.connect;
    bundle = "${appBundles.${id}}";
  };

  # T3 Code, from `package`.
  t3 = lib.getExe' cfg.package "t3";
  t3code = {
    name = lib.mkDefault "T3 Code";
    port = lib.mkDefault 3773;
    packages = [ cfg.package ];
    # The host sizes the VM, which grows on demand from its boot memory:
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
    packages = [ (noSelfUpdate cfg.herdr.package [ "herdr" ]) ];
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
      (noSelfUpdate cfg.herdrCore.package [
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
          between backticks.
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

  # The ports of some apps, each once: those with an address.
  portsOf = ids: lib.filter (p: p != null) (map (id: cfg.apps.${id}.port) ids);
  defaultApps = lib.attrNames (lib.filterAttrs (_: app: app.default) cfg.apps);

  # An app's own virtual host, without the web UI.
  vhostOf =
    ip: port:
    cfg.virtualHost
    // {
      locations."/" = {
        proxyPass = "http://${ip}:${toString port}";
        proxyWebsockets = true;
        extraConfig = ''
          proxy_read_timeout 1d;
          proxy_send_timeout 1d;
          client_max_body_size 64m;
        '';
      };
    };

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
      # The agent runs the apps.
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
        # The host signs a host key for the workspace's address, and the
        # keys of the person it is for.
        {
          path = "/etc/ssh/sshd_config.d/10-workbench.conf";
          content = ''
            HostKey /etc/ssh/ssh_host_ed25519_key
            HostKey /etc/ssh/ssh_host_ecdsa_key
            HostKey /etc/ssh/ssh_host_rsa_key
            HostKey /run/workspace/secrets/ssh-host/ssh_host_ed25519_key
            HostCertificate /run/workspace/secrets/ssh-host/ssh_host_ed25519_key-cert.pub
            TrustedUserCAKeys /run/workspace/config/ssh/user-ca.pub
            AuthorizedPrincipalsFile /run/workspace/config/ssh/principals/%u
          '';
        }
        {
          path = "/etc/profile.d/workbench.sh";
          content = ''
            PATH=/run/workspace/config/app/sw/bin:$PATH
          '';
        }
        # Commands over SSH that start no login shell, such as the ones
        # `herdr --remote` and herdr-core run, find the workspace's tools.
        {
          path = "/etc/environment";
          content = ''
            PATH="/run/workspace/config/app/sw/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/usr/local/games:/snap/bin"
          '';
        }
        {
          path = "/etc/systemd/system/workspace-agent.service";
          content = ''
            [Unit]
            Description=Workspace agent: memory reports, apps and host requests
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
      ];
      runcmd = [
        # The owner's user manager runs from boot, for the apps.
        [
          "loginctl"
          "enable-linger"
          ws.owner
        ]
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
      packages = packagesOf name ws.extraPackages ws.apps;
      closure = pkgs.closureInfo {
        rootPaths = [
          packages
          agent
        ]
        ++ map (id: appBundles.${id}) ws.apps;
      };
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
      sshAuthorizedKeys = map lib.strings.trim ws.sshAuthorizedKeys;
      closure = "${closure}/store-paths";
      registration = "${closure}/registration";
      packages = "${packages}";
      agent = "${agent}";
      apps = map appConfig ws.apps;
      environment = cfg.environment // ws.environment;
      environmentFiles = lib.filter (f: f != null) [
        cfg.environmentFile
        ws.environmentFile
      ];
      secretsDir = secretsDirOf name ws;
      inherit controlPort agentPort;
      ssh = sshCa;
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
      packages = packagesOf "self-service" [ ] ss.apps;
      closure = pkgs.closureInfo {
        rootPaths = [
          packages
          agent
        ]
        ++ map (id: appBundles.${id}) ss.apps;
      };
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
      inherit controlPort agentPort;
      cpus = 0;
      cloudHypervisor = lib.getExe pkgs.cloud-hypervisor;
      virtiofsd = lib.getExe pkgs.virtiofsd;
      qemuImg = lib.getExe' pkgs.qemu-utils "qemu-img";
      firmware = "${pkgs.OVMF-cloud-hypervisor.fd}/FV/CLOUDHV.fd";
      closure = "${closure}/store-paths";
      registration = "${closure}/registration";
      packages = "${packages}";
      agent = "${agent}";
      apps = map appConfig ss.apps;
      defaultApps = lib.filter (id: cfg.apps.${id}.default) ss.apps;
      inherit (cfg) environment;
      environmentFiles = lib.optional (cfg.environmentFile != null) cfg.environmentFile;
      socketGroup = "workbench";
      images = lib.getAttrs ss.os images;
      mkfsVfat = "${pkgs.dosfstools}/bin/mkfs.vfat";
      mcopy = "${pkgs.mtools}/bin/mcopy";
      inherit (ss) environmentCommand;
      ownerCommand = if ss.ownerCommand == null then "" else ss.ownerCommand;
      ssh = sshCa;
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
        apps = mkOption {
          type = types.listOf types.str;
          default = defaultApps;
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
        # Gone with apps: an assertion says where it went.
        appPort = mkOption {
          type = types.nullOr types.port;
          default = null;
          visible = false;
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
      description = "A workspace's apps are served at `<workspace>--<app>.<domain>`; one wildcard certificate covers them all.";
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
          ++ lib.mapAttrsToList (id: _: {
            assertion = builtins.match "[a-z][a-z0-9]{0,15}" id != null;
            message = "services.workbench.apps.${id}: an app's id is a lowercase letter, then lowercase letters and digits, at most 16.";
          }) cfg.apps
          ++ lib.mapAttrsToList (name: ws: {
            assertion = ws.appPort == null;
            message = "services.workbench.workspaces.${name}.appPort is gone: a workspace runs apps now, T3 Code at `${name}--t3code.${cfg.domain}`, which listens on services.workbench.apps.t3code.port.";
          }) cfg.workspaces
          ++ lib.mapAttrsToList (name: ws: {
            assertion =
              lib.all (id: cfg.apps ? ${id}) ws.apps && lib.allUnique ws.apps && lib.allUnique (portsOf ws.apps);
            message = "services.workbench.workspaces.${name}.apps: each must be one of services.workbench.apps, once, on a port of its own.";
          }) cfg.workspaces
          ++ lib.mapAttrsToList (name: ws: {
            assertion =
              builtins.match "[a-z]([a-z0-9-]{0,26}[a-z0-9])?" name != null
              && !lib.hasInfix "--" name
              && name != "ssh"
              && builtins.match "[a-z_][a-z0-9_.-]{0,31}" ws.owner != null
              && ws.owner != "root"
              && inSubnet ws.ip;
            message = "services.workbench.workspaces.${name}: the name must be a DNS label of at most 28 characters without `--`, other than `ssh`, whose state directory holds the SSH CAs, the owner a user name other than root, and ip an address in ${cfg.network.subnet}.2-254.";
          }) cfg.workspaces
          ++ lib.mapAttrsToList (name: ws: {
            assertion = ws.os == "nixos" -> ws.sudo;
            message = "services.workbench.workspaces.${name}: a NixOS workspace needs sudo; its owner rebuilds the system.";
          }) cfg.workspaces;

        services.workbench.apps = {
          t3code = lib.mkIf (cfg.package != null) t3code;
          herdr = lib.mkIf (cfg.herdr.package != null) herdr;
          herdrcore = lib.mkIf (cfg.herdrCore.package != null) herdrCore;
        };

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
              after = [
                "systemd-networkd.service"
                "workbench-ssh-ca.service"
              ];
              wants = [ "workbench-ssh-ca.service" ];
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

            # Made once; the guests' host keys are made at their first start.
            workbench-ssh-ca = {
              description = "The SSH certificate authorities of workspaces";
              wantedBy = [ "multi-user.target" ];
              before = [ "workbench-web.service" ];
              path = [ pkgs.openssh ];
              serviceConfig = {
                Type = "oneshot";
                RemainAfterExit = true;
              };
              script = ''
                install -d -m 0755 ${sshDir}
                install -d -m 0700 ${sshCa.hostKeys}
                for ca in host user; do
                  if [ ! -e ${sshDir}/$ca-ca ]; then
                    ssh-keygen -q -t ed25519 -N "" -C "workbench $ca CA of ${cfg.domain}" -f ${sshDir}/$ca-ca
                  fi
                  chmod 0644 ${sshDir}/$ca-ca.pub
                done
                chown root:root ${sshDir}/host-ca
                chown ${userCaSigner}:root ${sshDir}/user-ca
                chmod 0400 ${sshDir}/host-ca ${sshDir}/user-ca
              '';
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

        # With the web UI, one virtual host of its serves every app.
        services.nginx = {
          enable = true;
          recommendedProxySettings = true;
          virtualHosts = lib.mkIf (!cfg.web.enable) (
            lib.listToAttrs (
              lib.concatLists (
                lib.mapAttrsToList (
                  name: ws:
                  map (id: {
                    name = "${name}--${id}.${cfg.domain}";
                    value = vhostOf ws.ip cfg.apps.${id}.port;
                  }) (lib.filter (id: cfg.apps.${id}.port != null) ws.apps)
                ) cfg.workspaces
              )
            )
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
          "workbench-apps@" = selfServiceUnit "Change the apps of workspace %i" [
            "apps"
            "%i"
          ];
          "workbench-memory@" = selfServiceUnit "Change the memory of workspace %i" [
            "memory"
            "%i"
          ];
          workbench-self-service =
            lib.recursiveUpdate
              (selfServiceUnit "Users, taps and starts of workspaces made at run time" [ "setup" ])
              {
                wantedBy = [ "multi-user.target" ];
                # A new template makes every config again.
                restartTriggers = [ (builtins.toJSON selfServiceTemplate) ];
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
