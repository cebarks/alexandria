//! `alexandria dream`: run one or more of the five dreaming jobs once, on demand.
//!
//! Before this there was no way to ask "run that job now" without editing config. The workaround we
//! actually used was to set all five `*_interval_secs` to `1` (the loader only refuses `0`), restart
//! with `alexandria::dreaming=debug`, and revert — three mutations to a live service to answer one
//! question. This runs the same job bodies the scheduler runs, through the same entry point
//! (`dreaming::Jobs::run_job`), prints one report line per job and the run id its audit rows carry,
//! and exits. There is no second implementation of "run a job" to keep in step with the first.
//!
//! It is operator tooling, not an MCP tool and not a debug-UI route: a write route under `debug/`
//! would break the read-only posture the debug UI's no-auth stance rests on.
//!
//! Two things this command deliberately does **not** do:
//!
//! - **Arm access recording.** `system_config::arm_access_recording` is a boot step, and the stamp
//!   means "this build started counting retrievals here"; a one-shot CLI writing it would make
//!   `appraise`'s fail-closed gate judge memories it has no basis to judge. So on a store that has
//!   never booted this build, `appraise` demotes nothing and says why. That is the correct output,
//!   not a gap to route around.
//! - **Load the embedding model.** None of the five jobs embeds anything — they read stored vectors,
//!   stored heat and stored confidence — so the command neither needs the ~80MB model nor touches the
//!   embedding lock.

use std::path::Path;
use std::sync::Arc;

use alexandria_engine::dreaming::{ALL_JOBS, Job, JobReport};
use alexandria_storage::repos::ClusterRepo;
use alexandria_storage::{Database, schema};
use anyhow::anyhow;

use crate::config::Config;
use crate::dreaming;

const USAGE: &str = "Usage: alexandria dream [--job NAME]... [--max-rows N] | --help";

/// What the operator sees. The five job names and the single-writer constraint are both stated here
/// rather than left to `--help` of some other command, and
/// `the_help_names_every_job_and_the_single_writer_constraint` fails the build if either goes
/// missing — the same guard `summary()` has in `dreaming.rs`.
const HELP: &str = "\
alexandria dream - run dreaming jobs once, on demand

Usage: alexandria dream [--job NAME]... [--max-rows N]
       alexandria dream --help

Runs the jobs named, once each, then exits. With no --job it runs all five. They run in the
scheduler's own order - sweep, cluster, merge, collapse, appraise - whichever order you name
them, and duplicates are ignored. The [dreaming] cadences are neither consulted nor changed:
this answers \"run that job now\" without editing a live service's config and restarting it.

Options:
  --job NAME       all, sweep, cluster, merge, collapse or appraise (repeatable)
  --max-rows N     rows one job may write in this pass, overriding [dreaming]
                   max_rows_per_run for this invocation only. The config file is never
                   written to. Must be 1 or more.
  -h, --help       print this text and exit 0

SINGLE WRITER: the data store is SurrealKV and SurrealKV is single-writer, so only one process
can have it open at a time. This command therefore refuses while the alexandria service (or
another opener) holds it - no job runs, and the exit status is non-zero. Stop the service
first, or point ALEXANDRIA_DATA_DIR at a copy of the data directory and run it against that.

Every row this pass writes to maintenance_log carries the run id run-cli-<epoch>-<pid>, printed
on the last line with the number of rows found under it, so /debug/maintenance?run=<that id>
shows exactly what one invocation did. `sweep` writes no audit rows by design, so a pass whose
only job was sweep legitimately reports zero.";

/// What the command line asked for: print the help, or run this set of jobs.
#[derive(Debug)]
pub(crate) enum Invocation {
    Help,
    Run(Request),
}

/// A parsed `alexandria dream` command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    /// The jobs to run, in [`ALL_JOBS`] order, deduplicated. [`parse`] normalises both, so
    /// [`pass`] never has to re-order and the scheduler's fixed order cannot be bypassed.
    pub(crate) jobs: Vec<Job>,
    /// `--max-rows`, applied to the in-memory [`Config`] for this pass only.
    pub(crate) max_rows: Option<usize>,
}

/// One job's outcome, kept whether it succeeded or not: a job that errors must still produce a line
/// for the operator, and the others in the set still have to run.
#[derive(Debug, Clone)]
pub(crate) enum Entry {
    Ran(JobReport),
    Failed { job: Job, error: String },
}

impl Entry {
    /// The stdout line for this job. For a success it is the report rendering the scheduler also
    /// logs (`JobReport::summary_line`); for a failure it is the same `job=` prefix the loop's warn
    /// line uses.
    pub(crate) fn line(&self) -> String {
        match self {
            Entry::Ran(report) => report.summary_line(),
            Entry::Failed { job, error } => {
                format!("job={} failed: {error}", job.as_str())
            }
        }
    }
}

/// What one pass did, as data. The lines are printed as the jobs finish rather than at the end, so
/// an operator watching a pass over a large corpus is not staring at a silent terminal.
#[derive(Debug)]
pub(crate) struct Outcome {
    pub(crate) entries: Vec<Entry>,
    pub(crate) run_id: String,
    /// Rows in `maintenance_log` carrying `run_id`, read back after the pass through the same
    /// filter `/debug/maintenance?run=` uses. `None` means that read failed — the jobs already ran
    /// and their rows exist, so a failed tally costs the operator the count and nothing more.
    pub(crate) audit_rows: Option<usize>,
}

impl Outcome {
    /// The final line: the id to take to the debug UI, with what is filed under it.
    pub(crate) fn run_id_line(&self) -> String {
        let tally = match self.audit_rows {
            Some(rows) => format!("{rows} audit rows written"),
            None => "audit row count unavailable".to_string(),
        };
        format!(
            "run id {id}: {tally}, see /debug/maintenance?run={id}",
            id = self.run_id
        )
    }

    pub(crate) fn failures(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Failed { .. }))
            .count()
    }
}

/// The names an operator may type: the five jobs, plus `all`.
fn valid_names() -> String {
    let mut names: Vec<&str> = ALL_JOBS.into_iter().map(Job::as_str).collect();
    names.push("all");
    names.join(", ")
}

/// Hand-rolled, like the binary's other subcommands (`main.rs` matches argument slices). There is no
/// clap here and adding one for two flags would be a new dependency for nothing.
fn parse(args: &[&str]) -> anyhow::Result<Invocation> {
    let mut named: Vec<Job> = Vec::new();
    let mut max_rows: Option<usize> = None;
    let mut index = 0;

    while index < args.len() {
        let option = args[index];
        index += 1;
        match option {
            "--help" | "-h" => return Ok(Invocation::Help),
            "--job" => {
                let value = take_value(args, &mut index, option)?;
                if value == "all" {
                    named.extend(ALL_JOBS);
                    continue;
                }
                named.push(Job::from_name(value).ok_or_else(|| {
                    anyhow!(
                        "unknown job `{value}` (valid names: {}). {USAGE}",
                        valid_names()
                    )
                })?);
            }
            "--max-rows" => {
                let value = take_value(args, &mut index, option)?;
                let rows: usize = value.parse().map_err(|_| {
                    anyhow!("`--max-rows {value}` is not a number of rows. {USAGE}")
                })?;
                // The same refusal the config loader makes of `max_rows_per_run = 0`, for the same
                // reason: with a zero bound every job examines nothing and reports success.
                anyhow::ensure!(
                    rows > 0,
                    "`--max-rows 0` makes every job examine nothing and report success. {USAGE}"
                );
                max_rows = Some(rows);
            }
            other if other.starts_with("--job=") || other.starts_with("--max-rows=") => {
                anyhow::bail!(
                    "`{other}` is not accepted: write the option and its value as two arguments, \
                     e.g. `--job sweep`. {USAGE}"
                );
            }
            other => anyhow::bail!("unexpected argument `{other}`. {USAGE}"),
        }
    }

    // Fixed order and one of each, whatever the command line said. Same reasoning as the loop's
    // `due`: a run's effects have to be reproducible, and `--job collapse --job sweep` naming the
    // same work in the opposite order must not do something different.
    let jobs = if named.is_empty() {
        ALL_JOBS.to_vec()
    } else {
        ALL_JOBS
            .iter()
            .copied()
            .filter(|job| named.contains(job))
            .collect()
    };
    Ok(Invocation::Run(Request { jobs, max_rows }))
}

/// The value of an option that takes one, or a usage error naming the option that lost it.
fn take_value<'a>(args: &'a [&str], index: &mut usize, option: &str) -> anyhow::Result<&'a str> {
    if *index < args.len() {
        let value = args[*index];
        *index += 1;
        Ok(value)
    } else {
        anyhow::bail!("`{option}` needs a value. {USAGE}")
    }
}

/// The phrase the pinned 3.3.0 engine uses when the SurrealKV directory is already held. Measured by
/// opening a `tempfile::tempdir()` path twice:
/// `There was a problem with the datastore: Other error: Database at <path>/LOCK is already locked
/// by another process`, with no cause beneath it. Matched on this fragment rather than the whole
/// line because the path is interpolated into it; if a future engine rewords it,
/// `a_held_store_is_refused_by_name_before_any_job_runs` fails rather than letting the refusal
/// degrade into a mystery error.
///
/// A `mem://` store cannot be probed this way, and that asymmetry is worth stating: a second
/// `mem://` handle is a *different* database, so an in-memory connection proves nothing about
/// single-writer behaviour. Only the file-backed path does.
const LOCK_MARKER: &str = "already locked";

/// The one-line refusal an operator acts on: what is holding the store, and the two ways to stop it.
fn busy_message(data_dir: &Path) -> String {
    format!(
        "alexandria dream: the store at {} is already open, and SurrealKV is single-writer. \
         Stop the alexandria service (or anything else holding that directory), or point \
         ALEXANDRIA_DATA_DIR at a copy of it, and run this again. No job ran.",
        data_dir.display()
    )
}

/// Open the configured store, turning "already open" into [`busy_message`] and leaving every other
/// connect failure as it came.
async fn open_store(data_dir: &Path) -> anyhow::Result<Database> {
    Database::connect(data_dir).await.map_err(|e| {
        if e.to_string().contains(LOCK_MARKER) {
            anyhow::anyhow!(busy_message(data_dir))
        } else {
            e
        }
    })
}

/// `alexandria dream`. Loads config, then runs [`pass`].
pub(crate) async fn run(args: &[&str]) -> anyhow::Result<()> {
    let request = match parse(args)? {
        Invocation::Help => {
            println!("{HELP}");
            return Ok(());
        }
        Invocation::Run(request) => request,
    };

    let config = Config::load()?;
    if !config.dreaming.enabled {
        tracing::warn!(
            "[dreaming] enabled = false, so the background scheduler never runs; this pass runs the \
             jobs you named once anyway, and changes nothing about that setting"
        );
    }
    if config.database.data_dir.as_path() == Path::new(":memory:") {
        tracing::warn!(
            "database.data_dir is \":memory:\", which is a fresh empty store per process rather \
             than the running server's data: this pass will examine nothing and write nothing to \
             any corpus you care about. Point ALEXANDRIA_DATA_DIR at the real directory."
        );
    }

    let outcome = pass(&request, config).await?;
    let failed = outcome.failures();
    if failed > 0 {
        anyhow::bail!(
            "alexandria dream: {failed} of {} jobs failed (run id {})",
            outcome.entries.len(),
            outcome.run_id
        );
    }
    Ok(())
}

/// Run the requested jobs once against the store `config` points at.
///
/// Separate from [`run`] so the tests can hand it an explicit [`Config`] pointing at a
/// `tempfile::tempdir()`: `Config::load` reads the operator's real config file, which in a test
/// would mean running housekeeping jobs against their real corpus.
pub(crate) async fn pass(request: &Request, mut config: Config) -> anyhow::Result<Outcome> {
    if let Some(rows) = request.max_rows {
        config.dreaming.max_rows_per_run = rows;
    }

    // First, and therefore before anything is examined or written: if the store is held, this is the
    // point the command stops.
    let db = Arc::new(open_store(&config.database.data_dir).await?);
    schema::migrate(db.inner()).await?;

    let run_id = dreaming::cli_run_id(dreaming::now_secs());
    let jobs = dreaming::Jobs::new(Arc::clone(&db), &config);

    let mut entries = Vec::with_capacity(request.jobs.len());
    for job in &request.jobs {
        let entry = match jobs.run_job(*job, &run_id).await {
            Ok(report) => Entry::Ran(report),
            // The loop's own rule, kept: one job's failure is that job's news, not a reason to
            // abandon the rest of the set the operator asked for.
            Err(e) => {
                tracing::warn!(job = job.as_str(), "dreaming job failed: {e}");
                Entry::Failed {
                    job: *job,
                    error: e.to_string(),
                }
            }
        };
        println!("{}", entry.line());
        entries.push(entry);
    }

    // Read back through the same filter the debug UI's `?run=` applies, so the tally and the page
    // an operator is sent to cannot disagree about what one pass wrote.
    let audit_rows = match ClusterRepo::new(db.inner())
        .count_maintenance_logs(Some(&run_id))
        .await
    {
        Ok(rows) => Some(rows),
        Err(e) => {
            tracing::warn!("cannot count the audit rows under {run_id}: {e}");
            None
        }
    };
    let outcome = Outcome {
        entries,
        run_id,
        audit_rows,
    };
    println!("{}", outcome.run_id_line());
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alexandria_storage::repos::MemoryRepo;
    use std::time::Duration;

    /// Parse, or panic with the message the operator would have seen. Every test below builds its
    /// request this way rather than by hand, so the ordering and dedupe normalisation is exercised
    /// on the way into the job-running tests instead of assumed by them.
    fn request(args: &[&str]) -> Request {
        match parse(args) {
            Ok(Invocation::Run(request)) => request,
            Ok(Invocation::Help) => panic!("`{args:?}` is not a help request"),
            Err(e) => panic!("`{args:?}` must parse: {e}"),
        }
    }

    /// Jobs as typed names, for the assertions below.
    fn names(request: &Request) -> Vec<&'static str> {
        request.jobs.iter().map(|job| job.as_str()).collect()
    }

    #[test]
    fn with_no_job_named_it_runs_all_five() {
        assert_eq!(
            names(&request(&[])),
            vec!["sweep", "cluster", "merge", "collapse", "appraise"]
        );
        assert_eq!(request(&[]).max_rows, None);
    }

    /// The point of `all` is that it means everything; the point of normalising is that
    /// `--job collapse --job sweep` and `--job sweep --job collapse` are the same command.
    #[test]
    fn jobs_are_ordered_by_the_schedulers_order_and_duplicates_collapse() {
        assert_eq!(
            names(&request(&["--job", "collapse", "--job", "sweep"])),
            vec!["sweep", "collapse"]
        );
        assert_eq!(
            names(&request(&["--job", "merge", "--job", "merge"])),
            vec!["merge"],
            "a repeated job runs once, not twice"
        );
        assert_eq!(
            names(&request(&["--job", "all", "--job", "sweep"])),
            vec!["sweep", "cluster", "merge", "collapse", "appraise"],
            "`all` plus anything else is still everything"
        );
    }

    #[test]
    fn an_unknown_job_is_a_usage_error_naming_the_valid_names() {
        let error = parse(&["--job", "spike"]).unwrap_err().to_string();
        assert!(error.contains("unknown job `spike`"), "{error}");
        for name in ALL_JOBS.into_iter().map(Job::as_str) {
            assert!(error.contains(name), "{name} must be listed: {error}");
        }
        assert!(error.contains("all"), "`all` must be listed: {error}");
        assert!(error.contains("Usage:"), "{error}");

        // Exact spelling: the names written to `maintenance_log.job` are these, byte for byte, so
        // the CLI does not quietly accept a spelling the audit log would never show.
        for wrong in ["SWEEP", "Sweep", " sweep", "sweep "] {
            assert!(
                parse(&["--job", wrong]).is_err(),
                "`{wrong}` must be refused, not trimmed or case-folded"
            );
        }
    }

    #[test]
    fn a_missing_value_or_the_wrong_flag_form_is_a_usage_error() {
        assert!(
            parse(&["--job"])
                .unwrap_err()
                .to_string()
                .contains("needs a value")
        );
        assert!(
            parse(&["--max-rows"])
                .unwrap_err()
                .to_string()
                .contains("needs a value")
        );
        let spaced = parse(&["--job=sweep"]).unwrap_err().to_string();
        assert!(spaced.contains("--job sweep"), "{spaced}");
        assert!(
            parse(&["-v"])
                .unwrap_err()
                .to_string()
                .contains("unexpected argument")
        );
    }

    #[test]
    fn max_rows_is_parsed_and_zero_and_garbage_are_refused() {
        assert_eq!(request(&["--max-rows", "5000"]).max_rows, Some(5000));
        assert!(
            parse(&["--max-rows", "0"])
                .unwrap_err()
                .to_string()
                .contains("examine nothing")
        );
        assert!(
            parse(&["--max-rows", "many"])
                .unwrap_err()
                .to_string()
                .contains("not a number")
        );
    }

    #[test]
    fn help_short_circuits_and_is_not_a_request() {
        for args in [["--help"], ["-h"]] {
            assert!(matches!(parse(&args).unwrap(), Invocation::Help));
        }
    }

    /// `--help` is the only place an operator reads about the single-writer constraint before
    /// hitting it, and the job list is the only place they learn the names. Both are assertions
    /// here because both are prose in a string literal that nothing else compiles against.
    #[test]
    fn the_help_names_every_job_and_the_single_writer_constraint() {
        for name in ALL_JOBS.into_iter().map(Job::as_str) {
            assert!(HELP.contains(name), "help must name {name}");
        }
        assert!(HELP.contains("all"), "help must offer `all`");
        assert!(
            HELP.contains("SINGLE WRITER") && HELP.contains("single-writer"),
            "help must state the constraint the refusal exists for"
        );
        assert!(HELP.contains("--max-rows"), "help must document --max-rows");
        assert!(
            HELP.contains("run-cli-"),
            "help must state the run id shape"
        );
    }

    /// The store is opened before any job is constructed, so a held directory produces the refusal
    /// and no work: `count_maintenance_logs(None) == 0` under a fixture that *would* have produced
    /// a row is the proof, and the second half of this test supplies that proof by releasing the
    /// lock and running the same request successfully.
    #[tokio::test]
    async fn a_held_store_is_refused_by_name_before_any_job_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let held = Database::connect_persistent(&path).await.unwrap();
        schema::migrate(held.inner()).await.unwrap();
        let memories = MemoryRepo::new(held.inner());
        for _ in 0..2 {
            memories
                .create_fact("the same text twice", 0.5, &[0.1, 0.2], &[])
                .await
                .unwrap();
        }

        // The marker the classifier keys on, checked against the engine's own words rather than
        // against this file's memory of them. (`Database` has no `Debug`, so the failure is matched
        // rather than unwrapped.)
        let raw = match Database::connect_persistent(&path).await {
            Ok(_) => panic!(
                "a second open of a held SurrealKV path must fail, or there is no lock to refuse"
            ),
            Err(e) => e.to_string(),
        };
        assert!(
            raw.contains(LOCK_MARKER),
            "the engine no longer reports {LOCK_MARKER:?}, so the refusal below is no longer \
             detecting a lock: {raw}"
        );

        let mut config = Config::default();
        config.database.data_dir = path.clone();
        let collapse = request(&["--job", "collapse"]);
        let error = pass(&collapse, config.clone())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("already open"), "{error}");
        assert!(
            error.contains(&path.display().to_string()),
            "the refusal must name the directory it could not open: {error}"
        );
        assert!(error.contains("No job ran"), "{error}");
        assert_eq!(
            ClusterRepo::new(held.inner())
                .count_maintenance_logs(None)
                .await
                .unwrap(),
            0,
            "refused means refused: nothing was logged"
        );

        drop(held);
        // SurrealKV releases its lock on drop; the same pause `persistence_test.rs` uses.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let outcome = pass(&collapse, config).await.unwrap();
        assert_eq!(
            outcome.audit_rows,
            Some(1),
            "positive control: the same directory, released, does collapse those two facts"
        );
    }

    /// One report line per job, in scheduler order, then the run id line — and the audit row is
    /// reachable under that id through the very filter the printed URL uses.
    #[tokio::test]
    async fn a_pass_prints_one_line_per_job_and_ends_with_its_run_id() {
        let dir = tempfile::tempdir().unwrap();
        let config = seeded_store(dir.path(), &["duplicated line", "duplicated line"]).await;
        // Deliberately out of order: sweep must be rendered first whatever was typed.
        let collapse_first = request(&["--job", "collapse", "--job", "sweep"]);

        let outcome = pass(&collapse_first, config).await.unwrap();
        let lines: Vec<String> = outcome.entries.iter().map(Entry::line).collect();
        assert_eq!(
            lines,
            vec![
                // `create_fact` writes no heat row, so the sweep has nothing to materialise.
                "job=sweep examined=0 acted=0 skipped=0".to_string(),
                "job=collapse examined=2 acted=1 skipped=0".to_string(),
            ],
            "one line per job, in the scheduler's order"
        );
        assert!(
            outcome.run_id.starts_with("run-cli-"),
            "the id must be visibly a manual pass: {}",
            outcome.run_id
        );
        assert_eq!(
            outcome.audit_rows,
            Some(1),
            "the collapse row is filed under the id the operator is told"
        );
        let id = &outcome.run_id;
        assert_eq!(
            outcome.run_id_line(),
            format!("run id {id}: 1 audit rows written, see /debug/maintenance?run={id}")
        );
    }

    /// `--max-rows` bounds the writes of the pass it is given, and nothing else. Three identical
    /// rows collapse to two writes at the default bound and to one at `--max-rows 1`; the second
    /// store proves the default still applies afterwards, because the override lives in this
    /// process's `Config` and is never written to a config file.
    ///
    /// Two stores rather than two passes over one: a CLI process runs exactly one pass, so the run
    /// id carries a pid and not a per-process counter, and two passes in the same test process
    /// inside the same second would share one id and one tally. The drain-across-runs behaviour of
    /// a bounded pass is already pinned against the scheduler by
    /// `collapse_honours_max_rows_per_run_and_finishes_on_the_next_run`.
    #[tokio::test]
    async fn max_rows_bounds_the_writes_of_the_pass_it_is_given() {
        let bounded = tempfile::tempdir().unwrap();
        let three = ["three of a kind"; 3];
        let config = seeded_store(bounded.path(), &three).await;

        let outcome = pass(&request(&["--job", "collapse", "--max-rows", "1"]), config)
            .await
            .unwrap();
        assert_eq!(
            outcome.entries[0].line(),
            "job=collapse examined=3 acted=1 skipped=0",
            "one write, whatever the read"
        );
        assert_eq!(
            outcome.audit_rows,
            Some(1),
            "and one audit row under the run id"
        );

        let unbounded = tempfile::tempdir().unwrap();
        let outcome = pass(
            &request(&["--job", "collapse"]),
            seeded_store(unbounded.path(), &three).await,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome.entries[0].line(),
            "job=collapse examined=3 acted=2 skipped=0",
            "the same corpus without the flag collapses both duplicates"
        );
    }

    /// An empty store is the common case on a fresh install, and every job must read it as a
    /// successful pass that did nothing. Including `appraise`, which on a store that has never
    /// booted this build finds access recording unarmed — the command does not arm it, and that
    /// refusal is the correct output rather than an error to report.
    #[tokio::test]
    async fn a_pass_over_an_empty_store_is_five_quiet_successes_and_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        let config = seeded_store(dir.path(), &[]).await;

        let outcome = pass(&request(&[]), config).await.unwrap();
        let lines: Vec<String> = outcome.entries.iter().map(Entry::line).collect();
        assert_eq!(
            lines,
            vec![
                "job=sweep examined=0 acted=0 skipped=0".to_string(),
                "job=cluster examined=0 acted=0 skipped=0".to_string(),
                "job=merge examined=0 acted=0 skipped=0".to_string(),
                "job=collapse examined=0 acted=0 skipped=0".to_string(),
                "job=appraise examined=0 acted=0 skipped=0".to_string(),
            ]
        );
        assert_eq!(outcome.failures(), 0);
        assert_eq!(
            outcome.audit_rows,
            Some(0),
            "nothing acted on means nothing logged, and the pass says so rather than going quiet"
        );
    }

    /// Open `dir`, apply the schema, seed `contents` as live facts, then **release the store** so the
    /// command under test can open it — SurrealKV is single-writer, so a fixture that held its own
    /// handle would be handing every test the failure mode one of them exists to report.
    async fn seeded_store(dir: &Path, contents: &[&str]) -> Config {
        let path = dir.join("data");
        let db = Database::connect_persistent(&path).await.unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let memories = MemoryRepo::new(db.inner());
        for content in contents {
            memories
                .create_fact(content, 0.5, &[0.1, 0.2], &[])
                .await
                .unwrap();
        }
        drop(db);
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut config = Config::default();
        config.database.data_dir = path;
        config
    }
}
