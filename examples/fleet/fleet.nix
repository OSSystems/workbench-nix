# The fleet: the same on the front and every node. A node is plugged in by
# adding it to `nodes` and deploying every machine.
{ pkgs, ... }:
{
  services.workbench.fleet = {
    # One wildcard certificate, on the front, covers it and *.ws.example.com.
    domain = "ws.example.com";

    # The tunnel: 10.231.0.0/24 by default, the front its first address.
    front.wireguard = {
      # wg pubkey < the front's private key
      publicKey = "FRONT_PUBLIC_KEY=";
      # Where nodes reach the front, UDP 51820.
      endpoint = "front.example.com:51820";
    };

    # Each node: the /24 of its workspaces' addresses, and its own in the
    # tunnel, which the front takes its requests from.
    nodes = {
      a = {
        subnet = "10.232.0";
        wireguard = {
          address = "10.231.0.10";
          publicKey = "A_PUBLIC_KEY=";
        };
      };
      b = {
        subnet = "10.232.1";
        wireguard = {
          address = "10.231.0.11";
          publicKey = "B_PUBLIC_KEY=";
        };
      };
    };

    # What every workspace has, wherever it runs.
    extraPackages = [ pkgs.git ];
    environment.EDITOR = "vim";

    # Workspaces declared here live on the node they name, at an address
    # below .128 of its subnet; those people make in the UI go where
    # there is room, from .128.
    workspaces = {
      alice = {
        node = "a";
        owner = "alice";
        ip = "10.232.0.10";
      };
      bob = {
        node = "b";
        owner = "bob";
        ip = "10.232.1.10";
        # A path on the front, which gives it to b at each start.
        environmentFile = "/run/secrets/bob.env";
      };
    };
  };
}
