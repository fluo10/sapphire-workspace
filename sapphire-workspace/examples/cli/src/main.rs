mod commands;
mod mcp;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "sapphire-workspace",
    about = "Workspace file management: indexing, sync, and search"
)]
struct Cli {
    /// Workspace directory (env: SAPPHIRE_WORKSPACE_DIR).
    ///
    /// When omitted, the workspace root is discovered by walking up from the
    /// current directory looking for `.sapphire-workspace/`.  If no marker
    /// directory is found the current directory is used.
    #[arg(
        long,
        env = "SAPPHIRE_WORKSPACE_DIR",
        global = true,
        value_name = "DIR"
    )]
    workspace_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialise a new workspace (create .sapphire-workspace/config.toml)
    Init {
        /// Directory to initialise (defaults to current directory)
        path: Option<PathBuf>,
    },

    /// Run the full sync cycle: commit staged changes, pull remote, then push
    Sync,

    /// Index a single file and stage it for sync
    Upsert {
        /// Path of the file to index and stage
        path: PathBuf,
    },

    /// Remove a single file from the index and unstage it
    #[command(alias = "remove")]
    Delete {
        /// Path of the file to remove
        path: PathBuf,
    },

    /// Watch the workspace for file changes and update the index automatically
    Watch {
        /// Debounce interval in milliseconds before processing events (default: 300)
        #[arg(long, default_value_t = 300)]
        debounce_ms: u64,
    },

    /// Manage the retrieve (FTS/vector) index
    Cache {
        #[command(subcommand)]
        action: CacheCommand,
    },

    /// Start the MCP server (stdio transport)
    Mcp,
}

#[derive(Subcommand)]
enum CacheCommand {
    /// Incrementally sync the workspace into the retrieve index
    Sync,
    /// Delete the current index and rebuild it from scratch
    Rebuild,
    /// Show index location, schema version, and document count
    Info,
    /// Generate embeddings for documents that do not yet have a vector
    Embed,
    /// Remove stale index files from previous schema versions
    Clean,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let workspace_dir = cli.workspace_dir.as_deref();

    match cli.command {
        Command::Init { path } => commands::init::run(path.as_deref())?,
        Command::Sync => commands::sync::run(workspace_dir)?,
        Command::Upsert { path } => commands::upsert::run(workspace_dir, &path)?,
        Command::Delete { path } => commands::delete::run(workspace_dir, &path)?,
        Command::Watch { debounce_ms } => commands::watch::run(workspace_dir, debounce_ms)?,
        Command::Cache { action } => match action {
            CacheCommand::Sync => commands::cache::sync::run(workspace_dir)?,
            CacheCommand::Rebuild => commands::cache::rebuild::run(workspace_dir)?,
            CacheCommand::Info => commands::cache::info::run(workspace_dir)?,
            CacheCommand::Embed => commands::cache::embed::run(workspace_dir)?,
            CacheCommand::Clean => commands::cache::clean::run(workspace_dir)?,
        },
        Command::Mcp => mcp::run(workspace_dir)?,
    }
    Ok(())
}
