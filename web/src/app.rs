//! The pages, as Leptos components: the server renders them, and the same
//! components hydrate in the browser. Everything they show comes from a
//! [`Snapshot`] or a [`HostView`] the server made.

use std::collections::HashSet;

use leptos::prelude::*;
use leptos_meta::{MetaTags, Title, provide_meta_context};
use leptos_router::components::{Route, Router, Routes};
use leptos_router::hooks::{use_params_map, use_query_map};
use leptos_router::{SsrMode, path};
use workbench_model::self_service::{MIN_DISK_GIB, NewView};
use workbench_model::{
    Filter, HostView, List, Quotas, Row, SignInBlocker, Snapshot, State, Verb, Viewer, Workspace, gib, pct,
};

use crate::api::{self, Control};

/// An asset's URL with the build's version in it, which `site_pkg_dir`
/// carries as `pkg/<version>`: a browser never keeps one from an older
/// build.
#[must_use]
pub fn asset(options: &LeptosOptions, path: &str) -> String {
    match options.site_pkg_dir.strip_prefix("pkg/") {
        Some(v) => format!("/assets/{path}?v={v}"),
        None => format!("/assets/{path}"),
    }
}

/// The document every page is rendered in.
pub fn shell(options: LeptosOptions) -> impl IntoView {
    #[cfg(feature = "ssr")]
    crate::server::page_csp(false);
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                <meta name="color-scheme" content="light dark" />
                <link rel="icon" href=asset(&options, "mark.svg") type="image/svg+xml" />
                <link rel="stylesheet" href=asset(&options, "app.css") />
                <MetaTags />
                <HydrationScripts options />
            </head>
            <body>
                <App />
            </body>
        </html>
    }
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    provide_context(SignedIn(RwSignal::new(HashSet::new())));
    view! {
        <Router>
            <Routes fallback=|| view! { <Forbidden viewer=None /> }>
                <Route path=path!("/") view=ListPage ssr=SsrMode::Async />
                <Route path=path!("/host") view=HostPage ssr=SsrMode::Async />
                <Route path=path!("/new") view=NewPage ssr=SsrMode::Async />
                <Route path=path!("/w/:ws/console") view=ConsolePage ssr=SsrMode::Async />
                <Route path=path!("/w/:ws/:action") view=ActionPage ssr=SsrMode::Async />
            </Routes>
        </Router>
    }
}

/// The workspaces this browser has signed in to the app of, from
/// `localStorage`: their primary action is just Open.
#[derive(Clone, Copy)]
struct SignedIn(RwSignal<HashSet<String>>);

const SIGNED_KEY: &str = "workbench.signedIn";

fn local_storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

impl SignedIn {
    fn load(self) {
        let names = local_storage()
            .and_then(|s| s.get_item(SIGNED_KEY).ok().flatten())
            .and_then(|raw| serde_json::from_str::<HashSet<String>>(&raw).ok())
            .unwrap_or_default();
        self.0.set(names);
    }

    fn remember(self, name: &str) {
        self.0.update(|s| {
            s.insert(name.to_owned());
        });
        if let (Some(store), Ok(raw)) = (local_storage(), serde_json::to_string(&self.0.get_untracked())) {
            let _ = store.set_item(SIGNED_KEY, &raw);
        }
    }

    fn has(self, name: &str) -> bool {
        self.0.with(|s| s.contains(name))
    }
}

/// Sets the status of the page the server sends.
#[cfg_attr(not(feature = "ssr"), allow(unused_variables))]
fn set_status(code: u16) {
    #[cfg(feature = "ssr")]
    if let Some(res) = use_context::<leptos_axum::ResponseOptions>() {
        res.set_status(axum::http::StatusCode::from_u16(code).unwrap_or(axum::http::StatusCode::OK));
    }
}

#[component]
fn Icon(d: &'static str) -> impl IntoView {
    view! {
        <svg class="i" viewBox="0 0 24 24" aria-hidden="true">
            <path d=d />
        </svg>
    }
}

const ICON_BACK: &str = "m14 6-6 6 6 6";
const ICON_OUT: &str = "M7 17 17 7M9 7h8v8";
const ICON_SIGN_IN: &str = "M14 4h4a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-4M10 16l4-4-4-4M14 12H4";
const ICON_CONSOLE: &str = "m5 8 4 4-4 4M12 16h7";
const ICON_RESTART: &str = "M20 11a8 8 0 1 0-2.3 5.7M20 5v6h-6";
const ICON_STOP: &str = "M12 3v8M6.6 6.6a8 8 0 1 0 10.8 0";

fn initials(name: &str) -> String {
    name.chars().take(2).collect::<String>().to_uppercase()
}

fn os_name(os: &str) -> &'static str {
    match os {
        "ubuntu" => "Ubuntu",
        "nixos" => "NixOS",
        _ => "—",
    }
}

/// The top bar: the mark, the find field on the list, and the user.
#[component]
fn Top(viewer: Viewer, #[prop(optional)] find: Option<RwSignal<String>>) -> impl IntoView {
    let find_field = find.map(|q| {
        view! {
            <label class="cmd">
                <svg class="i" viewBox="0 0 24 24" aria-hidden="true">
                    <circle cx="11" cy="11" r="6.5" />
                    <path d="m20 20-4.2-4.2" />
                </svg>
                <input
                    id="find"
                    type="search"
                    placeholder="Find a workspace…"
                    autocomplete="off"
                    aria-label="Find a workspace"
                    prop:value=move || q.get()
                    on:input=move |ev| q.set(event_target_value(&ev))
                />
                <kbd>"Ctrl K"</kbd>
            </label>
        }
    });
    let admin = viewer.admin;
    view! {
        <header class="top">
            <a class="brand" href="/">
                <svg viewBox="0 0 24 24" aria-hidden="true">
                    <rect x="2.5" y="6" width="19" height="4" rx="1.5" />
                    <path d="M6 10v9M18 10v9M6 15h12" />
                </svg>
                "workbench"
            </a>
            {find_field}
            <details class="me">
                <summary>
                    {admin.then(|| view! { <span class="role">"admin"</span> })}
                    <span class="who">{viewer.name.clone()}</span>
                    <span class="av" aria-hidden="true">{initials(&viewer.name)}</span>
                </summary>
                <div class="menu">
                    <a href="/oauth2/sign_out" rel="external">"Sign out"</a>
                </div>
            </details>
        </header>
    }
}

/// The sidebar: the sections, and the viewer's own workspaces.
#[component]
fn Side(snap: Signal<Snapshot>, nav: &'static str) -> impl IntoView {
    let total = move || snap.with(|s| s.rows.len());
    let mine = move || {
        snap.with(|s| {
            s.rows.iter().filter(|r| s.viewer.owns(&r.ws)).map(|r| (r.ws.name.clone(), r.ws.state)).collect::<Vec<_>>()
        })
    };
    let admin = move || snap.with(|s| s.viewer.admin);
    view! {
        <nav class="side" aria-label="Sections">
            <a class="ni" class:a=nav == "workspaces" href="/">
                <span>"Workspaces"</span>
                <span class="x">{total}</span>
            </a>
            <Show when=admin>
                <a class="ni" class:a=nav == "host" href="/host">
                    <span>"Host"</span>
                </a>
            </Show>
            <Show when=move || !mine().is_empty()>
                <div class="h">"Yours"</div>
                <For each=mine key=|(name, state)| (name.clone(), *state) let:item>
                    <a class="ni" href=format!("/?w={}", item.0)>
                        <span class=format!("dot {}", item.1.dot())></span>
                        <span class="mono">{item.0.clone()}</span>
                    </a>
                </For>
            </Show>
        </nav>
    }
}

/// The page shown when the host cannot be read.
#[component]
fn Unreachable(detail: String) -> impl IntoView {
    set_status(502);
    view! {
        <Title text="Couldn't read the workspaces · workbench" />
        <div class="app">
            <main class="solo">
                <div class="card">
                    <h1>"Couldn't read the workspaces"</h1>
                    <div class="err">
                        <b>"The host didn't answer."</b>
                        <details open>
                            <summary>"Details"</summary>
                            <pre>{detail}</pre>
                        </details>
                    </div>
                    <div class="acts">
                        <a class="btn p" href="" rel="external">
                            "Try again"
                        </a>
                    </div>
                </div>
            </main>
        </div>
    }
}

/// A workspace that is not the viewer's, and one that does not exist,
/// alike, so names don't leak.
#[component]
fn Forbidden(viewer: Option<Viewer>) -> impl IntoView {
    set_status(403);
    let who = viewer.as_ref().map(|v| v.name.clone()).unwrap_or_default();
    view! {
        <Title text="Not yours · workbench" />
        <div class="app">
            {viewer.map(|viewer| view! { <Top viewer /> })}
            <main class="solo">
                <div class="card">
                    <h1>"This workspace isn't yours"</h1>
                    <p>
                        "It belongs to someone else, or it doesn't exist. You're signed in as "
                        <b>{who}</b>
                        "."
                    </p>
                    <div class="acts">
                        <a class="btn p" href="/">
                            "Your workspaces"
                        </a>
                    </div>
                </div>
            </main>
        </div>
    }
}

/// Loads the viewer's snapshot, then shows `page` with it.
fn with_snapshot<V: IntoView + 'static>(page: impl Fn(Snapshot) -> V + Send + Sync + Clone + 'static) -> impl IntoView {
    let snap = Resource::new_blocking(|| (), |()| api::get_snapshot());
    view! {
        <Suspense>
            {move || {
                let page = page.clone();
                Suspend::new(async move {
                    match snap.await {
                        Ok(s) => page(s).into_any(),
                        Err(e) => view! { <Unreachable detail=e.to_string() /> }.into_any(),
                    }
                })
            }}
        </Suspense>
    }
}

#[component]
fn ListPage() -> impl IntoView {
    with_snapshot(|s| view! { <Live initial=s /> })
}

/// Whether the screen is wide enough for the side panel next to the list.
fn wide() -> bool {
    window().match_media("(min-width: 761px)").ok().flatten().is_some_and(|m| m.matches())
}

/// The workspaces page, kept current by the server's event stream.
#[component]
fn Live(initial: Snapshot) -> impl IntoView {
    let snap = RwSignal::new(initial);
    let lost = RwSignal::new(None::<String>);
    let find = RwSignal::new(String::new());
    let query = use_query_map();
    let filter = Memo::new(move |_| {
        let admin = snap.with(|s| s.viewer.admin);
        Filter::parse(&query.with(|q| q.get("f").unwrap_or_default()), admin)
    });
    let list = Memo::new(move |_| {
        let selected = query.with(|q| q.get("w").unwrap_or_default());
        find.with(|f| snap.with(|s| List::build(s, filter.get(), &selected, f)))
    });
    let signed = expect_context::<SignedIn>();
    let viewer = snap.with_untracked(|s| s.viewer.clone());

    // In the browser: remembered sign-ins, live updates and the keys.
    Effect::new(move |_| {
        signed.load();
        follow_events(snap, lost, filter.get_untracked());
    });
    let navigate = leptos_router::hooks::use_navigate();
    let select = Callback::new(move |name: String| {
        let f = filter.get_untracked();
        let q = if f == Filter::All { format!("/?w={name}") } else { format!("/?f={}&w={name}", f.as_str()) };
        navigate(&q, leptos_router::NavigateOptions { replace: true, scroll: false, ..Default::default() });
    });
    Effect::new(move |_| keys(list, snap, find, select));

    let snap_sig: Signal<Snapshot> = snap.into();
    view! {
        <Title text="Workspaces · workbench" />
        <div class="app">
            <Top viewer find />
            <div
                class="lay"
                // No workspace, no side panel to keep room for.
                class:np=move || snap.with(|s| s.rows.is_empty())
                class:picked=move || list.with(|l| l.picked)
                id="live"
                data-filter=move || filter.get().as_str()
            >
                <Side snap=snap_sig nav="workspaces" />
                <section class="main" aria-label="Workspaces">
                    <Bar snap list />
                    <Show when=move || snap.with(|s| s.rows.is_empty())>
                        <div class="empty">
                            <div>
                                <h2>"You don't have a workspace yet"</h2>
                                <p>
                                    "A workspace is a VM of your own on this host, with the app in it and a disk that stays."
                                    {move || {
                                        if snap.with(|s| s.viewer.creates) {
                                            " You can make one yourself."
                                        } else {
                                            " An administrator creates it; ask one to add you."
                                        }
                                    }}
                                </p>
                                <Show when=move || snap.with(|s| s.viewer.creates)>
                                    <a class="btn p go" href="/new" rel="external">"New workspace"</a>
                                </Show>
                            </div>
                        </div>
                    </Show>
                    <Show when=move || {
                        snap.with(|s| !s.rows.is_empty()) && list.with(|l| l.groups.is_empty())
                    }>
                        <div class="empty">
                            <div>
                                <p>"No workspace matches."</p>
                            </div>
                        </div>
                    </Show>
                    <div class:adm=move || snap.with(|s| s.viewer.admin)>
                        <For
                            each=move || list.with(|l| l.groups.iter().map(|g| (g.label, g.state)).collect::<Vec<_>>())
                            key=|g| g.0
                            let:group
                        >
                            <GroupHead snap list label=group.0 state=group.1 />
                            <For
                                each=move || {
                                    list.with(|l| {
                                        l.groups
                                            .iter()
                                            .find(|g| g.label == group.0)
                                            .map(|g| {
                                                snap.with(|s| g.rows.iter().map(|&i| s.rows[i].ws.name.clone()).collect::<Vec<_>>())
                                            })
                                            .unwrap_or_default()
                                    })
                                }
                                key=String::clone
                                let:name
                            >
                                <RowView
                                    snap
                                    list
                                    name=name.clone()
                                    select
                                />
                            </For>
                        </For>
                    </div>
                    <Show when=move || list.with(|l| !l.groups.is_empty())>
                        <p class="hint">
                            <span><kbd>"J"</kbd><kbd>"K"</kbd>" move"</span>
                            <span><kbd>"↵"</kbd>" open"</span>
                            <span><kbd>"S"</kbd>" sign in"</span>
                            <span><kbd>"C"</kbd>" console"</span>
                            <span><kbd>"R"</kbd>" restart"</span>
                        </p>
                    </Show>
                </section>
                <For
                    each=move || snap.with(|s| s.rows.iter().map(|r| r.ws.name.clone()).collect::<Vec<_>>())
                    key=String::clone
                    let:name
                >
                    <Peek snap list name />
                </For>
            </div>
            <div class="banner" id="lost" hidden=move || lost.with(Option::is_none)>
                <b>"Reconnecting…"</b>
                " "
                <span>"Showing the state at " <time>{move || lost.get().unwrap_or_default()}</time> "."</span>
            </div>
        </div>
    }
}

#[component]
fn Bar(snap: RwSignal<Snapshot>, list: Memo<List>) -> impl IntoView {
    let chip = move |f: Filter, label: &'static str, href: &'static str| {
        view! {
            <a class="chip" class:a=move || list.with(|l| l.filter == f) href=href>
                {label}
            </a>
        }
    };
    view! {
        <div class="bar">
            <h1>"Workspaces"</h1>
            <span class="x">{move || snap.with(|s| s.rows.len())}</span>
            <nav class="chips" aria-label="Filter">
                {chip(Filter::All, "All", "/")}
                <Show when=move || snap.with(|s| s.viewer.admin)>{chip(Filter::Mine, "Mine", "/?f=mine")}</Show>
                {chip(Filter::Running, "Running", "/?f=running")}
                {chip(Filter::Stopped, "Stopped", "/?f=stopped")}
            </nav>
            <Show when=move || snap.with(|s| s.viewer.creates)>
                <a class="btn new" href="/new" rel="external">"New workspace"</a>
            </Show>
        </div>
    }
}

#[component]
fn GroupHead(snap: RwSignal<Snapshot>, list: Memo<List>, label: &'static str, state: State) -> impl IntoView {
    let _ = snap;
    let count = move || list.with(|l| l.groups.iter().find(|g| g.label == label).map_or(0, |g| g.rows.len()));
    view! {
        <div class="gh">
            <span class=format!("dot {}", state.dot())></span>
            {label}
            <span class="x">{count}</span>
        </div>
    }
}

/// One workspace's row, as it currently is.
fn row_of(snap: RwSignal<Snapshot>, name: String) -> Memo<Option<Row>> {
    Memo::new(move |_| snap.with(|s| s.find(&name).cloned()))
}

#[component]
fn RowView(snap: RwSignal<Snapshot>, list: Memo<List>, name: String, select: Callback<String>) -> impl IntoView {
    let row = row_of(snap, name.clone());
    let selected = {
        let name = name.clone();
        move || list.with(|l| l.selected.as_deref() == Some(&name))
    };
    let admin = snap.with_untracked(|s| s.viewer.admin);
    let on_click = {
        let name = name.clone();
        move |ev: leptos::ev::MouseEvent| {
            let in_action = ev
                .target()
                .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
                .and_then(|el| el.closest(".act").ok().flatten())
                .is_some();
            if wide() && !in_action {
                ev.prevent_default();
                select.run(name.clone());
            }
        }
    };
    view! {
        <div class="r" class:sel=selected data-ws=name.clone() on:click=on_click>
            {move || {
                row.get()
                    .map(|r| {
                        let ws = r.ws;
                        let state = ws.state;
                        let memory = (ws.memory_bytes > 0 && ws.max_memory_bytes > 0)
                            .then(|| {
                                view! {
                                    <i
                                        class=format!("m {}", state.dot())
                                        style=format!("width: {}%", pct(ws.memory_bytes, ws.max_memory_bytes))
                                    ></i>
                                }
                            });
                        view! {
                            <span class=format!("dot {}", state.dot()) title=state.label()></span>
                            <a class="nm" href=format!("/?w={}", ws.name)>
                                <span class="n mono">{ws.name.clone()}</span>
                                <span class="u">{r.note}</span>
                            </a>
                            {admin
                                .then(|| {
                                    view! {
                                        <span class="c">
                                            {if ws.owner.is_empty() { "—".to_owned() } else { ws.owner.clone() }}
                                        </span>
                                    }
                                })}
                            <span class="c">{os_name(&ws.os)}</span>
                            <span class="mem">
                                {if ws.memory_bytes > 0 {
                                    view! { <b>{gib(ws.memory_bytes)}</b> }.into_any()
                                } else {
                                    "—".into_any()
                                }}
                                {(ws.max_memory_bytes > 0).then(|| format!(" / {} GiB", gib(ws.max_memory_bytes)))}
                            </span>
                            <span class="act">
                                <Primary ws=ws.clone() />
                            </span>
                            {memory}
                        }
                            .into_any()
                    })
            }}
        </div>
    }
}

/// A form that starts, stops or restarts a workspace.
#[component]
fn ControlForm(
    ws: String,
    verb: Verb,
    #[prop(optional)] stay: bool,
    #[prop(optional, into)] class: String,
    children: Children,
) -> impl IntoView {
    let action = ServerAction::<Control>::new();
    view! {
        <ActionForm action>
            <input type="hidden" name="ws" value=ws />
            <input type="hidden" name="action" value=verb.as_str() />
            {stay.then(|| view! { <input type="hidden" name="stay" value="true" /> })}
            <button class=format!("btn {class}")>{children()}</button>
        </ActionForm>
    }
}

/// A form that signs a browser in to the app, in a new tab.
#[component]
fn SignInForm(ws: String, #[prop(optional, into)] class: String, children: Children) -> impl IntoView {
    let signed = expect_context::<SignedIn>();
    let (name, data) = (ws.clone(), ws.clone());
    view! {
        <form
            method="post"
            action="/api/sign-in"
            target="_blank"
            class=class
            data-ws=data
            on:submit=move |_| signed.remember(&name)
        >
            <input type="hidden" name="ws" value=ws />
            {children()}
        </form>
    }
}

/// A row's one primary action.
#[component]
fn Primary(ws: Workspace) -> impl IntoView {
    let signed = expect_context::<SignedIn>();
    if !ws.configured {
        return view! { <span class="wait">"workbench destroy"</span> }.into_any();
    }
    match ws.state {
        State::Running if ws.sign_in => {
            let name = ws.name.clone();
            let opened = Signal::derive(move || signed.has(&name));
            view! {
                <SignInForm ws=ws.name.clone() class="signin">
                    <button class="btn" data-key="enter" hidden=opened>
                        "Open and sign in"
                    </button>
                </SignInForm>
                <a
                    class="btn opened"
                    href=ws.url.clone()
                    target="_blank"
                    rel="noopener"
                    data-key="enter"
                    hidden=move || !opened.get()
                >
                    "Open"
                </a>
            }
            .into_any()
        }
        State::Running | State::Starting => view! {
            <a class="btn" href=ws.url.clone() target="_blank" rel="noopener" data-key="enter">
                "Open"
            </a>
        }
        .into_any(),
        State::Stopped => view! { <ControlForm ws=ws.name.clone() verb=Verb::Start>"Start"</ControlForm> }.into_any(),
        State::Failing => view! {
            <a class="btn" href=format!("/w/{}/console", ws.name) rel="external" data-key="enter">
                "Console"
            </a>
        }
        .into_any(),
        State::Stopping | State::Restarting => view! { <span class="wait">"up to 2 min"</span> }.into_any(),
    }
}

/// Opens the `<dialog>` with this id, if the browser has dialogs; the
/// link's page is the fallback.
fn open_dialog(ev: &leptos::ev::MouseEvent, id: &str) {
    let dialog = document()
        .get_element_by_id(id)
        .and_then(|el| wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlDialogElement>(el).ok());
    if let Some(d) = dialog
        && d.show_modal().is_ok()
    {
        ev.prevent_default();
    }
}

fn close_dialog(ev: &leptos::ev::Event) {
    let dialog = ev
        .target()
        .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
        .and_then(|el| el.closest("dialog").ok().flatten())
        .and_then(|el| wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlDialogElement>(el).ok());
    if let Some(d) = dialog {
        d.close();
    }
}

#[component]
fn RestartText(name: String) -> impl IntoView {
    view! {
        <h2 id=format!("restart-{name}-h")>"Restart " <span class="mono">{name.clone()}</span> "?"</h2>
        <dl>
            <dt>"Stops"</dt>
            <dd>"Everything running in the VM. Open app tabs lose their connection for about a minute."</dd>
            <dt>"Keeps"</dt>
            <dd>"Everything on its disk."</dd>
            <dt>"Brings"</dt>
            <dd>"The app and system now on the host."</dd>
        </dl>
    }
}

#[component]
fn StopText(name: String) -> impl IntoView {
    view! {
        <h2 id=format!("stop-{name}-h")>"Stop " <span class="mono">{name.clone()}</span> "?"</h2>
        <dl>
            <dt>"Stops"</dt>
            <dd>"The VM and the app."</dd>
            <dt>"Keeps"</dt>
            <dd>"Everything on its disk."</dd>
            <dt>"Lasts"</dt>
            <dd>"It stays stopped, also when the host reboots, until someone starts it."</dd>
        </dl>
    }
}

/// The confirmation dialog of a restart or a stop.
#[component]
fn ConfirmDialog(
    name: String,
    verb: Verb,
    #[prop(optional)] stay: bool,
    #[prop(optional)] note: Option<&'static str>,
) -> impl IntoView {
    let id = format!("{}-{name}", verb.as_str());
    let text = match verb {
        Verb::Stop => view! { <StopText name=name.clone() /> }.into_any(),
        _ => view! { <RestartText name=name.clone() /> }.into_any(),
    };
    let class = if verb == Verb::Stop { "danger" } else { "p" };
    view! {
        <dialog id=id.clone() class="dlg" aria-labelledby=format!("{id}-h") on:submit=move |ev| close_dialog(&ev)>
            {text}
            {note.map(|n| view! { <p class="small">{n}</p> })}
            <div class="acts">
                <button class="btn q" type="button" on:click=move |ev| close_dialog(&ev)>
                    "Cancel"
                </button>
                <ControlForm ws=name.clone() verb stay class=class>
                    {format!("{} {name}", verb.label())}
                </ControlForm>
            </div>
        </dialog>
    }
}

/// A workspace's side panel, shown when it is selected.
#[component]
fn Peek(snap: RwSignal<Snapshot>, list: Memo<List>, name: String) -> impl IntoView {
    let row = row_of(snap, name.clone());
    let selected = {
        let name = name.clone();
        move || list.with(|l| l.selected.as_deref() == Some(&name))
    };
    let configured = Memo::new(move |_| row.with(|r| r.as_ref().is_some_and(|r| r.ws.configured)));
    let state = Memo::new(move |_| row.with(|r| r.as_ref().map_or(State::Stopped, |r| r.ws.state)));
    let ws = move || row.with(|r| r.as_ref().map(|r| r.ws.clone()).unwrap_or_default());
    let head = move || {
        row.get().map(|r| {
            let ws = r.ws;
            let st = ws.state;
            view! {
                <div>
                    <h2 class="mono">{ws.name.clone()}</h2>
                    <div class="st">
                        <span class=format!("dot {}", st.dot())></span>
                        {st.label()}
                        {(!r.since.is_empty()).then(|| format!(" since {}", r.since))}
                    </div>
                </div>
                {(ws.app_update && st != State::Restarting)
                    .then(|| {
                        view! {
                            <div class="note">
                                <b>"Update"</b>
                                " A new app version is on the host. Restart to use it."
                            </div>
                        }
                    })}
                {(st == State::Failing)
                    .then(|| {
                        view! {
                            <div class="note bad">
                                <b>"Won't stay up."</b>
                                {(ws.restarts > 0).then(|| format!(" The VM restarted {} times.", ws.restarts))}
                                " Its console shows why."
                            </div>
                        }
                    })}
                {(!ws.configured)
                    .then(|| {
                        view! {
                            <div class="note bad">
                                <b>"Removed."</b>
                                " This workspace is no longer in the configuration; its disk is kept until "
                                <code>{format!("workbench destroy {}", ws.name)}</code>
                                "."
                            </div>
                        }
                    })}
                <dl class="kv">
                    {(!ws.owner.is_empty()).then(|| view! { <dt>"Owner"</dt><dd>{ws.owner.clone()}</dd> })}
                    {(!ws.url.is_empty()).then(|| view! { <dt>"Address"</dt><dd class="mono clip" title=ws.host().to_owned()>{ws.host().to_owned()}</dd> })}
                    {(!ws.ip.is_empty()).then(|| view! { <dt>"IP"</dt><dd class="mono">{ws.ip.clone()}</dd> })}
                    {(!ws.os.is_empty()).then(|| view! { <dt>"System"</dt><dd>{os_name(&ws.os)}</dd> })}
                    {(ws.max_memory_bytes > 0)
                        .then(|| {
                            view! {
                                <dt>"Memory"</dt>
                                <dd>
                                    {if ws.memory_bytes > 0 { gib(ws.memory_bytes) } else { "—".into() }}
                                    {format!(" of {} GiB", gib(ws.max_memory_bytes))}
                                    <span class="meter">
                                        <i style=format!("width: {}%", pct(ws.memory_bytes, ws.max_memory_bytes))></i>
                                        <b
                                            style=format!("left: {}%", pct(ws.boot_memory_bytes, ws.max_memory_bytes))
                                            title="boot size"
                                        ></b>
                                    </span>
                                </dd>
                            }
                        })}
                    <dt>"Disk on host"</dt>
                    <dd>
                        {format!(
                            "{} of {} GiB",
                            gib(i64::try_from(ws.disk_used_bytes).unwrap_or(i64::MAX)),
                            gib(i64::try_from(ws.disk_bytes).unwrap_or(i64::MAX)),
                        )}
                    </dd>
                </dl>
            }
        })
    };
    let actions = move || {
        let ws = ws();
        let st = state.get();
        if !configured.get() {
            return None;
        }
        let name = ws.name.clone();
        let open = matches!(st, State::Running | State::Starting).then(|| {
            let label = format!("Open {name}");
            view! {
                <a class="btn p" href=ws.url.clone() target="_blank" rel="noopener">
                    <span>{label} <Icon d=ICON_OUT /></span>
                    <kbd>"↵"</kbd>
                </a>
            }
        });
        let start = (st == State::Stopped).then(|| {
            let label = format!("Start {name}");
            view! {
                <ControlForm ws=name.clone() verb=Verb::Start class="p">
                    <span>{label}</span>
                    <kbd>"↵"</kbd>
                </ControlForm>
            }
        });
        let sign_in = (ws.sign_in && st == State::Running).then(|| {
            view! {
                <SignInForm ws=name.clone() class="signin-any">
                    <button class="btn" data-key="s">
                        <span><Icon d=ICON_SIGN_IN /> "Sign in to app"</span>
                        <kbd>"S"</kbd>
                    </button>
                </SignInForm>
            }
        });
        let console = (st != State::Stopped).then(|| {
            view! {
                <a class="btn" href=format!("/w/{name}/console") rel="external" data-key="c">
                    <span><Icon d=ICON_CONSOLE /> "Console"</span>
                    <kbd>"C"</kbd>
                </a>
            }
        });
        let restart = (st != State::Stopped && !st.changing()).then(|| {
            let id = format!("restart-{name}");
            let click = id.clone();
            view! {
                <a
                    class="btn"
                    href=format!("/w/{name}/restart")
                    data-dialog=id
                    data-key="r"
                    on:click=move |ev| open_dialog(&ev, &click)
                >
                    <span><Icon d=ICON_RESTART /> "Restart…"</span>
                    <kbd>"R"</kbd>
                </a>
            }
        });
        let stop = (st != State::Stopped && !st.changing()).then(|| {
            let id = format!("stop-{name}");
            let click = id.clone();
            view! {
                <a
                    class="btn"
                    href=format!("/w/{name}/stop")
                    data-dialog=id
                    aria-label=format!("Stop {name}…")
                    on:click=move |ev| open_dialog(&ev, &click)
                >
                    <span><Icon d=ICON_STOP /> "Stop…"</span>
                </a>
            }
        });
        // Only one made at run time may go from here.
        let destroy = (ws.self_service && !st.changing()).then(|| {
            view! {
                <a class="btn q bad" href=format!("/w/{name}/destroy") rel="external" aria-label=format!("Destroy {name}…")>
                    "Destroy…"
                </a>
            }
        });
        let foot = (stop.is_some() || destroy.is_some()).then(|| view! { <div class="foot">{stop} {destroy}</div> });
        Some(view! {
            <div class="stack">{open} {start} {sign_in} {console} {restart}</div>
            {foot}
        })
    };
    // The dialogs stay put while the panel changes, so live updates never
    // close one someone is reading.
    let dialogs = configured.get_untracked().then(|| {
        view! {
            <ConfirmDialog name=name.clone() verb=Verb::Restart />
            <ConfirmDialog name=name.clone() verb=Verb::Stop />
        }
    });
    view! {
        <aside class="peek" class:sel=selected data-ws=name.clone() aria-label=name.clone()>
            <a class="back" href="/">
                <Icon d=ICON_BACK />
                "Workspaces"
            </a>
            {head}
            {actions}
            {dialogs}
        </aside>
    }
}

/// Follows `/events`: every message is the viewer's snapshot.
fn follow_events(snap: RwSignal<Snapshot>, lost: RwSignal<Option<String>>, filter: Filter) {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;
    let url = if filter == Filter::All { "/events".to_owned() } else { format!("/events?f={}", filter.as_str()) };
    let Ok(events) = web_sys::EventSource::new(&url) else { return };
    // The stream ends with the page that follows it.
    let open = send_wrapper::SendWrapper::new(events.clone());
    on_cleanup(move || open.close());
    let last_good = RwSignal::new(String::new());
    let now = || js_sys::Date::new_0().to_locale_time_string("en-GB").as_string().unwrap_or_default();
    last_good.set(now());
    let on_rows = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
        if let Some(s) = e.data().as_string().and_then(|raw| serde_json::from_str::<Snapshot>(&raw).ok()) {
            snap.set(s);
            last_good.set(now());
            lost.set(None);
        }
    });
    let _ = events.add_event_listener_with_callback("rows", on_rows.as_ref().unchecked_ref());
    let on_lost = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| lost.set(Some(last_good.get_untracked())));
    let _ = events.add_event_listener_with_callback("lost", on_lost.as_ref().unchecked_ref());
    events.set_onerror(Some(on_lost.as_ref().unchecked_ref()));
    on_rows.forget();
    on_lost.forget();
}

/// The list's keys: `J`/`K` move, `↵` opens, `S` signs in, `C` opens the
/// console, `R` restarts, `/` or `Ctrl K` finds. They press the buttons a
/// click would, so they never act on their own.
fn keys(list: Memo<List>, snap: RwSignal<Snapshot>, find: RwSignal<String>, select: Callback<String>) {
    use wasm_bindgen::JsCast;
    let press = |scope: Option<web_sys::Element>, key: &str| {
        let Some(scope) = scope else { return };
        let Ok(all) = scope.query_selector_all(&format!("[data-key=\"{key}\"]")) else { return };
        for i in 0..all.length() {
            let Some(el) = all.item(i).and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok()) else { continue };
            if el.closest("[hidden]").ok().flatten().is_none() {
                el.click();
                return;
            }
        }
    };
    let handle = window_event_listener(leptos::ev::keydown, move |e: web_sys::KeyboardEvent| {
        let doc = document();
        let target = e.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok());
        let typing =
            target.as_ref().and_then(|t| t.closest("input, textarea, select, .xterm").ok().flatten()).is_some();
        let find_el = doc.get_element_by_id("find").and_then(|el| el.dyn_into::<web_sys::HtmlInputElement>().ok());
        let key = e.key();
        if let Some(f) = &find_el {
            if (key == "k" && (e.ctrl_key() || e.meta_key())) || (key == "/" && !typing) {
                e.prevent_default();
                let _ = f.focus();
                f.select();
                return;
            }
            if target.as_ref().is_some_and(|t| t.id() == "find") {
                match key.as_str() {
                    "Escape" => {
                        find.set(String::new());
                        let _ = f.blur();
                    }
                    "Enter" => {
                        if let Some(first) = visible_rows(list, snap, find).first() {
                            select.run(first.clone());
                        }
                        let _ = f.blur();
                    }
                    _ => {}
                }
                return;
            }
        }
        if typing
            || e.ctrl_key()
            || e.meta_key()
            || e.alt_key()
            || doc.query_selector("dialog[open]").ok().flatten().is_some()
        {
            return;
        }
        let names = visible_rows(list, snap, find);
        let current = list.with_untracked(|l| l.selected.clone());
        let i = current.as_ref().and_then(|c| names.iter().position(|n| n == c));
        let peek = doc.query_selector(".peek.sel").ok().flatten();
        match key.as_str() {
            "j" | "ArrowDown" if !names.is_empty() => {
                e.prevent_default();
                select.run(names[i.map_or(0, |i| (i + 1).min(names.len() - 1))].clone());
            }
            "k" | "ArrowUp" if !names.is_empty() => {
                e.prevent_default();
                select.run(names[i.map_or(0, |i| i.saturating_sub(1))].clone());
            }
            "Enter" if target.as_ref().is_some_and(|t| t.tag_name() == "BODY") => {
                press(doc.query_selector(".r.sel .act").ok().flatten(), "enter");
            }
            "s" => press(peek, "s"),
            "c" => press(peek, "c"),
            "r" => press(peek, "r"),
            _ => {}
        }
    });
    on_cleanup(move || handle.remove());
}

/// The names of the rows shown, in order.
fn visible_rows(list: Memo<List>, snap: RwSignal<Snapshot>, find: RwSignal<String>) -> Vec<String> {
    let q = find.get_untracked().trim().to_lowercase();
    list.with_untracked(|l| {
        snap.with_untracked(|s| {
            l.groups
                .iter()
                .flat_map(|g| g.rows.iter().map(|&i| s.rows[i].ws.name.clone()))
                .filter(|n| q.is_empty() || n.contains(&q))
                .collect()
        })
    })
}

#[component]
fn HostPage() -> impl IntoView {
    let data = Resource::new_blocking(|| (), |()| api::get_host());
    view! {
        <Title text="Host · workbench" />
        <Suspense>
            {move || Suspend::new(async move {
                match data.await {
                    Ok((snap, host)) => view! { <Host snap host /> }.into_any(),
                    Err(e) if e.to_string().contains("for admins") => view! { <Forbidden viewer=None /> }.into_any(),
                    Err(e) => view! { <Unreachable detail=e.to_string() /> }.into_any(),
                }
            })}
        </Suspense>
    }
}

#[component]
fn Host(snap: Snapshot, host: HostView) -> impl IntoView {
    let viewer = snap.viewer.clone();
    let snap = Signal::stored(snap);
    let total = host.total;
    let bar = |i: usize| if i == 0 { "sw1" } else { "sw2" };
    let committed = host
        .pools
        .iter()
        .enumerate()
        .map(|(i, p)| {
            view! { <i class=bar(i) style=format!("width: {}%", pct(p.committed, total)) title=p.name.clone()></i> }
        })
        .collect_view();
    let pending = host
        .pools
        .iter()
        .filter(|p| p.pending > 0)
        .map(|p| {
            view! {
                <i class="sw3" style=format!("width: {}%", pct(p.pending, total)) title=format!("{} waiting", p.name)></i>
            }
        })
        .collect_view();
    let waiting: i64 = host.pools.iter().map(|p| p.pending).sum();
    let legend = host
        .pools
        .iter()
        .enumerate()
        .map(|(i, p)| view! { <span><i class=bar(i)></i>{format!("{} {} GiB", p.name, gib(p.committed))}</span> })
        .collect_view();
    let pool_rows = host
        .pools
        .iter()
        .map(|p| {
            view! {
                <div role="row">
                    <span class="mono">{p.name.clone()}</span>
                    <span class="num">{format!("{} GiB", gib(p.committed))}</span>
                    <span class="num">{if p.pending > 0 { format!("{} GiB", gib(p.pending)) } else { "—".into() }}</span>
                    <span>{if p.denied.is_empty() { "—".to_owned() } else { p.denied.clone() }}</span>
                    {if p.stale {
                        view! { <span class="stale">{p.report.clone()}</span> }.into_any()
                    } else {
                        view! { <span class="small">{p.report.clone()}</span> }.into_any()
                    }}
                </div>
            }
        })
        .collect_view();
    let ws_rows = if host.workspaces.is_empty() {
        view! {
            <div>
                <span></span>
                <span class="small">"No workspace is running."</span>
            </div>
        }
        .into_any()
    } else {
        host.workspaces
            .iter()
            .map(|w| {
                view! {
                    <div role="row">
                        <span class=format!("dot {}", w.state.dot())></span>
                        <a class="mono" href=format!("/?w={}", w.name)>{w.name.clone()}</a>
                        <span>{w.owner.clone()}</span>
                        <span class="num">{format!("{} GiB", gib(w.memory_bytes))}</span>
                        <span class="num">{format!("{} GiB", gib(w.boot_memory_bytes))}</span>
                        <span class="num">{format!("{} GiB", gib(w.max_memory_bytes))}</span>
                        <span class="num">
                            {format!("{} GiB", gib(i64::try_from(w.disk_used_bytes).unwrap_or(i64::MAX)))}
                        </span>
                    </div>
                }
            })
            .collect_view()
            .into_any()
    };
    view! {
        <div class="app">
            <Top viewer />
            <div class="lay np">
                <Side snap nav="host" />
                <section class="main">
                    <div class="bar">
                        <h1>"Host"</h1>
                        <span class="chips small">{format!("updated {}", host.at)}</span>
                    </div>
                    <div class="hostw">
                        <section class="panel">
                            <header>
                                <h2>"Memory"</h2>
                                <span>{format!("{} GiB total", gib(host.total))}</span>
                                <span class="rt">{format!("{} GiB free", gib(host.free))}</span>
                            </header>
                            <div class="hbar">
                                {committed}
                                {pending}
                                <i class="sw4" style=format!("width: {}%", pct(host.reserve, total)) title="reserve"></i>
                            </div>
                            <div class="legend">
                                {legend}
                                <span><i class="sw3"></i>{format!("waiting {} GiB", gib(waiting))}</span>
                                <span><i class="sw4"></i>{format!("reserve {} GiB", gib(host.reserve))}</span>
                                <span><i class="sw5"></i>{format!("free {} GiB", gib(host.free))}</span>
                            </div>
                            <div class="tbl" role="table">
                                <div class="hd" role="row">
                                    <span>"Pool"</span>
                                    <span class="num">"Holds"</span>
                                    <span class="num">"Waiting"</span>
                                    <span>"Last denied"</span>
                                    <span>"Report"</span>
                                </div>
                                {pool_rows}
                            </div>
                        </section>
                        <section class="panel">
                            <header>
                                <h2>"Workspaces"</h2>
                                <span>"memory now, at boot and at most"</span>
                            </header>
                            <div class="tbl ws" role="table">
                                <div class="hd" role="row">
                                    <span></span>
                                    <span>"Workspace"</span>
                                    <span>"Owner"</span>
                                    <span class="num">"Now"</span>
                                    <span class="num">"Boot"</span>
                                    <span class="num">"Most"</span>
                                    <span class="num">"Disk"</span>
                                </div>
                                {ws_rows}
                            </div>
                        </section>
                        {host.quotas.map(|q| view! { <QuotasPanel q /> })}
                    </div>
                </section>
            </div>
        </div>
    }
}

/// The confirmation pages of start, stop and restart, and the sign-in
/// page, at `/w/<ws>/<action>`.
#[component]
fn ActionPage() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    with_snapshot(move |snap| {
        let name = params.with_untracked(|p| p.get("ws").unwrap_or_default());
        let action = params.with_untracked(|p| p.get("action").unwrap_or_default());
        let (error, detail) =
            query.with_untracked(|q| (q.get("error").unwrap_or_default(), q.get("detail").unwrap_or_default()));
        let viewer = snap.viewer.clone();
        let Some(ws) = snap.find(&name).map(|r| r.ws.clone()).filter(|ws| ws.configured) else {
            return view! { <Forbidden viewer=Some(viewer) /> }.into_any();
        };
        let body = if action == "sign-in" {
            view! { <SignInPage ws error detail /> }.into_any()
        } else if action == "destroy" && ws.self_service {
            view! { <DestroyPage ws error detail /> }.into_any()
        } else if let Ok(verb) = action.parse::<Verb>() {
            view! { <ConfirmPage ws verb error detail /> }.into_any()
        } else {
            return view! { <Forbidden viewer=Some(viewer) /> }.into_any();
        };
        let title = if action == "sign-in" {
            format!("Sign in to {name} · workbench")
        } else if action == "destroy" {
            format!("Destroy {name} · workbench")
        } else {
            format!("{} {name} · workbench", action.parse::<Verb>().map_or("", Verb::label))
        };
        view! {
            <Title text=title />
            <div class="app">
                <Top viewer />
                <main class="solo">
                    <div class="card">{body}</div>
                </main>
            </div>
        }
        .into_any()
    })
}

#[component]
fn ConfirmPage(ws: Workspace, verb: Verb, error: String, detail: String) -> impl IntoView {
    let name = ws.name.clone();
    let back = format!("/?w={name}");
    match error.as_str() {
        "busy" => {
            set_status(409);
            view! {
                <h1><span class="mono">{name.clone()}</span>{format!(" is already {}", ws.state)}</h1>
                <p>"Wait until it's done, then try again."</p>
                <div class="acts"><a class="btn p" href=back>"Back to workspaces"</a></div>
            }
            .into_any()
        }
        "failed" => {
            set_status(502);
            view! {
                <h1>{format!("Couldn't {verb} ")}<span class="mono">{name.clone()}</span></h1>
                <div class="err">
                    <b>"systemd refused."</b>
                    <details open><summary>"Details"</summary><pre>{detail}</pre></details>
                </div>
                <div class="acts"><a class="btn p" href=back>"Back to workspaces"</a></div>
            }
            .into_any()
        }
        _ => {
            let text = match verb {
                Verb::Restart => view! { <RestartText name=name.clone() /> }.into_any(),
                Verb::Stop => view! { <StopText name=name.clone() /> }.into_any(),
                Verb::Start => view! { <h1>"Start " <span class="mono">{name.clone()}</span> "?"</h1> }.into_any(),
            };
            let class = if verb == Verb::Stop { "danger" } else { "p" };
            view! {
                {text}
                <div class="acts">
                    <ControlForm ws=name.clone() verb class=class>{format!("{} {name}", verb.label())}</ControlForm>
                    <a class="btn q" href=back>"Cancel"</a>
                </div>
            }
            .into_any()
        }
    }
}

#[component]
fn SignInPage(ws: Workspace, error: String, detail: String) -> impl IntoView {
    let name = ws.name.clone();
    let blocker = ws.sign_in_blocker();
    let again = format!("/w/{name}/sign-in");
    let console = format!("/w/{name}/console");
    let kind = if error.is_empty() {
        match blocker {
            Some(SignInBlocker::Stopped) => "stopped",
            Some(SignInBlocker::Starting) => "starting",
            Some(SignInBlocker::NoHook) => "no-hook",
            None => "",
        }
        .to_owned()
    } else {
        error
    };
    let mono = |s: String| view! { <span class="mono">{s}</span> };
    let body = match kind.as_str() {
        "" => view! {
            <p>"The app in " {mono(name.clone())} " has a sign-in of its own. This makes a one-time link for this browser and opens the app with it."</p>
            <SignInForm ws=name.clone() class="acts">
                <button class="btn p">"Open and sign in" <Icon d=ICON_OUT /></button>
                <a class="btn q" href="/">"Back to workspaces"</a>
            </SignInForm>
        }
        .into_any(),
        "stopped" => view! {
            <div class="err"><b>{mono(name.clone())} " is stopped."</b>"Start it, then sign in."</div>
            <div class="acts">
                <ControlForm ws=name.clone() verb=Verb::Start class="p">{format!("Start {name}")}</ControlForm>
                <a class="btn q" href="/">"Back to workspaces"</a>
            </div>
        }
        .into_any(),
        "starting" => view! {
            <div class="err">
                <b>{mono(name.clone())} " is still starting."</b>
                "Its agent isn't answering yet."
                {(ws.os == "nixos").then_some(" A NixOS workspace takes longer on its first boot after an update.")}
            </div>
            <div class="acts"><a class="btn p" href=again rel="external">"Try again"</a><a class="btn q" href="/">"Back to workspaces"</a></div>
        }
        .into_any(),
        "no-hook" => view! {
            <p>"The app in " {mono(name.clone())} " has no sign-in of its own: opening it is all it takes."</p>
            <div class="acts"><a class="btn p" href=ws.url.clone()>{format!("Open {name}")}</a></div>
        }
        .into_any(),
        "timeout" => view! {
            <div class="err"><b>"The app took too long to answer."</b>"No link came back within 90 seconds."</div>
            <div class="acts"><a class="btn p" href=again rel="external">"Try again"</a><a class="btn" href=console rel="external">"Console"</a></div>
        }
        .into_any(),
        "not-a-link" => view! {
            <div class="err">
                <b>"The app answered something that isn't a link to " {mono(name.clone())} "."</b>
                "Its sign-in hook must print one URL on " {mono(ws.host().to_owned())} "."
            </div>
            <div class="acts"><a class="btn p" href=again rel="external">"Try again"</a></div>
        }
        .into_any(),
        _ => view! {
            <div class="err">
                <b>"The app couldn't make a sign-in link."</b>
                {(!detail.is_empty()).then(|| view! { <details open><summary>"Details"</summary><pre>{detail.clone()}</pre></details> })}
            </div>
            <div class="acts"><a class="btn p" href=again rel="external">"Try again"</a><a class="btn" href=console rel="external">"Console"</a></div>
        }
        .into_any(),
    };
    if !kind.is_empty() && kind != "no-hook" {
        set_status(if matches!(kind.as_str(), "stopped" | "starting") { 409 } else { 502 });
    }
    view! {
        <h1>"Sign in to " <span class="mono">{name.clone()}</span></h1>
        {body}
    }
}

/// The shared serial console.
#[component]
fn ConsolePage() -> impl IntoView {
    let params = use_params_map();
    with_snapshot(move |snap| {
        let name = params.with_untracked(|p| p.get("ws").unwrap_or_default());
        let viewer = snap.viewer.clone();
        let Some(ws) = snap.find(&name).map(|r| r.ws.clone()).filter(|ws| ws.configured) else {
            return view! { <Forbidden viewer=Some(viewer) /> }.into_any();
        };
        #[cfg(feature = "ssr")]
        crate::server::page_csp(true);
        let viewers = RwSignal::new(String::new());
        let me = viewer.name.clone();
        {
            let name = name.clone();
            Effect::new(move |_| crate::term::attach(&name, &me, viewers));
        }
        let restart_id = format!("restart-{name}");
        view! {
            <Title text=format!("{name} console · workbench") />
            <link rel="stylesheet" href="/assets/xterm/xterm.css" />
            <script src="/assets/xterm/xterm.js"></script>
            <div class="app">
                <Top viewer=viewer.clone() />
                <div class="cbar">
                    <a class="back" href=format!("/?w={name}")>
                        <Icon d=ICON_BACK />
                        "Workspaces"
                    </a>
                    <span class="x">"/"</span>
                    <b class="mono">{name.clone()}</b>
                    <span class="st">
                        <span class=format!("dot {}", ws.state.dot())></span>
                        {ws.state.label()}
                    </span>
                    <div class="r2">
                        <span class="small" id="viewers">{move || viewers.get()}</span>
                        {(!ws.state.changing())
                            .then(|| {
                                let id = restart_id.clone();
                                view! {
                                    <a
                                        class="btn"
                                        href=format!("/w/{name}/restart")
                                        data-dialog=id.clone()
                                        on:click=move |ev| open_dialog(&ev, &id)
                                    >
                                        <Icon d=ICON_RESTART />
                                        "Restart…"
                                    </a>
                                }
                            })}
                    </div>
                </div>
                <div class="term" id="term" data-ws=name.clone()>
                    <noscript>"The console needs JavaScript."</noscript>
                </div>
                <ConfirmDialog
                    name=name.clone()
                    verb=Verb::Restart
                    stay=true
                    note="The console stays open, so the boot menu shows here: any key stops its countdown."
                />
            </div>
        }
        .into_any()
    })
}

/// The New workspace page, for whoever may make one.
#[component]
fn NewPage() -> impl IntoView {
    let data = Resource::new_blocking(|| (), |()| api::get_new());
    let query = use_query_map();
    view! {
        <Title text="New workspace · workbench" />
        <Suspense>
            {move || Suspend::new(async move {
                match data.await {
                    Ok((snap, new)) => {
                        let get = |k: &str| query.with_untracked(|q| q.get(k).unwrap_or_default());
                        let filled = Filled {
                            error: get("error"),
                            name: get("name"),
                            os: get("os"),
                            memory: get("memory"),
                            disk: get("disk"),
                            keys: get("keys"),
                        };
                        view! { <NewForm viewer=snap.viewer new filled /> }.into_any()
                    }
                    Err(e) if e.to_string().contains("may not") => view! { <Forbidden viewer=None /> }.into_any(),
                    Err(e) => view! { <Unreachable detail=e.to_string() /> }.into_any(),
                }
            })}
        </Suspense>
    }
}

/// What a refused form had, to fill it again.
#[derive(Clone, Default)]
struct Filled {
    error: String,
    name: String,
    os: String,
    memory: String,
    disk: String,
    keys: String,
}

#[component]
fn NewForm(viewer: Viewer, new: NewView, filled: Filled) -> impl IntoView {
    let l = &new.limits;
    let left = new.quota.saturating_sub(new.owned);
    let or = |v: &str, d: String| if v.is_empty() { d } else { v.to_owned() };
    let name = or(&filled.name, if new.owned == 0 { new.slug.clone() } else { format!("{}-", new.slug) });
    let memory = or(&filled.memory, l.max_memory_gib.min(8).max(l.min_memory_gib()).to_string());
    let disk = or(&filled.disk, 64.min(l.max_disk_gib).to_string());
    let chosen = if filled.os.is_empty() { l.os.first().cloned().unwrap_or_default() } else { filled.os.clone() };
    let systems =
        l.os.iter()
            .map(|os| view! { <option value=os.clone() selected=*os == chosen>{os_name(os)}</option> })
            .collect_view();
    if !filled.error.is_empty() {
        set_status(422);
    }
    let quota = if left == 0 {
        view! {
            <div class="err">
                <b>"You have all the workspaces your quota allows."</b>
                {format!("Your quota is {}. Ask an admin for more, or destroy one.", new.quota)}
            </div>
        }
        .into_any()
    } else {
        view! { <p class="small">{format!("You may make {left} more of your quota of {}.", new.quota)}</p> }.into_any()
    };
    view! {
        <div class="app">
            <Top viewer />
            <main class="solo">
                <form class="card form" method="post" action="/api/create">
                    <h1>"New workspace"</h1>
                    <p>"A VM of your own, with the app in it and a disk that stays. It boots once it's made."</p>
                    {quota}
                    {(!filled.error.is_empty()).then(|| view! { <div class="err"><b>{filled.error.clone()}</b></div> })}
                    <label>
                        <span>"Name"</span>
                        <input name="name" class="mono" value=name required autocomplete="off" spellcheck="false" />
                        <small>{format!("{0} or {0}-something: lowercase letters, digits and dashes.", new.slug)}</small>
                    </label>
                    <label>
                        <span>"System"</span>
                        <select name="os">{systems}</select>
                    </label>
                    <label>
                        <span>"SSH keys"</span>
                        <textarea name="ssh_keys" class="mono" rows="3" spellcheck="false" placeholder="ssh-ed25519 AAAA… you@laptop">
                            {filled.keys.clone()}
                        </textarea>
                        <small>"One per line, to reach it over SSH from the host. Optional."</small>
                    </label>
                    <div class="pair">
                        <label>
                            <span>"Memory, at most"</span>
                            <input name="memory_gib" type="number" min=l.min_memory_gib() max=l.max_memory_gib value=memory />
                            <small>{format!("GiB, {} to {}; it boots with {}", l.min_memory_gib(), l.max_memory_gib, gib(l.boot_mem_mib << 20))}</small>
                        </label>
                        <label>
                            <span>"Disk"</span>
                            <input name="disk_gib" type="number" min=MIN_DISK_GIB max=l.max_disk_gib value=disk />
                            <small>{format!("GiB, {MIN_DISK_GIB} to {}", l.max_disk_gib)}</small>
                        </label>
                    </div>
                    <div class="acts">
                        <button class="btn p" disabled=left == 0>"Create"</button>
                        <a class="btn q" href="/">"Cancel"</a>
                    </div>
                </form>
            </main>
        </div>
    }
}

/// The Destroy page of a workspace made at run time: its name is typed
/// again.
#[component]
fn DestroyPage(ws: Workspace, error: String, detail: String) -> impl IntoView {
    let name = ws.name.clone();
    let err = match error.as_str() {
        "confirm" => {
            set_status(422);
            Some(
                view! { <div class="err"><b>{format!("That isn't {name}.")}</b>"Type its name exactly."</div> }
                    .into_any(),
            )
        }
        "failed" => {
            set_status(502);
            Some(
                view! {
                    <div class="err">
                        <b>"The host couldn't destroy it."</b>
                        <details open><summary>"Details"</summary><pre>{detail}</pre></details>
                    </div>
                }
                .into_any(),
            )
        }
        _ => None,
    };
    view! {
        <h1>"Destroy " <span class="mono">{name.clone()}</span> "?"</h1>
        <dl>
            <dt>"Deletes"</dt>
            <dd>"The VM, its disk and everything on it, for good."</dd>
            <dt>"Keeps"</dt>
            <dd>"Nothing. There is no undo."</dd>
        </dl>
        {err}
        <form class="form" method="post" action="/api/destroy">
            <input type="hidden" name="ws" value=name.clone() />
            <label>
                <span>"Type " <b class="mono">{name.clone()}</b> " to confirm"</span>
                <input name="confirm" class="mono" required autocomplete="off" spellcheck="false" />
            </label>
            <div class="acts">
                <button class="btn danger">{format!("Destroy {name}")}</button>
                <a class="btn q" href=format!("/?w={name}")>"Cancel"</a>
            </div>
        </form>
    }
}

/// The host page's quotas: each person's, which an admin changes in place.
#[component]
fn QuotasPanel(q: Quotas) -> impl IntoView {
    let rows = q
        .people
        .iter()
        .map(|p| {
            view! {
                <form role="row" method="post" action="/api/quota">
                    <input type="hidden" name="user" value=p.user.clone() />
                    <span>{p.user.clone()}</span>
                    <span class="num">{p.owned}</span>
                    <span class="set">
                        <input name="quota" type="number" min="0" max="100" value=p.quota aria-label=format!("Quota of {}", p.user) />
                        <button class="btn">"Save"</button>
                        {(!p.own).then(|| view! { <span class="small">"default"</span> })}
                    </span>
                </form>
            }
        })
        .collect_view();
    view! {
        <section class="panel">
            <header>
                <h2>"Quotas"</h2>
                <span>{format!("workspaces people make themselves; {} each unless set here", q.default)}</span>
            </header>
            <div class="tbl quotas" role="table">
                <div class="hd" role="row">
                    <span>"Person"</span>
                    <span class="num">"Has"</span>
                    <span>"Quota"</span>
                </div>
                {rows}
                <form role="row" method="post" action="/api/quota">
                    <span><input name="user" placeholder="uid" aria-label="uid" autocomplete="off" spellcheck="false" /></span>
                    <span></span>
                    <span class="set">
                        <input name="quota" type="number" min="0" max="100" value=q.default aria-label="Quota" />
                        <button class="btn">"Set"</button>
                    </span>
                </form>
            </div>
        </section>
    }
}
