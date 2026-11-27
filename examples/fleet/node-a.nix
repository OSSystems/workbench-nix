# Node a: it runs its workspaces' VMs and keeps their disks. Only the
# front reaches it, through the tunnel it keeps up.
{
  networking.hostName = "node-a";

  services.workbench.node = {
    enable = true;
    # Its name in the fleet's `nodes`.
    name = "a";
    # The workspaces' NAT to the internet.
    network.externalInterface = "enp6s0";
    # wg genkey; its public key is the fleet's nodes.a.
    wireguard.privateKeyFile = "/run/secrets/wireguard";
  };
}
