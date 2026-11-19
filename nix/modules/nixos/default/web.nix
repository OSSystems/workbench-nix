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

  origin = "https://${cfg.domain}";
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

  # A workspace lets in only its owner and admins, and its app gets the
  # browser's cookies without the SSO one.
  workspaceLocations = name: {
    "/oauth2/" = oauth2Location;
    "= /_workbench/authz" = {
      proxyPass = "http://unix:${run}/authz.sock:/authz";
      recommendedProxySettings = false;
      extraConfig = ''
        internal;
        proxy_pass_request_body off;
        proxy_set_header Content-Length "";
        proxy_set_header Cookie $http_cookie;
        proxy_set_header X-Workbench-Workspace ${name};
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
        rewrite ^ /denied?ws=${name}? break;
        proxy_set_header X-Workbench-User $workbench_user;
      '';
    };
    "/".extraConfig = ''
      auth_request /_workbench/authz;
      auth_request_set $workbench_user $upstream_http_x_workbench_user;
      auth_request_set $workbench_cookie $upstream_http_x_workbench_cookie;
      error_page 401 = @workbench_sign_in;
      error_page 403 = @workbench_denied;
      proxy_set_header Cookie $workbench_cookie;
      # An app's cookies are its host's: one for the whole domain could
      # stand in for the single sign-on cookie.
      proxy_cookie_domain ~.+ $host;
    '';
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
      description = "Users who see and control every workspace, and the host page.";
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

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir}/.stopped 0755 workbench-web workbench-web -"
    ];

    systemd.services.workbench-web = {
      description = "workbench web UI";
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
          ++ map (p: "--pool=${p}") pools
        );
        User = "workbench-web";
        Group = "workbench-web";
        RuntimeDirectory = "workbench-web";
        RuntimeDirectoryMode = "0750";
        UMask = "0007";
        Restart = "always";
        RestartSec = 2;
        ReadWritePaths = [ "${cfg.stateDir}/.stopped" ];
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

    # Start, stop and restart workspaces, and nothing else.
    security.polkit.enable = true;
    security.polkit.extraConfig = ''
      polkit.addRule(function(action, subject) {
        if (action.id == "org.freedesktop.systemd1.manage-units" && subject.user == "workbench-web") {
          var unit = action.lookup("unit"), verb = action.lookup("verb");
          if (/^workbench@[a-z][a-z0-9-]*\.service$/.test(unit) &&
              (verb == "start" || verb == "stop" || verb == "restart")) {
            return polkit.Result.YES;
          }
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
            "--scope=openid email profile"
            "--code-challenge-method=S256"
            "--email-domain=*"
            # Who signs in is preferred_username; the email decides nothing,
            # and Keycloak marks the emails it reads from LDAP unverified.
            "--insecure-oidc-allow-unverified-email"
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
                error_page 401 = @workbench_sign_in;
                proxy_set_header X-Workbench-User $workbench_user;
                # Live updates are a long response.
                proxy_buffering off;
                proxy_read_timeout 1d;
              '';
            };
          };
        };
      }
      // lib.mapAttrs' (
        name: _: lib.nameValuePair "${name}.${cfg.domain}" { locations = workspaceLocations name; }
      ) cfg.workspaces;
    };
  };
}
