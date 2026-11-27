use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use workbench::cli::{self, ExitCode};
use workbench::config::DEFAULT_CONFIG_DIRS;
use workbench::fleet::front::Fleet;
use workbench::fleet::{self, FRONT_SETTINGS, FrontSettings, NODE_SETTINGS, NodeSettings};
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
    /// Stop a workspace and delete its disk; on the front, one made at run
    /// time, with its record
    Destroy {
        workspace: String,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
        /// Where workspace disks live
        #[arg(long, default_value = "/var/lib/workbench")]
        state_root: PathBuf,
    },
    /// Become the workspace's VM, with what the front answers (internal:
    /// the unit's ExecStart)
    Run { workspace: String },
    /// Move a workspace made at run time to another node (on the front)
    Move { workspace: String, node: String },
    /// Free the name of a workspace whose node is lost for good (on the
    /// front)
    Forget { workspace: String },
    /// A node of a fleet (internal: its units)
    Node {
        /// The node's settings
        #[arg(long, default_value = NODE_SETTINGS)]
        settings: PathBuf,
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// A fleet's front (internal: its units)
    Front {
        /// The front's settings
        #[arg(long, default_value = FRONT_SETTINGS)]
        settings: PathBuf,
        #[command(subcommand)]
        command: FrontCommand,
    },
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
    /// Workspaces people make themselves, on the front (internal: the web
    /// UI's units and the reconciliation timer)
    SelfService {
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
    /// Move a workspace to another node, or pick up its move
    Move { workspace: String },
    /// Stop the workspaces of owners who left
    Reconcile,
}

// The comments are the help text, written for people, not rustdoc.
#[allow(clippy::doc_markdown)]
#[derive(Subcommand)]
enum NodeCommand {
    /// Serve the node's API to the front
    Serve,
    /// Put back, at boot and with every new template, the users and taps of
    /// workspaces made at run time, and start them
    Setup,
}

// The comments are the help text, written for people, not rustdoc.
#[allow(clippy::doc_markdown)]
#[derive(Subcommand)]
enum FrontCommand {
    /// Serve the front's API to the nodes
    Serve,
    /// At boot and after a deploy: clean up, and pick up or start moves
    Setup,
    /// Take the configs a host on its own made at run time as records on a
    /// node, once
    Import {
        node: String,
        /// The host's configs made at run time
        #[arg(default_value = "/var/lib/workbench/configs")]
        dir: PathBuf,
    },
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
        Command::Run { workspace } => {
            fleet::run::run(&NodeSettings::load(std::path::Path::new(NODE_SETTINGS))?, &workspace)
        }
        Command::Node { settings, command } => {
            let node = NodeSettings::load(&settings)?;
            rt()?.block_on(async {
                match command {
                    NodeCommand::Serve => fleet::node::serve(node).await,
                    NodeCommand::Setup => {
                        self_service::setup(&Template::load(std::path::Path::new(&node.template))?).await
                    }
                }
            })
        }
        Command::Front { settings, command } => {
            let front = FrontSettings::load(&settings)?;
            rt()?.block_on(async {
                match command {
                    FrontCommand::Serve => fleet::front::serve(front).await,
                    FrontCommand::Setup => Fleet::new(front).setup().await,
                    FrontCommand::Import { node, dir } => {
                        for name in Fleet::new(front).import(&node, &dir)? {
                            println!("{name}: a record on {node}");
                        }
                        Ok(())
                    }
                }
            })
        }
        Command::Move { workspace, node } => {
            rt()?.block_on(cli::request_move(&front()?, &workspace, &node, &mut std::io::stdout().lock()))
        }
        Command::Forget { workspace } => rt()?.block_on(front()?.forget(&workspace)),
        Command::Console { workspace } => cli::console(&dirs, &workspace),
        Command::Shutdown { workspace, timeout } => {
            rt()?.block_on(launch::shutdown(&dirs, &workspace, Duration::from_secs(timeout)))
        }
        Command::Memory { status_file, behind, reserve_percent, reserve_min_gib } => rt()?
            .block_on(memoryd::run(memoryd::Options { dirs, status_file, behind, reserve_percent, reserve_min_gib })),
        Command::GuestAgent { config } => rt()?.block_on(agent::run(&config)),
        Command::SelfService { command } => {
            let fleet = front()?;
            rt()?.block_on(async {
                match command {
                    SelfServiceCommand::Create { workspace } => fleet.create(&workspace).await,
                    SelfServiceCommand::Destroy { workspace } => fleet.destroy(&workspace).await,
                    SelfServiceCommand::Apps { workspace } => fleet.apps(&workspace).await,
                    SelfServiceCommand::Memory { workspace } => fleet.memory(&workspace).await,
                    SelfServiceCommand::Move { workspace } => fleet.move_workspace(&workspace).await,
                    SelfServiceCommand::Reconcile => fleet.reconcile().await,
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
            Fleet::of_this_machine()?.as_ref(),
            &workspace,
            yes,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
        )),
    }
}

/// This machine's fleet, as its front.
fn front() -> anyhow::Result<Fleet> {
    Fleet::of_this_machine()?.ok_or_else(|| anyhow::anyhow!("this machine isn't a fleet's front"))
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    /// The VMs' sockets are their users' and root's: no command shares them
    /// with anyone else.
    #[test]
    fn nothing_shares_the_vms_sockets() {
        assert!(super::Args::command().find_subcommand("share-sockets").is_none());
    }
}
