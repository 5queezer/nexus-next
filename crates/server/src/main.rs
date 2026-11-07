use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use nexus_core::Db;
use nexus_core::import::{self, ImportOptions};
use nexus_core::repo::cv_facts::{self, OwnerFacts};
use nexus_mcp::{McpState, Renderer, owner};
use nexus_server::backup;
use nexus_server::config::ServeArgs;
use nexus_server::serve;
use nexus_tailor::{LineChange, review_diff};
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
    Serve(Box<ServeArgs>),
    /// Set or change the owner passphrase, read from a prompt or stdin. Revokes all OAuth grants and
    /// API tokens. Restart the server afterwards.
    SetPassphrase {
        #[arg(long, env = "NEXUS_DATA_DIR", default_value = "/var/lib/nexus")]
        data_dir: PathBuf,
    },
    /// See or revoke what the owner has authorized, without touching other clients.
    Grants {
        #[arg(
            long,
            env = "NEXUS_DATA_DIR",
            default_value = "/var/lib/nexus",
            global = true
        )]
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
    /// See and change the CV's hard facts. Only the owner, on this machine, can do this: no tool can.
    CvFacts {
        #[arg(
            long,
            env = "NEXUS_DATA_DIR",
            default_value = "/var/lib/nexus",
            global = true
        )]
        data_dir: PathBuf,
        #[command(subcommand)]
        action: CvFactsAction,
    },
    /// Record that documents' files are truly gone, so backups work again. Refuses a document whose
    /// file exists, and refuses while the server runs.
    MarkBlobMissing {
        #[arg(long, env = "NEXUS_DATA_DIR", default_value = "/var/lib/nexus")]
        data_dir: PathBuf,
        /// The document IDs a failed backup named.
        #[arg(required = true)]
        ids: Vec<i64>,
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

#[derive(Subcommand)]
enum CvFactsAction {
    /// Print the facts, including banned phrases, to this terminal.
    Show,
    /// Replace the fields a profile can't express, from a JSON file: languages, metrics,
    /// allowedTokens, bannedPhrases, vocabulary. Fields left out stay as they are.
    Set {
        #[arg(long)]
        file: PathBuf,
    },
    /// Lock the facts. Prints every fact and piece of wording changed since the last lock and every URL
    /// taken from the wording, and a hash of that list. Writes nothing unless nothing changed, or you
    /// pass --confirm with that hash.
    Lock {
        /// The hash the printed list ended with: you have read it, and it is true. If anything was
        /// written since, it no longer matches, and nothing is locked.
        #[arg(long, value_name = "HASH")]
        confirm: Option<String>,
    },
    /// Unlock the facts, so that upsert_cv_profile can change them again.
    Unlock,
    /// Read a CV variant's newest revision against the text last approved, and with --confirm approve
    /// it. Only the owner, on this machine or on the dashboard, can do this: no tool can.
    Approve {
        #[arg(long)]
        variant: String,
        #[arg(long)]
        revision: i64,
        /// You have read the text, and it is true.
        #[arg(long)]
        confirm: bool,
    },
}

/// Who a change by this command is recorded as.
const CV_FACTS_ACTOR: &str = "cli";
/// About five rows of a wide terminal for one value.
const MAX_PRINTED_CHARS: usize = 1000;
/// A path names a value, which it does long before this, and a job's id is part of each of its paths.
const MAX_PRINTED_FIELD_CHARS: usize = 120;

/// The error a tool result says, in its own words.
fn tool_failure(result: &nexus_mcp::CallToolResult) -> anyhow::Error {
    let said = result
        .content
        .first()
        .and_then(|block| serde_json::to_value(block).ok());
    let text = said.as_ref().and_then(|block| block["text"].as_str());
    anyhow::anyhow!("{}", printable(text.unwrap_or("it failed")))
}

/// `text` for the owner's terminal, on one line. An agent can write nearly all of what these commands
/// print, and a control character or a line break could rewrite what the owner reads before confirming
/// it, or scroll it off the screen. So can what a terminal draws as nothing, or as a new line.
fn printable(text: &str) -> String {
    let unsafe_to_print = |c: char| {
        c.is_control()
            || matches!(c, '\u{ad}' | '\u{61c}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{e0000}'..='\u{e007f}')
    };
    text.chars()
        .map(|c| {
            if unsafe_to_print(c) {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// [`printable`], up to `most` characters. A longer value says so, because one long enough can scroll
/// the rest away. The dashboard shows every value whole.
fn cut(text: &str, most: usize) -> String {
    let total = text.chars().count();
    if total <= most {
        return printable(text);
    }
    let start: String = text.chars().take(most).collect();
    format!(
        "{}... [{} more characters, not printed here: read them on the dashboard]",
        printable(&start),
        total - most
    )
}

fn shown(text: &str) -> String {
    cut(text, MAX_PRINTED_CHARS)
}

/// Whether [`shown`] leaves part of `text` unprinted. What the owner wasn't shown can't be confirmed.
fn is_cut(text: &str) -> bool {
    text.chars().count() > MAX_PRINTED_CHARS
}

/// A diff, the one thing printed with its line breaks.
fn shown_by_line(text: &str) -> String {
    let lines: Vec<String> = text.lines().map(shown).collect();
    lines.join("\n")
}

async fn run_approve(db: &Db, variant: &str, revision: i64, confirm: bool) -> anyhow::Result<()> {
    let state = Arc::new(McpState::new(
        db.clone(),
        String::new(),
        String::new(),
        Renderer::current_exe(),
    ));
    let review = owner::review(state.clone(), variant)
        .await
        .map_err(|refused| tool_failure(&refused))?;
    if review.revision != revision {
        bail!(
            "nothing was approved: revision {revision} isn't the newest, {} is. Read that one: \
             `nexus cv-facts approve --variant {variant} --revision {}`",
            review.revision,
            review.revision
        );
    }
    if review.approved {
        println!("revision {revision} of {variant} is already approved");
        return Ok(());
    }
    if !review.locked {
        bail!(
            "nothing was approved: the facts aren't locked. Check them, then `nexus cv-facts lock`"
        );
    }
    if let Some(why) = review.whole_cv_because() {
        println!("{why}");
    }
    if review.baseline_is_replaced {
        println!("{}", owner::Review::REPLACED_ELSEWHERE);
    }
    match review.baseline_revision {
        None => println!("this is the whole CV:"),
        Some(at) if review.baseline_is_start => {
            println!("compared with the approved text this variant started from, revision {at} there:");
        }
        Some(at) if review.baseline_is_replaced => {
            println!("compared with revision {at}, the approved text this variant last held:");
        }
        Some(at) if review.baseline_is_own && review.baseline_is_lock => {
            println!("compared with revision {at}, approved by your lock of the facts:");
        }
        Some(at) if review.baseline_is_own => {
            println!("compared with revision {at}, the last you approved in this variant:");
        }
        Some(at) => println!(
            "compared with revision {at}, which holds text you approved in another variant:"
        ),
    }
    let lines = review_diff(review.baseline.as_deref(), &review.rendered_text);
    for line in &lines {
        match line.change {
            LineChange::Added => println!("+ {}", shown(&line.text)),
            LineChange::Removed => println!("- {}", shown(&line.text)),
            LineChange::Same => {}
        }
    }
    if !review.marked.is_empty() {
        println!(
            "{} Near: {}",
            owner::Review::DRAWN_MARKS,
            review
                .marked
                .iter()
                .map(|word| shown(word))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let source_changed = review
        .source_diff
        .lines()
        .any(|line| line.starts_with(['+', '-']));
    if source_changed && lines.iter().all(|line| line.change == LineChange::Same) {
        println!("The wording is unchanged, but the source isn't. Read how it changed:");
        println!("{}", shown_by_line(&review.source_diff));
    }
    for item in &review.items {
        println!("check {:?}: {}", item.rule, shown(&item.token));
    }
    let changed = lines
        .iter()
        .filter(|line| line.change != LineChange::Same)
        .count();
    println!(
        "That is {changed} changed lines and {} items, which your terminal may not hold. This is the text: a shape drawn over it shows only in the picture, so look at the stamped preview on the dashboard.",
        review.items.len()
    );
    let was_cut = lines
        .iter()
        .filter(|line| line.change != LineChange::Same)
        .map(|line| line.text.as_str())
        .chain(review.items.iter().map(|item| item.token.as_str()))
        .chain(review.source_diff.lines())
        .any(is_cut);
    if was_cut {
        bail!(
            "nothing was written. A value above was cut short, so this can't be confirmed here: \
             read it whole and approve it on the dashboard"
        );
    }
    if !confirm {
        bail!(
            "nothing was written. Approving says all of the above is true: if it is, run \
             `nexus cv-facts approve --variant {variant} --revision {revision} --confirm`"
        );
    }
    let result = owner::approve(
        state,
        variant,
        revision,
        &review.source_sha256,
        &review.facts_sha256,
        CV_FACTS_ACTOR,
    )
    .await;
    if result.is_error == Some(true) {
        return Err(tool_failure(&result));
    }
    println!("revision {revision} of {variant} approved");
    Ok(())
}

async fn run_cv_facts(db: &Db, action: CvFactsAction) -> anyhow::Result<()> {
    if let CvFactsAction::Show = action {
        let state = cv_facts::get(&mut *db.conn().await?).await?;
        let lines: Vec<String> = serde_json::to_string_pretty(&state)?
            .lines()
            .map(printable)
            .collect();
        println!("{}", lines.join("\n"));
        return Ok(());
    }
    if let CvFactsAction::Approve {
        variant,
        revision,
        confirm,
    } = &action
    {
        return run_approve(db, variant, *revision, *confirm).await;
    }
    let mut tx = db.begin_write().await?;
    let state = match action {
        CvFactsAction::Show | CvFactsAction::Approve { .. } => unreachable!("handled above"),
        CvFactsAction::Set { file } => {
            let bytes =
                std::fs::read(&file).with_context(|| format!("cannot read {}", file.display()))?;
            let fields: OwnerFacts = serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not valid", file.display()))?;
            cv_facts::set_owner_facts(&mut tx, &fields, CV_FACTS_ACTOR).await?
        }
        CvFactsAction::Lock { confirm } => {
            let preview = cv_facts::lock_preview(&mut tx).await?;
            for change in &preview.changes {
                println!(
                    "changed {}: {} -> {}",
                    cut(&change.field, MAX_PRINTED_FIELD_CHARS),
                    shown(&change.old),
                    shown(&change.new)
                );
            }
            for change in &preview.wording {
                println!(
                    "wording changed {}: {} -> {}",
                    cut(&change.field, MAX_PRINTED_FIELD_CHARS),
                    shown(&change.old),
                    shown(&change.new)
                );
            }
            for token in &preview.added {
                println!("added to allowed tokens: {}", shown(token));
            }
            println!(
                "That is {} facts, {} pieces of wording and {} tokens, which your terminal may not hold: read them all, or read them on the dashboard's facts page.",
                preview.changes.len(),
                preview.wording.len(),
                preview.added.len()
            );
            let was_cut = preview
                .changes
                .iter()
                .chain(&preview.wording)
                .flat_map(|change| [change.old.as_str(), &change.new])
                .chain(preview.added.iter().map(String::as_str))
                .any(is_cut)
                || preview
                    .changes
                    .iter()
                    .chain(&preview.wording)
                    .any(|change| change.field.chars().count() > MAX_PRINTED_FIELD_CHARS);
            if was_cut {
                anyhow::bail!(
                    "nothing was written. A value above was cut short, so this can't be confirmed \
                     here: read it whole and lock on the dashboard's facts page"
                );
            }
            let hash = preview.hash();
            if preview.needs_confirmation() && confirm.as_deref() != Some(hash.as_str()) {
                let moved = if confirm.is_some() {
                    "something changed since that hash was printed, and "
                } else {
                    ""
                };
                anyhow::bail!(
                    "{moved}nothing was written. Locking confirms all of the above as true, and a \
                     CV generated from this wording needs no further approval: if it is true, run \
                     `nexus cv-facts lock --confirm {hash}`"
                );
            }
            cv_facts::lock(&mut tx, CV_FACTS_ACTOR).await?.0
        }
        CvFactsAction::Unlock => cv_facts::unlock(&mut tx, CV_FACTS_ACTOR).await?,
    };
    tx.commit().await?;
    let lock = if state.is_locked() {
        "locked"
    } else {
        "unlocked"
    };
    println!("facts version {}, {lock}", state.version);
    Ok(())
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

/// Not a clap subcommand, so it stays out of `--help`, and handled before the async runtime starts,
/// so a compile runs in a plain process.
fn main() -> anyhow::Result<()> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "__render")
    {
        nexus_tailor::run_worker();
    }
    run()
}

#[tokio::main]
async fn run() -> anyhow::Result<()> {
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
        Command::CvFacts { data_dir, action } => {
            serve::init_logging(Level::INFO, std::io::stderr);
            let db = Db::open(&data_dir)
                .await
                .context("cannot open the database")?;
            let result = run_cv_facts(&db, action).await;
            db.close().await;
            result?;
        }
        Command::MarkBlobMissing { data_dir, ids } => {
            let changed = backup::mark_blob_missing(&data_dir, &ids).await?;
            println!("marked missing: documents {changed:?}");
        }
    }
    Ok(())
}
