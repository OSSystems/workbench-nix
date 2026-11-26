# Multi-node

The specification of workbench on several machines: a **front**, which
people reach and which decides, and **nodes**, which run the workspaces'
VMs. A node is plugged in by declaring it. A workspace lives on one node,
with its disk, until an admin moves it.

A guest sees no difference but its address, which a move changes:
`workspace.json`, the shares and the agent's contract stay as
[contracts.md](contracts.md) has them.

## Premises

- **Capacity, not availability.** A workspace is one long-lived VM whose
  disk is on one node. More nodes hold more workspaces; a node that fails
  takes its workspaces down until it is back. Backups and a redundant
  front come later.
- **Two roles, apart.** `services.workbench.front` and
  `services.workbench.node` are modules of their own;
  `services.workbench.fleet`, which every machine imports alike, is what
  they share. One machine may have both: a host on its own is a front and
  a node.
- **The front decides, nodes run.** Who owns a workspace, its node and
  address, its secrets, its SSH host key and the CAs are the front's. A
  node keeps its workspaces' disks and what runs them, and is given the
  rest at every start.
- **A node broken into reaches its own workspaces only:** never another
  node's workspaces' secrets, host keys or traffic, nor another node.
- **WireGuard, hub and spoke.** Each node is a peer of the front and of
  nothing else.
- **One build.** The front and the nodes are deployed together, always; a
  node of another build, such as one a failed deploy left behind, is not
  controlled.
- **No database**, as before: the registry is files on the front.

## Roles

```
people ──HTTPS──► front    nginx, oauth2-proxy, workbench-web, workbench-front;
                           the registry, the CAs and host keys, the secrets,
                           the devices, the quotas
                    │
                    │ WireGuard, hub and spoke
          ┌─────────┴─────────┐
       node a              node b              nodes never reach each other
    10.232.0.0/24       10.232.1.0/24
    workbench-node      workbench-node
    VMs and disks       VMs and disks
```

| | Front | Node |
|---|---|---|
| People reach | the UI, every app's address, the tunnel to SSH | nothing |
| Keeps | the registry, the secrets directories, the SSH CAs and host keys, the devices, the quotas, the closed nodes | the disks, the VM users, the taps, the seeds, the stop markers, the last config each workspace started with |
| Runs | `workbench-web`; `workbench-front`, which nodes ask; `workbench-create@`, `-destroy@` and `-move@`; the owners' reconciliation | `workbench@<ws>`, virtiofsd, the memory daemon; `workbench-node`, which the front asks |

`workbench-web` stays unprivileged and is what asks the nodes.
`workbench-front` is root's, because what a start needs, the secrets and
the host keys, is root's on the front; it listens on WireGuard only.

## The Nix modules

```nix
# fleet.nix, imported by the front and every node
services.workbench.fleet = {
  domain = "ws.example.com";
  wireguard.subnet = "10.231.0.0/24";   # the default
  front.wireguard = {
    address = "10.231.0.1";
    publicKey = "…";
    endpoint = "front.example.com:51820";
  };
  nodes = {
    a = { subnet = "10.232.0"; wireguard = { address = "10.231.0.10"; publicKey = "…"; }; };
    b = { subnet = "10.232.1"; wireguard = { address = "10.231.0.11"; publicKey = "…"; }; };
  };
  apps = { … };
  extraPackages = [ pkgs.claude-code ];
  allowedEndpoints = [ { address = "10.0.5.20"; port = 443; } ];
  workspaces.alice = { node = "a"; owner = "alice"; ip = "10.232.0.10"; };
};

# the front
services.workbench.front = {
  enable = true;
  virtualHost = { forceSSL = true; useACMEHost = "ws.example.com"; };
  wireguard.privateKeyFile = "/run/secrets/wireguard";
  admins = [ "alice" ];
  oidc = { … };
  selfService = { … };
};

# node b
services.workbench.node = {
  enable = true;
  name = "b";
  network.externalInterface = "enp6s0";
  wireguard.privateKeyFile = "/run/secrets/wireguard";
};
```

No option keeps its old name:

| Before | Now |
|---|---|
| `services.workbench.enable` | `front.enable`, `node.enable` |
| `domain`, `publicScheme`, `package`, `vmkitSource`, `apps`, `herdr`, `herdrCore`, `extraPackages`, `environment`, `allowedEndpoints`, `image`, `imageFormat`, `nixos`, `memory.bootMiB`, `memory.maxGiB`, `network.nameservers` | `fleet.*` |
| `workspaces.<ws>` | `fleet.workspaces.<ws>`, which also has `node` |
| `network.subnet` | `fleet.nodes.<n>.subnet` |
| `environmentFile`, `workspaces.<ws>.environmentFile`, `workspaces.<ws>.secretsDir` | `fleet.*`, paths on the front |
| `network.externalInterface`, `memory.reservePercent` | `node.*` |
| `virtualHost`, `web.*` | `front.*` |
| `stateDir` | `front.stateDir`, `node.stateDir` |

An old option fails the evaluation and says where it went.

The fleet's assertions hold on every machine: each workspace's node is
one of `nodes`, its `ip` in the lower half of that node's subnet, its
name none of those the state directory keeps; subnets and WireGuard
addresses are distinct, the addresses in `wireguard.subnet` and the
subnets outside it; a machine with `node.enable` is one of `nodes`, by
`node.name`.

WireGuard runs where a tunnel is needed: on the front when a node is on
another machine, and on every such node. The keys and the front's
`endpoint` are asserted there only, so a host on its own, a front and its
one node, declares no key. `front.wireguard.address` is the first address
of `wireguard.subnet` unless set; every node's address is declared, since
the front knows each node by it.

## Network

- **WireGuard**, `wg-workbench`, UDP port 51820: not `wb…`, which
  vmkit's `wb+` would take for a workspace's tap. The front peers with every
  node, with the node's WireGuard address and subnet as `AllowedIPs`; a
  node peers with the front only, with the front's address, and keeps the
  tunnel up (`PersistentKeepalive`), so the front needs no node's
  endpoint. The front forwards nothing from one peer to another.
- **Addresses.** A node's workspaces are in its `/24`, with the node at
  `.1` on every tap, as a host today: declared ones below `.128`, those
  made at run time from `.128`. A move gives a workspace an address in
  the new node's subnet, and so a new MAC and, for Ubuntu, a new seed:
  cloud-init sets the guest up again, as it does today when the owner's
  SSH keys change.
- **The front reaches guests** through WireGuard: nginx proxies to
  `<ip>:<port>` and the tunnel opens `<ip>:22`, as before. A node forwards
  from `wg-workbench` to its taps only what the front's WireGuard address sends,
  and the replies.
- **Guests** reach the internet, and not the host, private ranges or each
  other, as before. The WireGuard addresses are private, so a guest
  reaches neither the tunnel nor any node's or the front's API. A guest's
  address is inside its node's `AllowedIPs`, so the front takes a request
  from the node's WireGuard address only, never from the subnet behind it.
- **A machine with both roles** reaches its own node's guests through
  their taps and its node's API through a Unix socket, without WireGuard.

## The registry

The front's record of every workspace: its owner, its node and address,
its apps and sizes, and a move under way. It, never a node, says who owns
a workspace and where it is: the access check, the tunnel and the UI read
it, and a node's word for either is never taken.

- `/etc/workbench-front/<ws>.json` on the front, which Nix renders, for a
  declared workspace: where Nix means it to be. Not `/etc/workbench`,
  where a machine with both roles has its node's configs.
- `<stateDir>/registry/<ws>.json`, for every workspace: the node its disk
  is on, its address there and the move under way, and, for one made at
  run time, the rest of its record.

It is written atomically, under a lock of its own,
`<stateDir>/registry/.lock`: on a machine with both roles the node takes
self-service's while the front asks it. A name and an address are taken
by a `pending` record before any node is asked; one left `pending` is
cleaned up under the lock, and one whose node doesn't answer
stays, its name and address taken, until it does.

A node's own are not in it: the VM user's id, the tap, the seed.

## The node's API

`workbench-node`, root's, listens on the node's WireGuard address, port
7910, and answers the front's WireGuard address only. Requests and answers
are JSON; every request and answer has its sender's build in
`Workbench-Version`.
There is no request that runs a command of the caller's.

| Method | Path | What |
|---|---|---|
| `GET` | `/host` | memory (`MemTotal`, `MemAvailable`, the reserve, the pools), the disks' file system size and free space, the build |
| `GET` | `/workspaces` | every workspace the node has: what `workbench status` collects, with `booted.json` |
| `PUT` | `/workspaces/<ws>` | makes it from a record: the VM user, the tap, the seed and, unless the request is for a move, the disk |
| `DELETE` | `/workspaces/<ws>` | stops it and deletes its disk, VM user, tap and seed |
| `POST` | `/workspaces/<ws>/start`, `stop`, `restart` | as `workbench start`, `stop` (with its stop marker) and `restart`; `?wait=1` until systemd is done |
| `POST` | `/workspaces/<ws>/open-link` | the agent's `open-link`, over vsock |
| `GET` | `/workspaces/<ws>/console` | WebSocket to the serial console, shared as now; `?viewer=<name>` is who the others see |
| `GET` | `/workspaces/<ws>/disk` | the disk of a stopped workspace: its data only, holes skipped (`SEEK_DATA`), then the stream's sha256 |
| `PUT` | `/workspaces/<ws>/disk` | takes such a stream, and answers its sha256 |

A change of apps asks the node nothing: the record changes, and the next
start brings it. What waits for a restart is the difference between the
record and the node's `booted.json`, computed on the front as now.

## The front's API

`workbench-front`, root's, listens on the front's WireGuard address, port
7911, and answers the nodes' WireGuard addresses only.

`POST /start/<ws>` answers what a start needs, for a workspace the
registry has on the asking node and that isn't moving away from it:

- for one made at run time, its record;
- its secrets directory and secret environment: the files, and the
  secrets directory's environment file followed by every environment
  file, as `launch::collect_secrets` reads them;
- its SSH host key, a certificate for `<ws>.<domain>` signed now, and the
  user CA's public key.

Anything else is refused with the reason.

## Starting a workspace

`workbench run <ws>`, on the node:

1. asks the front, `POST /start/<ws>`; the unit fails, and systemd starts
   it again in 10 s, while the front can't be reached or refuses;
2. has its config: the node's Nix renders a declared one's, and one made
   at run time's is the node's template with the record;
3. writes the shares, the secrets from the answer, in the
   runtime directory, a tmpfs: nothing of them reaches the node's disk;
4. keeps the config it started with, for `workbench` on the node;
5. becomes cloud-hypervisor.

## Placement

A new workspace's node:

- **an admin picks it** in New workspace, which lists every node with its
  free memory, its workspaces' total maximum memory and its free disk; a
  closed or unreachable node, or one of another version, can't be picked;
- **otherwise**, and preselected for an admin, the open node with the
  most free memory now: `MemAvailable` less the reserve, what the host
  page shows.

That node must have at least the workspace's boot memory free, its disk
size free on disk, and as much memory as the workspace may grow to: the
host's memory is its node's, so a ceiling in GiB is checked against the
node's `MemTotal`, when it is made and when an admin changes it, and
`auto` is the node's own. Otherwise the request is refused: "No node has
room for it now. Ask an admin."

The node is picked once, at creation, and kept in the registry: another
rule later moves no one.

An admin closes and reopens a node on the host page; the front keeps it
in `<stateDir>/closed/<node>`. A closed node runs what it has, and takes
neither new workspaces nor moves.

## Moving a workspace

Admins move a workspace made at run time with Move on the host page, or
`workbench move <ws> <node>` on the front, to an open node of the same
version. `workbench-move@<ws>` runs it as root on the front, in phases the
registry keeps (`move`: source and its address, destination, phase, new
address, whether it ran):

1. **Reserve** an address in the destination's subnet, and make the
   workspace there without a disk (`PUT`).
2. **Stop** it on the source, noting whether it ran.
3. **Copy**: the front streams the source's `GET disk` into the
   destination's `PUT disk`; both sides' sha256 must be the stream's. A
   side silent for 2 minutes breaks it, as a node that lost power closes
   nothing; the destination renames the disk into place and syncs its
   directory before it answers.
4. **Switch**: the record's node and address become the destination's, in
   one write.
5. **Start** it on the destination if it ran, and wait for its agent's
   `ping`, up to 10 minutes.
6. **Clean up**: `DELETE` on the source.

A move that fails before the switch deletes the destination's copy and
starts the workspace again on the source if it ran. One that fails to
start switches back, then does the same. The source's disk goes in the
last phase only. A front that restarts in the middle picks the move up
from its phase: before the switch it goes back, after it goes on. Which
side of the switch it is on is the record's node: the source's or the
destination's.

While it moves, the workspace's actions are refused, its panel says
"Moving", and its apps answer a page that says so. It is down for as long
as the copy: only data travels, through the front, so 40 GiB at 1 Gb/s is
about 6 minutes.

## Declared workspaces

Nix places them: `node` and `ip` in the fleet. To move one, an admin
changes both and deploys. The old node stops it, as removing a workspace
from the configuration does, and keeps its disk; the front, finding its
disk on another node than Nix says, moves it through the same phases,
with the address Nix gives. The host page's Move refuses a declared
workspace, and says to change its `node`. An `ip` changed on the same
node is the record's at the deploy, and the VM's at its next start.

A declared workspace removed from Nix keeps its disk on its node, and the
host page says so, until `workbench destroy <ws>` runs there; at its next
setup, the front finds its node has it no more and forgets its record,
which frees its name.

## What people see

- **Owners** see nothing of nodes. A workspace being moved is **Moving**;
  one whose node doesn't answer is **Unreachable**; their actions wait.
- **Admins** see each workspace's node in its panel, pick a node in New
  workspace, and on the host page a section per node: its memory, its
  disk, its state (up, closed, unreachable, other version), Close or
  Reopen, and Move… on each workspace made at run time.
- `/api/workspaces` has the states `moving` and `unreachable`, and `node`
  for admins.

## Failures

| What | Then |
|---|---|
| A node doesn't answer | Its workspaces are Unreachable and their actions refused; their names and addresses stay taken. Their apps answer while their VMs run: the access check reads the registry, not the node. The tunnel to SSH refuses, since it can't tell the VM runs. |
| A node of another version | Not controlled, and the host page says so; its VMs run, and their apps answer. |
| The front is down | No one reaches any workspace. VMs keep running; a node that restarts waits, its workspaces with it, until the front is back. |
| A create stops half way | Its `pending` record is cleaned up under the lock, and what the node made of it deleted. |
| A move stops half way | It is picked up from its phase. |
| A node is lost for good | Its workspaces' disks are lost: there are no backups yet. An admin forgets them, `workbench forget <ws>`, which frees their names. |

## Security

- The front's API answers a node only for that node's workspaces; a node
  never gets another's secrets or host keys.
- A node's API answers the front only, and runs no command it is given.
- The user CA stays with `workbench-web`, and the host CA and the host
  keys with root, on the front. Nodes hold neither CA.
- A node has a workspace's secrets only while it starts and runs it, in a
  tmpfs.
- Guests never reach WireGuard.
- Only root opens a connection to the front's API through a node's
  tunnel, and only root and `workbench-web` to a node's API through the
  front's: every local user has the machine's WireGuard address, so
  `OUTPUT` owner rules, not the address, keep a VM's user or nginx out.
- A node broken into has its workspaces' disks, their secrets while they
  run and their host keys, and may lie about their state. It can't claim
  another workspace, reach another node or get another's secrets.

## From one host to a front and a node

1. The host becomes node `a`, its `network.subnet` the fleet's
   `nodes.a.subnet`: its VMs, disks and seeds stay.
2. A new machine becomes the front. It takes from the host, once, the SSH
   CAs and host keys (`/var/lib/workbench/ssh`), the configs made at run
   time, as records on `a` (`workbench front import a <their copy>`), the
   quotas, the devices (`/var/lib/workbench-web/devices.json`) and the
   secrets directories, whose sops secrets are encrypted for it.
3. `<domain>` and `*.<domain>` point at the front.
4. Both are deployed. A deploy restarts no VM (`workbench@` has
   `restartIfChanged = false`): each workspace asks the front at its next
   start, whenever that is, and is refused until the front has its record.

A host that stays on its own is a front and its one node: at the deploy,
`workbench front setup` makes a record of each of its node's configs
made at run time that the registry lacks, under the registry's lock, as
`workbench front import` would, once. Its owner keeps the workspace's
list row, apps and SSH, and its next start asks the front, which has it.
Everything else stays where it is: the CAs and host keys, the quotas, the
devices, the secrets directories, the seeds, the stop markers and the
marks of owners who left, an empty one of which starts the workspace
again when its owner comes back unless its node keeps it stopped.

## Tests

- Rust: the registry and its `pending` records; the move's phases, its
  failures and its resumption at each phase; placement; that the front
  answers a node only for its own workspaces, and only from its WireGuard
  address; the disk stream and its hash; and a config rendered from a
  template and a record being the one its request makes.
- NixOS, `checks.multi-node`: a front, with OpenLDAP, Dex and
  oauth2-proxy as `checks.workspaces` has them, nodes `a` and `b`, and a
  client. The declared `bob` boots on `b` with the front's secret and host
  certificate; alice's workspace made at run time lands on `b`, which has
  the most free memory, and `workbench move` takes it to `a` with a file
  written before, a new address and no disk left on `b`. A node gets only
  its own workspaces' starts, even one routing the other's addresses into
  its tunnel reaches nothing through the front, no guest reaches
  WireGuard or any API, and a node of another build takes no new
  workspace. From the client, signed in: bob reaches his app on `b`
  through the front's nginx, and its SSH server through
  workbench-connect's tunnel; an admin's pick in New workspace lands on
  `b`; with `b`'s `workbench-node` stopped, its workspaces are Unreachable
  in `/api/workspaces` and their apps still answer; with `b` closed on the
  host page, a new workspace goes to `a`; and Move from the UI takes one
  from `b` to `a` with its data.
- The other checks run with both roles on one machine; `checks.fleet`
  evaluates a front alone, a node alone and both, and every assertion.

## What the modules render

What this crate reads and runs, for the `fleet`, `front` and `node`
modules. A host on its own has both settings files, a front and its one
node. JSON files are read with unknown fields refused.

### Ports and sockets

| What | Where | Who reaches it |
|---|---|---|
| WireGuard | `wg-workbench`, UDP 51820 | the front and its nodes |
| The node's API | its WireGuard address, TCP 7910 | the front's WireGuard address; it refuses any other |
| The front's API | its WireGuard address, TCP 7911 | the nodes' WireGuard addresses; it refuses any other, a guest behind a node too |
| `/run/workbench-node/api.sock` | a machine with both roles | root and the node's `socketGroup` (`workbench`), `0660` |
| `/run/workbench-front/api.sock` | a machine with both roles | root, `0600`: the local node's `workbench run` |
| `/run/workbench-node/console.sock` | every node | root, `0600`: `workbench console` on the node |

Every request and answer carries `Workbench-Version`: the build, which
must be the same on both sides, else it is refused with `409`. It is
`WORKBENCH_BUILD` when the package is built, an identifier of the source
it is built from, of visible ASCII without spaces; without it, as for
`cargo build` and the tests, the crate's version. The `nix` packages,
`workbench` and `workbench-web`, set it to a hash of their source and
vmkit's, so the front and every node of one deploy share it, and it
changes with the code only: a commit to the docs makes no other build.

Workspace names the host keeps for its own directories in its state
directory are refused: `closed`, `configs`, `registry`, `self-service`
and `ssh`. The fleet's assertions should refuse them for declared
workspaces too.

### On a node

`/etc/workbench-node.json`:

```json
{
  "name": "b",
  "listen": "10.231.0.11:7910",
  "socket": "",
  "socketGroup": "",
  "frontAddress": "10.231.0.1",
  "front": "10.231.0.1:7911",
  "stateDir": "/var/lib/workbench",
  "template": "/etc/workbench-self-service.json",
  "configDirs": ["/etc/workbench", "/var/lib/workbench/configs"],
  "reservePercent": 10,
  "reserveMinGiB": 2,
  "pools": [{ "name": "workspaces", "status": "/run/vmkit/memory/workspaces.json" }]
}
```

| Field | Meaning |
|---|---|
| `name` | The node, one of `fleet.nodes`. |
| `listen` | Where its API listens, `<WireGuard address>:7910`; absent for none. |
| `socket`, `socketGroup` | The API's Unix socket on a machine with both roles, and the group that may use it; empty for none. |
| `frontAddress` | The front's WireGuard address, the only one the API answers. |
| `front` | The front's API: `<address>:7911`, or `/run/workbench-front/api.sock` on a machine with both roles. |
| `stateDir`, `configDirs` | Where its disks and stop markers are, and its configs; `configDirs` defaults to `/etc/workbench` and `<stateDir>/configs`. |
| `template` | The template of workspaces made at run time. |
| `reservePercent`, `reserveMinGiB`, `pools` | What `GET /host` reports: the memory daemon's reserve, and the pools' status files. |

`/etc/workbench-self-service.json`, on every node: the template of
workspaces made at run time, with the node's `subnet` and paths, and `limits.bootMemMiB`; a node
signs no key and runs no command, so `ssh`, `environmentCommand` and
`ownerCommand` are absent and `environmentFiles` empty. `secretsRoot` is
unused, a clean path. It has no `socketGroup`: the VMs' sockets are their
users' and root's, `workbench-node` shares the consoles, and the web UI
asks the node. Its `apps` are all the fleet's, as the front's settings
have them, and its `images` one for each guest system, since a node can't
tell which the front offers. Of `limits` it reads the boot memory only:
what people may make is the front's.

`/etc/workbench/<ws>.json`: the configs of the declared workspaces whose
`node` it is, without `ssh` and `environmentFiles`: the front
gives the secrets and the host key at every start.

| Unit | Runs | Notes |
|---|---|---|
| `workbench-node.service` | `workbench node serve` | root; `RuntimeDirectory=workbench-node`; after `wg-workbench`; `Restart=always` |
| `workbench-node-setup.service` | `workbench node setup` | at boot and when the template changes: puts back the users and taps of workspaces made at run time, and starts them |
| `workbench@<ws>.service` | `workbench run %i` | asks the front; `Restart=always`, `RestartSec=10`, `StartLimitIntervalSec=0` so it keeps trying |
| `workbench-memory.service` | `workbench memory` | grows and shrinks the VMs' memory |

`--settings <file>` gives `workbench node` another settings file.

### On the front

`/etc/workbench-front.json`:

```json
{
  "domain": "ws.example.com",
  "scheme": "https",
  "stateDir": "/var/lib/workbench",
  "listen": "10.231.0.1:7911",
  "socket": "",
  "localNode": "",
  "nodes": {
    "a": { "address": "10.231.0.10", "subnet": "10.232.0", "api": "10.231.0.10:7910" },
    "b": { "address": "10.231.0.11", "subnet": "10.232.1", "api": "10.231.0.11:7910" }
  },
  "declared": "/etc/workbench-front",
  "apps": [{ "id": "t3code", "name": "T3 Code", "port": 3773, "bundle": "/nix/store/…-t3code-app" }],
  "defaultApps": ["t3code"],
  "ssh": {
    "keygen": "/nix/store/…/bin/ssh-keygen",
    "hostCa": "/var/lib/workbench/ssh/host-ca",
    "userCa": "/var/lib/workbench/ssh/user-ca.pub",
    "hostKeys": "/var/lib/workbench/ssh/hosts"
  },
  "selfService": {
    "limits": { "os": ["ubuntu"], "bootMemMiB": 2048, "maxMemoryGiB": 16, "maxDiskGiB": 128, "quota": 1 },
    "apps": ["t3code"],
    "secretsRoot": "/var/lib/workbench-secrets",
    "environmentFiles": [],
    "environmentCommand": "/path/to/workspace-env",
    "ownerCommand": ""
  }
}
```

| Field | Meaning |
|---|---|
| `domain`, `scheme`, `stateDir` | Workspaces are `<scheme>://<ws>.<domain>`; where the registry and the rest are. |
| `listen`, `socket` | Where its API listens: `<WireGuard address>:7911`, and `/run/workbench-front/api.sock` on a machine with both roles. |
| `localNode` | The node on this machine, which asks through the socket; `""` for none. |
| `nodes.<n>` | Each node's WireGuard `address`, `subnet` and `api`: `<address>:7910`, or `/run/workbench-node/api.sock` for the local one. |
| `declared` | The directory of the declared workspaces' records. |
| `apps`, `defaultApps` | The fleet's apps as the nodes' templates have them, bundles included: what waits for a restart is told from them. |
| `ssh` | The CAs and the guests' host keys, root's; absent for none. |
| `selfService` | Absent unless people make workspaces: the limits, the apps offered by id, where their secrets go and the environment files they all have, and the two commands. |

`/etc/workbench-front/<ws>.json`, for each declared workspace, `0644`:

```json
{
  "name": "alice", "owner": "alice", "node": "a", "ip": "10.232.0.10",
  "os": "ubuntu", "apps": ["t3code"], "maxMemoryGiB": 16, "diskGiB": 64,
  "sshAuthorizedKeys": ["ssh-ed25519 …"],
  "secretsDir": "/var/lib/workbench-secrets/alice",
  "environmentFiles": ["/run/secrets/llm.env"]
}
```

`<stateDir>` on the front holds `registry/` (root's, `0755`, records
`0644`, which `workbench-web` reads), `closed/<node>` (`workbench-web`'s),
`.left/<ws>`, and `self-service/`: the requests, the outcomes and the
quotas, `requests/<ws>.move.json` and `results/<ws>.move.json` among
them. The secrets directories
(`<secretsRoot>/<ws>`, `0700`) and `ssh/` are root's.

| Unit | Runs | Notes |
|---|---|---|
| `workbench-front.service` | `workbench front serve` | root; `RuntimeDirectory=workbench-front`; after `wg-workbench`; `Restart=always` |
| `workbench-front-setup.service` | `workbench front setup` | oneshot, at boot and on every deploy that changes the settings or a declared record: cleans up, makes records of the local node's configs made at run time that the registry lacks, follows the declared workspaces' nodes and addresses, and forgets those Nix removed once their node has them no more |
| `workbench-create@`, `-destroy@`, `-apps@`, `-memory@` | `workbench self-service create %i`, and `destroy`, `apps`, `memory` | through the registry and the nodes; polkit lets `workbench-web` start them |
| `workbench-move@<ws>.service` | `workbench self-service move %i` | oneshot, `TimeoutStartSec=infinity`; polkit lets `workbench-web` start it |
| `workbench-self-service-reconcile` | `workbench self-service reconcile` | the timer |

On the front, as root: `workbench move <ws> <node>`, `workbench forget
<ws>`, `workbench destroy <ws>` and once, when a host becomes the fleet's
first node, `workbench front import <node> [/var/lib/workbench/configs]`,
which needs `selfService`: those workspaces' secrets and starts are its.
None takes the state directory's own names. `workbench move` reports the
outcome of this move only. `--settings <file>` gives `workbench front`
another settings file.

`workbench destroy <ws>` asks for the name typed again. On the front, for
a workspace made at run time, it does what the web UI's Destroy does: its
node deletes its disk, VM user, tap and seed, and the front its record,
secrets directory and host key; it is refused while the workspace moves.
On a node, it refuses a workspace made at run time and says to run it on
the front. A declared workspace's destroy deletes its disk on its node,
and on a machine with both roles its host key on the front.

### For the web UI

`workbench::fleet::front::Fleet` is what the UI runs on: `list`,
`nodes_view`, `control`, `open_link`, `console`, `config` and
`ssh_address` for the access check and the tunnel, `limits`, `set_closed`
and `ask_move`. It makes, destroys and changes workspaces by the request
and the unit; a request's `node` is an admin's pick. Its types are
`workbench_model::fleet`'s.

## Later

- Backups on another node, and bringing a lost node's workspaces back
  from them.
- A redundant front.
- Placement by priority or weight.
- A move that copies while the VM runs, then only what changed.

## Open

- A front without single sign-on, which `web.enable = false` made, is no
  more: the registry and the nodes' control live in `workbench-web`.
- Declared workspaces move by changing Nix, not from the UI.
- A move copies the disk through the front, twice its link's traffic.
