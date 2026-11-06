use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use nexus_core::import::{self, ImportOptions};

#[derive(Parser)]
#[command(
    name = "nexus",
    version,
    about = "Job-application CRM: MCP tool server and dashboard"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load an old-Nexus export into a data directory.
    Import {
        /// The export directory, holding MANIFEST.json.
        #[arg(long)]
        export: PathBuf,
        /// Where nexus.db and blobs/ live.
        #[arg(long)]
        data_dir: PathBuf,
        /// File of document IDs, one per line, whose recovered candidates are confirmed.
        #[arg(long)]
        approve_recovered: Option<PathBuf>,
        /// Replace a database holding rows no import wrote. Its blobs are left in place.
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Import {
            export,
            data_dir,
            approve_recovered,
            force,
        } => {
            let options = ImportOptions {
                export_dir: export,
                data_dir,
                approve_recovered,
                force,
            };
            let summary = import::run(&options).await.context("import failed")?;
            println!("{summary}");
        }
    }
    Ok(())
}
