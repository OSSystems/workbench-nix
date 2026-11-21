{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib) mkOption types;

  cfg = config.services.workbench;
  web = cfg.web;
  vmkit = config.services.vmkit;
  package = pkgs.callPackage ../../../packages/web.nix { vmkit = cfg.vmkitSource; };
  exe = lib.getExe package;

  ss = web.selfService;
  origin = "https://${cfg.domain}";
  redisSocket = config.services.redis.servers.workbench-oauth2.unixSocket;
  cookieName = "_workbench_auth";
  oauth2Proxy = "http://127.0.0.1:${toString web.oauth2ProxyPort}";
  run = "/run/workbench-web";

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
  escapedDomain = lib.replaceStrings [ "." ] [ "\\." ] cfg.domain;
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

  pools = [
    "workbench=${vmkit.memory.pools.workbench.statusFile}"
  ]
  ++ lib.mapAttrsToList (name: p: "${name}=${p.statusFile}") (
    lib.filterAttrs (name: _: name != "workbench") vmkit.memory.pools
  );
in
{
  options.services.workbench.web = {
    enable = lib.mkEnableOption ''
      the web UI at `https://<domain>`, and single sign-on in front of every
      workspace: only its owner and the admins reach a workspace's app
    '';
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
        workspaces people make themselves in the web UI, within a quota.
        Their configs go to `<stateDir>/configs`; see `docs/web-ui.md`
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
        type = types.ints.positive;
        default = cfg.memory.maxGiB;
        defaultText = lib.literalExpression "config.services.workbench.memory.maxGiB";
        description = "The most memory people may let their VM grow to.";
      };
      maxDiskGiB = mkOption {
        type = types.ints.positive;
        default = 128;
        description = "The largest disk people may ask for; at least 16 GiB.";
      };
      apps = mkOption {
        type = types.listOf types.str;
        default =
          lib.attrNames (lib.filterAttrs (_: a: a.default) cfg.apps)
          ++ lib.attrNames (lib.filterAttrs (_: a: !a.default) cfg.apps);
        defaultText = lib.literalMD "every app of `services.workbench.apps`, those a workspace has by default first";
        description = "The apps, of `services.workbench.apps`, people may give the workspaces they make, in the order offered.";
      };
      environmentCommand = mkOption {
        type = types.str;
        example = "/run/current-system/sw/bin/workspace-env";
        description = ''
          Run as root with the owner as its argument; prints `KEY=value`
          lines, the new workspace's secret environment, such as the app's
          credential. A failure, or nothing printed, refuses the request:
          no workspace is made without it.
        '';
      };
      ownerCommand = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "/run/current-system/sw/bin/workspace-owner";
        description = ''
          Run as root with an owner as its argument: exit 0 while they may
          keep their workspaces, 1 once they left; any other status, such as
          the directory being down, decides nothing. It is asked before a
          workspace is made and by the reconciliation timer, which stops the
          workspaces of those who left and keeps their disks. Null asks no
          one.
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
          if web.oidc.provider == "keycloak-oidc" then
            "${web.oidc.issuerUrl}/protocol/openid-connect/logout?id_token_hint={id_token}"
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

  config = lib.mkIf (cfg.enable && web.enable) {
    users = {
      users.workbench-web = {
        isSystemUser = true;
        group = "workbench-web";
        extraGroups = [ "workbench" ];
        description = "workbench web UI";
      };
      groups.workbench-web = { };
      # May use the VMs' sockets.
      groups.workbench = { };
      users.${config.services.nginx.user}.extraGroups = [ "workbench-web" ];
    };

    assertions = [
      {
        assertion = !(cfg.virtualHost.enableACME or false);
        message = "services.workbench.web: one virtual host serves every workspace, so `virtualHost` needs a wildcard certificate for *.${cfg.domain}, such as `useACMEHost`, not `enableACME`.";
      }
      {
        assertion =
          ss.enable
          -> lib.elem config.networking.firewall.checkReversePath [
            true
            "strict"
          ];
        message = "services.workbench.web.selfService: workspaces made at run time rely on the firewall's strict reverse path check to keep a guest to its own address.";
      }
      {
        # The host reads configs made at run time from there.
        assertion = ss.enable -> cfg.stateDir == "/var/lib/workbench";
        message = "services.workbench.web.selfService: needs the default stateDir, /var/lib/workbench.";
      }
      {
        # The upper half of the subnet is for workspaces made at run time.
        assertion =
          ss.enable
          -> lib.all (ws: lib.toInt (lib.last (lib.splitString "." ws.ip)) < 128) (
            lib.attrValues cfg.workspaces
          );
        message = "services.workbench.workspaces: with web.selfService, declared workspaces take addresses below ${cfg.network.subnet}.128; the rest are for workspaces made at run time.";
      }
      {
        assertion = ss.enable -> ss.maxDiskGiB >= 16;
        message = "services.workbench.web.selfService.maxDiskGiB: at least 16.";
      }
      {
        assertion =
          ss.enable
          ->
            lib.all (id: cfg.apps ? ${id}) ss.apps
            && lib.allUnique ss.apps
            && lib.allUnique (lib.filter (p: p != null) (map (id: cfg.apps.${id}.port) ss.apps));
        message = "services.workbench.web.selfService.apps: each must be one of services.workbench.apps, once, on a port of its own.";
      }
    ];

    services.redis.servers.workbench-oauth2 = {
      enable = true;
      port = 0;
      unixSocketPerm = 660;
      save = [ ];
    };

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir}/.stopped 0755 workbench-web workbench-web -"
    ]
    ++ lib.optionals ss.enable [
      "d ${cfg.stateDir}/configs 0755 root root -"
      "d ${cfg.stateDir}/.left 0755 root root -"
      # The VMs' users reach their own seed, by its name.
      "d ${cfg.stateDir}/.seeds 0711 root root -"
      # Root's, so the UI can't swap what root reads and writes in it: the
      # UI writes requests and quotas, root the outcomes.
      "d ${cfg.stateDir}/self-service 0755 root root -"
      "d ${cfg.stateDir}/self-service/requests 0750 workbench-web workbench-web -"
      "d ${cfg.stateDir}/self-service/quotas 0755 workbench-web workbench-web -"
      "d ${cfg.stateDir}/self-service/results 0755 root root -"
    ];

    systemd.services.workbench-web = {
      description = "workbench web UI";
      # It reads the template, with the apps it offers, when it starts.
      restartTriggers =
        lib.optional ss.enable
          config.environment.etc."workbench-self-service.json".source;
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      path = [ config.systemd.package ];
      serviceConfig = {
        ExecStart = lib.escapeShellArgs (
          [
            exe
            "--site=${package.site}"
            "--origin=${origin}"
            "--state-root=${cfg.stateDir}"
            "--oauth2-proxy=${oauth2Proxy}"
            "--cookie-name=${cookieName}"
            "--mem-reserve-percent=${toString cfg.memory.reservePercent}"
            "--mem-reserve-min-gib=${toString vmkit.memory.reserveMinGiB}"
          ]
          ++ map (a: "--admin=${a}") web.admins
          ++ map (g: "--admin-group=${g}") web.adminGroups
          ++ lib.optionals ss.enable (
            [ "--self-service=/etc/workbench-self-service.json" ] ++ map (g: "--creator-group=${g}") ss.groups
          )
          ++ map (p: "--pool=${p}") pools
        );
        User = "workbench-web";
        Group = "workbench-web";
        RuntimeDirectory = "workbench-web";
        RuntimeDirectoryMode = "0750";
        UMask = "0007";
        Restart = "always";
        RestartSec = 2;
        ReadWritePaths = [
          "${cfg.stateDir}/.stopped"
        ]
        ++ lib.optionals ss.enable [
          "${cfg.stateDir}/self-service/requests"
          "${cfg.stateDir}/self-service/quotas"
        ];
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

    # Start, stop and restart workspaces, make and destroy those made at
    # run time, and nothing else.
    security.polkit.enable = true;
    security.polkit.extraConfig = ''
      polkit.addRule(function(action, subject) {
        if (action.id == "org.freedesktop.systemd1.manage-units" && subject.user == "workbench-web") {
          var unit = action.lookup("unit"), verb = action.lookup("verb");
          if (/^workbench@[a-z][a-z0-9-]*\.service$/.test(unit) &&
              (verb == "start" || verb == "stop" || verb == "restart")) {
            return polkit.Result.YES;
          }
          ${lib.optionalString ss.enable ''
            if (/^workbench-(create|destroy|apps)@[a-z][a-z0-9-]*\.service$/.test(unit) && verb == "start") {
              return polkit.Result.YES;
            }
          ''}
        }
      });
    '';

    systemd.services.workbench-oauth2-proxy = {
      description = "Single sign-on for the workbench web UI and workspaces";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      serviceConfig = {
        ExecStart = lib.escapeShellArgs (
          [
            (lib.getExe pkgs.oauth2-proxy)
            "--http-address=127.0.0.1:${toString web.oauth2ProxyPort}"
            "--provider=${web.oidc.provider}"
            "--oidc-issuer-url=${web.oidc.issuerUrl}"
            "--client-id=${web.oidc.clientId}"
            "--client-secret-file=%d/client-secret"
            "--cookie-secret-file=%d/cookie-secret"
            "--cookie-name=${cookieName}"
            "--cookie-domain=.${cfg.domain}"
            "--cookie-samesite=lax"
            "--whitelist-domain=.${cfg.domain}"
            "--whitelist-domain=${cfg.domain}"
            "--redirect-url=${origin}/oauth2/callback"
            "--scope=${web.oidc.scope}"
            "--oidc-groups-claim=groups"
            # Sessions live in Redis, so a refresh keeps the cookie's ticket
            # and nginx need not pass a new cookie back.
            "--cookie-refresh=${web.sessionRefresh}"
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
            web.oidc.backendLogoutUrl != null
          ) "--backend-logout-url=${web.oidc.backendLogoutUrl}"
          ++ web.oidc.extraArgs
        );
        LoadCredential = [
          "client-secret:${web.oidc.clientSecretFile}"
          "cookie-secret:${web.oidc.cookieSecretFile}"
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

    services.nginx = {
      enable = true;
      recommendedProxySettings = true;
      virtualHosts = {
        ${cfg.domain} = cfg.virtualHost // {
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
        workbench-workspaces = cfg.virtualHost // {
          serverName = "~^(?<workbench_ws>[a-z][a-z0-9-]*?)(?:--(?<workbench_app>[a-z][a-z0-9]*))?\\.${escapedDomain}$";
          locations = workspaceLocations;
          extraConfig = (cfg.virtualHost.extraConfig or "") + ''
            if ($workbench_app = "") {
              return 302 ${origin}/?w=$workbench_ws;
            }
          '';
        };
      };
    };
  };
}
