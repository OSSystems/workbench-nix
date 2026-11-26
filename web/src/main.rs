//! `workbench-web`: the web UI's server, run by `workbench-web.service` as
//! its own user. It listens on Unix sockets only: nginx's group reaches the
//! UI and the access check, root reaches the shared consoles. With the
//! front's settings, `WORKBENCH_FRONT`, it runs on the fleet: the registry
//! and the nodes.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use leptos::config::LeptosOptions;
use workbench::config::DEFAULT_CONFIG_DIRS;
use workbench::fleet::FrontSettings;
use workbench::self_service::{Paths, Template};
use workbench_web::server::{
    AppState, Backend, Fleet, FleetBackend, HostBackend, Options, SelfService, asset_version, authz_router,
    connect_router, ui_router,
};

/// Serves workbench's web UI.
// The comments are the help text, written for people, not rustdoc.
#[allow(clippy::doc_markdown)]
#[derive(Parser)]
#[command(name = "workbench-web", version)]
struct Args {
    /// The front's settings: with them, every workspace is its registry's
    /// and its nodes', and the configs, the state root and the template
    /// aren't read
    #[arg(long, env = "WORKBENCH_FRONT", value_name = "FILE")]
    front: Option<PathBuf>,
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
    /// Where pairing, certificates and the SSH tunnel listen, for nginx
    /// without single sign-on
    #[arg(long, default_value = "/run/workbench-web/connect.sock")]
    connect_socket: PathBuf,
    /// Where the paired devices are kept
    #[arg(long, default_value = "/var/lib/workbench-web/devices.json")]
    devices: PathBuf,
    /// ssh-keygen, which signs people's keys
    #[arg(long, default_value = "ssh-keygen")]
    ssh_keygen: String,
    /// The user CA's private key
    #[arg(long, default_value = "/var/lib/workbench/ssh/user-ca")]
    user_ca: PathBuf,
    /// The host CA's public key
    #[arg(long, default_value = "/var/lib/workbench/ssh/host-ca.pub")]
    host_ca: PathBuf,
}

fn bind(path: &Path, mode: u32) -> anyhow::Result<tokio::net::UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path).with_context(|| path.display().to_string())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(listener)
}

/// How the UI is set up, and what it runs on: with the front's settings,
/// the fleet, whose settings say what people may make; otherwise this
/// host, from its configs and its template.
fn setup(args: &Args) -> anyhow::Result<(Options, Box<dyn Backend>)> {
    let pools = args
        .pools
        .iter()
        .map(|p| {
            p.split_once('=').map(|(n, f)| (n.to_owned(), PathBuf::from(f))).with_context(|| format!("--pool {p}"))
        })
        .collect::<anyhow::Result<_>>()?;
    let mut opts = Options {
        dirs: if args.config_dirs.is_empty() {
            DEFAULT_CONFIG_DIRS.map(PathBuf::from).into()
        } else {
            args.config_dirs.clone()
        },
        state_root: args.state_root.clone(),
        origin: args.origin.clone(),
        admins: args.admins.clone(),
        admin_groups: args.admin_groups.clone(),
        creator_groups: args.creator_groups.clone(),
        self_service: None,
        oauth2_proxy: args.oauth2_proxy.clone(),
        cookie_name: args.cookie_name.clone(),
        reserve_percent: args.reserve_percent,
        reserve_min_gib: args.reserve_min_gib,
        pools,
        devices: args.devices.clone(),
        ssh_keygen: args.ssh_keygen.clone(),
        user_ca: args.user_ca.clone(),
        host_ca: args.host_ca.clone(),
    };
    if let Some(path) = &args.front {
        let settings = FrontSettings::load(path)?;
        opts.state_root = settings.state_root();
        opts.self_service = settings
            .self_service
            .as_ref()
            .map(|ss| SelfService { limits: ss.limits.clone(), paths: Paths(settings.state_root()) });
        let backend = Box::new(FleetBackend::new(opts.clone(), Fleet::new(settings)));
        return Ok((opts, backend));
    }
    // A host on its own, until the modules render a fleet.
    if let Some(path) = &args.self_service {
        let t = Template::load(path)?;
        opts.self_service = Some(SelfService { paths: t.paths(), limits: t.limits });
    }
    Ok((opts.clone(), Box::new(HostBackend { opts })))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let (opts, backend) = setup(&args)?;
    let leptos = LeptosOptions::builder()
        .output_name("workbench-web")
        .site_root(args.site.to_string_lossy().into_owned())
        .site_pkg_dir(format!("pkg/{}", asset_version(&args.site)))
        .build();
    let state = AppState::new(opts, backend, leptos)?;
    let ui = bind(&args.ui_socket, 0o660)?;
    let authz = bind(&args.authz_socket, 0o660)?;
    let connect = bind(&args.connect_socket, 0o660)?;
    let ui_app = ui_router(state.clone(), &args.site);
    let authz_app = authz_router(state.clone());
    let connect_app = connect_router(state.clone());
    let local = state.clone();
    let console_socket = args.console_socket.clone();
    // A front's consoles are its nodes', which share them on the node too.
    let consoles = async {
        if local.backend.fleet().is_some() {
            std::future::pending().await
        } else {
            local.serve_consoles(&console_socket).await
        }
    };
    tokio::select! {
        r = axum::serve(ui, ui_app) => r.context("the UI")?,
        r = axum::serve(authz, authz_app) => r.context("the access check")?,
        r = axum::serve(connect, connect_app) => r.context("the way in from a terminal")?,
        r = consoles => r.context("the shared consoles")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A front's settings, as the front module renders them.
    const FRONT: &str = r#"{
        "domain": "ws.test", "scheme": "https", "stateDir": "/var/lib/workbench",
        "nodes": { "a": { "address": "10.231.0.10", "subnet": "10.232.0", "api": "10.231.0.10:7910" } },
        "declared": "/etc/workbench-front",
        "selfService": {
            "limits": { "os": ["ubuntu"], "bootMemMiB": 2048, "maxMemoryGiB": 16, "maxDiskGiB": 128, "quota": 1 },
            "secretsRoot": "/var/lib/workbench-secrets", "environmentCommand": "/bin/true"
        }
    }"#;

    /// On a front, what people may make is the front's: a template still
    /// passed isn't read, even one that is gone.
    #[test]
    fn a_front_reads_no_template() {
        let dir = tempfile::tempdir().unwrap();
        let front = dir.path().join("front.json");
        std::fs::write(&front, FRONT).unwrap();
        let gone = dir.path().join("no-such-template.json");
        let args = |front: Option<&std::path::Path>| {
            let mut argv = vec!["workbench-web", "--origin", "https://ws.test", "--site", "/x"];
            argv.extend(["--self-service", gone.to_str().unwrap()]);
            if let Some(f) = front {
                argv.extend(["--front", f.to_str().unwrap()]);
            }
            Args::try_parse_from(argv).unwrap()
        };
        let (opts, backend) = setup(&args(Some(&front))).unwrap();
        assert!(backend.fleet().is_some(), "it runs on the fleet");
        assert_eq!(opts.state_root, std::path::Path::new("/var/lib/workbench"));
        assert_eq!(opts.self_service.unwrap().limits.max_disk_gib, 128, "the front's limits");
        assert!(setup(&args(None)).is_err(), "a host on its own reads its template");
    }
}
