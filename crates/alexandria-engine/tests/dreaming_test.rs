//! Due-time arithmetic for the dreaming scheduler (GitHub issue #43). Pure: no database, no clock.
//!
//! These are the tests that make "one loop, five independent due-times" safe to write, because the
//! failure mode of a scheduler is silence: a job that quietly stops running looks healthy for days.

use alexandria_engine::dreaming::{
    ALL_JOBS, DEFAULT_COLD_HEAT_FLOOR, DEFAULT_COLLAPSE_INTERVAL_SECS,
    DEFAULT_DEMOTE_CONFIDENCE_CEILING, DEMOTED_CONFIDENCE, Intervals, Job, JobTiming,
    should_demote,
};

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
