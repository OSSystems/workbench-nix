# The fleet's WireGuard, hub and spoke: the front peers with every node on
# another machine, a node with the front only. The front forwards nothing
# between them; a node forwards to its guests only what the front's
# address sends, and the replies. Each API answers through the tunnel
# only the other side's address, which only root, and on the front the
# web UI, may connect from; no guest reaches the tunnel. A machine with
# both roles reaches its own node without it.
{
  config,
  lib,
  pkgs,
  utils,
  ...
}:

let
  fleet = config.services.workbench.fleet;
  front = config.services.workbench.front;
  node = config.services.workbench.node;
  wb = import ./lib.nix { inherit config lib pkgs; };
  inherit (wb.wireguard) interface port;

  frontAddress = fleet.front.wireguard.address;
  # The nodes the front reaches through the tunnel.
  remote = lib.filterAttrs (name: _: name != wb.localNode) fleet.nodes;
  isFront = front.enable;
  isNode = node.enable && !front.enable;
  up = (isFront && remote != { }) || isNode;
  self = fleet.nodes.${node.name} or { wireguard.address = "0.0.0.0"; };
  address = if isFront then frontAddress else self.wireguard.address;
  privateKeyFile = if isFront then front.wireguard.privateKeyFile else node.wireguard.privateKeyFile;

  peers =
    if isFront then
      lib.mapAttrsToList (_: n: {
        PublicKey = n.wireguard.publicKey;
        AllowedIPs = [
          "${n.wireguard.address}/32"
          "${n.subnet}.0/24"
        ];
      }) remote
    else
      [
        {
          PublicKey = fleet.front.wireguard.publicKey;
          AllowedIPs = [ "${frontAddress}/32" ];
          Endpoint = fleet.front.wireguard.endpoint;
          # The front needs no node's endpoint: each keeps its tunnel up.
          PersistentKeepalive = 25;
        }
      ];

  # What comes in through the tunnel: replies, and each side's API from
  # the other's address only.
  inRules =
    if isFront then
      lib.concatMapStrings (
        n:
        "iptables -A workbench-wg-in -s ${n.wireguard.address} -p tcp --dport ${toString wb.frontPort} -j ACCEPT\n"
      ) (lib.attrValues remote)
    else
      "iptables -A workbench-wg-in -s ${frontAddress} -p tcp --dport ${toString wb.nodePort} -j ACCEPT\n";

  # Who opens a connection to the other side's API through the tunnel:
  # root, which runs the front's units and `workbench run` on a node, and
  # on the front the web UI. Any other local user, a VM's own among them,
  # has the machine's address but gets no answer.
  outRules =
    let
      api = toString (if isFront then wb.nodePort else wb.frontPort);
      allow = [ "0" ] ++ lib.optional isFront "workbench-web";
    in
    lib.concatMapStrings (
      user:
      "iptables -A workbench-wg-out -p tcp --dport ${api} -m conntrack --ctstate NEW -m owner --uid-owner ${user} -j RETURN\n"
    ) allow
    + "iptables -A workbench-wg-out -p tcp --dport ${api} -m conntrack --ctstate NEW -j REJECT --reject-with tcp-reset\n";

  # What the tunnel forwards: nothing on the front; on a node, the front's
  # traffic to its guests.
  forward =
    if isFront then
      ''
        iptables -D FORWARD -i ${interface} -j DROP 2>/dev/null || true
        iptables -I FORWARD -i ${interface} -j DROP
        iptables -D FORWARD -o ${interface} -j DROP 2>/dev/null || true
        iptables -I FORWARD -o ${interface} -j DROP
      ''
    else
      ''
        iptables -N workbench-wg-fwd 2>/dev/null || iptables -F workbench-wg-fwd
        iptables -A workbench-wg-fwd -s ${frontAddress} -o wb+ -j ACCEPT
        iptables -A workbench-wg-fwd -j DROP
        iptables -D FORWARD -i ${interface} -j workbench-wg-fwd 2>/dev/null || true
        iptables -I FORWARD -i ${interface} -j workbench-wg-fwd
      '';
in
{
  config = lib.mkIf up {
    assertions = [
      {
        assertion = privateKeyFile != null;
        message =
          if isFront then
            "services.workbench.front.wireguard.privateKeyFile: the front has nodes on other machines, which it reaches over WireGuard."
          else
            "services.workbench.node.wireguard.privateKeyFile: the node reaches the front over WireGuard.";
      }
    ]
    ++ (
      if isFront then
        lib.mapAttrsToList (name: n: {
          assertion = n.wireguard.publicKey != null;
          message = "services.workbench.fleet.nodes.${name}.wireguard.publicKey: the front reaches node ${name} over WireGuard.";
        }) remote
      else
        [
          {
            assertion = fleet.front.wireguard.publicKey != null && fleet.front.wireguard.endpoint != null;
            message = "services.workbench.fleet.front.wireguard: a node on another machine than the front needs its publicKey and endpoint.";
          }
        ]
    );

    systemd.network = {
      enable = true;
      wait-online.enable = lib.mkDefault false;
      netdevs."30-${interface}" = {
        netdevConfig = {
          Kind = "wireguard";
          Name = interface;
        };
        wireguardConfig = {
          PrivateKeyFile = privateKeyFile;
          ListenPort = port;
          # The peers' AllowedIPs are routed through the tunnel.
          RouteTable = "main";
        };
        wireguardPeers = peers;
      };
      networks."30-${interface}" = {
        matchConfig.Name = interface;
        address = [ "${address}/32" ];
        networkConfig = {
          LinkLocalAddressing = "no";
          IPv6AcceptRA = false;
        };
        linkConfig.RequiredForOnline = "no";
      };
    };

    # The APIs wait for their address.
    systemd.services =
      let
        device = "sys-subsystem-net-devices-${utils.escapeSystemdPath interface}.device";
      in
      lib.genAttrs (lib.optional isFront "workbench-front" ++ lib.optional isNode "workbench-node") (_: {
        wants = [ device ];
        after = [ device ];
      });

    networking.firewall = {
      allowedUDPPorts = [ port ];
      extraCommands = ''
        iptables -N workbench-wg-in 2>/dev/null || iptables -F workbench-wg-in
        iptables -A workbench-wg-in -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
      ''
      + inRules
      + ''
        iptables -A workbench-wg-in -j DROP
        iptables -D INPUT -i ${interface} -j workbench-wg-in 2>/dev/null || true
        iptables -I INPUT -i ${interface} -j workbench-wg-in
        iptables -N workbench-wg-out 2>/dev/null || iptables -F workbench-wg-out
      ''
      + outRules
      + ''
        iptables -D OUTPUT -o ${interface} -j workbench-wg-out 2>/dev/null || true
        iptables -I OUTPUT -o ${interface} -j workbench-wg-out
      ''
      + forward;
      extraStopCommands = ''
        iptables -D INPUT -i ${interface} -j workbench-wg-in 2>/dev/null || true
        iptables -D OUTPUT -o ${interface} -j workbench-wg-out 2>/dev/null || true
        iptables -D FORWARD -i ${interface} -j workbench-wg-fwd 2>/dev/null || true
        iptables -D FORWARD -i ${interface} -j DROP 2>/dev/null || true
        iptables -D FORWARD -o ${interface} -j DROP 2>/dev/null || true
      '';
    };

    # A node's guests answer the front through the tunnel, and send nothing
    # else into it.
    services.vmkit.networks = lib.mkIf isNode {
      workbench.forwardRules = lib.mkBefore ''
        iptables -A wb-fwd -o ${interface} -d ${frontAddress} -m conntrack --ctstate ESTABLISHED,RELATED -j RETURN
        iptables -A wb-fwd -o ${interface} -j DROP
      '';
    };

    networking.networkmanager.unmanaged = lib.mkIf config.networking.networkmanager.enable [
      "interface-name:${interface}"
    ];
  };
}
