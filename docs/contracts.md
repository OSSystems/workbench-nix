# Contracts

What the host exchanges with a workspace's guest. The tests in
`src/launch.rs` and `src/agent.rs` pin it.

- `workspace.json` is read by `nixosModules.guest`, which a NixOS guest
  keeps at the version its owner last rebuilt with, a few releases behind
  the host perhaps. It changes with care: fields are added, not renamed or
  removed, and readers ignore what they do not know.
- The agent comes with the app bundle the host shares, so it is always the
  host's version, and so are its requests: they change with workbench.

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
  "agentPort": 5002
}
```

| Field | Meaning |
|---|---|
| `name` | The workspace, and the guest's host name. |
| `owner` | The person's user name in the guest; the app runs as this user. |
| `url` | Where the app is opened, the base of pairing links. |
| `ip`, `gateway`, `nameservers` | The guest's `/32` address, the host's address it routes through, DNS. |
| `appPort` | The port the app listens on. |
| `controlPort` | The vsock port the agent reports memory to, on the host. |
| `agentPort` | The vsock port the agent listens on for the host. |

## The agent

The agent listens on vsock port `agentPort`. Only the host reaches it,
through cloud-hypervisor's vsock socket (`CONNECT <port>`, as vmkit
describes), so it trusts what it is asked.

- The host sends one request: a JSON object on one line, ended by `\n`, of
  at most 64 KiB, within 10 s of connecting.
- The agent answers one JSON object on one line, ended by `\n`, of at most
  1 MiB, and closes the connection.

```json
{"op":"pair","args":["--json"]}
```

| Field | Meaning |
|---|---|
| `op` | The operation. |
| `args` | The operation's arguments; absent for none. |

```json
{"stdout":"{\"credential\":\"…\"}\n","stderr":"","exitCode":0}
```

| Field | Meaning |
|---|---|
| `stdout`, `stderr` | What the operation's command printed; absent when empty. |
| `exitCode` | How it exited; always present, `0` for an operation without a command. |
| `error` | Why the request failed; absent on success. When present, the other fields say nothing. |

Failures the agent reports in `error`: `no request` (nothing within 10 s),
`invalid JSON`, `unknown op`, and an operation's own reasons, such as an
unknown owner or a command that did not finish in time.

### `ping`

No arguments; the reply is `{"exitCode":0}`. The host's `workbench status`
uses it to show whether the agent is up.

### `pair`

Runs `t3 auth pairing create --base-url <url> --label workbench <args…>` as
the owner, in their home, with `T3CODE_HOME=~/.t3`, for at most 60 s, and
replies with what it printed and its exit code. `workbench pair <workspace>
<args…>` passes its arguments through.
