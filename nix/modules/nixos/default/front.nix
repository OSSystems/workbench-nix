# `services.workbench.front`: the machine people reach, which decides.
# It keeps the registry, the secrets directories, the SSH CAs and host
# keys, the devices and the quotas; it serves the web UI and single
# sign-on in front of every workspace; and `workbench-front`, its API,
# answers the nodes' starts.
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
  inherit (wb) exe;
  webPackage = pkgs.callPackage ../../../packages/web.nix { vmkit = fleet.vmkitSource; };
  webExe = lib.getExe webPackage;

  ss = front.selfService;
  inherit (front) stateDir;
  origin = "https://${fleet.domain}";
  redisSocket = config.services.redis.servers.workbench-oauth2.unixSocket;
  cookieName = "_workbench_auth";
  oauth2Proxy = "http://127.0.0.1:${toString front.oauth2ProxyPort}";
  run = "/run/workbench-web";

  # The nodes on other machines, which the front reaches over WireGuard.
  remote = lib.filterAttrs (name: _: name != wb.localNode) fleet.nodes;

  settings = builtins.toJSON (
    {
      inherit (fleet) domain;
      scheme = fleet.publicScheme;
      inherit stateDir;
      socket = if wb.bothRoles then wb.frontSocket else "";
      localNode = if wb.bothRoles then wb.localNode else "";
      nodes = lib.mapAttrs (name: n: {
        inherit (n) subnet;
        inherit (n.wireguard) address;
        api =
          if name == wb.localNode then wb.nodeSocket else "${n.wireguard.address}:${toString wb.nodePort}";
      }) fleet.nodes;
      declared = "/etc/${wb.declaredDir}";
      apps = map wb.appConfig wb.appIds;
      defaultApps = lib.filter (id: fleet.apps.${id}.default) wb.appIds;
      ssh = wb.sshCa;
    }
    # Its API on WireGuard, for the nodes on other machines.
    // lib.optionalAttrs (remote != { }) {
      listen = "${fleet.front.wireguard.address}:${toString wb.frontPort}";
    }
    // lib.optionalAttrs ss.enable {
      selfService = {
        limits = wb.frontLimits;
        inherit (ss) apps environmentCommand;
        inherit (wb) secretsRoot;
        environmentFiles = lib.optional (fleet.environmentFile != null) fleet.environmentFile;
        ownerCommand = if ss.ownerCommand == null then "" else ss.ownerCommand;
      };
    }
  );
  records = lib.mapAttrs (name: ws: builtins.toJSON (wb.record name ws)) fleet.workspaces;

  # oauth2-proxy at /oauth2/ on every host name, so no workspace name is
  # reserved; its cookie is the domain's, so one sign-in covers them all.
  # Browsers always come over https, even when a proxy in front terminates
  # it and nginx here sees http, so the way back is https.
  oauth2Location = {
    proxyPass = oauth2Proxy;
    extraConfig = ''
      proxy_set_header X-Auth-Request-Redirect https://$host$request_uri;
    '';
  };
  signInLocation = ''
    return 302 https://$host/oauth2/start?rd=https://$host$request_uri;
  '';

  # Every app of every workspace is served by one virtual host, at
  # `<workspace>--<app>.<domain>`: the access check names who may use it and
  # the app's address, so a workspace made at run time needs no reload. An
  # app gets the browser's cookies without the SSO one. A workspace's own
  # name leads to its panel.
  escapedDomain = lib.replaceStrings [ "." ] [ "\\." ] fleet.domain;
  workspaceLocations = {
    "/oauth2/" = oauth2Location;
    "= /_workbench/authz" = {
      proxyPass = "http://unix:${run}/authz.sock:/authz";
      recommendedProxySettings = false;
      extraConfig = ''
        internal;
        proxy_pass_request_body off;
        proxy_set_header Content-Length "";
        proxy_set_header Cookie $http_cookie;
        proxy_set_header X-Workbench-Workspace $workbench_ws;
        proxy_set_header X-Workbench-App $workbench_app;
        # Its answer carries the app's cookies, which may be a few KiB.
        proxy_buffer_size 16k;
        proxy_buffers 4 16k;
        proxy_busy_buffers_size 16k;
      '';
    };
    "@workbench_sign_in".extraConfig = signInLocation;
    "@workbench_denied" = {
      proxyPass = "http://unix:${run}/ui.sock";
      extraConfig = ''
        rewrite ^ /denied?ws=$workbench_ws&app=$workbench_app? break;
        proxy_set_header X-Workbench-User $workbench_user;
        proxy_set_header X-Workbench-Groups $workbench_groups;
      '';
    };
    "/" = {
      proxyPass = "http://$workbench_upstream";
      proxyWebsockets = true;
      extraConfig = ''
        auth_request /_workbench/authz;
        auth_request_set $workbench_user $upstream_http_x_workbench_user;
        auth_request_set $workbench_groups $upstream_http_x_workbench_groups;
        auth_request_set $workbench_cookie $upstream_http_x_workbench_cookie;
        auth_request_set $workbench_upstream $upstream_http_x_workbench_upstream;
        error_page 401 = @workbench_sign_in;
        error_page 403 = @workbench_denied;
        proxy_set_header Cookie $workbench_cookie;
        # An app's cookies are its host's: one for the whole domain could
        # stand in for the single sign-on cookie.
        proxy_cookie_domain ~.+ $host;
        proxy_read_timeout 1d;
        proxy_send_timeout 1d;
        client_max_body_size 64m;
      '';
    };
  };

  # The way in from a terminal, without single sign-on: workbench-web
  # checks a pairing code or a device's credential itself.
  connectLocation = {
    proxyPass = "http://unix:${run}/connect.sock";
    extraConfig = ''
      client_max_body_size 16k;
    '';
  };

  # What a person's computer downloads to reach their workspaces: the
  # script that sets it up, and workbench-connect for each Linux.
  client = pkgs.callPackage ../../../packages/workbench-connect.nix { };
  installScript = pkgs.writeText "install.sh" ''
    #!/bin/sh
    # Sets this computer up to reach your workspaces at ${fleet.domain} over SSH:
    # downloads workbench-connect, which pairs it with the code from a
    # workspace's Connect page and installs itself in ~/.local/bin.
    #
    #   curl -fsSL ${origin}/connect/install.sh | sh -s -- <code>
    set -eu
    if [ $# -ne 1 ]; then
      echo "usage: curl -fsSL ${origin}/connect/install.sh | sh -s -- <code>" >&2
      exit 2
    fi
    case "$(uname -s)" in
      Linux) ;;
      *) echo "workbench-connect runs on Linux only, for now." >&2; exit 1 ;;
    esac
    case "$(uname -m)" in
      x86_64 | amd64) arch=x86_64 ;;
      aarch64 | arm64) arch=aarch64 ;;
      *) echo "workbench-connect has no build for $(uname -m)." >&2; exit 1 ;;
    esac
    if ! command -v ssh >/dev/null; then
      echo "Install OpenSSH's client first: workbench-connect sets ssh up." >&2
      exit 1
    fi
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    curl -fsSL -o "$tmp/workbench-connect" "${origin}/connect/bin/workbench-connect-$arch-linux"
    chmod +x "$tmp/workbench-connect"
    "$tmp/workbench-connect" setup --server "${origin}" "$1"
  '';
  connectSite = pkgs.runCommand "workbench-connect-site" { } ''
    mkdir -p $out/bin
    cp ${installScript} $out/install.sh
    cp ${client.x86_64-linux}/bin/workbench-connect $out/bin/workbench-connect-x86_64-linux
    cp ${client.aarch64-linux}/bin/workbench-connect $out/bin/workbench-connect-aarch64-linux
  '';

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
in
{
  options.services.workbench.front = {
    enable = lib.mkEnableOption ''
      the fleet's front: the web UI at `https://<domain>`, single sign-on in
      front of every workspace, the registry, the secrets and the SSH CAs,
      and the API the nodes ask when they start a workspace
    '';
    stateDir = mkOption {
      type = types.str;
      default = "/var/lib/workbench";
      description = ''
        Where the registry, the SSH CAs and host keys, the requests, the
        quotas and the closed nodes are. A machine with both roles shares
        it with its node.
      '';
    };
    virtualHost = mkOption {
      type = types.attrsOf types.anything;
      example = {
        forceSSL = true;
        useACMEHost = "ws.example.com";
      };
      description = "nginx virtual host settings, such as TLS, of the web UI and every workspace: a wildcard certificate for `<domain>` and `*.<domain>`.";
    };
    wireguard.privateKeyFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/run/secrets/wireguard";
      description = ''
        The front's WireGuard private key, kept out of the Nix store and
        readable by `systemd-network`, such as a sops secret. Needed when a
        node is on another machine.
      '';
    };
    admins = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "alice" ];
      description = "Users who see and control every workspace, the host page and the quotas.";
    };
    adminGroups = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "workbench-admins" ];
      description = "Groups, from the identity provider's `groups` claim, whose members are admins too.";
    };
    sessionRefresh = mkOption {
      type = types.str;
      default = "5m";
      description = "How often a session is checked again with the identity provider, which drops people removed from a group or the directory.";
    };
    selfService = {
      enable = lib.mkEnableOption ''
        workspaces people make themselves in the web UI, within a quota, on
        the node with the most free memory unless an admin picks one; see
        `docs/web-ui.md`
      '';
      groups = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "workbench" ];
        description = "Groups whose members make workspaces of their own; admins may too.";
      };
      quota = mkOption {
        type = types.ints.unsigned;
        default = 1;
        description = "How many workspaces a person may make, unless an admin sets theirs in the web UI.";
      };
      os = mkOption {
        type = types.listOf (
          types.enum [
            "ubuntu"
            "nixos"
          ]
        );
        default = [
          "ubuntu"
          "nixos"
        ];
        description = "The guest systems offered.";
      };
      maxMemoryGiB = mkOption {
        type = types.either types.ints.positive (types.enum [ "auto" ]);
        default = fleet.memory.maxGiB;
        defaultText = lib.literalExpression "config.services.workbench.fleet.memory.maxGiB";
        description = "The most memory people may let their VM grow to; `auto` is its node's.";
      };
      maxDiskGiB = mkOption {
        type = types.ints.positive;
        default = 128;
        description = "The largest disk people may ask for; at least 16 GiB.";
      };
      apps = mkOption {
        type = types.listOf types.str;
        default = wb.appIds;
        defaultText = lib.literalMD "every app of `services.workbench.fleet.apps`, those a workspace has by default first";
        description = "The apps, of `services.workbench.fleet.apps`, people may give the workspaces they make, in the order offered.";
      };
      environmentCommand = mkOption {
        type = types.str;
        example = "/run/current-system/sw/bin/workspace-env";
        description = ''
          Run as root on the front with the owner as its argument; prints
          `KEY=value` lines, the new workspace's secret environment, such as
          the app's credential. A failure, or nothing printed, refuses the
          request: no workspace is made without it.
        '';
      };
      ownerCommand = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "/run/current-system/sw/bin/workspace-owner";
        description = ''
          Run as root on the front with an owner as its argument: exit 0
          while they may keep their workspaces, 1 once they left; any other
          status, such as the directory being down, decides nothing. It is
          asked before a workspace is made and by the reconciliation timer,
          which stops the workspaces of those who left and keeps their
          disks. Null asks no one.
        '';
      };
      reconcileInterval = mkOption {
        type = types.str;
        default = "15min";
        description = "How often the workspaces of owners who left are looked for.";
      };
    };
    oidc = {
      provider = mkOption {
        type = types.enum [
          "keycloak-oidc"
          "oidc"
        ];
        default = "keycloak-oidc";
        description = "oauth2-proxy's provider for the identity provider.";
      };
      issuerUrl = mkOption {
        type = types.str;
        example = "https://sso.example.com/realms/example";
        description = "The OIDC issuer. Its `preferred_username` claim must be the person's LDAP uid, the name workspaces have as `owner`.";
      };
      clientId = mkOption {
        type = types.str;
        default = "workbench";
        description = "The confidential OIDC client; its redirect URI is `https://<domain>/oauth2/callback`.";
      };
      clientSecretFile = mkOption {
        type = types.str;
        example = "/run/secrets/workbench-oidc";
        description = "File with the client's secret, kept out of the Nix store.";
      };
      cookieSecretFile = mkOption {
        type = types.str;
        example = "/run/secrets/workbench-cookie";
        description = "File with the secret the session cookie is encrypted with: 16, 24 or 32 bytes, such as `openssl rand -hex 16`.";
      };
      backendLogoutUrl = mkOption {
        type = types.nullOr types.str;
        default =
          if front.oidc.provider == "keycloak-oidc" then
            "${front.oidc.issuerUrl}/protocol/openid-connect/logout?id_token_hint={id_token}"
          else
            null;
        defaultText = lib.literalMD "Keycloak's logout endpoint, for `keycloak-oidc`";
        description = "Where signing out also ends the identity provider's session.";
      };
      extraArgs = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "More oauth2-proxy flags.";
      };
      scope = mkOption {
        type = types.str;
        default = "openid email profile";
        example = "openid email profile groups offline_access";
        description = "The scopes oauth2-proxy asks for. Some providers want `groups` for the groups claim and `offline_access` for refresh tokens.";
      };
    };
    oauth2ProxyPort = mkOption {
      type = types.port;
      default = 4180;
      description = "Local port of oauth2-proxy.";
    };
  };

  config = lib.mkIf front.enable {
    users = {
      users.workbench-web = {
        isSystemUser = true;
        group = "workbench-web";
        extraGroups = [ "workbench" ];
        description = "workbench web UI";
      };
      groups.workbench-web = { };
      # May use the local node's API, on a machine with both roles.
      groups.workbench = { };
      users.${config.services.nginx.user}.extraGroups = [ "workbench-web" ];
    };

    assertions = [
      {
        # Its node reads the marks of owners who left where it writes them.
        assertion = node.enable -> node.stateDir == stateDir;
        message = "services.workbench.front.stateDir: on a machine with both roles, the same as services.workbench.node.stateDir.";
      }
      {
        assertion = !(front.virtualHost.enableACME or false);
        message = "services.workbench.front: one virtual host serves every workspace, so `virtualHost` needs a wildcard certificate for *.${fleet.domain}, such as `useACMEHost`, not `enableACME`.";
      }
      {
        assertion = ss.enable -> ss.maxDiskGiB >= 16;
        message = "services.workbench.front.selfService.maxDiskGiB: at least 16.";
      }
      {
        assertion =
          ss.enable
          ->
            lib.all (id: fleet.apps ? ${id}) ss.apps
            && lib.allUnique ss.apps
            && lib.allUnique (wb.portsOf ss.apps);
        message = "services.workbench.front.selfService.apps: each must be one of services.workbench.fleet.apps, once, on a port of its own.";
      }
    ];

    environment.etc = {
      "workbench-front.json".text = settings;
    }
    // lib.mapAttrs' (
      name: text: lib.nameValuePair "${wb.declaredDir}/${name}.json" { inherit text; }
    ) records;

    services.redis.servers.workbench-oauth2 = {
      enable = true;
      port = 0;
      unixSocketPerm = 660;
      save = [ ];
    };

    systemd.tmpfiles.rules =
      # A machine with both roles has its node's.
      lib.optional (!(node.enable && node.stateDir == stateDir)) "d ${stateDir} 0711 root root -"
      ++ [
        # Root's, which the web UI reads.
        "d ${stateDir}/registry 0755 root root -"
        "d ${stateDir}/closed 0755 workbench-web workbench-web -"
        "d ${stateDir}/.left 0755 root root -"
        # Root's, so the UI can't swap what root reads and writes in it: the
        # UI writes requests and quotas, root the outcomes.
        "d ${stateDir}/self-service 0755 root root -"
        "d ${stateDir}/self-service/requests 0750 workbench-web workbench-web -"
        "d ${stateDir}/self-service/results 0755 root root -"
        "d ${wb.secretsRoot} 0700 root root -"
      ]
      ++ lib.optional ss.enable "d ${stateDir}/self-service/quotas 0755 workbench-web workbench-web -"
      ++ lib.mapAttrsToList (name: ws: "d ${wb.secretsDirOf name ws} 0700 root root -") fleet.workspaces;

    systemd.services = {
      # Made once; the guests' host keys are made at their first start.
      workbench-ssh-ca = {
        description = "The SSH certificate authorities of workspaces";
        wantedBy = [ "multi-user.target" ];
        before = [
          "workbench-web.service"
          "workbench-front.service"
        ];
        path = [ pkgs.openssh ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          install -d -m 0755 ${wb.sshDir}
          install -d -m 0700 ${wb.sshCa.hostKeys}
          for ca in host user; do
            if [ ! -e ${wb.sshDir}/$ca-ca ]; then
              ssh-keygen -q -t ed25519 -N "" -C "workbench $ca CA of ${fleet.domain}" -f ${wb.sshDir}/$ca-ca
            fi
            chmod 0644 ${wb.sshDir}/$ca-ca.pub
          done
          chown root:root ${wb.sshDir}/host-ca
          chown workbench-web:root ${wb.sshDir}/user-ca
          chmod 0400 ${wb.sshDir}/host-ca ${wb.sshDir}/user-ca
        '';
      };

      # The front's API, which the nodes ask when they start a workspace.
      workbench-front = {
        description = "The front's API, which the nodes ask";
        wantedBy = [ "multi-user.target" ];
        after = [
          "systemd-networkd.service"
          "workbench-ssh-ca.service"
        ];
        wants = [ "workbench-ssh-ca.service" ];
        restartTriggers = [ settings ];
        serviceConfig = {
          ExecStart = "${exe} front serve";
          RuntimeDirectory = "workbench-front";
          RuntimeDirectoryMode = "0700";
          # Its WireGuard address may come a moment after it starts.
          Restart = "always";
          RestartSec = 5;
        };
      };

      # At boot and on every deploy that changes the settings or a declared
      # record: puts the declared workspaces in the registry, cleans up what
      # stopped half way and starts the moves under way or that Nix asks
      # for.
      workbench-front-setup = {
        description = "The registry's declared workspaces, and the moves under way";
        wantedBy = [ "multi-user.target" ];
        after = [
          "systemd-networkd.service"
          "workbench-front.service"
        ]
        ++ lib.optional wb.bothRoles "workbench-node.service";
        restartTriggers = [ settings ] ++ lib.attrValues records;
        path = [ config.systemd.package ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          TimeoutStartSec = "10min";
          ExecStart = "${exe} front setup";
        };
      };

      # Runs as long as the copy takes; a deploy leaves it be.
      "workbench-move@" =
        lib.recursiveUpdate (selfServiceUnit "Move workspace %i to another node" [
          "move"
          "%i"
        ]) { serviceConfig.TimeoutStartSec = "infinity"; }
        // {
          restartIfChanged = false;
        };

      workbench-web = {
        description = "workbench web UI";
        # It reads the settings, with the apps it offers, when it starts.
        restartTriggers = [ settings ];
        wantedBy = [ "multi-user.target" ];
        after = [
          "network.target"
          "workbench-ssh-ca.service"
        ];
        wants = [ "workbench-ssh-ca.service" ];
        path = [ config.systemd.package ];
        # Where the web UI finds the fleet.
        environment.WORKBENCH_FRONT = wb.frontSettings;
        serviceConfig = {
          ExecStart = lib.escapeShellArgs (
            [
              webExe
              "--site=${webPackage.site}"
              "--origin=${origin}"
              "--oauth2-proxy=${oauth2Proxy}"
              "--cookie-name=${cookieName}"
              "--devices=/var/lib/workbench-web/devices.json"
              "--ssh-keygen=${pkgs.openssh}/bin/ssh-keygen"
              "--user-ca=${wb.sshDir}/user-ca"
              "--host-ca=${wb.sshDir}/host-ca.pub"
            ]
            ++ map (a: "--admin=${a}") front.admins
            ++ map (g: "--admin-group=${g}") front.adminGroups
            ++ lib.optionals ss.enable (map (g: "--creator-group=${g}") ss.groups)
          );
          User = "workbench-web";
          Group = "workbench-web";
          RuntimeDirectory = "workbench-web";
          RuntimeDirectoryMode = "0750";
          # The paired devices: a hash of each credential.
          StateDirectory = "workbench-web";
          StateDirectoryMode = "0700";
          UMask = "0007";
          Restart = "always";
          RestartSec = 2;
          ReadWritePaths = [
            "${stateDir}/closed"
            "${stateDir}/self-service/requests"
          ]
          ++ lib.optional ss.enable "${stateDir}/self-service/quotas";
          NoNewPrivileges = true;
          ProtectSystem = "strict";
          ProtectHome = true;
          PrivateTmp = true;
          PrivateDevices = true;
          ProtectKernelTunables = true;
          ProtectControlGroups = true;
          RestrictAddressFamilies = [
            "AF_UNIX"
            "AF_INET"
            "AF_INET6"
          ];
        };
      };

      workbench-oauth2-proxy = {
        description = "Single sign-on for the workbench web UI and workspaces";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        serviceConfig = {
          ExecStart = lib.escapeShellArgs (
            [
              (lib.getExe pkgs.oauth2-proxy)
              "--http-address=127.0.0.1:${toString front.oauth2ProxyPort}"
              "--provider=${front.oidc.provider}"
              "--oidc-issuer-url=${front.oidc.issuerUrl}"
              "--client-id=${front.oidc.clientId}"
              "--client-secret-file=%d/client-secret"
              "--cookie-secret-file=%d/cookie-secret"
              "--cookie-name=${cookieName}"
              "--cookie-domain=.${fleet.domain}"
              "--cookie-samesite=lax"
              "--whitelist-domain=.${fleet.domain}"
              "--whitelist-domain=${fleet.domain}"
              "--redirect-url=${origin}/oauth2/callback"
              "--scope=${front.oidc.scope}"
              "--oidc-groups-claim=groups"
              # Sessions live in Redis, so a refresh keeps the cookie's ticket
              # and nginx need not pass a new cookie back.
              "--cookie-refresh=${front.sessionRefresh}"
              "--session-store-type=redis"
              "--redis-connection-url=unix://${redisSocket}"
              "--code-challenge-method=S256"
              # Who signs in is preferred_username; the email decides nothing.
              # Many directory users have none, and oauth2-proxy refuses a
              # session without one, so the username stands in for it.
              "--oidc-email-claim=preferred_username"
              "--email-domain=*"
              "--set-xauthrequest"
              "--reverse-proxy"
              # Only nginx, on this host, may tell it the client's address.
              "--trusted-proxy-ip=127.0.0.1/32"
              "--trusted-proxy-ip=::1/128"
              "--skip-provider-button"
              "--upstream=static://202"
            ]
            ++ lib.optional (
              front.oidc.backendLogoutUrl != null
            ) "--backend-logout-url=${front.oidc.backendLogoutUrl}"
            ++ front.oidc.extraArgs
          );
          LoadCredential = [
            "client-secret:${front.oidc.clientSecretFile}"
            "cookie-secret:${front.oidc.cookieSecretFile}"
          ];
          DynamicUser = true;
          SupplementaryGroups = [ config.services.redis.servers.workbench-oauth2.group ];
          Restart = "always";
          RestartSec = 2;
          NoNewPrivileges = true;
          ProtectSystem = "strict";
          ProtectHome = true;
          PrivateTmp = true;
        };
      };
    }
    // lib.optionalAttrs ss.enable {
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
      workbench-self-service-reconcile = selfServiceUnit "Stop the workspaces of owners who left" [
        "reconcile"
      ];
    };
    systemd.timers.workbench-self-service-reconcile = lib.mkIf ss.enable {
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnBootSec = "5min";
        OnUnitActiveSec = ss.reconcileInterval;
      };
    };

    # Make, change, move and destroy workspaces made at run time, and
    # nothing else: the nodes start, stop and restart them.
    security.polkit.enable = true;
    security.polkit.extraConfig = ''
      polkit.addRule(function(action, subject) {
        if (action.id == "org.freedesktop.systemd1.manage-units" && subject.user == "workbench-web") {
          var unit = action.lookup("unit"), verb = action.lookup("verb");
          if (/^workbench-move@[a-z][a-z0-9-]*\.service$/.test(unit) && verb == "start") {
            return polkit.Result.YES;
          }
          ${lib.optionalString ss.enable ''
            if (/^workbench-(create|destroy|apps|memory)@[a-z][a-z0-9-]*\.service$/.test(unit) && verb == "start") {
              return polkit.Result.YES;
            }
          ''}
        }
      });
    '';

    services.nginx = {
      enable = true;
      recommendedProxySettings = true;
      virtualHosts = {
        ${fleet.domain} = front.virtualHost // {
          locations = {
            "/oauth2/" = oauth2Location;
            "= /oauth2/auth" = {
              proxyPass = oauth2Proxy;
              extraConfig = ''
                internal;
                proxy_pass_request_body off;
                proxy_set_header Content-Length "";
              '';
            };
            "@workbench_sign_in".extraConfig = signInLocation;
            # Public: the script and the client, which hold no secret.
            "/connect/".alias = "${connectSite}/";
            "= /connect/pair" = connectLocation;
            "= /connect/cert" = connectLocation;
            # The tunnel to a workspace's SSH server, which lasts as long as
            # the session in it.
            "/connect/ssh/" = {
              proxyPass = "http://unix:${run}/connect.sock";
              proxyWebsockets = true;
              extraConfig = ''
                proxy_buffering off;
                proxy_read_timeout 1d;
                proxy_send_timeout 1d;
              '';
            };
            "/" = {
              proxyPass = "http://unix:${run}/ui.sock";
              proxyWebsockets = true;
              extraConfig = ''
                auth_request /oauth2/auth;
                auth_request_set $workbench_user $upstream_http_x_auth_request_preferred_username;
                auth_request_set $workbench_groups $upstream_http_x_auth_request_groups;
                error_page 401 = @workbench_sign_in;
                proxy_set_header X-Workbench-User $workbench_user;
                proxy_set_header X-Workbench-Groups $workbench_groups;
                # Live updates are a long response.
                proxy_buffering off;
                proxy_read_timeout 1d;
              '';
            };
          };
        };
        workbench-workspaces = front.virtualHost // {
          serverName = "~^(?<workbench_ws>[a-z][a-z0-9-]*?)(?:--(?<workbench_app>[a-z][a-z0-9]*))?\\.${escapedDomain}$";
          locations = workspaceLocations;
          extraConfig = (front.virtualHost.extraConfig or "") + ''
            if ($workbench_app = "") {
              return 302 ${origin}/?w=$workbench_ws;
            }
          '';
        };
      };
    };
  };
}
