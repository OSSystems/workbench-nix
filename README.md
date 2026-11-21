# workbench-nix

**Persistent, isolated per-person workspaces** on NixOS: one long-lived
[cloud-hypervisor](https://www.cloudhypervisor.org/) VM each, an Ubuntu
24.04 or NixOS guest the person owns, running the apps they open in the
browser, each at `https://<workspace>--<app>.<domain>`. A person may have
several workspaces.

The host owns everything around the apps: the VM, its disk, network
isolation, secrets, tools and memory. The apps are what the person opens in
the browser; see [Apps](#apps).

Built on [vmkit-nix](https://github.com/OSSystems/vmkit-nix): workspaces share a
host with other vmkit controllers only through its guest networks and
memory pools.

## Usage

```nix
{
  inputs = {
    vmkit.url = "github:OSSystems/vmkit-nix";
    workbench = {
      url = "github:OSSystems/workbench-nix";
      inputs.vmkit.follows = "vmkit";
    };
  };

  # in your NixOS configuration
  imports = [ inputs.workbench.nixosModules.default ];

  services.workbench = {
    enable = true;
    domain = "ws.example.com";            # one wildcard certificate covers it
    virtualHost = { forceSSL = true; useACMEHost = "ws.example.com"; };
    network.externalInterface = "enp6s0";
    environment.ANTHROPIC_BASE_URL = "https://llm-proxy.example.com";
    allowedEndpoints = [ { address = "10.0.5.20"; port = 443; } ];  # LLM proxy
    extraPackages = [ pkgs.claude-code ];

    workspaces = {
      alice = { owner = "alice"; ip = "10.232.0.10"; };
      alice-scratch = { owner = "alice"; ip = "10.232.0.11"; sudo = false; diskGiB = 128; };
      bob = {
        owner = "bob";
        ip = "10.232.0.12";
        environmentFile = "/run/secrets/bob-llm.env";
      };
      carol = { owner = "carol"; ip = "10.232.0.13"; os = "nixos"; };  # see NixOS below
    };
  };
}
```

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
- **Declared in Nix, no reconciler.** Each workspace is rendered to
  `/etc/workbench/<name>.json` plus an instance of the template unit.
  Configs are also looked up in `/var/lib/workbench/configs`, which
  `/etc` overrides, for a later self-service mode. Removing a workspace from
  the configuration stops its VM and **keeps its disk** in
  `/var/lib/workbench/<name>`; only `workbench destroy` deletes it.
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
  `/var/lib/workbench-secrets/<name>`, fill it with sops-nix or similar)
  is copied read-only to `/run/workspace/secrets` at every start. Its
  `environment` file, followed by the global and per-workspace
  `environmentFile`, is the apps' secret environment; the
  `environment` options are the non-secret one.
- **Network.** Each workspace has its own tap and a static `/32` route
  (`network.subnet`, default `10.232.0`; the host is `.1`). Guests reach the
  internet, but not the host, private ranges or each other, except for the
  global and per-workspace `allowedEndpoints`; a guest can only send from
  its own address. nginx on the host proxies `<workspace>--<app>.<domain>`
  to the app's port in the guest, with websockets and day-long timeouts.
- **Memory.** Workspaces boot with `memory.bootMiB` and grow through
  virtio-mem up to `memory.maxGiB` under pressure, with free page reporting.
  They are a `first` pool in vmkit's `services.vmkit.memory.pools`: on a
  host with `yielding` pools, those only boot into and grow into memory
  workspaces leave, except for what each is guaranteed.

Day to day, on the host:

```sh
sudo workbench status
sudo workbench restart alice        # with the host's apps and packages now
sudo workbench stop alice           # stays stopped, also across reboots
sudo workbench start alice
sudo workbench console alice        # serial console, Ctrl-] quits
sudo workbench destroy alice        # asks for the name, then deletes the disk
```

Only x86_64 hosts are supported for now.

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
`owner` account (optionally with passwordless `sudo`) and its SSH keys, the
network, the virtiofs mounts and the agent and app services. The app's
closure is mounted at `/nix/store` itself, so it costs the guest nothing.

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
bad rebuild, attach `workbench console <name>` on the host and restart the
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
- creates the owner's account in `wheel` if it does not exist yet, and
  writes `/etc/nixos` on a new disk;
- lets sshd read the owner's keys from the config share, with passwords off;
- starts `workspace-agent.service`, which reports memory, makes sign-in
  links and runs each app as the owner with the workspace's environment, as
  `workspace-app-<id>.service`;
- prepends the host's tools (`extraPackages`) to every login's `PATH`;
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
| `/run/workspace/config/store-paths`, `registration` | The closure's paths, and their validity for `nix-store --load-db` |
| `/run/workspace/secrets` | The secrets directory; its `environment` file is the app's secret environment |
| the `store` share | The app's closure, at `/nix/store` (Ubuntu) or `/run/workspace/host/nix/store` (NixOS) |
| vsock | The agent reports memory to the host on `controlPort` and answers it on `agentPort` |

## Web UI

`services.workbench.web` serves a web UI at `https://<domain>` and puts
single sign-on in front of every workspace: whatever the app does about
accounts, only the workspace's owner and the admins reach it. People see
and control their own workspaces: open their apps, start, stop, restart and
the serial console. Admins see all of them and the host's memory.

```nix
services.workbench.web = {
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

With `web.selfService`, members of its groups make their own workspaces in
the UI, within a quota admins change there, and destroy them:

```nix
services.workbench.web = {
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
`workbench-create@<ws>.service` makes the workspace as root, with its VM
user as a userdb record and its tap as systemd-networkd files.

The UI is `workbench-web`, a Leptos app in Rust: the server renders the
pages and the browser hydrates them with the same components compiled to
WebAssembly. It runs as its own user, reaches the VMs through sockets its
group may use and starts, stops and restarts workspaces through polkit.
[docs/web-ui.md](docs/web-ui.md) has the whole design.

## Apps

An app is what runs in a workspace and is opened from the browser, at
`https://<workspace>--<app>.<domain>`, an origin of its own. The host offers
them, `services.workbench.apps`; each workspace has the ones its `apps`
names, those whose `default` is true unless it says otherwise, and may
have none.
`https://<workspace>.<domain>` leads to the workspace's panel in the web UI.

```nix
services.workbench.apps.notes = {
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
  nothing of it, and its panel has no **Open**.
- **`default = false`** makes an app people opt into: a workspace has it
  when its `apps` names it, and the New workspace form leaves it unticked.

### T3 Code

[T3 Code](https://github.com/pingdotgg/t3code) is the app `t3code` when
`package` is set, by default to the `t3code` of
[llm-agents.nix](https://github.com/numtide/llm-agents.nix), where
workbench's AI tools come from; it runs `t3 serve` on port 3773. It has no
accounts: a browser gets in by following a one-time pairing link, made for
the owner inside the guest. The module gives it the sign-in hook below,
around `t3 auth pairing create`.

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
nix flake check   # rustfmt, clippy and the tests, the module evaluations, and the VM test
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

`checks.guest` evaluates the system a new NixOS workspace writes and
rebuilds. `checks.workspaces` boots two workspaces in a NixOS VM test, one
Ubuntu and one NixOS; it needs nested virtualization on the machine that
builds it (`kvm_amd`/`kvm_intel` with `nested=1`). The test runs the default
app, T3 Code, from llm-agents.nix: with `https://cache.numtide.com` among
the substituters, it is downloaded rather than built.
