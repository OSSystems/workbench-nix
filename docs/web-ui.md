# Web UI

The specification of workbench's web UI: what it shows, who may do what,
and how each piece works on the host. The static mockup of every screen is
[`web-ui/index.html`](web-ui/index.html); `workbench-web` implements it,
and `services.workbench.web` sets it up.

## Premises

- **One address.** The UI is at `https://<domain>`, each workspace at
  `https://<ws>.<domain>`. One certificate covers `<domain>` and
  `*.<domain>`.
- **Single sign-on everywhere.** Keycloak, through oauth2-proxy, guards the
  UI and every workspace. Nobody reaches a workspace's app without signing in,
  whatever the app does about accounts.
- **Three roles, from the directory's groups.** A person sees and controls
  the workspaces they own. Members of the self-service groups also create
  their own, within a quota. Admins, members of the admin groups or names in
  the Nix configuration, see and control all of them.
- **No database.** State comes from the workspace configs, systemd, the VM's
  sockets and a few files the host writes: stop markers, the quotas and the
  configs of workspaces made at run time. It is what `workbench status`
  collects, called as a function.
- **Rendered on the server, hydrated in the browser.** A Leptos app in
  Rust: the server renders every page, and the same components, compiled to
  WebAssembly, take over in the browser for live updates, dialogs and
  keyboard shortcuts. Actions are server functions, and their forms post
  without JavaScript too. What the browser and the server exchange, and
  the checks on it, are one crate's, `workbench-model`, compiled for both.
- **Works on a phone.** The same pages at 360 px wide: the list becomes the
  page and the side panel becomes its own screen.
- **Dark and light**, following the system's setting.

## Identity

```
browser ──► nginx ──auth_request──► oauth2-proxy ──OIDC──► Keycloak
              │
              ├─ <domain>        ──► workbench web   (unix socket)
              └─ <ws>.<domain>   ──► the workspace's app  (after an owner check)
```

- **oauth2-proxy** is the Keycloak client. It lives at `/oauth2/` on every
  host name, so no workspace name is reserved, and its cookie is set on
  `.<domain>` so one sign-in covers the UI and all workspaces. It runs with
  `--set-xauthrequest` and a cookie named `_workbench_auth`.
- **The user** is Keycloak's `preferred_username`, which must be the
  person's LDAP uid: the same name workspaces have as `owner`. nginx takes it
  from `X-Auth-Request-Preferred-Username` and passes it to `workbench-web`
  as `X-Workbench-User`, overwriting whatever the browser sent.
- **The groups** are the ID token's `groups` claim: Keycloak's client has a
  group membership mapper (`full.path=false`), and the realm reads the groups
  from LDAP. nginx passes `X-Auth-Request-Groups` as `X-Workbench-Groups`, a
  comma-separated list, overwritten like the user. workbench never talks to
  LDAP itself.
- **Sessions are checked again every 5 minutes** (`--cookie-refresh=5m`):
  oauth2-proxy refreshes the tokens with Keycloak, so a person removed from
  a group, or from the directory, loses what it gave within minutes. The
  session lives in Redis on the host, so the cookie is a small ticket that
  survives the refresh without nginx passing a new one back.
- **`workbench-web` listens only on unix sockets** that nginx's group can
  reach, so that header cannot be forged.
- **Signing out** is `/oauth2/sign_out`, which also ends the Keycloak session
  (`--backend-logout-url`).

### Workspaces behind SSO

All workspaces share one virtual host, `*.<domain>`, so a workspace made at
run time needs no nginx reload. It takes the workspace's name from the host
name and has an `auth_request` to `workbench-web`'s authorization socket
(`/run/workbench-web/authz.sock`, `GET /authz`). That gets the name and the
browser's cookies, and:

1. asks oauth2-proxy (`/oauth2/auth`) who the cookie belongs to and their
   groups;
2. answers `401` when nobody is signed in (nginx redirects to
   `/oauth2/start?rd=<the URL>`), `403` when the user may not use the
   workspace (see [who may what](#who-may-what)), `200` otherwise;
3. with the `200`, returns the app's address in `X-Workbench-Upstream`
   (`<ip>:<appPort>`, from the config), which nginx proxies to, and the
   cookies minus `_workbench_auth*` in `X-Workbench-Cookie`, which nginx
   sends to the app instead of the browser's. **The app never sees the SSO
   cookie**, so code running in a workspace can't replay it against the UI
   or another workspace.

The app's answers can't set a cookie for the whole domain either: nginx
makes every `Domain` its host's, so a workspace's app can't put an SSO
cookie of its choosing in a browser that way. Script in the app still can,
since the workspaces share the domain: the most it does is sign that
browser in as the app's owner, never as the person who opened it.

What oauth2-proxy says about the SSO cookie, the user and the groups, is
cached for 10 s per SSO cookie; the app's cookies are no part of the key.
The decision and the address are made for each request, from that and the
workspace's config. On a 403, nginx shows the UI's `/denied` page at the
workspace's address, styled from `https://<domain>`, with a link back to
it.

The check lives in `workbench-web`, not in nginx, so runtime configs, the
groups, the admin list and the owner rule are in one tested place.

### Who may what

| | Sees and controls | Creates |
|---|---|---|
| Owner of a workspace from the Nix configuration | that workspace | — |
| Owner of a workspace made at run time | that workspace, while in a self-service group | — |
| Member of `web.selfService.groups` | their own | their own, within their quota |
| Admin: in `web.adminGroups`, or named in `web.admins` | every workspace, the host page, the quotas | their own, within their quota |

A workspace made at run time is reachable by its owner only while the
owner is in a self-service group: leaving the group cuts it off at the next
session refresh, before the reconciliation below stops its VM.

## Screens

```
https://<domain>/                    Workspaces
https://<domain>/?w=<ws>             Workspaces, with <ws> in the side panel
https://<domain>/w/<ws>/sign-in      Sign in to <ws>'s app
https://<domain>/w/<ws>/console      Serial console
https://<domain>/host                Host memory and quotas (admins)
https://<domain>/new                 New workspace (self-service)
https://<domain>/w/<ws>/destroy      Destroy a workspace made at run time
```

### Workspaces (`/`)

A top bar with the mark, a field that finds a workspace by name (`Ctrl K`
or `/`) and the user's menu (**Sign out**). A sidebar with **Workspaces**,
**Host** (admins only), and under **Yours** the user's workspaces with
their status dot. The main area lists workspaces in
groups by state (Running, Starting, Failing, Stopped), with chips to filter
by All, Running and Stopped. Admins get a chip for **Mine** and an Owner
column.

A row has the status dot, the name, a short note (the address, "booting",
"since Nov 3"), the system, the memory and one primary action:

| State | Primary action |
|---|---|
| Running, the app was never opened from this browser | **Open and sign in** |
| Running | **Open** |
| Starting | **Open** (the app may already answer) |
| Stopped | **Start** |
| Failing | **Console** |
| Stopping, Restarting | none; the row says what it waits for |

**A line along the bottom edge of each row is its memory:** its length is
what the VM holds now out of the most it may grow to. Sage while running,
amber while starting, red while failing; a stopped workspace has none.

**The side panel** shows the selected workspace: name, status and since
when, the update notice, then Address, Owner (admins), System, Memory (a
meter with a tick at the boot size), Disk on host, and the actions:

- **Open** (`↵`)
- **Sign in to app** (`S`), only when the app supports it
- **Console** (`C`)
- **Restart…** (`R`), unless it is stopped
- **Stop…**, apart at the bottom below a rule, plain: its dialog says what
  it costs
- **Destroy…**, at the other end of that row, in red, only for a workspace
  made at run time

On a phone the panel is a page of its own (`/?w=<ws>` with the list
hidden), reached by tapping a row.

Keyboard: `J`/`K` move, `↵` opens, `S` signs in, `C` opens the console, `R`
restarts, `/` or `Ctrl K` focuses the command field. Shortcuts never act on
their own; Restart and Stop still open their dialog.

#### States

The state of a workspace comes from its unit, the VM and the agent:

| Unit `ActiveState`/`SubState` | VM | Agent | Shown as |
|---|---|---|---|
| `active`/`running` | `Running` | answers | **Running** |
| `activating`, or `active` and the agent doesn't answer | | | **Starting** |
| `deactivating` | | | **Stopping**, or **Restarting** after a restart |
| `inactive` | | | **Stopped** |
| `failed`, or `activating`/`auto-restart` | | | **Failing** |

The unit has `Restart=always`, so a VM that keeps crashing cycles through
`activating`/`auto-restart`; that is Failing, not Starting. A state always
has a label, never only a color.

- **Loading:** skeleton rows, the first time only.
- **Empty:** "You don't have a workspace yet", what one is, and **New
  workspace** for whoever may create one; that an admin creates it for
  everyone else.
- **Starting:** "booting"; a NixOS workspace takes longer on its first boot
  after an update, while it copies the app onto its disk.
- **Failing:** a notice in the panel: "The VM won't stay up. Its console
  shows why." with **Console** and **Restart…**.
- **App update:** "A new app version is on the host. Restart to use it." with
  **Restart…** (see [App updates](#app-updates)).
- **Lost connection:** when the live updates drop, a bar says "Reconnecting…
  Showing the state at 10:42." and actions stay enabled: they are plain
  forms, and their answer says if they failed.

### Sign in to app (`/w/<ws>/sign-in`)

Some apps have accounts of their own on top of the host's SSO; T3 Code, for
one, lets a browser in only after it follows a one-time link. The UI gets
that link from the app and sends the browser to it, so the person never
copies a credential:

1. **Open and sign in**, or **Sign in to app**, posts the workspace's name
   to `/api/sign-in`, in a new tab.
2. `workbench-web` asks the guest agent for a link (`op: "sign-in"`, over
   vsock). The agent runs the app's `bin/sign-in-link` as the owner; it
   prints one URL and nothing else on stdout.
3. The URL must start with the workspace's own URL. `workbench-web` answers
   `303 See Other` to it, with `Cache-Control: no-store`. The link is never
   shown and never logged.
4. The browser remembers, in `localStorage`, that it has signed in to `<ws>`,
   so the next time the primary action is just **Open**.

The `GET` of `/w/<ws>/sign-in` is a page with one button. It signs in
nothing, so link previews don't spend links, and it is the address to send
someone: "Sign in to `<ws>`'s app".

An app without `bin/sign-in-link` gets no sign-in action: **Open** is all,
and the app's own screen takes over. The agent's `ping` answer says whether
the hook exists.

Errors, on the sign-in page above its button:

| Situation | Message | Action |
|---|---|---|
| Stopped | "`<ws>` is stopped. Start it, then sign in." | **Start `<ws>`** |
| The agent doesn't answer | "`<ws>` is still starting." | **Try again** |
| The hook failed | "The app couldn't make a sign-in link." Its stderr under Details. | **Try again**, **Console** |
| Not a URL of the workspace | "The app answered something that isn't a link to `<ws>`." | **Try again** |
| 90 s without an answer | "The app took too long to answer." | **Try again** |

**Another device** (a link or QR code for a phone) is for later; it would
be a second action in the panel.

### Start, stop and restart

| Action | Confirmation | The dialog says |
|---|---|---|
| **Start** | none | — |
| **Restart** | dialog | Stops everything running in the VM; open app tabs lose their connection for about a minute. Keeps everything on its disk. Brings the app and system now on the host. |
| **Stop** | dialog, red button | Stops the VM and the app. Keeps everything on its disk. It stays stopped, also across host reboots, until someone starts it. |
| **Destroy** | its page, typing the name | Deletes the disk and everything on it, for good. Only for a workspace made at run time, by its owner or an admin; the others stay `workbench destroy`. |

Without JavaScript, **Restart…** and **Stop…** are links to
`/w/<ws>/restart` and `/w/<ws>/stop`, pages with the same text and button.

After a confirm, the page comes back with the row in its new state; the
live updates carry it to the end. An action on a workspace that is already
changing state is refused with "`<ws>` is already restarting".

**Stopped stays stopped.** Stop writes `/var/lib/workbench/.stopped/<ws>`
before it stops the unit; the unit has
`ConditionPathExists=!/var/lib/workbench/.stopped/%i`, so a host reboot
skips it. Start removes the file first. `workbench start` and
`workbench stop` do the same from the shell.

### Console (`/w/<ws>/console`)

The VM's serial console in the browser, for the owner and admins: what the
guest prints while it boots, the `ttyS0` login, and the boot menu of a
NixOS workspace whose new generation doesn't boot.

- A terminal fills the page, under a bar with the workspace, its state,
  **Restart…** and the number of people watching ("2 connected").
- It is **shared**: `workbench-web` keeps one connection to the serial
  socket per running VM, keeps the last 256 KiB of output, and fans it out
  to every browser. Everyone types into the same console.
- New viewers first get the kept output, so opening the console of a VM
  that just failed shows why. A workspace of the same name made again for
  someone else starts with none of it, and the viewers of the one before
  are let go.
- **Restart with console** restarts the VM with the console open, for the
  boot menu: systemd-boot waits `boot.loader.timeout` seconds, 1 by default.
- When the VM stops, the terminal says so and reconnects when it starts.
- `workbench console` on the host attaches to the same shared console when
  `workbench-web` runs, and to the serial socket otherwise.

### New workspace (`/new`)

For members of the self-service groups and admins: **New workspace** in the
bar and the empty state. A page with one form:

| Field | Rule |
|---|---|
| Name | `<slug>` or `<slug>-<suffix>`, where the slug is the uid with `.` and `_` turned into `-`: `samuel.silva` makes `samuel-silva` or `samuel-silva-data`. It is still a workspace name, a DNS label of at most 28 characters, so a uid with a dot never becomes a host name. |
| System | the systems `web.selfService.os` allows |
| SSH keys | pasted, one per line, `<type> <key> [comment]`; the directory has no SSH keys to take |
| Memory | the most the VM grows to, from the boot size up to `web.selfService.maxMemoryGiB` |
| Disk | from 16 GiB up to `web.selfService.maxDiskGiB` |

The page says how many of their quota the person has left. **Create**
refuses, saying why, a name taken by any workspace, a disk or a config, a
name outside the rule, a quota already used, or limits beyond the host's.

**The quota** is how many workspaces made at run time a person may own:
`web.selfService.quota`, 1 unless set, for everyone, and per person what an
admin sets on the host page, in `<stateDir>/self-service/quotas/quotas.json`,
without a rebuild. Workspaces from the Nix configuration don't count.

**How it is made.** `workbench-web` has no privilege, so it writes the
request to `<stateDir>/self-service/requests/<ws>.json` and starts
`workbench-create@<ws>.service`, which polkit lets it start and nothing else. That
unit runs `workbench self-service create <ws>` as root, which:

1. checks the request again: the name rule, the quota, the limits and that
   the name is free;
2. runs `web.selfService.environmentCommand <owner>` and keeps what it
   prints as the workspace's secret environment. If it fails or prints
   nothing, the request is refused with "Your workspace has no credential
   for its app yet. Ask an admin." **No workspace is made without one**;
3. picks the first free address in the subnet's upper half, `.128` to
   `.254`. With self-service on, the Nix configuration must declare its
   workspaces below `.128`;
4. adds the VM's user, `wb-<ws>`, as a userdb record in `/run/userdb`. With
   `users.mutableUsers = false` NixOS deletes users added any other way;
5. adds its tap as systemd-networkd files in `/run/systemd/network`;
6. for Ubuntu, writes its cloud-init seed;
7. writes the config to `<stateDir>/configs/<ws>.json`, makes the disk and
   the secrets directory, and starts `workbench@<ws>`.

`workbench-self-service.service`, at boot, puts the userdb records and
network files back from the configs, since `/run` is empty then, and starts
the workspaces not stopped. The answer to the form is the list with the new
workspace selected, booting; or the form again with why it was refused,
from `<stateDir>/self-service/results/<ws>.json`. `self-service/` is root's:
the UI writes only in `requests/` and `quotas/`, and root reads them without
following links, so the UI can't point root elsewhere. Making and destroying
take one lock, so an address or a user id is never handed out while it is
being taken away, and a name whose disk or secrets directory is still there
is taken. Before its first change, a create or a destroy writes the config
it is working on to `self-service/pending/`, root's. If it dies half way,
the next create, destroy or boot cleans up what that config names, unless
the config is in `configs/`, which means the workspace is whole.
The reconciliation timer marks workspaces under the same lock, and a new
workspace never inherits a stop marker from an earlier one with its name.

The host reads `/etc/workbench` and then `<stateDir>/configs`. A name in
both, which the Nix configuration can declare after someone made it, loads
from neither, so neither workspace starts on the other's disk until one of
them goes. Workspaces from the Nix configuration show in the UI as they
are, without **Destroy**.

### Leaving

A person removed from the self-service groups, or from the directory, never
signs in again to say so. A timer, `workbench-self-service-reconcile`, asks
for each owner of a workspace made at run time with
`web.selfService.ownerCommand <owner>`: exit 0 means they stay, exit 1 that
they left; anything else, such as the directory being down, decides
nothing. For those who left it marks their workspaces `<stateDir>/.left/<ws>`
and stops them, at every pass while they run, whoever started them; the list
shows "owner left · disk kept". Disks stay until an admin destroys
them. The mark is what keeps them stopped, not Stop's: an owner who comes
back gets it removed, and the workspaces they hadn't stopped themselves
start again.

### Host (`/host`, admins)

Host memory, from `/proc/meminfo` and `/run/vmkit/memory/<pool>.json`:

- one bar of the host's total: what each pool holds (`committedBytes`),
  what it is waiting for (`pendingBytes`), the reserve, and what's free;
- a row per pool with those numbers and "denied 4 min ago" when `deniedAt`
  is recent;
- "no data from `<pool>` since 10:12" when its `updatedAt` is over 30 s old;
- below, every workspace with its owner, memory now, boot and most;
- **Quotas:** each person who owns a workspace made at run time or has a
  quota of their own, how many they own, and their quota, which the admin
  changes in place. A uid field adds someone.

### Errors

- **403** at `<domain>/w/<ws>/…` for a workspace that isn't yours, and the
  same page for one that doesn't exist, so names don't leak.
- **403** at `<ws>.<domain>` from the authorization check: "This workspace
  isn't yours. You're signed in as bob." with **Sign out** and a link to
  the UI.
- **502** when the host can't be read (systemd or a config fails): "Couldn't
  read the workspaces from the host." with the error under Details.

## App updates

A restart brings the app and the system on the host now; until then the VM
runs what it started with. `workbench run` writes the store path it starts
the app from to `<runtimeDir>/app`. The UI compares it with the config's
`app`: when they differ, the workspace has an update and the panel says so.

## The service

`workbench-web.service` runs `workbench-web` as its own user,
`workbench-web`:

- It reads the configs and `/run/vmkit/memory/*.json`, and runs `systemctl
  show`, which needs no privilege.
- It is in the group `workbench`, which gets read and write on each VM's
  sockets: `<runtimeDir>/vm` becomes setgid `wb-<ws>:workbench`, and the
  unit's `ExecStartPost`, `workbench share-sockets`, makes the API, vsock
  and serial sockets `0660` once cloud-hypervisor has made them.
- A polkit rule lets it start, stop and restart `workbench@*.service`, and
  nothing else.
- It owns `/var/lib/workbench/.stopped/`.
- It listens on `/run/workbench-web/ui.sock` and
  `/run/workbench-web/authz.sock`, `0660` with nginx's group, and on
  `/run/workbench-web/console.sock`, for `workbench console`.

Every `POST` must have `Origin: https://<domain>`. The workspaces are
sibling subdomains, so they are the same site and `SameSite` cookies don't
stop them; the `Origin` check does.

### What each action calls

| Action | Underneath |
|---|---|
| List | the configs, `systemctl show`, cloud-hypervisor's `Info`, an agent `ping`; in parallel, 2 s per VM |
| Start | remove the stop marker, `systemctl start workbench@<ws>` |
| Stop | write the stop marker, `systemctl stop workbench@<ws>`: `workbench shutdown` presses the power button and waits up to 90 s |
| Restart | `systemctl restart workbench@<ws>` |
| Create | the request file, `systemctl start workbench-create@<ws>`, 5 min |
| Destroy | `systemctl start workbench-destroy@<ws>` |
| Quota | `<stateDir>/self-service/quotas.json`, written in place |
| Sign in | the agent's `sign-in` over vsock, 90 s |
| Console | the serial socket, shared |
| Host | `/proc/meminfo`, `/run/vmkit/memory/*.json` |

### Live updates

`GET /events` is a server-sent event stream. While anyone listens, the
server collects the state every 2 s and sends what the listener may see,
as JSON, whenever something changed; the hydrated page renders the list and
side panels from it and keeps the selection. Without JavaScript the page is
just a page, current when loaded.

## Endpoints

| Method | Path | Answer |
|---|---|---|
| `GET` | `/` | the Workspaces page; `?w=<ws>` selects one |
| `GET` | `/events` | event stream of the list |
| `GET` | `/api/workspaces` | the user's workspaces as JSON; all of them for admins |
| `GET` | `/w/<ws>/start`, `/w/<ws>/stop`, `/w/<ws>/restart` | confirmation pages |
| `GET` | `/w/<ws>/sign-in` | the sign-in page |
| `POST` | `/api/control` | server function: `ws` and `action` (`start`, `stop`, `restart`); `303` to `/?w=<ws>` from a form |
| `POST` | `/api/sign-in` | server function: `ws`; `303` to the app's link |
| `GET` | `/new` | the New workspace page |
| `POST` | `/api/create` | server function: `name`, `os`, `ssh_keys`, `memory_gib`, `disk_gib`; `303` to `/?w=<ws>` |
| `GET` | `/w/<ws>/destroy` | the Destroy page |
| `POST` | `/api/destroy` | server function: `ws`, `confirm`, the name typed again; `303` to `/` |
| `POST` | `/api/quota` | server function, admins: `user`, `quota`; `303` to `/host` |
| `GET` | `/w/<ws>/console` | the console page |
| `GET` | `/w/<ws>/console/socket` | WebSocket to the shared console |
| `GET` | `/host` | host memory, admins |
| `GET` | `/denied` | the 403 page nginx shows at a workspace's address |
| `GET` | `/authz` | on `authz.sock` only, for nginx |

A workspace as `/api/workspaces` gives it:

```json
{
  "name": "alice",
  "owner": "alice",
  "url": "https://alice.ws.example.com",
  "os": "ubuntu",
  "state": "running",
  "since": "2026-11-16T09:12:00Z",
  "appUpdate": true,
  "signIn": true,
  "memoryBytes": 3435973836,
  "bootMemoryBytes": 2147483648,
  "maxMemoryBytes": 17179869184,
  "diskBytes": 68719476736,
  "diskUsedBytes": 44023414784
}
```

## The Nix module

```nix
services.workbench.web = {
  enable = true;
  admins = [ "alice" ];
  adminGroups = [ "workbench-admins" ];
  oidc = {
    issuerUrl = "https://sso.example.com/realms/example";
    clientId = "workbench";
    clientSecretFile = "/run/secrets/workbench-oidc";
    cookieSecretFile = "/run/secrets/workbench-cookie";
  };
  selfService = {
    enable = true;
    groups = [ "workbench" ];
    quota = 1;
    os = [ "ubuntu" "nixos" ];
    maxMemoryGiB = 16;
    maxDiskGiB = 128;
    # Prints the workspace's secret environment for an owner; fails or
    # prints nothing when there is none.
    environmentCommand = "/run/current-system/sw/bin/workspace-env";
    # Exits 0 while an owner may keep their workspaces.
    ownerCommand = "/run/current-system/sw/bin/workspace-owner";
  };
};
```

It sets up `workbench-web.service`, oauth2-proxy, the nginx virtual host
for `<domain>` and `auth_request` on every workspace's, the group, the
socket permissions, the stop marker condition and the polkit rule. Without
`web.enable`, workspaces are served as before, without SSO.

The Keycloak client is a confidential OIDC client with the one redirect
URI `https://<domain>/oauth2/callback`, a mapper that puts the LDAP uid in
`preferred_username`, and a group membership mapper, `full.path=false`, in
the ID token and userinfo. A sign-in that starts at a workspace's address
comes back through that callback, then to the workspace.

Both commands run as root, with the owner as their one argument. What a
credential is and where people are listed is the host's business; the
module knows neither.

## Tests

- Rust: the authorization rule, the `authz` answer and its cookie filter,
  the state derivation, the sign-in link check, the console fan-out, and
  the pages and server functions against fake systemd and agent.
- Rust, too: the name rule, the SSH key check, the quota, the request's
  validation and the address picked.
- NixOS: `checks.workspaces` has OpenLDAP with the users and groups, Dex
  reading it with its LDAP connector, oauth2-proxy and the UI, so groups
  come from the directory as in production. It signs in as alice and as
  bob, checks each reaches only their own workspace and gets 403 on the
  other's, that the app never sees the SSO cookie, that stop survives a
  reboot of the unit's condition, and that sign-in redirects to a link of
  the workspace. For self-service: alice, in the group, creates a workspace
  and reaches it; bob, outside it, can't create; the quota refuses alice's
  second; a failing credential command refuses the request; and taking
  alice out of the group cuts her off at the next refresh, and the timer
  stops her VM.

## Later

- **Credentials issued on demand**, through the credential provider's
  own API, instead of a command that reads them.
- **A page per workspace** with tabs: Console, then secrets, SSH keys, port
  previews and the NixOS config. The name in the panel links to it.
- **Sign in another device**, with a QR code.

## Open

- Whether cloud-hypervisor's serial socket takes more than one client. The
  shared console doesn't need it, but `workbench console` without
  `workbench-web` does.
- T3 Code's `bin/sign-in-link` belongs in t3code-nix. Until then the
  workbench module writes it, around `t3 auth pairing create --json`.
- A workspace's app can't use paths under `/oauth2/`: oauth2-proxy has them.
- The name rule is a prefix: someone whose slug is `samuel` may take
  `samuel-silva`, before `samuel.silva` does. Names are first come.
