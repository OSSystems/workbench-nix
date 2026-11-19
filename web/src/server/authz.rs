//! nginx's `auth_request` for every workspace: only its owner and the
//! admins reach its app, and the app gets the browser's cookies without the
//! single sign-on one, so code in a workspace cannot replay it against the
//! UI or another workspace.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body_util::Empty;
use hyper::body::Bytes;
use workbench_model::is_user_name;

use super::{AppState, lock};

/// How long an answer about a cookie is reused.
const CACHE_FOR: Duration = Duration::from_secs(10);

/// `GET /authz`, with the workspace in `X-Workbench-Workspace`.
pub async fn handle(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let name = headers.get("x-workbench-workspace").and_then(|v| v.to_str().ok()).unwrap_or_default();
    // An app's cookie may hold any byte: only the SSO cookie must be text.
    let cookie = headers.get(header::COOKIE).map_or(&[][..], HeaderValue::as_bytes);
    let (sso, theirs) = split_cookie(cookie, &state.opts.cookie_name);
    let user = match whoami(&state, &String::from_utf8_lossy(&sso)).await {
        Ok(user) => user,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };
    let Some(viewer) = state.viewer(&user) else { return StatusCode::UNAUTHORIZED.into_response() };
    let mut out = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&viewer.name) {
        out.insert("x-workbench-user", v);
    }
    let allowed = state.backend.config(name).is_ok_and(|c| viewer.admin || c.owner == viewer.name);
    if !allowed {
        return (StatusCode::FORBIDDEN, out).into_response();
    }
    if let Ok(v) = HeaderValue::from_bytes(&theirs) {
        out.insert("x-workbench-cookie", v);
    }
    (StatusCode::OK, out).into_response()
}

/// Who the single sign-on cookie is signed in as, asked of oauth2-proxy at
/// most every 10 s per cookie; empty for nobody. Only that cookie: an
/// app's, which may change at every request, would miss the cache.
async fn whoami(state: &AppState, cookie: &str) -> anyhow::Result<String> {
    if cookie.is_empty() {
        return Ok(String::new());
    }
    if let Some((user, at)) = lock(&state.authz).get(cookie)
        && at.elapsed() < CACHE_FOR
    {
        return Ok(user.clone());
    }
    let user = state.backend.whoami(cookie).await?;
    let mut cache = lock(&state.authz);
    cache.retain(|_, (_, at)| at.elapsed() < CACHE_FOR);
    cache.insert(cookie.to_owned(), (user.clone(), Instant::now()));
    Ok(user)
}

/// Asks oauth2-proxy at `base` whose session a cookie holds; empty when
/// none.
pub async fn ask_oauth2_proxy(base: &str, cookie: &str) -> anyhow::Result<String> {
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
            Ok(user)
        }
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Ok(String::new()),
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
