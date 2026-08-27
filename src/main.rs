mod auth;
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

        /// Notion token; falls back to credentials stored by `login`
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

    /// Log in with OAuth using your own Notion public integration. Opens the
    /// browser, then stores the access token in ~/.config/md2notion/.
    Login {
        /// OAuth client ID of your public integration
        #[arg(long, env = "NOTION_OAUTH_CLIENT_ID")]
        client_id: String,

        /// OAuth client secret of your public integration
        #[arg(long, env = "NOTION_OAUTH_CLIENT_SECRET", hide_env_values = true)]
        client_secret: String,

        /// Localhost port for the OAuth redirect. Your integration must list
        /// http://localhost:<port>/callback as a redirect URI.
        #[arg(long, default_value_t = 8237)]
        port: u16,
    },

    /// Remove credentials stored by `login`
    Logout,
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
        Command::Login {
            client_id,
            client_secret,
            port,
        } => auth::login(&client_id, &client_secret, port).await,
        Command::Logout => {
            if auth::logout()? {
                println!("Logged out — stored credentials removed.");
            } else {
                println!("No stored credentials found.");
            }
            Ok(())
        }
    }
}
