//! `workbench-web`: the web UI's server, run by `workbench-web.service` on
//! the front as its own user. It runs on the fleet the front's settings,
//! `WORKBENCH_FRONT`, describe: the registry and the nodes. It listens on
//! Unix sockets only, which nginx's group reaches: the UI, the access check
//! and the way in from a terminal.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use leptos::config::LeptosOptions;
use workbench::fleet::FrontSettings;
use workbench::self_service::Paths;
use workbench_web::server::{
    AppState, Backend, Fleet, FleetBackend, Options, SelfService, asset_version, authz_router, connect_router,
    ui_router,
};

/// Serves workbench's web UI.
// The comments are the help text, written for people, not rustdoc.
#[allow(clippy::doc_markdown)]
#[derive(Parser)]
#[command(name = "workbench-web", version)]
struct Args {
    /// The front's settings: its registry, its nodes and what people may
    /// make
    #[arg(long, env = "WORKBENCH_FRONT", value_name = "FILE")]
    front: PathBuf,
    /// The UI's own origin, https://<domain>
    #[arg(long)]
    origin: String,
    /// A user who sees and controls every workspace; repeat for each
    #[arg(long = "admin", value_name = "USER")]
    admins: Vec<String>,
    /// A group whose members are admins; repeat for each
    #[arg(long = "admin-group", value_name = "GROUP")]
    admin_groups: Vec<String>,
    /// A group whose members make workspaces of their own; repeat for each
    #[arg(long = "creator-group", value_name = "GROUP")]
    creator_groups: Vec<String>,
    /// oauth2-proxy, which the access check asks who a cookie belongs to
    #[arg(long, default_value = "http://127.0.0.1:4180")]
    oauth2_proxy: String,
    /// The single sign-on cookie, which apps never get
    #[arg(long, default_value = "_workbench_auth")]
    cookie_name: String,
    /// The site: assets/ and the WebAssembly in pkg/
    #[arg(long)]
    site: PathBuf,
    /// Where the UI listens, for nginx
    #[arg(long, default_value = "/run/workbench-web/ui.sock")]
    ui_socket: PathBuf,
    /// Where the access check listens, for nginx
    #[arg(long, default_value = "/run/workbench-web/authz.sock")]
    authz_socket: PathBuf,
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

/// How the UI is set up, and the fleet it runs on, whose settings say what
/// people may make.
fn setup(args: &Args) -> anyhow::Result<(Options, Box<dyn Backend>)> {
    let settings = FrontSettings::load(&args.front)?;
    let opts = Options {
        origin: args.origin.clone(),
        admins: args.admins.clone(),
        admin_groups: args.admin_groups.clone(),
        creator_groups: args.creator_groups.clone(),
        self_service: settings
            .self_service
            .as_ref()
            .map(|ss| SelfService { limits: ss.limits.clone(), paths: Paths(settings.state_root()) }),
        oauth2_proxy: args.oauth2_proxy.clone(),
        cookie_name: args.cookie_name.clone(),
        devices: args.devices.clone(),
        ssh_keygen: args.ssh_keygen.clone(),
        user_ca: args.user_ca.clone(),
        host_ca: args.host_ca.clone(),
    };
    let backend = Box::new(FleetBackend::new(opts.clone(), Fleet::new(settings)));
    Ok((opts, backend))
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
    tokio::select! {
        r = axum::serve(ui, ui_app) => r.context("the UI")?,
        r = axum::serve(authz, authz_app) => r.context("the access check")?,
        r = axum::serve(connect, connect_app) => r.context("the way in from a terminal")?,
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

    /// It runs on the fleet the front's settings describe, and what people
    /// may make is the front's; without them it doesn't run.
    #[test]
    fn it_runs_on_the_fronts_settings() {
        let dir = tempfile::tempdir().unwrap();
        let front = dir.path().join("front.json");
        std::fs::write(&front, FRONT).unwrap();
        let args = |front: Option<&std::path::Path>| {
            let mut argv = vec!["workbench-web", "--origin", "https://ws.test", "--site", "/x"];
            if let Some(f) = front {
                argv.extend(["--front", f.to_str().unwrap()]);
            }
            Args::try_parse_from(argv)
        };
        let (opts, backend) = setup(&args(Some(&front)).unwrap()).unwrap();
        assert_eq!(backend.fleet().state_root(), std::path::Path::new("/var/lib/workbench"));
        assert_eq!(opts.self_service.unwrap().limits.max_disk_gib, 128, "the front's limits");
        assert!(args(None).is_err(), "no front, no UI");
    }
}
