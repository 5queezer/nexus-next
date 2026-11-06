use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use nexus_core::import::{self, ImportOptions};
use nexus_server::backup;
use nexus_server::config::ServeArgs;
use nexus_server::serve;
use tokio::signal::unix::{SignalKind, signal};

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
    /// Run the server.
    Serve(ServeArgs),
    /// Write an encrypted, verified backup. Safe while the server runs.
    Backup {
        #[arg(long, env = "NEXUS_DATA_DIR", default_value = "/var/lib/nexus")]
        data_dir: PathBuf,
        /// The backup file to create.
        #[arg(long)]
        out: PathBuf,
        /// File of age public keys (age1...), one per line. The board never needs the private key.
        #[arg(long, env = "NEXUS_BACKUP_RECIPIENT_FILE")]
        recipient_file: PathBuf,
    },
    /// Restore a backup into a data directory. Stop the server first.
    Restore {
        #[arg(long)]
        from: PathBuf,
        /// The age identity file (private key) matching a recipient of the backup.
        #[arg(long)]
        identity: PathBuf,
        #[arg(long, env = "NEXUS_DATA_DIR", default_value = "/var/lib/nexus")]
        data_dir: PathBuf,
        /// Replace an existing database, keeping it as nexus.db.before-restore.
        #[arg(long)]
        force: bool,
    },
}

async fn shutdown_signal() {
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    tracing::info!("shutting down");
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
        Command::Serve(args) => {
            let config = args
                .validate()
                .map_err(|e| anyhow::anyhow!("invalid configuration:\n{e}"))?;
            serve::init_logging(config.log_level, std::io::stdout);
            let listener = tokio::net::TcpListener::bind(config.bind)
                .await
                .with_context(|| format!("cannot bind {}", config.bind))?;
            serve::run(config, listener, shutdown_signal()).await?;
        }
        Command::Backup {
            data_dir,
            out,
            recipient_file,
        } => {
            let recipients = backup::read_recipients(&recipient_file)?;
            let summary = backup::backup(&data_dir, &out, &recipients).await?;
            println!("backup written: {summary}");
        }
        Command::Restore {
            from,
            identity,
            data_dir,
            force,
        } => {
            let summary = backup::restore(&from, &identity, &data_dir, force).await?;
            println!("restored: {summary}");
        }
    }
    Ok(())
}
