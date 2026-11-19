use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use workbench::cli::{self, ExitCode};
use workbench::config::DEFAULT_CONFIG_DIRS;
use workbench::memoryd::{self, BehindPool};
use workbench::{agent, launch};

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
    /// Create a T3 Code pairing link for the owner; flags after the name go
    /// to t3
    Pair {
        workspace: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
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
    /// The agent inside the guest (internal)
    GuestAgent {
        /// Workspace settings from the config share
        #[arg(long, default_value = "/run/workspace/config/workspace.json")]
        config: PathBuf,
    },
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
        Command::Shutdown { workspace, timeout } => {
            rt()?.block_on(launch::shutdown(&dirs, &workspace, Duration::from_secs(timeout)))
        }
        Command::Memory { status_file, behind, reserve_percent, reserve_min_gib } => rt()?
            .block_on(memoryd::run(memoryd::Options { dirs, status_file, behind, reserve_percent, reserve_min_gib })),
        Command::GuestAgent { config } => rt()?.block_on(agent::run(&config)),
        Command::Pair { workspace, args } => rt()?.block_on(cli::pair(&dirs, &workspace, args)),
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
