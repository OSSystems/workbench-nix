# What the fleet, front and node modules share: the package, addresses,
# apps and their bundles, a workspace's packages and seed, its config on
# its node and its record on the front, and the template of workspaces
# made at run time. Not a module: each of them imports it with its
# arguments.
{
  config,
  lib,
  pkgs,
}:

let
  fleet = config.services.workbench.fleet;
  front = config.services.workbench.front;
  node = config.services.workbench.node;
in
rec {
  package = pkgs.callPackage ../../../package.nix { vmkit = fleet.vmkitSource; };
  exe = lib.getExe' package "workbench";

  controlPort = 5001;
  agentPort = 5002;
  # WireGuard's interface and port. Not `wb…`: vmkit matches every
  # workspace tap as `wb+`.
  wireguard = {
    interface = "wg-workbench";
    port = 51820;
  };
  nodePort = 7910;
  frontPort = 7911;
  nodeSocket = "/run/workbench-node/api.sock";
  frontSocket = "/run/workbench-front/api.sock";
  frontSettings = "/etc/workbench-front.json";
  template = "/etc/workbench-self-service.json";
  declaredDir = "workbench-front";
  secretsRoot = "/var/lib/workbench-secrets";

  # The node on this machine, if it has one, and whether the front is here
  # too: a machine with both roles reaches its own node through sockets.
  localNode = if node.enable then node.name else null;
  bothRoles = front.enable && node.enable;

  # Names the host keeps for its own directories in its state directory.
  reserved = [
    "closed"
    "configs"
    "registry"
    "self-service"
    "ssh"
  ];

  # IPv4 addresses, as numbers for ranges.
  octets = ip: map lib.toInt (lib.splitString "." ip);
  isAddress =
    s:
    let
      o = "(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])";
    in
    builtins.match "${o}\\.${o}\\.${o}\\.${o}" s != null;
  # Three octets, as the crate takes a /24: no leading zeros.
  isSubnet = s: isAddress "${s}.0";
  toNumber = ip: lib.foldl (n: o: n * 256 + o) 0 (octets ip);
  fromNumber =
    n:
    lib.concatMapStringsSep "." toString [
      (n / 16777216)
      (lib.mod (n / 65536) 256)
      (lib.mod (n / 256) 256)
      (lib.mod n 256)
    ];
  # A CIDR range: its first address and its size.
  range =
    cidr:
    let
      parts = lib.splitString "/" cidr;
      length = lib.toInt (lib.elemAt parts 1);
      size = lib.foldl (n: _: n * 2) 1 (lib.genList lib.id (32 - length));
    in
    {
      base = toNumber (lib.elemAt parts 0) / size * size;
      inherit size;
    };
  isRange =
    s:
    let
      parts = lib.splitString "/" s;
    in
    lib.length parts == 2
    && isAddress (lib.elemAt parts 0)
    && builtins.match "[0-9]|[12][0-9]|3[0-2]" (lib.elemAt parts 1) != null;
  inRange =
    cidr: ip:
    let
      r = range cidr;
      n = toNumber ip;
    in
    n >= r.base && n < r.base + r.size;
  overlaps =
    a: b:
    let
      ra = range a;
      rb = range b;
    in
    ra.base < rb.base + rb.size && rb.base < ra.base + ra.size;
  firstAddress = cidr: fromNumber ((range cidr).base + 1);

  hex2 = n: lib.toLower (lib.fixedWidthString 2 "0" (lib.toHexString n));
  tapOf = ws: "wb" + lib.concatMapStrings hex2 (octets ws.ip);
  macOf = ws: "02:00:" + lib.concatMapStringsSep ":" hex2 (octets ws.ip);
  userOf = name: "wb-${name}";
  hostFor = name: "${name}.${fleet.domain}";
  secretsDirOf = name: ws: if ws.secretsDir != null then ws.secretsDir else "${secretsRoot}/${name}";
  environmentFilesOf =
    ws:
    lib.filter (f: f != null) [
      fleet.environmentFile
      ws.environmentFile
    ];

  # The front's SSH certificate authorities: one signs the guests' host
  # keys, at every start, the other people's keys, in the web UI; each
  # private key is readable by its signer only.
  sshDir = "${front.stateDir}/ssh";
  sshCa = {
    keygen = "${pkgs.openssh}/bin/ssh-keygen";
    hostCa = "${sshDir}/host-ca";
    userCa = "${sshDir}/user-ca.pub";
    hostKeys = "${sshDir}/hosts";
  };

  maxGiBOf = ws: if ws.memory.maxGiB != null then ws.memory.maxGiB else fleet.memory.maxGiB;
  memMiB =
    ws:
    let
      maxGiB = maxGiBOf ws;
    in
    {
      boot = if ws.memory.bootMiB != null then ws.memory.bootMiB else fleet.memory.bootMiB;
      max = if maxGiB == "auto" then "auto" else maxGiB * 1024;
    };

  # The fleet's apps, those a workspace has by default first: what every
  # node's template and the front's settings list.
  appIds =
    lib.attrNames (lib.filterAttrs (_: a: a.default) fleet.apps)
    ++ lib.attrNames (lib.filterAttrs (_: a: !a.default) fleet.apps);
  defaultApps = lib.attrNames (lib.filterAttrs (_: app: app.default) fleet.apps);
  # The ports of some apps, each once: those with an address.
  portsOf = ids: lib.filter (p: p != null) (map (id: fleet.apps.${id}.port) ids);

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
        paths = fleet.extraPackages ++ extraPackages ++ lib.concatMap (id: fleet.apps.${id}.packages) apps;
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
  ) fleet.apps;
  appConfig = id: {
    inherit id;
    inherit (fleet.apps.${id}) name port oneshot;
    connect = lib.optionals (fleet.apps.${id}.port == null) fleet.apps.${id}.connect;
    bundle = "${appBundles.${id}}";
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
        # The front signs a host key for the workspace's address, and the
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

  networkConfig = gateway: ws: {
    version = 2;
    ethernets.uplink = {
      match.macaddress = macOf ws;
      addresses = [ "${ws.ip}/32" ];
      routes = [
        {
          to = "0.0.0.0/0";
          via = gateway;
          on-link = true;
        }
      ];
      nameservers.addresses = fleet.network.nameservers;
    };
  };

  seed =
    gateway: name: ws:
    let
      user = userData name ws;
      network = builtins.toJSON (networkConfig gateway ws);
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

  # The paths every VM's config names.
  vmPaths = {
    cloudHypervisor = lib.getExe pkgs.cloud-hypervisor;
    virtiofsd = lib.getExe pkgs.virtiofsd;
    qemuImg = lib.getExe' pkgs.qemu-utils "qemu-img";
    firmware = "${pkgs.OVMF-cloud-hypervisor.fd}/FV/CLOUDHV.fd";
  };

  # A declared workspace's config on its node: as on a host on its own,
  # without the SSH CAs and the environment files, which the front keeps
  # and gives at every start. Its secretsDir is the front's, a clean path
  # the node doesn't read.
  workspaceConfig =
    {
      subnet,
      stateDir,
      socketGroup,
    }:
    name: ws:
    let
      gateway = "${subnet}.1";
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
      url = "${fleet.publicScheme}://${hostFor name}";
      stateDir = "${stateDir}/${name}";
      runtimeDir = "/run/workbench/${name}";
      inherit (ws) os diskGiB cpus;
      bootMemMiB = mem.boot;
      maxMemMiB = mem.max;
      tap = tapOf ws;
      mac = macOf ws;
      inherit gateway;
      inherit (fleet.network) nameservers;
      sshAuthorizedKeys = map lib.strings.trim ws.sshAuthorizedKeys;
      closure = "${closure}/store-paths";
      registration = "${closure}/registration";
      packages = "${packages}";
      agent = "${agent}";
      apps = map appConfig ws.apps;
      environment = fleet.environment // ws.environment;
      environmentFiles = [ ];
      secretsDir = secretsDirOf name ws;
      inherit controlPort agentPort;
    }
    // vmPaths
    // lib.optionalAttrs (socketGroup != "") { inherit socketGroup; }
    // (
      if ws.os == "ubuntu" then
        {
          inherit (fleet) image imageFormat;
          seed = seed gateway name ws;
        }
      else
        {
          inherit (fleet.nixos) image imageFormat;
        }
    );

  # A declared workspace's record on the front: who owns it, the node and
  # address Nix gives it, what it is made of, and its secrets, paths on
  # the front.
  record = name: ws: {
    inherit name;
    inherit (ws)
      owner
      node
      ip
      os
      apps
      diskGiB
      ;
    maxMemoryGiB = maxGiBOf ws;
    sshAuthorizedKeys = map lib.strings.trim ws.sshAuthorizedKeys;
    secretsDir = secretsDirOf name ws;
    environmentFiles = environmentFilesOf ws;
  };

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

  # What every workspace made at run time on a node shares: `workbench`
  # fills in the rest from the front's record, as `workspaceConfig` would.
  # A node can't tell which systems the front offers, so it has an image
  # for each; it signs no key and runs no command.
  templateFor =
    {
      subnet,
      stateDir,
      socketGroup,
      limits,
    }:
    let
      packages = packagesOf "self-service" [ ] appIds;
      closure = pkgs.closureInfo {
        rootPaths = [
          packages
          agent
        ]
        ++ map (id: appBundles.${id}) appIds;
      };
    in
    {
      inherit limits;
      inherit (fleet) domain;
      inherit stateDir subnet socketGroup;
      scheme = fleet.publicScheme;
      # Unused on a node: the front keeps the secrets.
      inherit secretsRoot;
      inherit (fleet.network) nameservers;
      inherit controlPort agentPort;
      cpus = 0;
      closure = "${closure}/store-paths";
      registration = "${closure}/registration";
      packages = "${packages}";
      agent = "${agent}";
      apps = map appConfig appIds;
      defaultApps = lib.filter (id: fleet.apps.${id}.default) appIds;
      inherit (fleet) environment;
      environmentFiles = [ ];
      images = {
        ubuntu = {
          inherit (fleet) image imageFormat;
          userData = userData "@name@" {
            owner = "@owner@";
            sshAuthorizedKeys = [ ];
            sudo = true;
          };
        };
        nixos = { inherit (fleet.nixos) image imageFormat; };
      };
      mkfsVfat = "${pkgs.dosfstools}/bin/mkfs.vfat";
      mcopy = "${pkgs.mtools}/bin/mcopy";
      environmentCommand = "";
      ownerCommand = "";
    }
    // vmPaths;

  # The limits of what people make, as the front's settings have them.
  frontLimits =
    let
      ss = front.selfService;
    in
    {
      inherit (ss)
        os
        quota
        maxMemoryGiB
        maxDiskGiB
        ;
      bootMemMiB = fleet.memory.bootMiB;
    };
}
