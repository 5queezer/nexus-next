use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use nexus_core::Db;
use nexus_core::import::{self, ImportOptions};
use nexus_server::backup;
use nexus_server::config::ServeArgs;
use nexus_server::serve;
use tokio::signal::unix::{SignalKind, signal};
use tracing::Level;

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
    /// Set or change the owner passphrase, read from a prompt or stdin. Revokes all OAuth grants and
    /// API tokens. Restart the server afterwards.
    SetPassphrase {
        #[arg(long, env = "NEXUS_DATA_DIR", default_value = "/var/lib/nexus")]
        data_dir: PathBuf,
    },
    /// See or revoke what the owner has authorized, without touching other clients.
    Grants {
        #[arg(long, env = "NEXUS_DATA_DIR", default_value = "/var/lib/nexus", global = true)]
        data_dir: PathBuf,
        #[command(subcommand)]
        action: GrantAction,
    },
    /// Issue the API token and print it once. Revokes any earlier API token.
    IssueToken {
        #[arg(long, env = "NEXUS_DATA_DIR", default_value = "/var/lib/nexus")]
        data_dir: PathBuf,
        /// A label for your own reference.
        #[arg(long, default_value = "default")]
        name: String,
    },
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
        /// Keep the backup's API token and OAuth clients and grants. Without this, every client
        /// authorizes again. Use it only for a backup you know you wrote.
        #[arg(long)]
        keep_credentials: bool,
    },
}

#[derive(Subcommand)]
enum GrantAction {
    /// One line per authorization with a live token.
    List,
    /// Revoke one authorization's tokens at once. Its client must authorize again.
    Revoke {
        /// The ID from `grants list`, also logged as `grant=`.
        id: String,
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

/// On a terminal, asks twice without echo. Otherwise reads one line from stdin. Never from argv,
/// which every local user can read.
fn read_new_passphrase() -> anyhow::Result<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        let passphrase = rpassword::prompt_password("New owner passphrase: ")?;
        if rpassword::prompt_password("Repeat it: ")? != passphrase {
            bail!("the two entries differ, nothing was changed");
        }
        return Ok(passphrase);
    }
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

async fn run_grants(db: &Db, action: GrantAction) -> anyhow::Result<()> {
    match action {
        GrantAction::List => {
            for grant in nexus_auth::list_grants(db).await? {
                println!(
                    "{}\t{}\t{:?}\t{}\t{}\tissued {}\texpires {}",
                    grant.id,
                    grant.client_id,
                    grant.client_name,
                    grant.redirect_origin,
                    grant.scopes,
                    grant.issued_at,
                    grant.expires_at
                );
            }
        }
        GrantAction::Revoke { id } => {
            let revoked = nexus_auth::revoke_grant(db, &id).await?;
            println!(
                "revoked {} access and {} refresh tokens",
                revoked.access_tokens, revoked.refresh_tokens
            );
        }
    }
    Ok(())
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
        Command::SetPassphrase { data_dir } => {
            serve::init_logging(Level::INFO, std::io::stderr);
            if std::env::var_os("NEXUS_OWNER_PASSPHRASE_HASH").is_some() {
                bail!("NEXUS_OWNER_PASSPHRASE_HASH is set and would override the new passphrase");
            }
            let passphrase = read_new_passphrase()?;
            let db = Db::open(&data_dir)
                .await
                .context("cannot open the database")?;
            let path = nexus_auth::owner_passphrase_file(&data_dir);
            let result = nexus_auth::set_owner_passphrase(&db, &path, &passphrase).await;
            db.close().await;
            result?;
            eprintln!(
                "Passphrase hash written to {}. Restart nexus for it to apply: until then a running \
                 server still accepts the old passphrase. Each OAuth client must authorize again.",
                path.display()
            );
        }
        Command::Grants { data_dir, action } => {
            serve::init_logging(Level::INFO, std::io::stderr);
            let db = Db::open(&data_dir)
                .await
                .context("cannot open the database")?;
            let result = run_grants(&db, action).await;
            db.close().await;
            result?;
        }
        Command::IssueToken { data_dir, name } => {
            serve::init_logging(Level::INFO, std::io::stderr);
            let db = Db::open(&data_dir)
                .await
                .context("cannot open the database")?;
            let token = nexus_auth::issue_api_token(&db, &name).await;
            db.close().await;
            println!("{}", token?);
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
            keep_credentials,
        } => {
            let summary =
                backup::restore(&from, &identity, &data_dir, force, keep_credentials).await?;
            println!("restored: {summary}");
        }
    }
    Ok(())
}
