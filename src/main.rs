mod markdown;
mod notion;
mod sync;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "md2notion", version, about = "Sync a folder of Markdown files to Notion pages")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sync all .md files in a folder to Notion. The mapping between files
    /// and pages lives in Notion itself (a managed database), so repeated
    /// syncs from any machine update the same pages.
    Sync {
        /// Folder containing markdown files (searched recursively)
        dir: PathBuf,

        /// Notion parent (URL or ID): a page to hold the managed database,
        /// or the database itself
        #[arg(long, env = "MD2NOTION_PARENT")]
        parent: Option<String>,

        /// Notion integration token
        #[arg(long, env = "NOTION_TOKEN", hide_env_values = true)]
        token: Option<String>,

        /// Re-upload every file even if its content is unchanged
        #[arg(long)]
        force: bool,

        /// Archive Notion pages whose source file was deleted
        #[arg(long)]
        prune: bool,

        /// Query Notion and print the plan without changing anything
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Sync {
            dir,
            parent,
            token,
            force,
            prune,
            dry_run,
        } => {
            sync::run(sync::SyncOptions {
                dir,
                parent,
                token,
                force,
                prune,
                dry_run,
            })
            .await
        }
    }
}
