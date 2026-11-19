use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use workbench::config::DEFAULT_CONFIG_DIRS;
use workbench::launch;

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
    /// Become the workspace's VM (internal: the unit's ExecStart)
    Run { workspace: String },
    /// Power the VM off cleanly (internal: the unit's ExecStop)
    Shutdown {
        workspace: String,
        /// How long the guest may take to power off, in seconds
        #[arg(long, default_value_t = 90)]
        timeout: u64,
    },
}

fn main() {
    let args = Args::parse();
    if let Err(e) = run(args) {
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
        Command::Shutdown { workspace, timeout } => {
            rt()?.block_on(launch::shutdown(&dirs, &workspace, Duration::from_secs(timeout)))
        }
    }
}
