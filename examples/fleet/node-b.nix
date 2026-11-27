# Node b: as a, by another name, on its own hardware.
{
  networking.hostName = "node-b";

  services.workbench.node = {
    enable = true;
    name = "b";
    network.externalInterface = "eno1";
    wireguard.privateKeyFile = "/run/secrets/wireguard";
  };
}
