# Contracts

What the host exchanges with a workspace's guest. The tests in
`src/launch.rs` and `src/agent.rs` pin it.

- `workspace.json` is read by `nixosModules.guest`, which a NixOS guest
  keeps at the version its owner last rebuilt with, a few releases behind
  the host perhaps. It changes with care: fields are added, not renamed or
  removed, and readers ignore what they do not know.
- The agent is the one the host shares at every start, so it is the
  version of the host that started the VM, and so are its requests: they
  change with workbench. A host newer than the VM's agent finds no app it
  knows of in it, and says the apps come with its next restart.

What workbench exchanges with other vmkit controllers on the host (the
memory pool status files, the control channel's `stats` request) is
vmkit's contract, in vmkit's `docs/contracts.md`.

## `workspace.json`

The config share's `/run/workspace/config/workspace.json`, written at every
start: what the agent and `nixosModules.guest` set the guest up from.

```json
{
  "name": "alice",
  "owner": "alice",
  "url": "https://alice.ws.example.com",
  "ip": "10.232.0.10",
  "gateway": "10.232.0.1",
  "nameservers": ["1.1.1.1", "8.8.8.8"],
  "appPort": 3773,
  "controlPort": 5001,
  "agentPort": 5002,
  "apps": [
    { "id": "t3code", "name": "T3 Code", "port": 3773, "url": "https://alice--t3code.ws.example.com" }
  ]
}
```

| Field | Meaning |
|---|---|
| `name` | The workspace, and the guest's host name. |
| `owner` | The person's user name in the guest; the apps run as this user. |
| `url` | The workspace's address, which leads to its panel. |
| `ip`, `gateway`, `nameservers` | The guest's `/32` address, the host's address it routes through, DNS. |
| `appPort` | The first port of an app, or 0: what guests from before apps read. Nothing reads it now. |
| `controlPort` | The vsock port the agent reports memory to, on the host. |
| `agentPort` | The vsock port the agent listens on for the host. |
| `apps` | The apps the VM boots with: each one's id, name, port and address, without `port` and `url` for an app with no address, which people reach from a terminal. Its bundle is `config/apps/<id>`. |

## The shares

The config share, `/run/workspace/config`, holds besides `workspace.json`:

| Path | What |
|---|---|
| `app` | The workspace's packages: `sw/` on everyone's PATH, `bin/workspace-agent`, which starts `agent/bin/workspace-agent`, and `bin/serve`, which only waits: guests set up before apps run it as the owner. A newer workbench leaves it as it is. |
| `agent` | The agent: `bin/workspace-agent`. A host from before it was apart shares none, and its `app/bin/workspace-agent` is the agent. |
| `apps/<id>` | An app's bundle: `bin/serve`, and `bin/open-link` when it signs browsers in. |
| `store-paths`, `registration` | The closure of all of them, which a NixOS guest copies into its store. |
| `authorized_keys/<owner>`, `environment` | The owner's SSH keys and the non-secret environment. |

The agent starts each app at boot with `systemd-run`, as the unit
`workspace-app-<id>`: its `bin/serve`, as the owner in their home, with
`Restart=always`, the environment and secret environment files, the
packages first on `PATH`, and `APP_PORT` and `APP_URL` for an app with an
address. An agent that restarts leaves the units it finds.

## The agent

The agent listens on vsock port `agentPort`. Only the host reaches it,
through cloud-hypervisor's vsock socket (`CONNECT <port>`, as vmkit
describes), so it trusts what it is asked.

- The host sends one request: a JSON object on one line, ended by `\n`, of
  at most 64 KiB, within 10 s of connecting.
- The agent answers one JSON object on one line, ended by `\n`, of at most
  1 MiB, and closes the connection.

```json
{"op":"open-link","args":["t3code"]}
```

| Field | Meaning |
|---|---|
| `op` | The operation. |
| `args` | The operation's arguments; absent for none. |

```json
{"stdout":"https://alice.ws.example.com/pair#token=…\n","exitCode":0}
```

| Field | Meaning |
|---|---|
| `openLinks` | In the answer to `ping`: the apps with a sign-in hook. Absent when none. |
| `stdout`, `stderr` | What the operation's command printed, each cut at 64 KiB; absent when empty. |
| `exitCode` | How it exited; always present, `0` for an operation without a command. |
| `error` | Why the request failed; absent on success. When present, the other fields say nothing. |

Failures the agent reports in `error`: `no request` (nothing within 10 s),
`invalid JSON`, `unknown op`, and an operation's own reasons, such as an
unknown owner, no such app, an app without an address or a sign-in hook,
or a command that did not finish in time.

### `ping`

No arguments; the reply is `{"exitCode":0}`, with `openLinks` naming the
apps whose bundle has `bin/open-link`. The host's `workbench status` and the
web UI use it to show whether the agent is up and which apps sign browsers
in.

### `open-link`

One argument, an app the VM booted with. Runs its `bin/open-link` as the
owner, in their home, with `HOME`, `USER`, `LOGNAME`, `WORKSPACE_URL`,
`APP_URL` (the app's `url`), `APP_PORT` and a `PATH` with the packages'
`sw/bin` first, for at most 60 s, and replies with what it printed and its
exit code. The hook prints one URL on the app's own origin; the host checks
that before it uses it.
