# Web UI

The specification of workbench's web UI: what it shows, who may do what,
and how each piece works on the front. The static mockup of every screen
is [`web-ui/index.html`](web-ui/index.html); `workbench-web` implements
it, and `services.workbench.front` sets it up.

## Premises

- **One address.** The UI is at `https://<domain>`, each app of a
  workspace with an address at `https://<ws>--<app>.<domain>`, and
  `https://<ws>.<domain>` leads to the workspace's panel; over SSH,
  `<ws>.<domain>` is the workspace. One certificate covers `<domain>` and
  `*.<domain>`.
- **A workspace is a VM; apps are what runs in it**, opened from outside
  it, in the browser or from a terminal. None, one or several, and none is
  its main one. See [Apps](#apps).
- **Single sign-on everywhere.** Keycloak, through oauth2-proxy, guards the
  UI and every workspace. Nobody reaches a workspace's app without signing in,
  whatever the app does about accounts; a terminal gets in with a computer
  set up from the UI, see [From a terminal](#from-a-terminal).
- **Three roles, from the directory's groups.** A person sees and controls
  the workspaces they own. Members of the self-service groups also create
  their own, within a quota. Admins, members of the admin groups or names in
  the Nix configuration, see and control all of them.
- **No database.** State comes from the front's registry and every node at
  once, as [multi-node.md](multi-node.md) has it: each node says what
  `workbench status` collects there, from the configs, systemd and the
  VMs' sockets. Besides, a few files the front writes: the quotas, the
  closed nodes and the UI's paired devices. A host on its own is a front
  and its one node, on one machine.
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
              ├─ <domain>              ──► workbench web   (unix socket)
              ├─ <ws>--<app>.<domain>  ──► that app in the VM  (after an owner check)
              └─ <ws>.<domain>         ──► 302 to <domain>/?w=<ws>

ssh ──► workbench-connect ──WebSocket──► nginx ──► workbench web ──► <ws>:22
                                        <domain>/connect/ssh/<ws>, no SSO:
                                        a device's credential, the owner check
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
  session lives in Redis on the front, so the cookie is a small ticket that
  survives the refresh without nginx passing a new one back.
- **`workbench-web` listens only on unix sockets** that nginx's group can
  reach, so that header cannot be forged.
- **Signing out** is `/oauth2/sign_out`, which also ends the Keycloak session
  (`--backend-logout-url`).

### Workspaces behind SSO

All apps of all workspaces share one virtual host, `*.<domain>`, so a
workspace made at run time, or an app added to one, needs no nginx reload.
It takes the workspace's name and the app's id from the host name,
`<ws>--<app>`; workspace names have no `--`, and app ids no dash, so it
reads one way. A host name without an app is sent to the workspace's
panel. The rest has an `auth_request` to `workbench-web`'s authorization
socket (`/run/workbench-web/authz.sock`, `GET /authz`). That gets the two
names and the browser's cookies, and:

1. asks oauth2-proxy (`/oauth2/auth`) who the cookie belongs to and their
   groups;
2. answers `401` when nobody is signed in (nginx redirects to
   `/oauth2/start?rd=<the URL>`), `403` when the user may not use the
   workspace (see [who may what](#who-may-what)) or it hasn't the app, `200`
   otherwise. An app it has is one of its config's, or one the running VM
   has that its next start leaves out;
3. with the `200`, returns the app's address in `X-Workbench-Upstream`
   (`<ip>:<port>`, from the registry), which nginx proxies to, and the
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
workspace's record. On a 403, nginx shows the UI's `/denied` page at the
app's address, styled from `https://<domain>`: "isn't yours", or, for a
workspace of the viewer's, that it has no such app and a link to its panel.

The check lives in `workbench-web`, not in nginx, so the registry, the
groups, the admin list and the owner rule are in one tested place, which
the tunnel to a workspace's SSH server asks too.

The owner, the address and the apps are the registry's, never a node's:
an app answers while its VM runs even when its node's API doesn't. An app
the record no longer has still opens while the running VM has it, until
its restart: the check tells it from a state collected within the last
second, whoever asked for it, and collects one when it has none. While
a workspace moves its VM is down, so the check answers `403` and the
`/denied` page says so, with a `503`: "`<ws>` is moving to another node.
It is down while its disk is copied, and its apps answer again once it's
there."

An app without an address has nothing there: the check answers `403`, as
for an app the workspace hasn't, and the page says it is reached from a
terminal.

### Who may what

| | Sees and controls | Creates |
|---|---|---|
| Owner of a workspace from the Nix configuration | that workspace | — |
| Owner of a workspace made at run time | that workspace, while in a self-service group | — |
| Member of `front.selfService.groups` | their own | their own, within their quota |
| Admin: in `front.adminGroups`, or named in `front.admins` | every workspace, the host page with its nodes, the quotas, the memory and the moves of workspaces made at run time | their own, within their quota |

A workspace made at run time is reachable by its owner only while the
owner is in a self-service group: leaving the group cuts it off at the next
session refresh, before the reconciliation below stops its VM.

## Apps

An app is what runs in a workspace and is opened from outside it. One with
a port is opened from a browser, at an address of its own,
`<ws>--<app>.<domain>`, so each is an origin of its own and their cookies
never mix. One without, such as herdr's runtimes, is reached from a
terminal of the person's own computer, over SSH through the front. The
fleet offers them, `services.workbench.fleet.apps`; each workspace says which it has,
from none to all, and has those whose `default` is true unless it says
otherwise. T3 Code, `t3code`, is opened in the browser, and every workspace
has it by default; Herdr, `herdr`, and Herdr app, `herdrcore`, are reached
from a terminal, and workspaces opt into them.

An app is a bundle the node shares with the guest at `config/apps/<id>`:
`bin/serve`, which the guest agent runs at every boot as the owner, as a
unit of its own, `workspace-app-<id>`, with `APP_PORT` and `APP_URL` for
an app with an address, and the owner's user manager, which lingers, at
`XDG_RUNTIME_DIR`; and, if the app has a sign-in of its own,
`bin/open-link`, which prints a one-time link that signs a browser in to
it. systemd restarts a server when it ends; a oneshot app's `serve`, such
as Herdr app's, starts what it runs as user services and ends, and its unit
stays active.

**What the VM booted with stays until it restarts.** The record and the
node's template say what the next start boots: the workspace's packages
and its apps, each at a version. `workbench run` writes what it boots to
`<runtimeDir>/booted.json` on the node. Their difference, computed on the
front in one place, `workbench_model::apps::changes`, is everything that
waits for a restart:

| Change | Comes from | The panel says |
|---|---|---|
| `appAdded` | an app in the record, not in the VM | "T3 Code comes with the next restart." It doesn't open until then. |
| `appRemoved` | an app in the VM, not in the record | "T3 Code goes with the next restart." It still opens until then. |
| `appUpdated` | another bundle, port or kind (oneshot) | "A new version of T3 Code comes with the next restart." |
| `packages` | another packages bundle: what is on the PATH | "New packages come with the next restart." |

The agent is apart from the packages: a newer workbench brings a newer
agent with the next start, and is no change.

A VM that doesn't run has no changes: its next start boots the record
whatever it is. Each app's stage, ready, coming, going or updating, and
whether it opens now, follow from the changes; the UI renders them and
decides nothing. Anything else a restart brings later, such as memory, is
one more kind of change.

A VM a node from before apps started left `<runtimeDir>/app` instead: it
runs no app the node knows of, so its apps come with its next restart.

## From a terminal

An app without an address is reached over SSH, from a terminal of the
person's own computer, with nothing but HTTPS to the front between them: no
key to register, no port to open. The person's computer gets in with a
credential the UI gave it, which stands in for single sign-on.

**Certificate authorities.** The front has two SSH CAs in
`<stateDir>/ssh`, which `workbench-ssh-ca.service` makes once: the host
CA, root's, and the user CA, which only `workbench-web` reads. At every
start, the front signs the workspace's host key, which it keeps in
`<stateDir>/ssh/hosts/<ws>`, for `<ws>.<domain>`, and gives it to the
node, whose `workbench run` shares it root-only in the secrets share. The guest's sshd serves it, and trusts
the user CA's keys for the owner only: `ssh/principals/<owner>`, in the
config share, is the one `AuthorizedPrincipalsFile`, with the owner's
name, so a certificate never signs in as another account, root included.
The owner's keys from the Nix configuration work as well.

**Pairing.**

1. A workspace's Connect page, behind single sign-on, makes a pairing
   code: it works once, for 10 minutes, for the person signed in, and
   lives in `workbench-web`'s memory.
2. On their computer, `curl -fsSL https://<domain>/connect/install.sh | sh
   -s -- <code>` downloads `workbench-connect` for its architecture and
   runs `workbench-connect setup <code>`, which installs it in
   `~/.local/bin`.
3. `setup` trades the code, `POST /connect/pair`, for a device
   credential, which works for 30 days or until revoked, while the person
   signs in to the UI at least every 7 days. The UI keeps its hash, never
   the credential, in `/var/lib/workbench-web/devices.json`, with its
   person, their groups, when single sign-on last said them, its label and
   when it was made and last used. A person has at most 32.
4. `setup` writes `~/.config/workbench`: the credential, `0600`, an
   ed25519 key pair, `known_hosts` with `@cert-authority *.<domain>` and the
   host CA, and `ssh_config`, which it includes at the top of
   `~/.ssh/config`. For `Host *.<domain>`, it has the person as `User`,
   `workbench-connect tunnel %h` as `ProxyCommand`, the key, its
   certificate and those `known_hosts`.

**Certificates.** `POST /connect/cert`, with the credential, signs the
computer's public key with the user CA for 8 hours, with the person as
its one principal. ssh loads its certificate before it runs `ProxyCommand`,
so the tunnel would renew it too late; `ssh_config` has `Match exec
"workbench-connect login"`, which ssh runs as it reads its configuration
and which gets a new certificate when this one ends within 15 minutes.

**The tunnel.** `GET /connect/ssh/<ws>` is a WebSocket, with the
credential, to port 22 of the workspace's VM, which only its node and the
front reach.
At every connection it asks the access check's owner rule, from the same
function, with the person and the groups single sign-on last gave them,
and whether the VM runs; while it is open, it asks the device and the
owner rule again every 30 seconds, and closes once either says no, so
revoking a device ends its sessions. Then sshd lets in the person the
certificate names, by name: an admin passes the tunnel to someone else's
workspace, but not sshd.

These endpoints are on a socket of their own, `connect.sock`, which nginx
reaches without single sign-on; each checks what it is given:

| Endpoint | Checks |
|---|---|
| `GET /connect/install.sh`, `/connect/bin/workbench-connect-<arch>-linux` | nothing: nginx serves them from the store, and they hold no secret |
| `POST /connect/pair` | the code |
| `POST /connect/cert` | the credential |
| `GET /connect/ssh/<ws>` | the credential, the owner rule and that the VM runs |

**Devices.** `/devices`, from the user's menu, and the Connect page list
the person's devices, with when each was set up, last used and works
until, and revoke them. Every visit to the UI, the panel included, notes
the person's groups for their devices as single sign-on says them then. A
device whose person hasn't signed in for 7 days works no more until they
do: someone removed from the directory or from a group loses the way in
from a terminal within a week, as they lost it in the browser.

**The apps.** Herdr, `herdr`, runs the TUI's server, `herdr server`, as
any app; `herdr --remote <ws>.<domain>` attaches to it. Herdr app,
`herdrcore`, is oneshot: it imports the workspace's environment and
secrets into the owner's user manager, then runs `hc server start`, which
starts herdr-core's core as a user service, and its terminals' holders
after it; herdr-app's Add machine and `hc machine add <ws>.<domain>`
reach it. Their commands find themselves over SSH: an Ubuntu guest's
`/etc/environment`, and a NixOS guest's `/etc/set-environment`, which bash
reads for SSH commands, put the workspace's tools first on the `PATH`.
`herdr update` and `hc update` say that a newer one comes with the
workspace's host.

## Screens

```
https://<domain>/                    Workspaces
https://<domain>/?w=<ws>             Workspaces, with <ws> in the side panel
https://<domain>/w/<ws>/open/<app>   Open <app> of <ws>
https://<domain>/w/<ws>/connect/<app>   Connect to <app> of <ws>, from a terminal
https://<domain>/devices             The person's devices
https://<domain>/w/<ws>/apps         The apps of a workspace made at run time
https://<domain>/w/<ws>/console      Serial console
https://<domain>/host                The nodes and the quotas (admins)
https://<domain>/new                 New workspace (self-service)
https://<domain>/w/<ws>/destroy      Destroy a workspace made at run time
https://<domain>/w/<ws>/move         Move a workspace to another node (admins)
```

### Workspaces (`/`)

A top bar with the mark, a field that finds a workspace by name (`Ctrl K`
or `/`) and the user's menu (**Your devices**, **Sign out**). A sidebar with **Workspaces**,
**Host** (admins only), and under **Yours** the user's workspaces with
their status dot. The main area lists workspaces in
groups by state (Failing, Unreachable, Changing, Starting, Running,
Stopped), with chips to filter by All, Running and Stopped. Changing is
Restarting, Stopping and Moving. Admins get a chip for
**Mine** and an Owner column.

A row has the status dot, the name, a short note (its apps, "T3 Code after
a restart", "no apps", "booting", "since Nov 3"), the system, the memory and
one primary action:

| State | Primary action |
|---|---|
| Running, with an app with an address | **Open**, its first such app, with a one-time link the first time from this browser |
| Starting, with an app | **Open** (the app may already answer) |
| Running or Starting without one, Failing | **Console** |
| Stopped | **Start** |
| Stopping, Restarting | none; the row says what it waits for |
| Moving | none: "until it's there" |
| Unreachable | none: "waiting for its node" |

**A line along the bottom edge of each row is its memory:** its length is
what the VM holds now out of the most it may grow to. Sage while running,
amber while starting, red while failing; a stopped, moving or unreachable
workspace has none.

**The side panel** shows the selected workspace: name, status and since
when, what waits for a restart, then Owner (admins), Node (admins),
System, Memory (a meter with a tick at the boot size, and for an admin on
a workspace made at run time **Edit…** at its right, which opens the
memory form in a dialog, its page without JavaScript), Disk on its node,
its apps and the actions:

- **Apps**: each app's **Open `<app>`**, the first primary (`↵`), and a ⋯
  menu with **Open on another device…**, **Open with a new link** and
  **Copy address**. An app that comes with the next restart shows
  "T3 Code · after a restart", disabled. A workspace made at run time has
  **Edit…** there, and without apps a dashed box, "No apps yet", with
  **Add an app…**; one from the Nix configuration says its console is the
  way in. An app without an address has **Connect to `<app>`…** instead,
  which leads to its Connect page; for an admin on someone else's
  workspace, "`<app>` · from `<owner>`'s terminal", disabled, as SSH to it
  signs in as its owner.
- **Start `<ws>`**, when stopped
- **Console** (`C`)
- **Restart…** (`R`), unless it is stopped; primary when something waits
  for it
- **Stop…**, apart at the bottom below a rule, plain: its dialog says what
  it costs
- **Destroy…**, at the other end of that row, in red, only for a workspace
  made at run time

On a phone the panel is a page of its own (`/?w=<ws>` with the list
hidden), reached by tapping a row.

Keyboard: `J`/`K` move, `↵` opens, `C` opens the console, `R` restarts, `/` or `Ctrl K` focuses the command field. Shortcuts never act on
their own; Restart and Stop still open their dialog.

#### States

The state of a workspace comes from its unit, the VM and the agent, as its
node says them:

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

Two more come from the registry and the nodes, never from systemd, and
owners see them as anyone does, with nothing of nodes: not
even in what a refusal says, where "Its node, a, doesn't answer" reads
"Its node doesn't answer" for anyone but an admin:

| Registry, node | Shown as |
|---|---|
| a move under way | **Moving** |
| its node doesn't answer, or is of another build | **Unreachable** |

- **Loading:** skeleton rows, the first time only.
- **Empty:** "You don't have a workspace yet", what one is, and **New
  workspace** for whoever may create one; that an admin creates it for
  everyone else.
- **Starting:** "booting"; a NixOS workspace takes longer on its first boot
  after an update, while it copies the apps onto its disk.
- **Failing:** a notice in the panel: "The VM won't stay up. Its console
  shows why." with **Console** and **Restart…**.
- **Waiting for a restart:** what the next start changes, as above, with
  **Restart…** primary.
- **Lost connection:** when the live updates drop, a bar says "Reconnecting…
  Showing the state at 10:42." and actions stay enabled: they are plain
  forms, and their answer says if they failed.
- **Moving:** a note in the panel, "Moving to another node. It is down
  while its disk is copied. Its actions wait until it's there.", and its
  actions, its apps included, shown disabled.
- **Unreachable:** a note, "Its node doesn't answer. What it shows is what
  it last said. Its actions wait until its node is back.", its last known
  details greyed, and its actions disabled. The UI keeps in memory what
  each workspace's node last said of it. An action posted anyway is
  refused: "`<ws>`'s node doesn't answer".

### Open an app (`/w/<ws>/open/<app>`)

Some apps have accounts of their own on top of single sign-on; T3 Code, for
one, lets a browser in only after it follows a one-time link. The UI gets
that link from the app and sends the browser to it, so the person never
copies a credential:

1. **Open `<app>`** posts the workspace and the app to `/api/open`, in a
   new tab.
2. `workbench-web` asks the workspace's node, which asks the guest agent
   for a link (`op: "open-link"`, over vsock). The agent runs the app's `bin/open-link` as the owner; it prints
   one URL and nothing else on stdout.
3. The URL must be on the app's own origin. `workbench-web` answers
   `303 See Other` to it, with `Cache-Control: no-store`. The link is never
   shown and never logged.
4. The browser remembers, in `localStorage`, that it opened `<ws>/<app>`,
   so the next time **Open** is just the app's address. **Open with a new
   link**, in the ⋯ menu, signs it in again.

**Open on another device…** asks for the same link as data, for a dialog
with its QR code and the link to copy: it works once, and whoever has it
gets in. It needs JavaScript; without, the menu item leads to the open page.

The `GET` of `/w/<ws>/open/<app>` is a page with one button. It opens
nothing, so link previews don't spend links. An app without
`bin/open-link` opens at its address; the agent's `ping` answer names the
apps with the hook.

Errors, on the open page:

| Situation | Message | Action |
|---|---|---|
| Stopped | "`<ws>` is stopped. Start it, then open `<app>`." | **Start `<ws>`** |
| It comes with the next restart | "`<app>` comes with the next restart of `<ws>`." | **Restart `<ws>`…** |
| The agent doesn't answer | "`<app>` isn't answering in `<ws>` yet." | **Try again**, **Console** |
| The hook failed | "`<app>` couldn't make a sign-in link." Its stderr under Details. | **Try again**, **Console** |
| Not a URL of the app | "`<app>` answered something that isn't a link to it." | **Try again** |
| 90 s without an answer | "`<app>` took too long to answer." | **Try again** |

### The apps of a workspace (`/w/<ws>/apps`)

A workspace made at run time changes its apps from its panel, **Edit…** or
**Add an app…**, in a dialog, or on this page without JavaScript: a
checkbox per app `front.selfService.apps` offers, then **Save** or **Save and
restart**; a stopped workspace has only **Save**. Nothing on its disk
changes. A workspace from the Nix configuration has its apps there.

`workbench-web` writes the request to
`<stateDir>/self-service/requests/<ws>.apps.json` and starts
`workbench-apps@<ws>.service`, which runs `workbench self-service apps <ws>`
as root on the front: under the registry's lock, it checks the apps are
offered, puts them in the record and says how it went in
`results/<ws>.apps.json`. The node asks nothing of it: the next start
brings them. One
change of a workspace's apps at a time: another while one runs is refused,
since it would join that run, and an outcome older than the request counts
for nothing. **Save and restart** then restarts the workspace; **Save**
leaves the change waiting for the next start, and the panel says so.

Every deploy makes the nodes' configs again from their template, keeping
what each workspace was given: its system, even one no longer offered,
and its memory.

### The memory of a workspace (`/w/<ws>/memory`, admins)

The most a workspace made at run time grows to, which its owner picked on
**New workspace**: an admin changes it from the panel's **Edit…**, a
dialog, or here, from the boot size to
`front.selfService.maxMemoryGiB`, or to `auto` where that is `auto` too:
as far as its node has memory, read at every start. A size beyond its
node's memory is refused. Then **Save** or **Save and restart**; a
stopped workspace has only **Save**. The VM boots with its boot memory
and the node plugs more as it runs short, up to that most, which is the
only cap: T3 Code leaves its agents' memory to the VM. A workspace from
the Nix configuration has its memory there. Its owner sees the memory, not
the page.

As with apps, `workbench-web` writes
`<stateDir>/self-service/requests/<ws>.memory.json` and starts
`workbench-memory@<ws>.service`, which runs `workbench self-service memory
<ws>` as root on the front: under the registry's lock, it checks the
limits, puts the most in the record, and says how it went in
`results/<ws>.memory.json`; the node boots it with a boot size no larger. One change at a time, and **Save** leaves it
for the next start. Later deploys keep it.

### Connect to an app (`/w/<ws>/connect/<app>`)

For an app without an address, the owner sees, under **Workspaces**, the
way back, and the app's name. Anyone else, an admin included, gets `403`:
the workspace's SSH server takes only its owner's certificates.

1. **Set this computer up**: `curl -fsSL https://<domain>/connect/install.sh
   | sh -s -- <code>`, with a new code every time the page renders, and
   **Copy**, which says "Copied" once the clipboard took it, and otherwise
   selects the command. Once per computer, on Linux.
2. **Reach `<app>`**: the steps of the app's `connect`, with `@host@` as
   `<ws>.<domain>` and commands between backticks: `herdr --remote
   <ws>.<domain>` for Herdr; herdr-app's Add machine, or `hc machine add
   <ws>.<domain>`, for Herdr app; `ssh <ws>.<domain>` for any other. A
   first step that ends in its one command has it in a box with **Copy**;
   the rest are text.
3. **Your devices**, as on `/devices`, each with **Revoke**.

Once the viewer has a computer set up, reaching the app comes first, and
setting another up folds under **Another computer?**.

The page is `no-store`: its code is the viewer's. Its referrer policy is
the UI's, `same-origin`: the UI moves within one document, and
`no-referrer` would send its forms with `Origin: null`. An app with an
address has none, a `404`, as has an app the workspace hasn't; neither
makes a code. Nor does a request from a workspace's page that isn't a
person following a link (`Sec-Fetch-Site` other than `same-origin` or
`none`, and not a navigation to a document): it gets `403`, so a
workspace can't spend the viewer's codes in the background.

### Start, stop and restart

| Action | Confirmation | The dialog says |
|---|---|---|
| **Start** | none | — |
| **Restart** | dialog | Stops everything running in the VM; open app tabs lose their connection for about a minute. Keeps everything on its disk. Brings the apps and packages it has now, as the host has them. |
| **Stop** | dialog, red button | Stops the VM and its apps. Keeps everything on its disk. It stays stopped, also across host reboots, until someone starts it. |
| **Destroy** | its page, typing the name | Deletes the disk and everything on it, for good. Only for a workspace made at run time, by its owner or an admin, as `workbench destroy` on the front; a declared one is `workbench destroy` on its node. |

Without JavaScript, **Restart…** and **Stop…** are links to
`/w/<ws>/restart` and `/w/<ws>/stop`, pages with the same text and button.

After a confirm, the page comes back with the row in its new state; the
live updates carry it to the end. An action on a workspace that is already
changing state is refused with "`<ws>` is already restarting".

**Stopped stays stopped.** Stop asks the node, which writes
`<stateDir>/.stopped/<ws>` before it stops the unit; the unit has
`ConditionPathExists=!<stateDir>/.stopped/%i`, so a reboot of the node
skips it. Start removes the file first. `workbench start` and
`workbench stop` on the node do the same from the shell.

### Console (`/w/<ws>/console`)

The VM's serial console in the browser, for the owner and admins: what the
guest prints while it boots, the `ttyS0` login, and the boot menu of a
NixOS workspace whose new generation doesn't boot.

- A terminal fills the page, under a bar with the workspace, its state,
  **Restart…** and the number of people watching ("2 connected").
- It is **shared**: `workbench-node` keeps one connection to the serial
  socket per running VM, keeps the last 256 KiB of output, and fans it out
  to every viewer. The front relays each browser's WebSocket to it,
  `GET /workspaces/<ws>/console?viewer=<name>`. Everyone types into the same
  console.
- New viewers first get the kept output, so opening the console of a VM
  that just failed shows why. A workspace deleted and made again with its
  name starts with none of it, and the viewers of the one before are let
  go.
- **Restart with console** restarts the VM with the console open, for the
  boot menu: systemd-boot waits `boot.loader.timeout` seconds, 1 by default.
- When the VM stops, the terminal says so and reconnects when it starts.
- `workbench console` on the node attaches to the same shared console,
  `/run/workbench-node/console.sock`, when `workbench-node` runs, and to
  the serial socket otherwise: the same output, viewers and input.
- A node that doesn't answer is a `502`; a workspace that moves has no
  console until it's there.

### New workspace (`/new`)

For members of the self-service groups and admins: **New workspace** in the
bar and the empty state. A page with one form:

| Field | Rule |
|---|---|
| Name | `<slug>` or `<slug>-<suffix>`, where the slug is the uid with `.` and `_` turned into `-`: `samuel.silva` makes `samuel-silva` or `samuel-silva-data`. It is still a workspace name, a DNS label of at most 28 characters, so a uid with a dot never becomes a host name. |
| System | the systems `front.selfService.os` allows |
| SSH keys | pasted, one per line, `<type> <key> [comment]`; the directory has no SSH keys to take |
| Memory | the most the VM grows to, from the boot size up to `front.selfService.maxMemoryGiB`; where that is `auto`, the largest node's, and `auto` too, which the form starts with |
| Disk | from 16 GiB up to `front.selfService.maxDiskGiB` |
| Apps | checkboxes of `front.selfService.apps`, those whose `default` is true checked; none is fine, and they can change later |
| Node | admins: every node, with its free memory, its workspaces' total maximum memory and its free disk, a closed one too; a closed, unreachable or other-build node is listed, says which, and can't be picked. It starts on the one placement takes for the form as it starts, the open node with the most memory free. Anyone else's workspace goes where placement puts it |

The page says how many of their quota the person has left. **Create**
refuses, saying why, a name taken by any workspace, a disk or a config, a
name outside the rule, a quota already used, limits beyond the front's,
and a node that is closed ("Node b is closed: it takes no new
workspaces."), doesn't answer or is of another build. The admin's pick is
the request's `node`; a pick from anyone else is ignored.

**The quota** is how many workspaces made at run time a person may own:
`front.selfService.quota`, 1 unless set, for everyone, and per person what an
admin sets on the host page, in `<stateDir>/self-service/quotas/quotas.json`,
without a rebuild. Workspaces from the Nix configuration don't count.

**How it is made.** `workbench-web` has no privilege, so it writes the
request to `<stateDir>/self-service/requests/<ws>.json` and starts
`workbench-create@<ws>.service`, which polkit lets it start and nothing
else. That unit runs `workbench self-service create <ws>` as root on the
front, which, under the registry's lock:

1. checks the request again: the name rule, the quota, the limits and that
   the name is free;
2. runs `front.selfService.environmentCommand <owner>` and keeps what it
   prints as the workspace's secret environment, in its secrets directory
   on the front. If it fails or prints nothing, the request is refused
   with "Your workspace has no credential for its app yet. Ask an admin."
   **No workspace is made without one**;
3. picks the node, the admin's or the one placement takes, and the first
   free address in the upper half of that node's subnet, `.128` to `.254`:
   the Nix configuration declares its workspaces below `.128`;
4. takes the name and the address with a `pending` record in the
   registry;
5. asks the node, `PUT /workspaces/<ws>`, to make it: the VM's user,
   `wb-<ws>`, as a userdb record in `/run/userdb`, since with
   `users.mutableUsers = false` NixOS deletes users added any other way;
   its tap, as systemd-networkd files in `/run/systemd/network`; for
   Ubuntu, its cloud-init seed; the config in `<stateDir>/configs/<ws>.json`
   and the disk;
6. makes the record whole, and starts it on the node.

`workbench-node-setup.service`, on each node at boot and whenever a deploy
changes its template, makes each config again from the template: the
node's paths, the packages and the apps' bundles change, the workspace's
own settings, its address, its VM user, its apps and the image its disk
came from don't. An app no longer offered goes, and a config from before
apps gets every app offered, which was its one. An Ubuntu workspace's
cloud-init seed is written again when what it says changed, such as the
nameservers, with an instance id made from it, so the guest sets itself up
again at its next boot, as a declared one does. A running VM keeps what it
booted with, and the panel shows what its restart brings. Then it puts the
userdb records and network files back from the configs, since `/run` is
empty at boot, and starts the workspaces not stopped. At every start the
node renders the config again from the front's record, which has the
apps and the memory people changed.

The answer to the form is the list with the new workspace selected,
booting; or the form again with why it was refused, from
`<stateDir>/self-service/results/<ws>.json`. `self-service/` is root's:
the UI writes only in `requests/` and `quotas/`, and root reads them
without following links, so the UI can't point root elsewhere. A create
that dies half way leaves its `pending` record, which the next create,
destroy or `workbench-front-setup` cleans up, with what the node made of
it; one whose node doesn't answer stays, its name and address taken, until
it does. A name whose secrets directory or host key is still on the front
is taken.

A node reads `/etc/workbench` and then `<stateDir>/configs`. A name in
both, which the Nix configuration can declare after someone made it, loads
from neither, so neither workspace starts on the other's disk until one of
them goes. Workspaces from the Nix configuration show in the UI as they
are, without **Destroy**.

### Leaving

A person removed from the self-service groups, or from the directory, never
signs in again to say so. A timer, `workbench-self-service-reconcile`, asks
for each owner of a workspace made at run time, on the front, with
`front.selfService.ownerCommand <owner>`: exit 0 means they stay, exit 1
that they left; anything else, such as the directory being down, decides
nothing. For those who left it marks their workspaces
`<stateDir>/.left/<ws>` on the front and stops them through their node,
which keeps them stopped across its reboots, at every pass while they run,
whoever started them; the list shows "owner left · disk kept". Disks stay
until an admin destroys them. An owner who comes back gets the mark
removed, and the workspaces the timer had stopped start again; those
they had stopped themselves stay stopped.

### Host (`/host`, admins)

A section per node, then the quotas. Each section has:

- the node's name and state: **up**, **closed**, **unreachable** or **other
  build**, with why for an admin; **Close `<node>`** on an open one,
  **Reopen `<node>`** on a closed one, whether it answers or not: a node
  closed for maintenance and down says both, and reopens. The front keeps
  a closed node in `<stateDir>/closed/<node>`: it runs what it has, and
  takes neither new workspaces nor moves;
- its memory, from what it says of itself, `/proc/meminfo` and its
  `/run/vmkit/memory/<pool>.json`: one bar of its total, what each pool
  holds (`committedBytes`), what it is waiting for (`pendingBytes`), the
  reserve, and what's free; a row per pool with those numbers and "denied
  4 min ago" when `deniedAt` is recent, and "no data from `<pool>` since
  10:12" when its `updatedAt` is over 30 s old;
- its disk: "Disk: 412 GiB free of 900 GiB. Its workspaces may grow to 48
  GiB together.";
- every workspace the registry has on it, running or not, and the disks it
  has that the registry doesn't, with its owner, memory now, boot and most,
  its disk, and **Move…** on each workspace made at run time; a declared
  one says "declared in Nix", since Nix places it.

**Quotas:** each person who owns a workspace made at run time or has a
quota of their own, how many they own, and their quota, which the admin
changes in place. A uid field adds someone.

### Move a workspace (`/w/<ws>/move`, admins)

**Move…** opens a dialog, or this page without JavaScript: "On a now. It
is down while its disk is copied through the front, about 6 minutes for 40
GiB, and starts on the new node if it ran. Its address changes; its disk
and apps don't.", then a choice of the open nodes of the same build other
than its own, each with its memory and disk free, and **Move `<ws>`**.
With none, "No other node takes it now." and the button disabled. A
declared workspace's page says to change its `node` in Nix and deploy.

`workbench-web` writes `<stateDir>/self-service/requests/<ws>.move.json`,
`{"node": "<node>"}`, through `Fleet::ask_move`, and starts
`workbench-move@<ws>.service`, which runs the move as root in the phases
[multi-node.md](multi-node.md) has, and says how it went in
`results/<ws>.move.json`. Its outcome is shown as a create's: the host page
once it is there, or this page again with why not, such as "Node b is
closed: it takes no moves." or a move that went back. The UI refuses
before asking a declared workspace, its own node, a node that takes no
moves, and a workspace already moving or whose node doesn't answer. One
move of a workspace at a time; one the unit hasn't finished within the
UI's wait, 11 minutes, says so, and the list shows it Moving until it's
there.

### Errors

- **403** at `<domain>/w/<ws>/…` for a workspace that isn't yours, and the
  same page for one that doesn't exist, so names don't leak.
- **403** at `<ws>--<app>.<domain>` from the authorization check: "This
  workspace isn't yours. You're signed in as bob." with **Sign out** and a
  link to the UI; **404** "`<ws>` has no app `<app>`" for one of the
  viewer's, with a link to its panel, and "`<ws>` has no address for
  `<app>`" for an app reached from a terminal.
- **502** when the registry can't be read: "Couldn't read the workspaces"
  with the error under Details. A node that doesn't answer is no error:
  its workspaces are Unreachable.

## The service

`workbench-web.service` runs `workbench-web` on the front as its own user,
`workbench-web`, with `WORKBENCH_FRONT`, the front's settings file
(`/etc/workbench-front.json`), or `--front <file>`. It runs on
`workbench::fleet::front::Fleet`: the registry and the nodes. It needs no
privilege:

- It reads `<stateDir>/registry/` and the declared records, and asks every
  node's API, `<address>:7910` through WireGuard or, for the node on the
  same machine, `/run/workbench-node/api.sock`, as the node's
  `socketGroup`, `workbench`.
- What people may make is the front settings' `selfService`: its limits,
  apps and paths; `auto` is the largest node's memory.
- A polkit rule lets it start `workbench-create@`, `workbench-destroy@`,
  `workbench-apps@`, `workbench-memory@` and `workbench-move@`, and nothing
  else: the nodes start, stop and restart workspaces.
- It writes `<stateDir>/closed/`, where Close and Reopen go, and the
  requests and the quotas in `<stateDir>/self-service/`.
- It reaches guests' apps and SSH servers at the registry's addresses,
  through WireGuard, or the taps of its own node.
- It listens on `/run/workbench-web/ui.sock`,
  `/run/workbench-web/authz.sock` and `/run/workbench-web/connect.sock`,
  `0660` with nginx's group.
- It reads the user CA's private key, which it signs people's keys with,
  and keeps the paired devices in its state directory,
  `/var/lib/workbench-web`.

Every `POST` must have `Origin: https://<domain>`. The workspaces are
sibling subdomains, so they are the same site and `SameSite` cookies don't
stop them; the `Origin` check does.

### What each action calls

| Action | Underneath |
|---|---|
| List | `Fleet::list`: the registry, and every node's `GET /workspaces` at once, 8 s each, which collects the configs, `systemctl show`, cloud-hypervisor's `Info` and an agent `ping`; with `Fleet::nodes_view`, every node's `GET /host` |
| Start, Stop, Restart | `Fleet::control`: the node's `POST /workspaces/<ws>/<verb>`, which writes or removes the stop marker and runs `systemctl`; a stop's `workbench shutdown` presses the power button and waits up to 90 s |
| Create | the request file, `systemctl start workbench-create@<ws>`, up to 10 min; its `node` is an admin's pick |
| Destroy | `systemctl start workbench-destroy@<ws>` |
| Apps | the request file, `systemctl start workbench-apps@<ws>`, which changes the record, then a restart if asked |
| Memory | the request file, `systemctl start workbench-memory@<ws>`, which changes the record, then a restart if asked |
| Quota | `<stateDir>/self-service/quotas/quotas.json`, written in place |
| Open | `Fleet::open_link`: the node's `POST /workspaces/<ws>/open-link`, the agent's `open-link` over vsock, 90 s |
| Connect | a code in memory; `devices.json`; `ssh-keygen -s` with the user CA; a TCP connection to `<ip>:22` |
| Console | `Fleet::console`: the node's console WebSocket, relayed |
| Access check, tunnel | `Fleet::config`, the registry: the owner, the address and the apps |
| Host | `Fleet::nodes_view`, and the list |
| Close, Reopen | `Fleet::set_closed`: `<stateDir>/closed/<node>` |
| Move | `Fleet::ask_move`: the request file, `systemctl start workbench-move@<ws>`, its outcome |

### Live updates

`GET /events` is a server-sent event stream. While anyone listens, the
server collects the state every 2 s and sends what the listener may see,
as JSON, whenever something changed. A collection runs apart from what
asks for it: while one runs, every page and stream gets the last state,
so a node slow to answer slows none of them. Only the first collection,
the answer to an action, and a page asked for when the last state is over
5 s old, which no one looked at since, wait for a new one, all but the
first for at most 3 s. The hydrated page renders the list and side panels from it and keeps
the selection. Without JavaScript the page is
just a page, current when loaded.

## Endpoints

| Method | Path | Answer |
|---|---|---|
| `GET` | `/` | the Workspaces page; `?w=<ws>` selects one |
| `GET` | `/events` | event stream of the list |
| `GET` | `/api/workspaces` | the user's workspaces as JSON; all of them for admins, with each one's `node` |
| `GET` | `/w/<ws>/start`, `/w/<ws>/stop`, `/w/<ws>/restart` | confirmation pages |
| `GET` | `/w/<ws>/open/<app>` | the app's open page |
| `POST` | `/api/control` | server function: `ws` and `action` (`start`, `stop`, `restart`); `303` to `/?w=<ws>` from a form |
| `POST` | `/api/open` | server function: `ws`, `app`; `303` to the app's link |
| `POST` | `/api/device-link` | server function: `ws`, `app`; the link and its QR code as JSON |
| `GET` | `/w/<ws>/apps` | the apps page of a workspace made at run time |
| `POST` | `/api/apps` | server function: `ws`, `apps[<i>]`, `restart`; `303` to `/?w=<ws>` |
| `GET` | `/w/<ws>/memory` | the memory page, admins |
| `POST` | `/api/memory` | server function, admins: `ws`, `memory_gib`, `restart`; `303` to `/?w=<ws>` |
| `GET` | `/new` | the New workspace page |
| `POST` | `/api/create` | server function: `name`, `os`, `ssh_keys`, `memory_gib`, `disk_gib`, `apps[<i>]`, and an admin's `node`; `303` to `/?w=<ws>` |
| `GET` | `/w/<ws>/destroy` | the Destroy page |
| `POST` | `/api/destroy` | server function: `ws`, `confirm`, the name typed again; `303` to `/` |
| `POST` | `/api/quota` | server function, admins: `user`, `quota`; `303` to `/host` |
| `GET` | `/w/<ws>/console` | the console page |
| `GET` | `/w/<ws>/console/socket` | WebSocket to the node's shared console |
| `GET` | `/host` | the nodes and the quotas, admins |
| `POST` | `/api/node` | server function, admins: `node`, `closed` (`true` closes, `false` reopens); `303` to `/host` |
| `GET` | `/w/<ws>/move` | the Move page, admins |
| `POST` | `/api/move` | server function, admins: `ws`, `node`; `303` to `/host`, or to `/w/<ws>/move?error=<why>` |
| `GET` | `/denied` | the page nginx shows at an app's address it refused |
| `GET` | `/w/<ws>/connect/<app>` | the Connect page of an app without an address, its owner's |
| `GET` | `/api/connect` | server function: `ws`, `app`; a new code and the viewer's devices |
| `GET` | `/devices` | the viewer's devices |
| `POST` | `/api/revoke-device` | server function: `id`, `back`; `303` to `back` |
| `GET` | `/authz` | on `authz.sock` only, for nginx |
| `GET` | `/connect/install.sh`, `/connect/bin/<file>` | without SSO: the script that sets a computer up, and `workbench-connect` |
| `POST` | `/connect/pair` | without SSO, on `connect.sock`: `{"code", "label"}` for `{"credential", "user", "domain", "hostCa", "expires"}` |
| `POST` | `/connect/cert` | without SSO, with the credential: `{"publicKey"}` for `{"certificate", "validBefore"}` |
| `GET` | `/connect/ssh/<ws>` | without SSO, with the credential: the WebSocket to `<ip>:22` |

A workspace as `/api/workspaces` gives it to its owner; an admin gets its
`node` too, and its `state` is also `moving` or `unreachable`:

```json
{
  "name": "alice",
  "owner": "alice",
  "url": "https://alice.ws.example.com",
  "os": "ubuntu",
  "state": "running",
  "since": "2026-11-16T09:12:00Z",
  "apps": [
    { "id": "t3code", "name": "T3 Code", "url": "https://alice--t3code.ws.example.com", "openLink": true },
    { "id": "herdr", "name": "Herdr", "connect": ["In a terminal: `herdr --remote @host@`"] }
  ],
  "changes": [{ "kind": "packages" }],
  "memoryBytes": 3435973836,
  "bootMemoryBytes": 2147483648,
  "maxMemoryBytes": 17179869184,
  "diskBytes": 68719476736,
  "diskUsedBytes": 44023414784
}
```

## The Nix module

```nix
services.workbench.front = {
  enable = true;
  virtualHost = { forceSSL = true; useACMEHost = "ws.example.com"; };
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
    apps = [ "t3code" ];
    # Prints the workspace's secret environment for an owner; fails or
    # prints nothing when there is none.
    environmentCommand = "/run/current-system/sw/bin/workspace-env";
    # Exits 0 while an owner may keep their workspaces.
    ownerCommand = "/run/current-system/sw/bin/workspace-owner";
  };
};
```

With `services.workbench.fleet`, which the front and every node import
alike, and on a host on its own `services.workbench.node` too: see the
README and [multi-node.md](multi-node.md). It sets up
`workbench-web.service` with `WORKBENCH_FRONT`, oauth2-proxy, the nginx
virtual host for `<domain>`, with `/connect/` apart from single sign-on,
and the one, with `auth_request`, for every app, the SSH CAs, the group of
the local node's API and the polkit rule. A front always has the web UI.

The Keycloak client is a confidential OIDC client with the one redirect
URI `https://<domain>/oauth2/callback`, a mapper that puts the LDAP uid in
`preferred_username`, and a group membership mapper, `full.path=false`, in
the ID token and userinfo. A sign-in that starts at a workspace's address
comes back through that callback, then to the workspace.

Both commands run as root on the front, with the owner as their one
argument. What a credential is and where people are listed is the front's
business; the module knows neither.

## Tests

- Rust: the authorization rule, the `authz` answer and its cookie filter,
  the state derivation, what waits for a restart and each app's stage, the
  open link check, the console fan-out, and the pages and server functions
  against a fake front.
- Rust, too: apps without an address and oneshot ones, the host key and
  its certificate, codes that work once and not for long, credentials that
  expire, are revoked and are kept as hashes, certificates for the person
  by name, and the tunnel, end to end, refusing another person's, a
  revoked or no credential and a VM that doesn't run.
- `workbench-connect`: what it tells ssh, the include it puts first, and
  when it renews its certificate.
- Rust, too: the name rule, the SSH key check, the quota, the request's
  validation and the address picked.
- Rust, against fake nodes: the list merging every node, with
  Moving and Unreachable and what an unreachable node last said; nodes for
  admins only; the access check and the tunnel from the registry while a
  node's API is down; New workspace's nodes and the admin's pick; Close and
  Reopen; Move, its refusals and its outcome; the console relayed.
- NixOS: `checks.workspaces`, a host on its own, has OpenLDAP with the
  users and groups, Dex reading it with its LDAP connector, oauth2-proxy
  and the UI, so groups come from the directory as in production. It signs
  in as alice and as
  bob, checks each reaches only their own workspace and gets 403 on the
  other's, that the app never sees the SSO cookie, that stop survives a
  reboot of the unit's condition, and that opening an app redirects to a
  link on its origin. For self-service: alice, in the group, creates a
  workspace and reaches its app; she takes the app away, which waits for a
  restart and goes with it, and adds it back; a config from before apps
  gets its app back; bob, outside the group, can't create; the quota
  refuses alice's second; a failing credential command refuses the
  request; and taking alice out of the group cuts her off at the next
  refresh, and the timer stops her VM.
- NixOS, from a terminal: alice and bob, as users of the client, set
  their computers up with a code from the Connect page, by
  `install.sh`; `ssh <ws>.<domain>` reaches each one's workspace through
  the tunnel, by certificates both ways, and bob's credential doesn't reach
  alice's, nor once he revoked it. In both guests herdr's server and
  herdr-core's core run; `herdr machine add` and `herdr --machine` reach the
  server without a terminal, `hc ls` over SSH sees the core the unit
  started, and a pane or terminal of each has the workspace's environment
  and secrets.
- NixOS, across machines: `checks.multi-node` gives its front the same
  OpenLDAP, Dex and oauth2-proxy, and a client. Signed in, bob reaches an
  app of his workspace on `b` through the front's nginx, and its SSH
  server through workbench-connect's tunnel; an admin's pick in New
  workspace lands on `b`; with `b`'s `workbench-node` stopped its
  workspaces are Unreachable in `/api/workspaces`, and their apps still
  answer; with `b` closed on the host page, a new workspace goes to `a`;
  and Move from the UI takes one from `b` to `a`, with its data.

## Later

- **Credentials issued on demand**, through the credential provider's
  own API, instead of a command that reads them.
- **A page per workspace** with tabs: Console, then secrets, SSH keys, port
  previews and the NixOS config. The name in the panel links to it.
- **A terminal in the UI**, like the console's, for an app without an
  address, besides the person's own.
- **workbench-connect for macOS and Windows.**

## Open

- Whether cloud-hypervisor's serial socket takes more than one client. The
  shared console doesn't need it, but `workbench console` without
  `workbench-node` does.
- T3 Code's `serve` and `openLink` are the workbench module's, around
  `t3 serve` and `t3 auth pairing create --json`: t3code-nix's NixOS
  module is for a shared host, one server per person.
- An app can't use paths under `/oauth2/`: oauth2-proxy has them.
- A device's credential stands in for single sign-on for up to 7 days
  after the person last signed in to the UI: someone removed from the
  directory, or from a group, keeps the way in from a terminal, with the
  groups single sign-on last gave them, until then, unless their device is
  revoked. Only its person revokes it. Their workspaces made at run time
  are stopped by the reconciliation timer all the same.
- `ProxyCommand workbench-connect tunnel %h` hands the host name to a
  shell, as any `ProxyCommand` with `%h`: OpenSSH 9.6 and later, and the
  distributions' fixes for CVE-2023-51385, refuse a name with shell
  characters first, but an older ssh doesn't.
- A computer whose `herdr` or `hc` is of another generation than the
  workspace's offers to copy its own into the guest's `~/.local/bin`,
  apart from the version the host decides: herdr asks in a terminal and
  refuses without one, and `hc machine add --install` copies it without
  asking.
- The name rule is a prefix: someone whose slug is `samuel` may take
  `samuel-silva`, before `samuel.silva` does. Names are first come.
