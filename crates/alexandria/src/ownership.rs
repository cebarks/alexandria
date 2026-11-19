//! Offline account enrollment and adoption of an explicitly backed-up legacy store.

use std::io::Read;
use std::path::{Path, PathBuf};

use alexandria_storage::access::{AccessRepo, SystemContext};
use alexandria_storage::models::{OperationId, PrincipalIds};
use alexandria_storage::ownership_migration::{
    BackupReceipt, OwnershipMigration, Report, check_ownership_ready,
};
use alexandria_storage::repos::{AuthRepo, IdentityRepo};
use alexandria_storage::{Database, schema, system_config};
use anyhow::{Context, Result, bail, ensure};

use crate::{Config, dreaming};

const DEFAULT_PAGE_ROWS: usize = 256;
const MAX_PASSWORD_BYTES: usize = 4096;
const USAGE: &str = "Usage: alexandria bootstrap-owner --login LOGIN [--password-file FILE] [--backup-data-dir PATH] [--max-rows N]";
const HELP: &str = "Stop Alexandria before running this command: SurrealKV is single-writer.
For a new account, supply its password in a private UTF-8 file (not as a command-line value).
For a legacy corpus, supply an independent offline copy and keep its original source-manifest sidecar.
To resume, use the same --login and --backup-data-dir. If you also provide a password file for an existing account, it must match; this command never resets a password.
Progress is written to stderr; the verified JSON report is written to stdout.";

struct Request {
    login: String,
    password_file: Option<PathBuf>,
    backup_dir: Option<PathBuf>,
    page_rows: usize,
}

impl Request {
    fn parse(args: &[String]) -> Result<Self> {
        let mut login = None;
        let mut password_file = None;
        let mut backup_dir = None;
        let mut page_rows = None;
        let mut args = args.iter().map(String::as_str);
        while let Some(flag) = args.next() {
            let slot = match flag {
                "--login" => &mut login,
                "--password-file" => &mut password_file,
                "--backup-data-dir" => &mut backup_dir,
                "--max-rows" => &mut page_rows,
                _ => bail!("unexpected option {flag:?}. {USAGE}"),
            };
            ensure!(slot.is_none(), "duplicate option {flag}. {USAGE}");
            *slot = Some(
                args.next()
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .with_context(|| format!("{flag} needs a value. {USAGE}"))?,
            );
        }
        let login = login.with_context(|| format!("--login is required. {USAGE}"))?;
        let login = alexandria_mcp::auth::enrollable_login(login)?;
        let page_rows = match page_rows {
            Some(rows) => {
                let rows: usize = rows
                    .parse()
                    .context("--max-rows needs a positive integer")?;
                ensure!(rows > 0, "--max-rows must be positive");
                rows
            }
            None => DEFAULT_PAGE_ROWS,
        };
        Ok(Self {
            login,
            password_file: password_file.map(PathBuf::from),
            backup_dir: backup_dir.map(PathBuf::from),
            page_rows,
        })
    }
}

pub(crate) async fn run(args: &[String]) -> Result<()> {
    if matches!(args, [help] if matches!(help.as_str(), "--help" | "-h")) {
        println!("{USAGE}\n{HELP}");
        return Ok(());
    }
    let request = Request::parse(args)?;
    let config = Config::load()?;
    let path = &config.database.data_dir;
    let mut db = open_store(path).await?;
    // This command uses the stored identity, not the configured model. Refuse unfinished embedding
    // targets before loading a receipt or provisioning an account, and never arm the access clock.
    system_config::stored_embedding_space(db.inner())
        .await
        .with_context(|| format!("Refused bootstrap for data_dir={}", path.display()))?;
    let allow_new_owner = allow_enrollment(&db, request.backup_dir.is_some()).await?;
    let receipt = request
        .backup_dir
        .as_deref()
        .map(|backup| read_receipt(path, backup))
        .transpose()?;

    schema::migrate(db.inner()).await?;
    let system = db.claim_system_authority()?;
    let run_id = dreaming::cli_run_id(dreaming::now_secs());
    // The engine requires an active PrincipalIds pair. Only enrollment can precede the engine's
    // payload/snapshot verification; begin cannot publish a journal or adopt rows until it verifies.
    let nominated = nominate(&db, &system, &request, allow_new_owner, &run_id).await?;
    let report = if let Some(receipt) = receipt {
        migrate(&db, &system, nominated, receipt, request.page_rows, &run_id).await?
    } else {
        eprintln!("{run_id} owner ready; no legacy ownership migration required");
        Report {
            rows_migrated: 0,
            held_reminders: 0,
        }
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn open_store(path: &Path) -> Result<Database> {
    ensure!(
        path != Path::new(":memory:"),
        "bootstrap-owner requires a persistent data directory"
    );
    // Open as the actual migration writer, never as a probe followed by a racy second open.
    Database::connect_persistent(path).await.map_err(|error| {
        if error.chain().any(|cause| cause.to_string().contains("already locked")) {
            anyhow::anyhow!(
                "alexandria bootstrap-owner: the store at {} is already open, and SurrealKV is single-writer. Stop the alexandria service (or anything else holding that directory) and run this again. No bootstrap ran.",
                path.display()
            )
        } else {
            error
        }
    })
}

async fn allow_enrollment(db: &Database, has_backup: bool) -> Result<bool> {
    // The landed admission API exposes these two reasons as diagnostics, not typed states. Only
    // those known reasons permit going past a refusal; begin still owns the authoritative checks.
    match check_ownership_ready(db.inner()).await {
        Ok(()) => Ok(!has_backup),
        Err(error) if has_backup && error.to_string().starts_with("Legacy ownership migration required") => Ok(true),
        Err(error) if has_backup && error.to_string().starts_with("Ownership migration incomplete") => Ok(false),
        Err(error) if !has_backup => Err(error).context(
            "Cannot bootstrap this store without a ready ownership state. For offline legacy adoption or resume, supply the original --backup-data-dir",
        ),
        Err(error) => Err(error),
    }
}

fn read_receipt(source: &Path, backup: &Path) -> Result<BackupReceipt> {
    let source = source
        .canonicalize()
        .context("Resolve live store directory")?;
    let backup = backup
        .canonicalize()
        .context("Resolve backup receipt directory")?;
    ensure!(
        !source.starts_with(&backup) && !backup.starts_with(&source),
        "Backup receipt directory and live store must not overlap"
    );
    BackupReceipt::read(&backup)
}

async fn nominate(
    db: &Database,
    system: &SystemContext,
    request: &Request,
    allow_new_owner: bool,
    run_id: &str,
) -> Result<PrincipalIds> {
    if let Some(account) = AuthRepo::new(db.inner())
        .account_by_login(&request.login)
        .await?
    {
        let access = AccessRepo::new(db);
        let principal = access
            .local_principal(system, &account.account_id()?)
            .await?;
        let owner = access.authorize(&principal).await?;
        if let Some(path) = &request.password_file {
            let password = read_password(path)?;
            ensure!(
                AuthRepo::password_matches(account.password_hash.as_deref(), &password).await?,
                "The password file does not match the existing account; no password was changed"
            );
        }
        return Ok(PrincipalIds {
            account: owner.account().clone(),
            workspace: owner.workspace().clone(),
        });
    }
    ensure!(
        allow_new_owner,
        "The nominated account does not exist. For a resume use the original --login; for account enrollment on a ready store omit --backup-data-dir"
    );
    let path = request
        .password_file
        .as_deref()
        .context("--password-file is required for a new account")?;
    let password = read_password(path)?;
    let hash = AuthRepo::create_password_digest(&password).await?;
    let operation = OperationId::new(run_id);
    Ok(IdentityRepo::new(db.inner())
        .bootstrap_owner(&request.login, hash.as_str(), &operation)
        .await?)
}

fn read_password(path: &Path) -> Result<String> {
    let metadata = path.metadata().context("Inspect password file")?;
    ensure!(metadata.is_file(), "Password file must be a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "Password file must not grant group or other permissions (use chmod 600)"
        );
    }
    let mut password = String::new();
    std::fs::File::open(path)
        .context("Open password file")?
        .take((MAX_PASSWORD_BYTES + 1) as u64)
        .read_to_string(&mut password)
        .context("Read password file as UTF-8")?;
    ensure!(
        password.len() <= MAX_PASSWORD_BYTES,
        "Password file is too large"
    );
    if password.ends_with('\n') {
        password.pop();
        if password.ends_with('\r') {
            password.pop();
        }
    }
    ensure!(!password.trim().is_empty(), "Password must not be blank");
    Ok(password)
}

async fn migrate(
    db: &Database,
    system: &SystemContext,
    owner: PrincipalIds,
    receipt: BackupReceipt,
    page_rows: usize,
    run_id: &str,
) -> Result<Report> {
    let migration = OwnershipMigration::new(db);
    eprintln!("{run_id} verifying backup receipt and ownership nomination");
    let id = migration.begin(system, owner, receipt).await?;
    for page in 1u64.. {
        let progress = migration.step(&id, page_rows).await?;
        eprintln!("{run_id} page {page} checkpointed");
        if progress.ready_to_verify {
            break;
        }
    }
    let report = migration.verify_and_complete(&id).await?;
    eprintln!("{run_id} ownership migration verified and complete");
    Ok(report)
}
