# workbench-nix

**Persistent, isolated per-person workspaces** on NixOS: one long-lived
[cloud-hypervisor](https://www.cloudhypervisor.org/) VM each, an Ubuntu
24.04 or NixOS guest the person owns, running the apps they open from
outside it: in the browser, each at `https://<workspace>--<app>.<domain>`,
or from their own terminal, over SSH through the front. A person may have
several workspaces.

workbench runs as a **fleet**: a **front**, which people reach and which
decides, and **nodes**, which run the workspaces' VMs and keep their
disks. One machine may be both: a host on its own is a front and a node.
A node is plugged in by declaring it; a workspace lives on one node until
an admin moves it. [docs/fleet.md](docs/fleet.md) shows it at work, with
the web UI's screens; [docs/multi-node.md](docs/multi-node.md) has the
whole design.

The fleet owns everything around the apps: the VM, its disk, network
isolation, secrets, tools and memory. The apps are what the person opens
from outside the workspace; see [Apps](#apps).

Built on [vmkit-nix](https://github.com/OSSystems/vmkit-nix): workspaces share a
node with other vmkit controllers only through its guest networks and
memory pools.

## Usage

Every machine imports the module and the same `fleet`; the front enables
`front`, each node `node`, with its own name.

```nix
{
  inputs = {
    vmkit.url = "github:OSSystems/vmkit-nix";
    workbench = {
      url = "github:OSSystems/workbench-nix";
      inputs.vmkit.follows = "vmkit";
    };
  };

  # in each machine's NixOS configuration
  imports = [ inputs.workbench.nixosModules.default ./fleet.nix ];
}
```

```nix
# fleet.nix: the same on the front and every node
{ pkgs, ... }:
{
  services.workbench.fleet = {
    domain = "ws.example.com";            # one wildcard certificate covers it
    front.wireguard = {
      address = "10.231.0.1";
      publicKey = "…";
      endpoint = "front.example.com:51820";
    };
    nodes = {
      a = { subnet = "10.232.0"; wireguard = { address = "10.231.0.10"; publicKey = "…"; }; };
      b = { subnet = "10.232.1"; wireguard = { address = "10.231.0.11"; publicKey = "…"; }; };
    };
    environment.ANTHROPIC_BASE_URL = "https://llm-proxy.example.com";
    allowedEndpoints = [ { address = "10.0.5.20"; port = 443; } ];  # LLM proxy
    extraPackages = [ pkgs.claude-code ];

    workspaces = {
      alice = { node = "a"; owner = "alice"; ip = "10.232.0.10"; };
      alice-scratch = { node = "a"; owner = "alice"; ip = "10.232.0.11"; sudo = false; diskGiB = 128; };
      bob = {
        node = "b";
        owner = "bob";
        ip = "10.232.1.10";
        environmentFile = "/run/secrets/bob-llm.env";   # a path on the front
      };
      carol = { node = "b"; owner = "carol"; ip = "10.232.1.11"; os = "nixos"; };  # see NixOS below
    };
  };
}
```

```nix
# the front
services.workbench.front = {
  enable = true;
  virtualHost = { forceSSL = true; useACMEHost = "ws.example.com"; };
  wireguard.privateKeyFile = "/run/secrets/wireguard";
  admins = [ "alice" ];
  oidc = { … };                          # see Web UI below
};

# node b
services.workbench.node = {
  enable = true;
  name = "b";
  network.externalInterface = "enp6s0";
  wireguard.privateKeyFile = "/run/secrets/wireguard";
};
```

A host on its own has both roles and one node, which it reaches through
sockets: no WireGuard, no keys.

```nix
services.workbench = {
  fleet = {
    domain = "ws.example.com";
    nodes.a = { subnet = "10.232.0"; wireguard.address = "10.231.0.10"; };
    workspaces.alice = { node = "a"; owner = "alice"; ip = "10.232.0.10"; };
  };
  front = { enable = true; virtualHost = { … }; oidc = { … }; };
  node = { enable = true; name = "a"; network.externalInterface = "enp6s0"; };
};
```

### The front and its nodes

- **WireGuard, hub and spoke**, on `wg-workbench`, UDP port 51820, in
  `fleet.wireguard.subnet` (`10.231.0.0/24` by default). The front peers
  with every node on another machine and forwards nothing between them;
  a node peers with the front only and keeps the tunnel up, so the front
  needs no node's endpoint. The private keys are files the machines are
  given, such as sops secrets, readable by `systemd-network`; the public
  keys are in the fleet (`wg genkey | tee key | wg pubkey`).
- **The front decides.** Its registry, `/var/lib/workbench/registry`, says
  who owns each workspace and which node it is on; it keeps the secrets
  directories, the SSH CAs and host keys, the devices and the quotas.
  `workbench-front` answers a node's start, from the node's WireGuard
  address only and only for that node's workspaces, with what the start
  needs: the secrets, a host key signed for `<workspace>.<domain>` and the
  user CA.
- **A node runs.** `workbench-node`, which answers the front's WireGuard
  address only, makes, starts, stops and deletes its workspaces and
  streams their disks. A node keeps the disks, the VM users, the taps and
  the seeds, and holds a workspace's secrets only in its runtime
  directory while it runs. A start the front refuses, or one while the
  front can't be reached, is tried again every 10 seconds.
- **One build.** The front and the nodes are deployed together: each
  request between them carries the package's build, and one of another
  build is refused.
- **Placement.** A workspace made at run time goes to the node an admin
  picks, or else to the open node with the most free memory, and stays
  there. Admins close and reopen a node, and move a workspace made at run
  time: `workbench move <workspace> <node>` on the front, or the host page.
  A declared workspace moves when its `node` and `ip` change in Nix: the
  front moves its disk at the next deploy.

### From one host to a front and a node

1. The host becomes node `a`: its `network.subnet` is the fleet's
   `nodes.a.subnet`, its workspaces get `node = "a"`, and the options move
   as [docs/multi-node.md](docs/multi-node.md#the-nix-modules) lists.
   Its VMs, disks and seeds stay.
2. A new machine becomes the front. It takes from the host, once, the SSH
   CAs and host keys (`/var/lib/workbench/ssh`), the quotas
   (`/var/lib/workbench/self-service/quotas`), the devices
   (`/var/lib/workbench-web/devices.json`) and the secrets directories,
   whose sops secrets are encrypted for it; and the configs made at run
   time, as records on `a`: `workbench front import a <their copy>`.
3. `<domain>` and `*.<domain>` point at the front.
4. Both are deployed. A deploy restarts no VM: each workspace asks the
   front at its next start, whenever that is, and is refused until the
   front has its record.

A host that stays on its own moves its options, `fleet`, and `front` and
`node` on the same machine, and deploys. At that deploy,
`workbench-front-setup` makes a record of each workspace it made at run
time, from its config in `/var/lib/workbench/configs`, on its one node:
their owners keep their list rows, apps and SSH, and every workspace asks
the front at its next start. Its CAs, host keys, quotas, devices, secrets
directories, seeds and stop markers stay where they are.

## How a workspace runs

- **One unit per workspace.** `workbench@<name>.service` prepares the
  disk and the shares, starts virtiofsd and then *becomes*
  cloud-hypervisor, so the VM is the unit and no daemon's child. Restarting
  or upgrading the memory daemon or the CLI never stops a VM. A deploy does
  not restart workspaces either: a new app or package set reaches a
  workspace at its next restart (`workbench restart alice`).
  Stopping a workspace presses its ACPI power button and waits for the guest
  to shut down; `workbench stop` keeps it stopped across host reboots until
  `workbench start`.
- **Declared in Nix, no reconciler.** Each workspace is rendered, on its
  node, to `/etc/workbench/<name>.json` plus an instance of the template
  unit, and on the front to its record, `/etc/workbench-front/<name>.json`.
  The configs of workspaces made at run time are in
  `/var/lib/workbench/configs`, which `/etc` overrides. Removing a
  workspace from the configuration stops its VM and **keeps its disk** in
  `/var/lib/workbench/<name>` on its node; only `workbench destroy`
  deletes it.
- **A guest the owner owns.** The OS image (`image` for Ubuntu,
  `nixos.image` for NixOS) is converted into a persistent raw disk on first
  start and grown to `diskGiB`; the guest boots with the cloud-hypervisor
  UEFI firmware (`OVMF-cloud-hypervisor`, `CLOUDHV.fd`) and sets itself up
  from what the host shares; see [Guest operating
  systems](#guest-operating-systems). The owner may change the guest
  freely; what matters for security is enforced on the host.
- **Tools from the host, read-only.** The guest is shared only the closure
  of its packages and apps (`extraPackages` and the apps' bundles), bind-mounted path by path in
  the unit's private mount namespace and served over virtiofs; it never sees
  the rest of the host store.
- **Secrets stay out of the store.** `secretsDir` (default
  `/var/lib/workbench-secrets/<name>` on the front, fill it with sops-nix
  or similar) is given to the node at every start and copied read-only to
  `/run/workspace/secrets`; nothing of it reaches the node's disk. Its
  `environment` file, followed by the global and per-workspace
  `environmentFile`, paths on the front, is the apps' secret environment;
  the `environment` options are the non-secret one.
- **Network.** Each workspace has its own tap and a static `/32` route in
  its node's subnet (`fleet.nodes.<node>.subnet`; the node is `.1`).
  Declared workspaces take `.2` to `.127`, those made at run time `.128`
  on. Guests reach the internet, but not their node, private ranges, the
  WireGuard tunnel or each other, except for the global and per-workspace
  `allowedEndpoints`; a guest can only send from its own address. nginx on
  the front proxies `<workspace>--<app>.<domain>` to the app's port in the
  guest, through WireGuard for a node on another machine, with websockets
  and day-long timeouts.
- **SSH by certificate.** The front has two SSH certificate authorities in
  `/var/lib/workbench/ssh`, made at its first boot. One signs each
  workspace's host key, which the front keeps and gives the node at every
  start, for `<workspace>.<domain>`; the guest's sshd trusts keys the
  other signs, for the user they name. The web UI signs people's keys
  with it; see [From a terminal](#from-a-terminal). Only the front and the
  guest's node reach a guest's port 22.
- **Memory.** Workspaces boot with `fleet.memory.bootMiB` and grow
  through virtio-mem up to `fleet.memory.maxGiB` under pressure, with free
  page reporting. `auto`, there or in one workspace's `memory.maxGiB`, is
  its node's memory, read at every start; growth still stops short of the
  node's `memory.reservePercent`, so a ceiling in GiB is what keeps one
  workspace from taking what the others would grow into. Where
  `front.selfService.maxMemoryGiB` is `auto`, workspaces made at run time
  may be `auto` too, and new ones are unless their owner picks a size.
  They are a `first` pool in vmkit's `services.vmkit.memory.pools`: on a
  node with `yielding` pools, those only boot into and grow into memory
  workspaces leave, except for what each is guaranteed.

Day to day, on a workspace's node:

```sh
sudo workbench status
sudo workbench restart alice        # with the fleet's apps and packages now
sudo workbench stop alice           # stays stopped, also across reboots
sudo workbench start alice
sudo workbench console alice        # serial console, Ctrl-] quits
sudo workbench destroy alice        # a declared one: asks for the name, then deletes the disk
```

A workspace made at run time is the front's: on its node, `workbench
destroy` refuses it and says to run it on the front. On the front:

```sh
sudo workbench destroy alice-dev    # asks for the name, then deletes it: on its node, its
                                    # disk, VM user, tap and seed; here, its record, secrets
                                    # and host key; refused while it moves
sudo workbench move alice-dev b     # a workspace made at run time, to node b
sudo workbench forget alice-dev     # one whose node is lost for good
```

Only x86_64 machines are supported for now.

## Guest operating systems

`os` picks a workspace's guest, `ubuntu` by default. Both get the same from
the host, and the host enforces the same on both: the network, the
secrets, the memory and the apps' versions. They differ in who sets the guest
up and where the apps' closure lives.

| | Ubuntu | NixOS |
|---|---|---|
| Set up by | cloud-init, from a seed the host builds | `workbench.nixosModules.guest`, from the config share |
| The owner changes it with | `apt`, files, anything | `/etc/nixos` and `sudo nixos-rebuild switch` |
| App's closure | the host's, shared at `/nix/store` | copied into the guest's own store |
| `sudo` | optional | required |

### Ubuntu

A Nix-built cloud-init NoCloud seed, attached as a second disk, creates the
`owner` account (optionally with passwordless `sudo`) and its SSH keys,
lets it linger, so its user manager runs from boot, and writes the network,
the virtiofs mounts, the agent's service and sshd's settings for the host's
key and CA. `/etc/environment` puts the workspace's tools first on the
`PATH` of every session, also of commands over SSH that start no login
shell. The app's closure is mounted at `/nix/store` itself, so it costs the
guest nothing.

### NixOS

A NixOS workspace is a NixOS system on its persistent disk that the owner
configures and rebuilds like any NixOS machine, with its own `/nix/store`
and Nix daemon. What makes it a workspace is one module, which the host's
first system and every system the owner builds import.

**First boot.** The disk is made from `nixos.image`: by default a NixOS of
the host's nixpkgs with `nixosModules.guest` and nothing else. On a disk
without `/etc/nixos/flake.nix`, the guest creates the owner's account and
writes the flake it is rebuilt from:

```nix
# /etc/nixos/flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    workbench = {
      url = "github:OSSystems/workbench-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, workbench, ... }:
    {
      nixosConfigurations."carol" = nixpkgs.lib.nixosSystem {
        modules = [
          workbench.nixosModules.guest
          ./configuration.nix
        ];
      };
    };
}
```

```nix
# /etc/nixos/configuration.nix
{ pkgs, ... }:
{
  nixpkgs.hostPlatform = "x86_64-linux";

  users.users."carol" = {
    isNormalUser = true;
    extraGroups = [ "wheel" ];
    # The user manager runs from boot, for the workspace's apps.
    linger = true;
  };

  environment.systemPackages = with pkgs; [ git vim ];

  system.stateVersion = "26.05";
}
```

**After that the system is the owner's.** They edit `/etc/nixos`, keep it in
git if they like, and run:

```sh
sudo nixos-rebuild switch          # the flake's configuration named after the host: the workspace
cd /etc/nixos && sudo nix flake update && sudo nixos-rebuild switch
```

The guest fetches its inputs and substitutes or builds its packages itself,
over the internet; its memory grows for the build like for any other load.
A rebuilt system is the next boot's too; `/etc/nixos` is never written
again. systemd-boot keeps the previous generations: to start one after a
bad rebuild, attach `workbench console <name>` on its node, or open its
console in the web UI, and restart the
workspace; the menu waits `boot.loader.timeout`, one second by default.

**What `nixosModules.guest` does**, at every boot:

- boots from the disk: systemd-boot on the ESP, the root file system grown
  to the disk, the serial console on `ttyS0`;
- mounts the config share at `/run/workspace/config`, the secrets share at
  `/run/workspace/secrets` and the host's closure at
  `/run/workspace/host/nix/store`, all read-only;
- configures the address, the route through the host, DNS and the hostname
  from `workspace.json`, with systemd-networkd;
- copies the closure of the packages and apps into `/nix/store`, only the
  paths the store lacks, registers them and keeps them alive with the GC
  roots `/nix/var/nix/gcroots/workspace-app`, `workspace-app-<id>` and
  `workspace-agent`, which the agent also makes as it starts, for guests
  set up with an older `nixosModules.guest`;
- creates the owner's account in `wheel` if it does not exist yet, lets it
  linger unless the system's configuration says otherwise, and writes
  `/etc/nixos` on a new disk;
- lets sshd read the owner's keys from the config share, serve the host's
  key for the workspace and trust the host's user CA, with passwords off;
- starts `workspace-agent.service`, which reports memory, makes sign-in
  links and runs each app as the owner with the workspace's environment, as
  `workspace-app-<id>.service`;
- prepends the host's tools (`extraPackages` and the apps' packages) to
  every shell's `PATH`, also of commands over SSH, which bash reads
  `/etc/set-environment` for;
- leaves the guest firewall off, enables flakes and lets `wheel` use sudo
  without a password.

Settings marked as defaults in the module (the firewall, sudo, SSH
passwords, the boot loader timeout) are the owner's to override. The rest is
what the host relies on.

**What the owner must keep:**

- `workbench.nixosModules.guest` among the flake's modules. Without it the
  next boot has no network, app or memory reports. The host's isolation
  does not depend on it: a guest without it is a VM that reaches nothing it
  could not reach before.
- Their account. The module adds one at boot if it is missing, which needs
  `users.mutableUsers` (NixOS's default); declaring it, as the generated
  configuration does, is what works with immutable users.
- The file systems and boot loader the module sets, unless they replace
  the disk layout with one that boots the same way.

**The app's closure is a copy.** The guest's store is its own, so the host
cannot mount its closure there the way it does for Ubuntu: a path that the
next closure drops would stay registered without its files. Instead every
boot copies the paths the guest lacks from `/run/workspace/host/nix/store`.
A first boot, and the first boot after a new app or `extraPackages`, takes
as long as copying them, and each NixOS workspace keeps the closure on its
disk: about 3 GiB for T3 Code and the agents it runs, which counts against
`diskGiB`. Old versions go when the owner collects garbage.

**Versions.** The host decides the apps' versions, as for Ubuntu; the owner
decides the system's, including the version of `nixosModules.guest`
through the flake's `workbench` input. What the module reads from the host
is listed below; it changes with care, so a guest a few releases behind
keeps working.

### What the host shares with a guest

| Path in the guest | What |
|---|---|
| `/run/workspace/config/workspace.json` | `name`, `owner`, `url`, `ip`, `gateway`, `nameservers`, `appPort`, `controlPort`, `agentPort`, `apps` |
| `/run/workspace/config/app` | The packages: `sw/bin` holds `extraPackages` and the apps' packages; `bin/workspace-agent` starts the agent |
| `/run/workspace/config/agent` | The agent, `bin/workspace-agent`, the host's workbench |
| `/run/workspace/config/apps/<id>` | An app's bundle: `bin/serve`, and `bin/open-link` if it signs browsers in |
| `/run/workspace/config/environment` | The non-secret environment, for systemd's `EnvironmentFile=` |
| `/run/workspace/config/authorized_keys/<owner>` | The owner's SSH keys |
| `/run/workspace/config/ssh/user-ca.pub` | The host's user CA, which sshd trusts for the owner |
| `/run/workspace/config/ssh/principals/<owner>` | The owner's name, the one principal sshd takes from a certificate |
| `/run/workspace/config/store-paths`, `registration` | The closure's paths, and their validity for `nix-store --load-db` |
| `/run/workspace/secrets` | The secrets directory; its `environment` file is the app's secret environment |
| `/run/workspace/secrets/ssh-host` | The workspace's SSH host key, root's, and its certificate from the host CA |
| the `store` share | The app's closure, at `/nix/store` (Ubuntu) or `/run/workspace/host/nix/store` (NixOS) |
| vsock | The agent reports memory to the host on `controlPort` and answers it on `agentPort` |

## Web UI

The front serves a web UI at `https://<domain>` and puts single sign-on in
front of every workspace: whatever the app does about accounts, only the
workspace's owner and the admins reach it. People see and control their
own workspaces: open their apps, set their computers up to reach them from
a terminal, start, stop, restart and the serial console. Admins see all of
them and every node's memory.

```nix
services.workbench.front = {
  enable = true;
  admins = [ "alice" ];
  oidc = {
    issuerUrl = "https://sso.example.com/realms/example";
    clientId = "workbench";
    clientSecretFile = "/run/secrets/workbench-oidc";
    cookieSecretFile = "/run/secrets/workbench-cookie"; # openssl rand -hex 16
  };
};
```

The identity provider, Keycloak by default, needs a confidential OIDC
client with the redirect URI `https://<domain>/oauth2/callback`, and its
`preferred_username` claim must be the LDAP uid workspaces have as
`owner`; a `groups` claim gives the roles below. One virtual host serves
every workspace, so the certificate must be a wildcard for `*.<domain>`, as
well as `<domain>`, given through `virtualHost` (such as `useACMEHost`).

### Workspaces people make themselves

With `front.selfService`, members of its groups make their own workspaces in
the UI, within a quota admins change there, and destroy them:

```nix
services.workbench.front = {
  adminGroups = [ "workbench-admins" ];
  selfService = {
    enable = true;
    groups = [ "workbench" ];
    # Prints the workspace's secret environment, the app's credential,
    # for an owner; failing or printing nothing refuses the request.
    environmentCommand = "/path/to/workspace-env";
    # Exit 0 while an owner stays, 1 once they left the group or the
    # directory; the timer then stops their workspaces and keeps the disks.
    ownerCommand = "/path/to/workspace-owner";
  };
};
```

The UI stays unprivileged: it writes the request, and
`workbench-create@<ws>.service` on the front places the workspace and has
its node make it as root, with its VM user as a userdb record and its tap
as systemd-networkd files.

The UI is `workbench-web`, a Leptos app in Rust: the server renders the
pages and the browser hydrates them with the same components compiled to
WebAssembly. It runs on the front as its own user, on the registry and
the nodes' APIs, which start, stop and restart the VMs and share their
consoles; polkit lets it start only the front's units that make, change,
move and destroy workspaces. [docs/web-ui.md](docs/web-ui.md) has the
whole design.

## Apps

An app is what runs in a workspace and is opened from outside it: in the
browser, at `https://<workspace>--<app>.<domain>`, an origin of its own,
or, for an app without an address, from a terminal on the person's own
computer, over SSH through the front. The fleet offers them,
`services.workbench.fleet.apps`; each workspace has the ones its `apps` names,
those whose `default` is true unless it says otherwise, and may have none.
`https://<workspace>.<domain>` leads to the workspace's panel in the web UI.

```nix
services.workbench.fleet.apps.notes = {
  name = "Notes";
  port = 8080;
  packages = [ pkgs.mynotes ];
  # Run as the owner at every boot; systemd restarts it when it ends.
  serve = ''exec mynotes --listen 0.0.0.0:"$APP_PORT"'';
};
```

The guest agent runs each app's `serve` at every boot as the owner, with
the workspace's environment and secrets, as `workspace-app-<id>.service`.
A VM keeps the apps it booted with: a change to them, or a new version,
comes with its next restart, and the web UI says what waits for it.

- **`port = null`** makes an app without an address: nginx reaches
  nothing of it, and its panel has **Connect…** instead of **Open**, with
  the steps of its `connect`.
- **`default = false`** makes an app people opt into: a workspace has it
  when its `apps` names it, and the New workspace form leaves it unticked.
- **`oneshot = true`** makes `serve` start what the app runs and end, such
  as services of the owner's user manager, which runs from boot. Its unit
  stays active and is never restarted.

### T3 Code

[T3 Code](https://github.com/pingdotgg/t3code) is the app `t3code` when
`fleet.package` is set, by default to the `t3code-server` of
[t3code-nix](https://github.com/OSSystems/t3code-nix), T3 Code with
OSSystems' patches, with the agents of
[llm-agents.nix](https://github.com/numtide/llm-agents.nix), where
workbench's other AI tools come from, on its PATH. It runs `t3 serve` on
port 3773. It has no accounts: a browser gets in by following a one-time
pairing link, made for the owner inside the guest. The module gives it the
sign-in hook below, around `t3 auth pairing create`.

T3 Code caps its agents' memory at a share of the RAM the machine has when
an agent starts. A workspace boots with its boot memory and grows on demand
up to its most, so that cap would stop agents long before the VM's own
limit: the app runs with `T3CODE_AGENT_SLICE_MEMORY_POLICY=host-managed`,
one of t3code-nix's patches, and the VM's most is the only cap.

### Herdr and Herdr app

[herdr](https://herdr.dev) has two runtimes, apps without an address that
workspaces opt into, from llm-agents.nix too:

| App | What runs in the workspace | From the person's computer |
|---|---|---|
| `herdr`, Herdr | the TUI's server, `herdr server`, in the foreground | `herdr --remote <workspace>.<domain>` |
| `herdrcore`, Herdr app | herdr-core's core, which `hc server start` starts as a user service | herdr-app: Add machine, `<workspace>.<domain>`; or `hc machine add <workspace>.<domain>` |

```nix
services.workbench.fleet.workspaces.alice.apps = [ "t3code" "herdr" "herdrcore" ];
```

Their panes and terminals have the workspace's environment and secrets:
Herdr's server has them as any app does, and Herdr app imports them into
the owner's user manager before it starts the core, whose terminals are
user services. Both find themselves over SSH on the workspace's `PATH`,
and `herdr update` and `hc update` say that a newer one comes with the
host. `fleet.herdr.package` and `fleet.herdrCore.package` choose the versions.

### From a terminal

People reach a workspace's SSH server from their own computer, with no key
to register and no port but the host's HTTPS:

1. A workspace's **Connect…** page, behind single sign-on, shows a command
   with a code that works once, for 10 minutes, for the person signed in:

   ```sh
   curl -fsSL https://<domain>/connect/install.sh | sh -s -- <code>
   ```

2. It downloads `workbench-connect`, a static binary for x86_64 or
   aarch64 Linux, which installs itself in `~/.local/bin` and trades the
   code for a credential of that computer's, which works for 30 days while
   the person signs in to the UI at least every 7. It
   writes `~/.config/workbench` and includes its `ssh_config` at the top of
   `~/.ssh/config`.
3. `ssh <workspace>.<domain>` then signs in as the person: `workbench-connect`
   gets the computer's key signed for them for 8 hours, and is the
   `ProxyCommand`, a WebSocket to the host, which lets it through to the
   workspace's port 22 if the workspace is theirs and runs. The host CA
   vouches for the workspace's host key.

`/devices`, in the user's menu, lists a person's computers, and revokes
them. [docs/web-ui.md](docs/web-ui.md#from-a-terminal) has the whole design.

### The sign-in hook

An app may have `openLink`, a script the agent runs as the owner, with the
app's address in `APP_URL`, when someone opens the app from the web UI; it
prints one URL on the app's own origin that signs a browser in, and nothing
else on stdout. `workbench open-link` prints it:

```sh
sudo workbench open-link alice t3code
```

An app without the hook is opened at its address.

## Development

```sh
nix develop       # cargo, clippy, rustfmt, rust-analyzer, lld, wasm-bindgen; links ./vmkit
cargo test --workspace
cargo test -p workbench-web --features ssr
nix build .#web .#web.site   # the UI's server, and its WebAssembly and assets
nix build .#workbench-connect .#workbench-connect.aarch64-linux   # the client, static
(cd connect && cargo test)   # the client, a crate of its own
nix flake check   # rustfmt, clippy and the tests, the module evaluations, and the VM tests
nix fmt -- $(git ls-files '*.nix')
```

workbench is a Rust crate built against the vmkit crate of the flake's
`vmkit` input, the same revision whose NixOS module the host imports: a
host whose `vmkit` input follows its own builds workbench against that one.
`Cargo.toml` takes vmkit from `./vmkit`, which `nix develop` links to the
locked input and the package copies in; its dependencies are locked in
`Cargo.lock` with workbench's. To work on both at once, link `./vmkit` to
a vmkit checkout of your own; `nix develop` leaves such a link alone.

What the host and its guests exchange, `workspace.json` and the agent's
requests, is described in [docs/contracts.md](docs/contracts.md).

`workbench-connect`, in `connect/`, has a `Cargo.lock` of its own and no
dependency on workbench's crates or vmkit. Its aarch64 build cross-compiles
Rust's standard library for musl, which no binary cache has: the first
build of the front takes a while longer.

`checks.guest` evaluates the system a new NixOS workspace writes and
rebuilds. `checks.fleet` and `checks.shared-host` evaluate what the
modules render for a front, a node and both, and their assertions.
`checks.workspaces` boots two workspaces in a NixOS VM test on a host on
its own, one Ubuntu and one NixOS, each with T3 Code and herdr's two
runtimes; `checks.multi-node` boots a front and two nodes over
WireGuard, makes a workspace and moves it between them, and has a client
use the web UI across them: an app and the SSH tunnel to a node's guest,
an admin's node pick, Close and Move, and a node that doesn't answer.
Both need nested
virtualization on the machine that builds them (`kvm_amd`/`kvm_intel`
with `nested=1`). `checks.workspaces` runs the default
app, T3 Code, from t3code-nix, which OSSystems' Garnix builds and caches:
a machine without that cache among its substituters builds it.
