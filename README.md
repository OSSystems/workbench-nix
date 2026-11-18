# workbench-nix

**Persistent, isolated per-person workspaces** on NixOS: one long-lived
[cloud-hypervisor](https://www.cloudhypervisor.org/) VM each, an Ubuntu
24.04 guest the person owns, running the web app they open at
`https://<workspace>.<domain>`. A person may have several.

The host owns everything around the app: the VM, its disk, network
isolation, secrets, tools and memory. The app is what the person opens in
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
  workspace at its next restart (`systemctl restart workbench@alice`).
  Stopping a workspace presses its ACPI power button and waits for the guest
  to shut down.
- **Declared in Nix, no reconciler.** Each workspace is rendered to
  `/etc/workbench/<name>.json` plus an instance of the template unit.
  Configs are also looked up in `/var/lib/workbench/configs`, which
  `/etc` overrides, for a later self-service mode. Removing a workspace from
  the configuration stops its VM and **keeps its disk** in
  `/var/lib/workbench/<name>`; only `workbench destroy` deletes it.
- **Ubuntu guest.** The pinned Ubuntu cloud image (`image`) is converted into
  a persistent raw disk on first start and grown to `diskGiB`; the guest
  boots with the cloud-hypervisor UEFI firmware (`OVMF-cloud-hypervisor`,
  `CLOUDHV.fd`). A Nix-built cloud-init NoCloud seed creates the `owner`
  account (optionally with passwordless `sudo`), the network, the mounts and
  the app's service, which runs as the owner. The owner may change
  the guest freely; what matters for security is enforced on the host.
- **Tools from the host, read-only.** The guest mounts at `/nix/store` only
  the closure of its app (the app plus `extraPackages`), bind-mounted path by
  path in the unit's private mount namespace and shared over virtiofs; it
  never sees the rest of the host store.
- **Secrets stay out of the store.** `secretsDir` (default
  `/var/lib/workbench-secrets/<name>`, fill it with sops-nix or similar)
  is copied read-only to `/run/workspace/secrets` at every start. Its
  `environment` file, followed by the global and per-workspace
  `environmentFile`, is the app's secret environment; the
  `environment` options are the non-secret one.
- **Network.** Each workspace has its own tap and a static `/32` route
  (`network.subnet`, default `10.232.0`; the host is `.1`). Guests reach the
  internet, but not the host, private ranges or each other, except for the
  global and per-workspace `allowedEndpoints`; a guest can only send from
  its own address. nginx on the host proxies `<workspace>.<domain>` to the
  guest's app port, with websockets and day-long timeouts.
- **Memory.** Workspaces boot with `memory.bootMiB` and grow through
  virtio-mem up to `memory.maxGiB` under pressure, with free page reporting.
  They are a `first` pool in vmkit's `services.vmkit.memory.pools`: on a
  host with `yielding` pools, those only boot into and grow into memory
  workspaces leave, except for what each is guaranteed.

Day to day, on the host:

```sh
sudo workbench status
sudo workbench console alice        # serial console, Ctrl-] quits
sudo workbench destroy alice        # asks for the name, then deletes the disk
```

Only x86_64 hosts are supported for now.

## Apps

Each workspace runs one app: the web UI its owner opens at
`https://<workspace>.<domain>`.

### T3 Code

[T3 Code](https://github.com/pingdotgg/t3code) is the default
(`package`, from [t3code-nix](https://github.com/OSSystems/t3code-nix)); it
runs `t3 serve` on `appPort`. It has no accounts, so pairing is its
authentication: a pairing link for the owner is made inside the guest, over
vsock.

```sh
sudo workbench pair alice           # flags after the name go to t3
```

## Development

```sh
nix flake check   # go test, an evaluation next to another vmkit controller, and the VM test
nix fmt -- $(git ls-files '*.nix')
```

`checks.workspaces` boots two workspaces in a NixOS VM test; it needs nested
virtualization on the machine that builds it (`kvm_amd`/`kvm_intel` with
`nested=1`). The test runs the default app, T3 Code, which is not in a
binary cache, so the first build compiles it.
