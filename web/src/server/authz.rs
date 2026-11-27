//! nginx's `auth_request` for every app of every workspace: only those the
//! UI lets use the workspace reach its apps, at the address the answer
//! names, and an app gets the browser's cookies without the single sign-on
//! one, so code in a workspace cannot replay it against the UI or another
//! workspace.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body_util::Empty;
use hyper::body::Bytes;
use workbench::config::Config;
use workbench_model::{Viewer, is_user_name};

use super::{AppState, Identity, lock};

/// How long what oauth2-proxy said about a cookie is reused; the decision
/// is made for every request.
const CACHE_FOR: Duration = Duration::from_secs(10);

/// `GET /authz`, with the workspace in `X-Workbench-Workspace` and the app
/// in `X-Workbench-App`.
pub async fn handle(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let header = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let (name, app) = (header("x-workbench-workspace"), header("x-workbench-app"));
    // An app's cookie may hold any byte: only the SSO cookie must be text.
    let cookie = headers.get(header::COOKIE).map_or(&[][..], HeaderValue::as_bytes);
    let (sso, theirs) = split_cookie(cookie, &state.opts.cookie_name);
    let who = match whoami(&state, &String::from_utf8_lossy(&sso)).await {
        Ok(who) => who,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };
    let Some(viewer) = state.viewer(&who.user, &who.groups) else { return StatusCode::UNAUTHORIZED.into_response() };
    let mut out = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&viewer.name) {
        out.insert("x-workbench-user", v);
    }
    // Only for the page nginx shows on a refusal, which tells "not yours"
    // from "no such app" by the owner rule: an answer that lets the
    // request through carries the app's cookies, and must stay small.
    let mut refused = out.clone();
    if let Ok(v) = HeaderValue::from_str(&who.groups.join(",")) {
        refused.insert("x-workbench-groups", v);
    }
    let Some(c) = reachable_by(&state, &viewer, name) else { return (StatusCode::FORBIDDEN, refused).into_response() };
    // While it moves its VM is down, wherever the registry says it is: the
    // UI's page says it moves.
    if moving(&state, name) {
        return (StatusCode::FORBIDDEN, refused).into_response();
    }
    // An app the record no longer has opens while the VM that runs still
    // has it, as a state a second old at most says, so it goes with the
    // restart: without one, it is refused. Only an app the last list saw
    // run, or any before the first list, asks for one.
    let mut reachable = state.backend.reachable(&c);
    let ran = reachable.iter().any(|a| a.id == app) || !state.collected().await;
    if !c.apps.iter().any(|a| a.id == app) && ran {
        reachable = if state.refresh().await { state.backend.reachable(&c) } else { c.apps.clone() };
    }
    // The UI's page for a workspace without the app tells the two apart.
    // An app without an address has nothing here to reach.
    let Some(port) = reachable.into_iter().find(|a| a.id == app).and_then(|a| a.port) else {
        return (StatusCode::FORBIDDEN, refused).into_response();
    };
    if let Ok(v) = HeaderValue::from_str(&format!("{}:{port}", c.ip)) {
        out.insert("x-workbench-upstream", v);
    }
    if let Ok(v) = HeaderValue::from_bytes(&theirs) {
        out.insert("x-workbench-cookie", v);
    }
    (StatusCode::OK, out).into_response()
}

/// The config of a workspace `viewer` may reach, by the owner rule of the
/// list, from the config alone: a workspace made at run time needs its
/// owner still in a self-service group. Its apps through nginx and its SSH
/// server through the tunnel are reached by this one rule.
pub(crate) fn reachable_by(state: &AppState, viewer: &Viewer, name: &str) -> Option<Config> {
    state.backend.config(name).ok().filter(|c| viewer.may_use(&c.owner, c.self_service))
}

/// Whether a workspace moves: the registry says so.
pub(crate) fn moving(state: &AppState, name: &str) -> bool {
    state.backend.fleet().record(name).is_ok_and(|r| r.is_moving())
}

/// Who the single sign-on cookie is signed in as, and their groups, asked
/// of oauth2-proxy at most every 10 s per cookie; no one has an empty
/// name. Only that cookie: an app's, which may change at every request,
/// would miss the cache.
async fn whoami(state: &AppState, cookie: &str) -> anyhow::Result<Identity> {
    if cookie.is_empty() {
        return Ok(Identity::default());
    }
    if let Some((who, at)) = lock(&state.authz).get(cookie)
        && at.elapsed() < CACHE_FOR
    {
        return Ok(who.clone());
    }
    let who = state.backend.whoami(cookie).await?;
    let mut cache = lock(&state.authz);
    cache.retain(|_, (_, at)| at.elapsed() < CACHE_FOR);
    cache.insert(cookie.to_owned(), (who.clone(), Instant::now()));
    Ok(who)
}

/// The groups in oauth2-proxy's comma-separated header.
#[must_use]
pub fn groups(header: &str) -> Vec<String> {
    header.split(',').map(str::trim).filter(|g| !g.is_empty()).map(str::to_owned).collect()
}

/// Asks oauth2-proxy at `base` whose session a cookie holds, and their
/// groups; no one when none.
pub async fn ask_oauth2_proxy(base: &str, cookie: &str) -> anyhow::Result<Identity> {
    let url: hyper::Uri = format!("{base}/oauth2/auth").parse().context("--oauth2-proxy")?;
    let authority = url.authority().context("--oauth2-proxy needs a host")?.clone();
    let ask = async {
        let tcp = tokio::net::TcpStream::connect(authority.as_str()).await?;
        let (mut send, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tcp)).await?;
        tokio::spawn(conn);
        let req = hyper::Request::get(url.path())
            .header(hyper::header::HOST, authority.as_str())
            .header(hyper::header::COOKIE, cookie)
            .body(Empty::<Bytes>::new())?;
        anyhow::Ok(send.send_request(req).await?)
    };
    let res = tokio::time::timeout(Duration::from_secs(5), ask).await.context("oauth2-proxy did not answer")??;
    match res.status() {
        StatusCode::OK | StatusCode::ACCEPTED => {
            let header = |k: &str| res.headers().get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
            let mut user = header("x-auth-request-preferred-username");
            if user.is_empty() {
                user = header("x-auth-request-user");
            }
            if !is_user_name(&user) {
                bail!("oauth2-proxy gave the user {user:?}");
            }
            Ok(Identity { user, groups: groups(&header("x-auth-request-groups")) })
        }
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Ok(Identity::default()),
        s => bail!("oauth2-proxy answered {s}"),
    }
}

/// Splits a `Cookie` header in the single sign-on cookie, in parts or not,
/// and the others, which keep their bytes.
#[must_use]
pub fn split_cookie(header: &[u8], name: &str) -> (Vec<u8>, Vec<u8>) {
    let name = name.as_bytes();
    let is_split = |key: &[u8]| {
        key.strip_prefix(name)
            .and_then(|r| r.strip_prefix(b"_"))
            .is_some_and(|n| !n.is_empty() && n.iter().all(u8::is_ascii_digit))
    };
    let (sso, theirs): (Vec<&[u8]>, Vec<&[u8]>) =
        header.split(|&b| b == b';').map(<[u8]>::trim_ascii).filter(|part| !part.is_empty()).partition(|part| {
            let key = part.iter().position(|&b| b == b'=').map_or(*part, |i| &part[..i]);
            key == name || is_split(key)
        });
    (sso.join(&b"; "[..]), theirs.join(&b"; "[..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_the_sso_cookie() {
        let name = "_workbench_auth";
        let drop = |h: &[u8]| String::from_utf8_lossy(&split_cookie(h, name).1).into_owned();
        assert_eq!(drop(b"other=1; _workbench_auth=abc"), "other=1");
        assert_eq!(
            drop(b"_workbench_auth_0=a; t3_session=x; _workbench_auth_1=b; _workbench_authx=keep"),
            "t3_session=x; _workbench_authx=keep"
        );
        assert_eq!(drop(b"_workbench_auth_=x;;  a=b "), "_workbench_auth_=x; a=b");
        assert_eq!(drop(b""), "");
        assert_eq!(
            split_cookie(b"app=caf\xc3\xa9; _workbench_auth=a", name),
            (b"_workbench_auth=a".to_vec(), b"app=caf\xc3\xa9".to_vec())
        );
        assert_eq!(
            split_cookie(b"_workbench_auth_0=a; t3=x; _workbench_auth_1=b", name).0,
            b"_workbench_auth_0=a; _workbench_auth_1=b"
        );
    }
}
