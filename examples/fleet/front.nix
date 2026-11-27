# The front: the machine people reach. It signs them in, serves the web UI
# and every app's address, keeps the registry, the CAs and the secrets,
# and is the hub of the tunnel.
{ pkgs, ... }:
{
  networking.hostName = "front";
  networking.firewall.allowedTCPPorts = [
    80
    443
  ];

  # *.ws.example.com, which needs a DNS challenge.
  security.acme = {
    acceptTerms = true;
    defaults.email = "admin@example.com";
    certs."ws.example.com" = {
      extraDomainNames = [ "*.ws.example.com" ];
      dnsProvider = "cloudflare";
      environmentFile = "/run/secrets/acme-dns";
      group = "nginx";
    };
  };

  services.workbench.front = {
    enable = true;
    virtualHost = {
      forceSSL = true;
      useACMEHost = "ws.example.com";
    };
    # wg genkey; readable by systemd-network, such as a sops secret.
    wireguard.privateKeyFile = "/run/secrets/wireguard";
    adminGroups = [ "workbench-admins" ];
    oidc = {
      issuerUrl = "https://sso.example.com/realms/example";
      clientSecretFile = "/run/secrets/oidc-client";
      cookieSecretFile = "/run/secrets/oidc-cookie";
    };
    # Members of `workbench` make their own workspaces in the UI.
    selfService = {
      enable = true;
      groups = [ "workbench" ];
      # Prints a new workspace's secret environment, such as its owner's
      # LLM key, from the front's secrets: the node gets it at each start.
      environmentCommand = toString (
        pkgs.writeShellScript "workspace-env" ''
          printf 'ANTHROPIC_API_KEY=%s\n' "$(cat /run/secrets/llm-keys/"$1")"
        ''
      );
    };
  };
}
