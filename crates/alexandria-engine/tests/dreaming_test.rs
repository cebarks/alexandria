//! Due-time arithmetic for the dreaming scheduler (GitHub issue #43). Pure: no database, no clock.
//!
//! These are the tests that make "one loop, five independent due-times" safe to write, because the
//! failure mode of a scheduler is silence: a job that quietly stops running looks healthy for days.

use alexandria_engine::dreaming::collapse::{Candidate, UNKNOWN_CREATED_AT, duplicate_groups};
use alexandria_engine::dreaming::{
    ALL_JOBS, DEFAULT_COLD_HEAT_FLOOR, DEFAULT_COLLAPSE_INTERVAL_SECS,
    DEFAULT_DEMOTE_CONFIDENCE_CEILING, DEMOTED_CONFIDENCE, Intervals, Job, JobTiming,
    should_demote,
};

fn candidate(id: &str, content: &str, confidence: f64, created_at: i64) -> Candidate {
    Candidate {
        id: id.to_string(),
        content: content.to_string(),
        confidence,
        created_at,
    }
}

fn timing(job: Job, interval: u64, last_run: Option<u64>) -> JobTiming {
    JobTiming {
        job,
        interval_secs: interval,
        last_run,
    }
}

#[test]
fn a_job_is_due_once_its_interval_has_elapsed() {
    let timing = timing(Job::Sweep, 3600, Some(1_000));
    assert!(!timing.is_due(4_599), "not yet");
    assert!(timing.is_due(4_600), "due at exactly the boundary");
    assert!(timing.is_due(9_000), "and still due well past it");
}

/// Which clock gets stored is the entire difference between a slow tick and a spin.
///
/// The loop stamps each job with the clock read *after* it returned, so `last_run` is a completion
/// time and the interval measures rest. Stamped with the tick's start instead, a job that outlived
/// its own interval is due again the moment the loop recomputes: `remaining_secs` saturates to 0,
/// `sleep(0)` returns immediately, and the job runs back-to-back with no rest for as long as the
/// overrun lasts — the hot loop the binary refuses to *start* on for a zero interval, arrived at by
/// a different road. And because one tick's due set shared that single stale stamp, a slow job also
/// cancelled the wait of every shorter interval in the same tick.
#[test]
fn a_job_that_overran_its_interval_waits_a_full_interval_from_completion() {
    let interval = 300;
    let tick_started = 1_000;
    let finished = 1_400;

    let stale = timing(Job::Cluster, interval, Some(tick_started));
    assert!(
        stale.is_due(finished),
        "an overrunning job marked at the tick's start is due again at once"
    );
    assert_eq!(
        stale.remaining_secs(finished),
        0,
        "so the loop sleeps nothing and runs it back-to-back"
    );

    let current = timing(Job::Cluster, interval, Some(finished));
    assert!(
        !current.is_due(finished),
        "a completion stamp measures rest from the end of the work"
    );
    assert_eq!(current.remaining_secs(finished), interval);
}

/// Catch-up is one run, not one per missed interval. A process down for a day runs the daily job
/// once when it comes back — the same lazy-due discipline the reminder scheduler uses, where a
/// missed tick costs latency and never correctness.
#[test]
fn a_missed_interval_catches_up_with_a_single_run() {
    let timing = timing(Job::Collapse, DEFAULT_COLLAPSE_INTERVAL_SECS, Some(0));
    assert!(timing.is_due(DEFAULT_COLLAPSE_INTERVAL_SECS * 24));

    let schedule = Intervals::default().initial_schedule(0);
    let due = schedule.due(DEFAULT_COLLAPSE_INTERVAL_SECS * 24);
    assert_eq!(
        due.iter().filter(|j| **j == Job::Collapse).count(),
        1,
        "a day of downtime must queue Collapse exactly once, not 24 times: {due:?}"
    );
}

#[test]
fn the_boot_schedule_runs_maintenance_immediately_and_defers_the_daily_jobs() {
    let schedule = Intervals::default().initial_schedule(100_000);
    let due = schedule.due(100_000);
    assert_eq!(
        due,
        vec![Job::Cluster, Job::Merge],
        "Cluster and Merge preserve today's `tokio::time::interval` first-tick behaviour; the \
         sweep and the two daily passes wait an interval so a supervisor restart does not scan \
         the corpus on every boot"
    );

    let later = 100_000 + DEFAULT_COLLAPSE_INTERVAL_SECS;
    let due = schedule.due(later);
    assert!(
        due.contains(&Job::Collapse) && due.contains(&Job::Appraise),
        "the deferred jobs must arrive on their own cadence: {due:?}"
    );
}

/// One job's due-time must not be disturbed by another running. That is what lets a poisoned row
/// fail the sweep without also stopping cluster maintenance.
#[test]
fn marking_one_job_running_leaves_the_others_alone() {
    let mut schedule = Intervals::default().initial_schedule(0);
    for job in ALL_JOBS {
        schedule.mark_run(job, 0);
    }
    let now = 400;
    assert_eq!(
        schedule.due(now),
        vec![Job::Cluster, Job::Merge],
        "300s elapsed"
    );

    schedule.mark_run(Job::Cluster, now);
    assert_eq!(
        schedule.due(now),
        vec![Job::Merge],
        "running Cluster must not re-queue it and must not reset Merge's clock"
    );
    assert_eq!(
        schedule.due(700),
        vec![Job::Cluster, Job::Merge],
        "both come round again on their own cadence"
    );
}

#[test]
fn due_jobs_come_back_in_declared_order_regardless_of_cadence() {
    let schedule = Intervals::default().initial_schedule(0);
    let due = schedule.due(10_000);
    let positions: Vec<usize> = due
        .iter()
        .map(|job| ALL_JOBS.iter().position(|j| j == job).unwrap())
        .collect();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "a tick must run jobs in the fixed order, so its effects are reproducible: {due:?}"
    );
}

/// A zero interval is a config footgun — it means "due on every tick", which turns a daily pass
/// into a hot loop. The engine states the behaviour so the binary's refusal to start on one is a
/// decision rather than an accident.
#[test]
fn a_zero_interval_is_due_every_tick() {
    let timing = timing(Job::Sweep, 0, Some(5_000));
    assert!(timing.is_due(5_000));
    assert!(timing.is_due(5_001));
}

#[test]
fn the_loop_sleeps_until_the_soonest_job_is_due() {
    let schedule = Intervals::default().initial_schedule(0);
    // Cluster and Merge have no last_run, so they are due immediately and the loop must not sleep.
    assert_eq!(schedule.next_wait_secs(0), 0);

    let mut schedule = Intervals {
        cluster_secs: 300,
        merge_secs: 300,
        sweep_secs: 3600,
        collapse_secs: 86_400,
        appraise_secs: 86_400,
    }
    .initial_schedule(0);
    for job in ALL_JOBS {
        schedule.mark_run(job, 0);
    }
    assert_eq!(
        schedule.next_wait_secs(0),
        300,
        "the shortest interval wins"
    );
    assert_eq!(schedule.next_wait_secs(250), 50);
    assert_eq!(schedule.next_wait_secs(300), 0);
    assert_eq!(
        schedule.next_wait_secs(100_000),
        0,
        "everything overdue sleeps zero, and the tick runs the whole due set"
    );
}

#[test]
fn job_names_match_the_audit_allowlist() {
    let names: Vec<&str> = ALL_JOBS.iter().map(|job| job.as_str()).collect();
    assert_eq!(
        names,
        vec!["sweep", "cluster", "merge", "collapse", "appraise"],
        "`maintenance_log.job` values are asserted against a closed list in v008; renaming one \
         here without editing the schema turns every write into a rejected row"
    );
}

/// `alexandria dream --job` accepts these and nothing else, and its refusal message is built by
/// listing them, so both halves of the CLI's vocabulary come from one source.
#[test]
fn a_job_name_round_trips_through_the_spelling_an_operator_types() {
    for job in ALL_JOBS {
        assert_eq!(Job::from_name(job.as_str()), Some(job), "{}", job.as_str());
    }

    // Exact match only: `maintenance_log.job` carries these bytes, so an accept-and-lowercase path
    // would let a command line name a spelling the audit log never shows.
    for not_a_job in [
        "SWEEP", "Sweep", " sweep", "sweep ", "", "all", "dream", "sweeps",
    ] {
        assert_eq!(
            Job::from_name(not_a_job),
            None,
            "`{not_a_job}` is not a job name"
        );
    }
}

/// The line `alexandria dream` prints per job, in the field order the scheduler logs them. Pinned
/// here because the engine owns the counters, so this is the one place that shape is a value a
/// reader can check.
#[test]
fn a_report_renders_as_the_three_counters_in_the_logged_order() {
    let mut report = alexandria_engine::dreaming::JobReport::new(Job::Appraise);
    report.examined = 12;
    report.acted = 3;
    assert_eq!(
        report.summary_line(),
        "job=appraise examined=12 acted=3 skipped=0",
        "skipped is rendered even at zero, so the line has one shape whatever the job did"
    );

    report.skipped = 9;
    assert_eq!(
        report.summary_line(),
        "job=appraise examined=12 acted=3 skipped=9"
    );
}

#[test]
fn every_job_has_an_interval_and_reports_start_empty() {
    let intervals = Intervals::default();
    for job in ALL_JOBS {
        assert!(
            intervals.interval_for(job) > 0,
            "{job:?} must default to a real interval, not 0"
        );
    }
    let report = alexandria_engine::dreaming::JobReport::new(Job::Sweep);
    assert_eq!((report.examined, report.acted), (0, 0));
}

/// The demote rule's truth table. Every condition is necessary, so each is tested by relaxing it
/// alone and asserting nothing is demoted.
#[test]
fn demote_requires_cold_unused_and_low_confidence_together() {
    let (floor, ceiling) = (DEFAULT_COLD_HEAT_FLOOR, DEFAULT_DEMOTE_CONFIDENCE_CEILING);
    assert!(should_demote(0.0, 0, ceiling, floor, ceiling));
    assert!(
        should_demote(floor, 0, ceiling, floor, ceiling),
        "boundary: heat exactly at the floor counts as cold"
    );

    assert!(
        !should_demote(0.0, 1, ceiling, floor, ceiling),
        "one access is enough to exempt"
    );
    assert!(
        !should_demote(floor * 1.01, 0, ceiling, floor, ceiling),
        "warm is exempt"
    );
    assert!(
        !should_demote(0.0, 0, ceiling * 1.01, floor, ceiling),
        "above the confidence ceiling is exempt"
    );
}

/// The floor and the ceiling must actually govern the decision. A config key whose value no code
/// reads is the exact mistake `[heat] spacing_halflife_secs` made before it was split, so the test
/// takes the knobs as arguments rather than letting the predicate reach for the constants.
#[test]
fn the_demote_knobs_govern_the_decision() {
    assert!(!should_demote(0.4, 0, 0.5, 0.05, 0.5));
    assert!(
        should_demote(0.4, 0, 0.5, 0.5, 0.5),
        "a higher floor demotes it"
    );
    assert!(
        !should_demote(0.4, 0, 0.5, 0.5, 0.4),
        "a lower ceiling exempts it"
    );
}

/// Demoting must not re-arm itself. `Appraise` runs daily, and a rule that matches its own output
/// would rewrite the same confidence, log the same audit row and report the same "acted" count
/// forever — which reads like progress in a trace log while doing nothing new. Exempting the
/// demoted value puts the guarantee in the predicate instead of in a job-side guard that a second
/// caller would have to remember.
#[test]
fn a_demoted_memory_cannot_be_demoted_further() {
    let (floor, ceiling) = (DEFAULT_COLD_HEAT_FLOOR, DEFAULT_DEMOTE_CONFIDENCE_CEILING);
    assert!(
        DEMOTED_CONFIDENCE < ceiling,
        "the demoted value must sit below the ceiling, or demotion would be a no-op"
    );
    assert!(
        !should_demote(0.0, 0, DEMOTED_CONFIDENCE, floor, ceiling),
        "already demoted: the second pass must leave it alone"
    );
    assert!(
        should_demote(0.0, 0, DEMOTED_CONFIDENCE * 1.01, floor, ceiling),
        "anything above the demoted value is still eligible"
    );
}

/// Only byte-identical content groups. A one-character difference — a trailing space, a different
/// case — is a judgement call that belongs to #38's judge, not to a background job that soft-deletes
/// rows on a threshold nobody chose.
#[test]
fn only_byte_identical_content_collapses() {
    assert!(
        duplicate_groups(&[
            candidate("fact:a", "the token rotates", 0.5, 100),
            candidate("fact:b", "the token rotates ", 0.5, 200),
            candidate("fact:c", "The token rotates", 0.5, 300),
        ])
        .is_empty(),
        "near-identical is not identical"
    );
    assert_eq!(
        duplicate_groups(&[
            candidate("fact:a", "the token rotates", 0.5, 100),
            candidate("fact:b", "the token rotates", 0.5, 200),
        ])
        .len(),
        1
    );
}

#[test]
fn singletons_produce_no_group() {
    assert!(
        duplicate_groups(&[
            candidate("fact:a", "one", 0.9, 100),
            candidate("fact:b", "two", 0.1, 200),
            candidate("fact:c", "three", 0.5, 300),
        ])
        .is_empty(),
        "a group of one would have the caller soft-delete nothing while logging that it acted"
    );
    assert!(
        duplicate_groups(&[]).is_empty(),
        "and neither does an empty corpus"
    );
}

#[test]
fn the_survivor_is_the_most_confident() {
    let groups = duplicate_groups(&[
        candidate("fact:low", "same", 0.2, 100),
        candidate("fact:high", "same", 0.9, 500),
        candidate("fact:mid", "same", 0.5, 300),
    ]);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].survivor, "fact:high");
    assert_eq!(
        groups[0].collapsed,
        vec!["fact:low", "fact:mid"],
        "collapsed ids sort, so one run's audit rows come out in a stable order"
    );
}

#[test]
fn a_confidence_tie_goes_to_the_oldest() {
    let groups = duplicate_groups(&[
        candidate("fact:new", "same", 0.5, 900),
        candidate("fact:old", "same", 0.5, 100),
    ]);
    assert_eq!(
        groups[0].survivor, "fact:old",
        "the row that has been around longest is the one other memories may already reference"
    );
}

/// A row nobody can date must not win survivorship over one that can be. The survivor is the row
/// that stays retrievable, so promoting the undateable one is the worse of the two errors.
#[test]
fn an_undateable_row_loses_the_tie() {
    let groups = duplicate_groups(&[
        candidate("fact:aaa", "same", 0.5, UNKNOWN_CREATED_AT),
        candidate("fact:zzz", "same", 0.5, 100),
    ]);
    assert_eq!(groups[0].survivor, "fact:zzz");
}

/// The id tiebreak is what makes the function total. Without it, two rows equal on confidence and
/// timestamp would be ordered by HashMap iteration, and the same corpus could collapse in the
/// opposite direction on the next run — an audit trail that contradicts itself.
#[test]
fn a_total_tie_breaks_on_id_and_never_flips() {
    let rows = vec![
        candidate("fact:bbb", "same", 0.5, 100),
        candidate("fact:aaa", "same", 0.5, 100),
    ];
    let forward = duplicate_groups(&rows);
    let mut backward = rows.clone();
    backward.reverse();
    assert_eq!(forward, duplicate_groups(&backward));
    assert_eq!(forward[0].survivor, "fact:aaa");
}

/// The read that feeds this is a database query whose row order carries no guarantee, so the grouping
/// has to be a function of the input *set* rather than of its order.
#[test]
fn grouping_is_independent_of_input_order() {
    let rows = vec![
        candidate("fact:a", "alpha", 0.5, 100),
        candidate("fact:b", "alpha", 0.7, 200),
        candidate("fact:c", "beta", 0.5, 300),
        candidate("fact:d", "beta", 0.5, 400),
        candidate("fact:e", "gamma", 0.5, 500),
    ];
    let expected = duplicate_groups(&rows);
    for rotation in 1..rows.len() {
        let mut rotated = rows[rotation..].to_vec();
        rotated.extend_from_slice(&rows[..rotation]);
        assert_eq!(
            duplicate_groups(&rotated),
            expected,
            "rotation by {rotation} changed the result"
        );
    }
    assert_eq!(
        expected.len(),
        2,
        "alpha and beta each collapse; gamma is a singleton"
    );
    assert_eq!(
        expected
            .iter()
            .map(|group| group.survivor.as_str())
            .collect::<Vec<_>>(),
        vec!["fact:b", "fact:c"],
        "groups come back sorted by survivor id"
    );
}
