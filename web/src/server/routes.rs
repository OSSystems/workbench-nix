//! The UI's HTTP routes, which nginx reaches on a Unix socket after single
//! sign-on: the pages, their server functions, live updates, the consoles
//! and the assets; and the access check's, on a socket of its own.

use std::convert::Infallible;
use std::path::Path;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use leptos::prelude::*;
use leptos_axum::{LeptosRoutes, generate_route_list, handle_server_fns_with_context};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use workbench_model::apps::is_app_id;
use workbench_model::{Viewer, is_workspace_name};

use super::{AppState, authz, console, csp};
use crate::app::{App, asset, shell};

/// The UI on `ui.sock`. `site` holds `assets/` and the WebAssembly in
/// `pkg/`.
pub fn ui_router(state: AppState, site: &Path) -> Router {
    let routes = generate_route_list(App);
    let context = {
        let state = state.clone();
        move || {
            provide_context(state.clone());
            leptos::nonce::provide_nonce();
        }
    };
    let cached =
        || SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=3600"));
    // The WebAssembly's URL has the build's version in it.
    let pkg = format!("/{}", state.leptos.site_pkg_dir);
    let versioned = SetResponseHeaderLayer::overriding(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    Router::new()
        .leptos_routes_with_context(&state, routes, context, {
            let options = state.leptos.clone();
            move || shell(options.clone())
        })
        .route("/api/workspaces", get(workspaces))
        .route("/api/{*name}", get(server_fn).post(server_fn))
        .route("/events", get(events))
        .route("/w/{ws}/console/socket", get(console::socket))
        .route("/denied", get(denied))
        .nest_service(
            "/assets",
            tower::ServiceBuilder::new().layer(cached()).service(ServeDir::new(site.join("assets"))),
        )
        .nest_service(&pkg, tower::ServiceBuilder::new().layer(versioned).service(ServeDir::new(site.join("pkg"))))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// The access check on `authz.sock`.
pub fn authz_router(state: AppState) -> Router {
    Router::new().route("/authz", get(authz::handle)).with_state(state)
}

/// Everything on the UI's socket comes through nginx after single sign-on,
/// which names the user in `X-Workbench-User`. Every `POST` must come from
/// the UI's own origin: workspaces are sibling subdomains, the same site,
/// so `SameSite` cookies don't stop them; this does.
async fn guard(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let (user, groups) = {
        let header = |k: &str| req.headers().get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
        (header("x-workbench-user"), authz::groups(&header("x-workbench-groups")))
    };
    let Some(viewer) = state.viewer(&user, &groups) else {
        return (StatusCode::UNAUTHORIZED, "not signed in").into_response();
    };
    if req.method() == Method::POST {
        let origin = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok());
        if origin != Some(state.opts.origin.as_str()) {
            return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
        }
    }
    // So must a GET that makes a pairing code, or a workspace's page could
    // spend the viewer's codes in the background; a person following a
    // link from one still gets the page.
    if req.method() == Method::GET && makes_code(req.uri().path()) && !from_ui_or_navigation(req.headers()) {
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }
    req.extensions_mut().insert(viewer);
    req.extensions_mut().insert(super::Groups(groups));
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.entry(header::REFERRER_POLICY).or_insert(HeaderValue::from_static("same-origin"));
    if !h.contains_key(header::CONTENT_SECURITY_POLICY)
        && let Ok(v) = csp(&state.opts, None, "'self'").parse()
    {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    if !h.contains_key(header::CACHE_CONTROL)
        && h.get(header::CONTENT_TYPE).is_some_and(|t| t.as_bytes().starts_with(b"text/html"))
    {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
}

/// Whether a GET of `path` makes a pairing code: the Connect page, and
/// the server function behind it.
fn makes_code(path: &str) -> bool {
    path == "/api/connect"
        || path
            .strip_prefix("/w/")
            .is_some_and(|rest| matches!(rest.split('/').collect::<Vec<_>>()[..], [_, "connect", _]))
}

/// Whether a request comes from the UI's own pages, or is a person
/// navigating to it. A browser without `Sec-Fetch-Site` says neither.
fn from_ui_or_navigation(h: &axum::http::HeaderMap) -> bool {
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok());
    match get("sec-fetch-site") {
        None | Some("same-origin" | "none") => true,
        Some(_) => get("sec-fetch-mode") == Some("navigate") && get("sec-fetch-dest") == Some("document"),
    }
}

async fn server_fn(State(state): State<AppState>, req: Request<Body>) -> impl IntoResponse {
    handle_server_fns_with_context(move || provide_context(state.clone()), req).await
}

/// `GET /api/workspaces`: what the viewer sees, as JSON.
async fn workspaces(State(state): State<AppState>, axum::Extension(viewer): axum::Extension<Viewer>) -> Response {
    match state.snapshot_for(&viewer).await {
        Ok(snap) => {
            let all: Vec<_> = snap.rows.into_iter().map(|r| r.ws).collect();
            ([(header::CACHE_CONTROL, "no-store")], axum::Json(all)).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

/// `GET /events`: while anyone listens, the viewer's snapshot every 2 s
/// when it changed, as `rows` events; `lost` when the host can't be read.
async fn events(State(state): State<AppState>, axum::Extension(viewer): axum::Extension<Viewer>) -> impl IntoResponse {
    let stream =
        futures_util::stream::unfold((state, viewer, String::new(), true), |(state, viewer, last, first)| async move {
            if !first {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            let (event, last) = match state.snapshot_for(&viewer).await {
                Ok(snap) => {
                    let json = serde_json::to_string(&snap).unwrap_or_default();
                    if json == last {
                        (Event::default().comment("ping"), last)
                    } else {
                        (Event::default().event("rows").data(json.clone()), json)
                    }
                }
                Err(e) => (Event::default().event("lost").data(format!("{e:#}").replace('\n', " ")), last),
            };
            Some((Ok::<_, Infallible>(event), (state, viewer, last, false)))
        });
    (
        [(header::CACHE_CONTROL, "no-store"), (header::HeaderName::from_static("x-accel-buffering"), "no")],
        Sse::new(stream).keep_alive(KeepAlive::default()),
    )
}

/// `GET /denied?ws=<name>&app=<id>`: the page nginx shows at an app's
/// address when the workspace isn't the viewer's or hasn't the app, styled
/// from the UI's origin.
async fn denied(
    State(state): State<AppState>,
    axum::Extension(viewer): axum::Extension<Viewer>,
    query: axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let ws = query.get("ws").filter(|n| is_workspace_name(n)).cloned();
    let app = query.get("app").filter(|a| is_app_id(a)).cloned().unwrap_or_default();
    let origin = state.opts.origin.clone();
    let domain = state.opts.domain().to_owned();
    let theirs =
        ws.as_ref().and_then(|n| state.backend.config(n).ok()).filter(|c| viewer.may_use(&c.owner, c.self_service));
    if let (Some(c), false) = (theirs, app.is_empty()) {
        let terminal = state.backend.reachable(&c).iter().any(|a| a.id == app && a.port.is_none());
        return no_app(&state, &c.name, &app, terminal);
    }
    let what = ws.map_or_else(|| "This workspace".to_owned(), |n| format!("{n}.{domain}"));
    let page = view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                <meta name="color-scheme" content="light dark" />
                <title>"Not yours · workbench"</title>
                <link rel="icon" href=format!("{origin}{}", asset(&state.leptos, "mark.svg")) type="image/svg+xml" />
                <link rel="stylesheet" href=format!("{origin}{}", asset(&state.leptos, "app.css")) />
            </head>
            <body>
                <div class="app">
                    <main class="solo">
                        <div class="card">
                            <h1>"This workspace isn't yours"</h1>
                            <p>
                                <span class="mono">{what}</span>
                                " belongs to someone else. You're signed in as "
                                <b>{viewer.name.clone()}</b>
                                "."
                            </p>
                            <div class="acts">
                                <a class="btn p" href=format!("{origin}/")>
                                    "Your workspaces"
                                </a>
                                <a class="btn q" href="/oauth2/sign_out">
                                    "Sign out"
                                </a>
                            </div>
                        </div>
                    </main>
                </div>
            </body>
        </html>
    }
    .to_html();
    let policy = format!("default-src 'none'; style-src {origin}; img-src {origin}; frame-ancestors 'none'");
    (StatusCode::FORBIDDEN, [(header::CONTENT_SECURITY_POLICY, policy)], Html(page)).into_response()
}

/// The page at the address of an app a workspace of the viewer's hasn't,
/// or has without an address: one reached from a terminal.
fn no_app(state: &AppState, name: &str, app: &str, terminal: bool) -> Response {
    let origin = state.opts.origin.clone();
    let (title, says) = if terminal {
        (
            "No address · workbench",
            "It has no address to open in a browser: you reach it from your terminal. Its panel says how.",
        )
    } else {
        ("No such app · workbench", "Its apps, and how to add one, are on its panel.")
    };
    let head = if terminal { " has no address for " } else { " has no app " };
    let page = view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                <meta name="color-scheme" content="light dark" />
                <title>{title}</title>
                <link rel="icon" href=format!("{origin}{}", asset(&state.leptos, "mark.svg")) type="image/svg+xml" />
                <link rel="stylesheet" href=format!("{origin}{}", asset(&state.leptos, "app.css")) />
            </head>
            <body>
                <div class="app">
                    <main class="solo">
                        <div class="card">
                            <h1><span class="mono">{name.to_owned()}</span>{head} <span class="mono">{app.to_owned()}</span></h1>
                            <p>{says}</p>
                            <div class="acts">
                                <a class="btn p" href=format!("{origin}/?w={name}")>
                                    {format!("Open {name}'s panel")}
                                </a>
                            </div>
                        </div>
                    </main>
                </div>
            </body>
        </html>
    }
    .to_html();
    let policy = format!("default-src 'none'; style-src {origin}; img-src {origin}; frame-ancestors 'none'");
    (StatusCode::NOT_FOUND, [(header::CONTENT_SECURITY_POLICY, policy)], Html(page)).into_response()
}
