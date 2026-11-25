use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use workbench::cli::{self, ExitCode};
use workbench::config::DEFAULT_CONFIG_DIRS;
use workbench::memoryd::{self, BehindPool};
use workbench::self_service::{self, Template};
use workbench::{agent, launch};
use workbench_model::Verb;

/// Persistent per-person workspaces: long-lived cloud-hypervisor VMs.
#[derive(Parser)]
#[command(name = "workbench", version)]
struct Args {
    /// Directory of <workspace>.json configs; repeat to search several, the
    /// first winning [default: /etc/workbench, /var/lib/workbench/configs]
    #[arg(long = "config-dir", global = true, value_name = "DIR")]
    config_dirs: Vec<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

// The comments are the help text, written for people, not rustdoc.
#[allow(clippy::doc_markdown)]
#[derive(Subcommand)]
enum Command {
    /// List workspaces, their VMs and disks
    Status {
        /// Print JSON
        #[arg(long)]
        json: bool,
        /// Where workspace disks live
        #[arg(long, default_value = "/var/lib/workbench")]
        state_root: PathBuf,
    },
    /// Start a workspace
    Start {
        workspace: String,
        #[command(flatten)]
        state: StateRoot,
    },
    /// Stop a workspace; it stays stopped across host reboots until started
    Stop {
        workspace: String,
        #[command(flatten)]
        state: StateRoot,
    },
    /// Restart a workspace, with the apps and packages now on the host
    Restart {
        workspace: String,
        #[command(flatten)]
        state: StateRoot,
    },
    /// Print a link that signs a browser in to an app of the workspace
    #[command(alias = "sign-in")]
    OpenLink {
        workspace: String,
        /// The app's id; a workspace with one app needs none
        app: Option<String>,
    },
    /// Attach to the serial console (Ctrl-] quits)
    Console { workspace: String },
    /// Stop a workspace and delete its disk
    Destroy {
        workspace: String,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
        /// Where workspace disks live
        #[arg(long, default_value = "/var/lib/workbench")]
        state_root: PathBuf,
    },
    /// Become the workspace's VM (internal: the unit's ExecStart)
    Run { workspace: String },
    /// Let the web UI use the VM's sockets (internal: the unit's ExecStartPost)
    ShareSockets { workspace: String },
    /// Power the VM off cleanly (internal: the unit's ExecStop)
    Shutdown {
        workspace: String,
        /// How long the guest may take to power off, in seconds
        #[arg(long, default_value_t = 90)]
        timeout: u64,
    },
    /// Grow and shrink workspace memory (internal: workbench-memory.service)
    Memory {
        /// Where to publish the memory workspaces hold
        #[arg(long = "memory-status", value_name = "FILE")]
        status_file: PathBuf,
        /// <status file>=<GiB> of a pool that yields to workspaces and the
        /// memory it keeps whatever they want; repeat for each
        #[arg(long, value_name = "FILE=GIB")]
        behind: Vec<BehindPool>,
        /// Host memory reserve, percent of MemTotal
        #[arg(long = "mem-reserve-percent", default_value_t = 10.0)]
        reserve_percent: f64,
        /// Minimum host memory reserve, GiB
        #[arg(long = "mem-reserve-min-gib", default_value_t = 2.0)]
        reserve_min_gib: f64,
    },
    /// Workspaces people make themselves (internal: the web UI's units and
    /// the boot and reconciliation services)
    SelfService {
        /// The host's template for them
        #[arg(long, default_value = self_service::TEMPLATE)]
        template: PathBuf,
        #[command(subcommand)]
        command: SelfServiceCommand,
    },
    /// The agent inside the guest (internal)
    GuestAgent {
        /// Workspace settings from the config share
        #[arg(long, default_value = "/run/workspace/config/workspace.json")]
        config: PathBuf,
    },
}

// The comments are the help text, written for people, not rustdoc.
#[allow(clippy::doc_markdown)]
#[derive(Subcommand)]
enum SelfServiceCommand {
    /// Make the workspace the web UI asked for
    Create { workspace: String },
    /// Give a workspace made at run time the apps the web UI asked for
    Apps { workspace: String },
    /// Give a workspace made at run time the most memory an admin asked for
    /// in the web UI
    Memory { workspace: String },
    /// Stop a workspace made at run time and delete it
    Destroy { workspace: String },
    /// Put back, at boot, the users and taps of workspaces made at run time,
    /// and start them
    Setup,
    /// Stop the workspaces of owners who left
    Reconcile,
}

#[derive(clap::Args)]
struct StateRoot {
    /// Where workspace disks and stop markers live
    #[arg(long = "state-root", default_value = "/var/lib/workbench")]
    path: PathBuf,
}

fn main() {
    let args = Args::parse();
    if let Err(e) = run(args) {
        if let Some(ExitCode(code)) = e.downcast_ref::<ExitCode>() {
            std::process::exit(*code);
        }
        eprintln!("workbench: {e:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let dirs =
        if args.config_dirs.is_empty() { DEFAULT_CONFIG_DIRS.map(PathBuf::from).into() } else { args.config_dirs };
    let rt = || tokio::runtime::Builder::new_multi_thread().enable_all().build();
    match args.command {
        // Becomes cloud-hypervisor: no runtime threads left behind.
        Command::Run { workspace } => launch::run(&dirs, &workspace),
        Command::Console { workspace } => cli::console(&dirs, &workspace),
        Command::ShareSockets { workspace } => {
            // The sockets come once the disk is made, which the unit's
            // TimeoutStartSec bounds.
            rt()?.block_on(launch::share_sockets(&dirs, &workspace, Duration::from_secs(24 * 3600)))
        }
        Command::Shutdown { workspace, timeout } => {
            rt()?.block_on(launch::shutdown(&dirs, &workspace, Duration::from_secs(timeout)))
        }
        Command::Memory { status_file, behind, reserve_percent, reserve_min_gib } => rt()?
            .block_on(memoryd::run(memoryd::Options { dirs, status_file, behind, reserve_percent, reserve_min_gib })),
        Command::GuestAgent { config } => rt()?.block_on(agent::run(&config)),
        Command::SelfService { template, command } => {
            let t = Template::load(&template)?;
            rt()?.block_on(async {
                match command {
                    SelfServiceCommand::Create { workspace } => self_service::create(&dirs, &t, &workspace).await,
                    SelfServiceCommand::Destroy { workspace } => self_service::destroy(&t, &workspace).await,
                    SelfServiceCommand::Apps { workspace } => self_service::apps(&t, &workspace).await,
                    SelfServiceCommand::Memory { workspace } => self_service::memory(&t, &workspace).await,
                    SelfServiceCommand::Setup => self_service::setup(&t).await,
                    SelfServiceCommand::Reconcile => self_service::reconcile(&t).await,
                }
            })
        }
        Command::Start { workspace, state } => {
            rt()?.block_on(cli::control(&dirs, &state.path, Verb::Start, &workspace))
        }
        Command::Stop { workspace, state } => rt()?.block_on(cli::control(&dirs, &state.path, Verb::Stop, &workspace)),
        Command::Restart { workspace, state } => {
            rt()?.block_on(cli::control(&dirs, &state.path, Verb::Restart, &workspace))
        }
        Command::OpenLink { workspace, app } => rt()?.block_on(cli::open_link(&dirs, &workspace, app.as_deref())),
        Command::Status { json, state_root } => {
            rt()?.block_on(cli::status(&mut std::io::stdout().lock(), &dirs, &state_root, json))
        }
        Command::Destroy { workspace, yes, state_root } => rt()?.block_on(cli::destroy(
            &dirs,
            &state_root,
            &workspace,
            yes,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
        )),
    }
}
