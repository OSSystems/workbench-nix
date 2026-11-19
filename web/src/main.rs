//! `workbench-web`: the web UI's server, run by `workbench-web.service` as
//! its own user. It listens on Unix sockets only: nginx's group reaches the
//! UI and the access check, root reaches the shared consoles.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use leptos::config::LeptosOptions;
use workbench::config::DEFAULT_CONFIG_DIRS;
use workbench::self_service::Template;
use workbench_web::server::{AppState, HostBackend, Options, SelfService, asset_version, authz_router, ui_router};

/// Serves workbench's web UI.
// The comments are the help text, written for people, not rustdoc.
#[allow(clippy::doc_markdown)]
#[derive(Parser)]
#[command(name = "workbench-web", version)]
struct Args {
    /// Directory of <workspace>.json configs; repeat to search several
    #[arg(long = "config-dir", value_name = "DIR")]
    config_dirs: Vec<PathBuf>,
    /// Where workspace disks and stop markers live
    #[arg(long, default_value = "/var/lib/workbench")]
    state_root: PathBuf,
    /// The UI's own origin, https://<domain>
    #[arg(long)]
    origin: String,
    /// A user who sees and controls every workspace; repeat for each
    #[arg(long = "admin", value_name = "USER")]
    admins: Vec<String>,
    /// A group whose members are admins; repeat for each
    #[arg(long = "admin-group", value_name = "GROUP")]
    admin_groups: Vec<String>,
    /// Lets people make workspaces, from the host's template for them
    #[arg(long = "self-service", value_name = "TEMPLATE")]
    self_service: Option<PathBuf>,
    /// A group whose members make workspaces of their own; repeat for each
    #[arg(long = "creator-group", value_name = "GROUP")]
    creator_groups: Vec<String>,
    /// oauth2-proxy, which the access check asks who a cookie belongs to
    #[arg(long, default_value = "http://127.0.0.1:4180")]
    oauth2_proxy: String,
    /// The single sign-on cookie, which apps never get
    #[arg(long, default_value = "_workbench_auth")]
    cookie_name: String,
    /// Host memory reserve, percent of MemTotal
    #[arg(long = "mem-reserve-percent", default_value_t = 10.0)]
    reserve_percent: f64,
    /// Minimum host memory reserve, GiB
    #[arg(long = "mem-reserve-min-gib", default_value_t = 2.0)]
    reserve_min_gib: f64,
    /// <name>=<status file> of a memory pool on the host; repeat for each
    #[arg(long = "pool", value_name = "NAME=FILE")]
    pools: Vec<String>,
    /// The site: assets/ and the WebAssembly in pkg/
    #[arg(long)]
    site: PathBuf,
    /// Where the UI listens, for nginx
    #[arg(long, default_value = "/run/workbench-web/ui.sock")]
    ui_socket: PathBuf,
    /// Where the access check listens, for nginx
    #[arg(long, default_value = "/run/workbench-web/authz.sock")]
    authz_socket: PathBuf,
    /// Where the shared consoles listen, for `workbench console`
    #[arg(long, default_value = "/run/workbench-web/console.sock")]
    console_socket: PathBuf,
}

fn bind(path: &Path, mode: u32) -> anyhow::Result<tokio::net::UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path).with_context(|| path.display().to_string())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(listener)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let pools = args
        .pools
        .iter()
        .map(|p| {
            p.split_once('=').map(|(n, f)| (n.to_owned(), PathBuf::from(f))).with_context(|| format!("--pool {p}"))
        })
        .collect::<anyhow::Result<_>>()?;
    let opts = Options {
        dirs: if args.config_dirs.is_empty() {
            DEFAULT_CONFIG_DIRS.map(PathBuf::from).into()
        } else {
            args.config_dirs
        },
        state_root: args.state_root,
        origin: args.origin,
        admins: args.admins,
        admin_groups: args.admin_groups,
        creator_groups: args.creator_groups,
        self_service: match &args.self_service {
            Some(path) => {
                let t = Template::load(path)?;
                Some(SelfService { paths: t.paths(), limits: t.limits })
            }
            None => None,
        },
        oauth2_proxy: args.oauth2_proxy,
        cookie_name: args.cookie_name,
        reserve_percent: args.reserve_percent,
        reserve_min_gib: args.reserve_min_gib,
        pools,
    };
    let leptos = LeptosOptions::builder()
        .output_name("workbench-web")
        .site_root(args.site.to_string_lossy().into_owned())
        .site_pkg_dir(format!("pkg/{}", asset_version(&args.site)))
        .build();
    let state = AppState::new(opts.clone(), Box::new(HostBackend { opts }), leptos);
    let ui = bind(&args.ui_socket, 0o660)?;
    let authz = bind(&args.authz_socket, 0o660)?;
    let ui_app = ui_router(state.clone(), &args.site);
    let authz_app = authz_router(state.clone());
    let local = state.clone();
    let console_socket = args.console_socket.clone();
    tokio::select! {
        r = axum::serve(ui, ui_app) => r.context("the UI")?,
        r = axum::serve(authz, authz_app) => r.context("the access check")?,
        r = local.serve_consoles(&console_socket) => r.context("the shared consoles")?,
    }
    Ok(())
}
