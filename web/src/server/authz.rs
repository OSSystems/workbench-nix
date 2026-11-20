//! nginx's `auth_request` for every workspace: only those the UI lets use
//! it reach its app, at the address the answer names, and the app gets the
//! browser's cookies without the single sign-on one, so code in a workspace
//! cannot replay it against the UI or another workspace.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body_util::Empty;
use hyper::body::Bytes;
use workbench_model::is_user_name;

use super::{AppState, Identity, lock};

/// How long what oauth2-proxy said about a cookie is reused; the decision
/// is made for every request.
const CACHE_FOR: Duration = Duration::from_secs(10);

/// `GET /authz`, with the workspace in `X-Workbench-Workspace`.
pub async fn handle(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let name = headers.get("x-workbench-workspace").and_then(|v| v.to_str().ok()).unwrap_or_default();
    // An app's cookie may hold any byte: only the SSO cookie must be text.
    let cookie = headers.get(header::COOKIE).map_or(&[][..], HeaderValue::as_bytes);
    let sso = String::from_utf8_lossy(cookie);
    let who = match whoami(&state, &sso).await {
        Ok(who) => who,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };
    let Some(viewer) = state.viewer(&who.user, &who.groups) else { return StatusCode::UNAUTHORIZED.into_response() };
    let mut out = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&viewer.name) {
        out.insert("x-workbench-user", v);
    }
    // The owner rule of the list, from the config alone: a workspace made
    // at run time needs its owner still in a self-service group.
    let config = state.backend.config(name).ok().filter(|c| viewer.may_use(&c.owner, c.self_service));
    let Some(c) = config else { return (StatusCode::FORBIDDEN, out).into_response() };
    if let Ok(v) = HeaderValue::from_str(&format!("{}:{}", c.ip, c.app_port)) {
        out.insert("x-workbench-upstream", v);
    }
    if let Ok(v) = HeaderValue::from_bytes(&without_cookie(cookie, &state.opts.cookie_name)) {
        out.insert("x-workbench-cookie", v);
    }
    (StatusCode::OK, out).into_response()
}

/// Who a cookie header is signed in as, and their groups, asked of
/// oauth2-proxy at most every 10 s per cookie; no one has an empty name.
async fn whoami(state: &AppState, cookie: &str) -> anyhow::Result<Identity> {
    if !cookie.contains(&state.opts.cookie_name) {
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

/// Drops the single sign-on cookie, split in parts or not, from a `Cookie`
/// header; the other cookies keep their bytes.
#[must_use]
pub fn without_cookie(header: &[u8], name: &str) -> Vec<u8> {
    let name = name.as_bytes();
    let is_split = |key: &[u8]| {
        key.strip_prefix(name)
            .and_then(|r| r.strip_prefix(b"_"))
            .is_some_and(|n| !n.is_empty() && n.iter().all(u8::is_ascii_digit))
    };
    let kept: Vec<&[u8]> = header
        .split(|&b| b == b';')
        .map(<[u8]>::trim_ascii)
        .filter(|part| {
            let key = part.iter().position(|&b| b == b'=').map_or(*part, |i| &part[..i]);
            !part.is_empty() && key != name && !is_split(key)
        })
        .collect();
    kept.join(&b"; "[..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_the_sso_cookie() {
        let name = "_workbench_auth";
        let drop = |h: &[u8]| String::from_utf8_lossy(&without_cookie(h, name)).into_owned();
        assert_eq!(drop(b"other=1; _workbench_auth=abc"), "other=1");
        assert_eq!(
            drop(b"_workbench_auth_0=a; t3_session=x; _workbench_auth_1=b; _workbench_authx=keep"),
            "t3_session=x; _workbench_authx=keep"
        );
        assert_eq!(drop(b"_workbench_auth_=x;;  a=b "), "_workbench_auth_=x; a=b");
        assert_eq!(drop(b""), "");
        assert_eq!(without_cookie(b"app=caf\xc3\xa9; _workbench_auth=a", name), b"app=caf\xc3\xa9");
    }
}
