//! Task 8 — the server-owned project identity normalization, as pure behavioral tests.
//!
//! The rule these tests hold is that **one repository is one project**, and that the server, not the
//! client and not a heuristic, decides what "the same repository" means. Two spellings of one remote
//! (an SSH `git@host:org/repo.git` and an HTTPS `https://host/org/repo`) must arrive at one identity,
//! because a client's choice of transport is not a fact about the code. And two identities that differ
//! in any *meaningful* way — a different port, a different path, a different host — must stay apart,
//! because silently merging them would move one project's memories into another's partition.
//!
//! Nothing here touches the database: the engine owns the algorithm, storage owns the alias rows, and
//! the split is the point. The tests run against the pinned behavior, so a change to
//! `alexandria_storage::schema::v009`'s alias assertions cannot quietly invalidate one of them — the
//! workspace-qualified uniqueness itself is pinned in storage's `identity_schema_test`.

use alexandria_engine::project_identity::{
    CanonicalRepoKey, ProjectIdentityError, RemoteTransport, SanitizedRemote, normalize_remote,
};

fn remote(
    host: &str,
    port: Option<u16>,
    path: &str,
    transport: RemoteTransport,
) -> SanitizedRemote {
    SanitizedRemote {
        host: host.to_string(),
        port,
        path: path.to_string(),
        transport,
    }
}

fn ssh(host: &str, path: &str) -> SanitizedRemote {
    remote(host, None, path, RemoteTransport::Ssh)
}

fn https(host: &str, path: &str) -> SanitizedRemote {
    remote(host, None, path, RemoteTransport::Https)
}

fn key(host: &str, port: Option<u16>, path: &str) -> CanonicalRepoKey {
    CanonicalRepoKey {
        host: host.to_string(),
        port,
        path: path.to_string(),
    }
}

/// The reason this equivalence is load-bearing rather than cosmetic: both spellings appear in the wild
/// for one repository (the same `git remote -v` prints an ssh URL to a human and an https URL to a CI
/// token), and if they normalized apart the same worktree would accumulate one project per transport —
/// the "unrelated forks" collapse the design forbids, run in reverse.
#[test]
fn ssh_and_https_notations_of_one_repo_normalize_alike() {
    let ssh_form = normalize_remote(&ssh("github.com", "anten/alexandria")).expect("ssh form");
    let https_form =
        normalize_remote(&https("github.com", "/anten/alexandria")).expect("https form");
    assert_eq!(
        ssh_form, https_form,
        "transport is not part of a repository's identity: {ssh_form:?} vs {https_form:?}"
    );
    // The trailing `.git` is a spelling too, and the leading slash of an URL path is one.
    let dot_git = normalize_remote(&remote(
        "github.com",
        None,
        "/anten/alexandria.git",
        RemoteTransport::Https,
    ))
    .expect("https with .git");
    assert_eq!(dot_git, ssh_form, "`.git` must not fork an identity");
    assert_eq!(
        ssh_form,
        key("github.com", None, "anten/alexandria"),
        "the canonical rendering is host/path with neither the transport, the `.git` nor a leading slash"
    );
}

/// Standard ports are not identities either — but a non-default one is, because it names a different
/// server process. This is the distinction a client cannot get wrong on the server's behalf.
#[test]
fn nondefault_ports_stay_distinct_from_the_default() {
    assert_eq!(
        normalize_remote(&remote(
            "git.example.com",
            Some(22),
            "org/repo",
            RemoteTransport::Ssh
        ))
        .expect("ssh 22"),
        normalize_remote(&ssh("git.example.com", "org/repo")).expect("ssh unport"),
        "SSH's standard port must normalize away"
    );
    assert_eq!(
        normalize_remote(&remote(
            "git.example.com",
            Some(443),
            "org/repo",
            RemoteTransport::Https
        ))
        .expect("https 443"),
        normalize_remote(&https("git.example.com", "org/repo")).expect("https unport"),
        "HTTPS's standard port must normalize away"
    );
    // A port that is standard for the *other* transport is still a port: `host:22` over HTTPS is not
    // the same server as `host` over HTTPS, and folding it would merge two different services.
    let ssh_22 =
        normalize_remote(&remote("h", Some(22), "p", RemoteTransport::Ssh)).expect("ssh 22");
    let https_22 =
        normalize_remote(&remote("h", Some(22), "p", RemoteTransport::Https)).expect("https 22");
    assert_eq!(ssh_22, key("h", None, "p"), "22 is SSH's default");
    assert_eq!(
        https_22,
        key("h", Some(22), "p"),
        "22 is not HTTPS's default, so it has to survive"
    );
    assert_ne!(
        normalize_remote(&remote("h", Some(8443), "p", RemoteTransport::Https)).expect("8443"),
        normalize_remote(&https("h", "p")).expect("default"),
        "a non-default port collapsed into the default would merge two servers"
    );
    assert_eq!(
        normalize_remote(&remote("h", Some(0), "p", RemoteTransport::Https)),
        Err(ProjectIdentityError::InvalidPort),
        "port 0 is not a port"
    );
}

/// Host case is a typo; path case is content.
///
/// One host can be spelled any way and still resolve, while `Org/Repo` and `org/repo` are different
/// repositories on a case-sensitive server — merging *those* is the "collapse unrelated … path
/// identities" the design rules out. The consequence of v009's lowercase `normalized_alias` assertion
/// is pinned by the second half: the identity keeps the case, the workspace-qualified alias cannot
/// distinguish it. See `CanonicalRepoKey::to_alias`.
#[test]
fn path_case_is_preserved_and_host_case_is_not() {
    let mixed = normalize_remote(&https("GitHub.com", "Org/Repo")).expect("mixed case");
    assert_eq!(
        mixed,
        key("github.com", None, "Org/Repo"),
        "host lowercases, path keeps its case: {mixed:?}"
    );
    let padded = normalize_remote(&https("  github.com  ", "Org/Repo")).expect("padded host");
    assert_eq!(
        padded, mixed,
        "surrounding spaces and host case are spellings; neither touches the path"
    );
    let lowered = normalize_remote(&https("github.com", "org/repo")).expect("lower path");
    assert_eq!(lowered, key("github.com", None, "org/repo"));
    assert_ne!(
        mixed, lowered,
        "path case is part of the identity, so normalizing must not fold it away"
    );
    // …and the alias, which the unique index is built on, is the case-folded rendering of both.
    assert_eq!(
        mixed.to_alias(),
        lowered.to_alias(),
        "v009 asserts a stored alias equals its own lowercase, so one row can carry both spellings"
    );
}

/// One suffix, not more. `repo.git.git` is a repository called `repo.git`, and stripping twice would
/// rename it.
#[test]
fn one_trailing_git_suffix_is_stripped() {
    assert_eq!(
        normalize_remote(&https("h", "a/repo.git")).expect("one"),
        key("h", None, "a/repo")
    );
    assert_eq!(
        normalize_remote(&https("h", "a/repo.git.git")).expect("two"),
        key("h", None, "a/repo.git"),
        "only the trailing `.git` is a spelling; a second one is part of the name"
    );
    assert_eq!(
        normalize_remote(&https("h", "a/repo.git/")).expect("trailing slash"),
        key("h", None, "a/repo"),
        "a trailing slash is a spelling too"
    );
    assert!(
        normalize_remote(&https("h", ".git")).is_err(),
        "`.git` alone leaves no repository behind"
    );
}

/// The rejection list, as one test because the answer must be *the same kind of answer* for every
/// entry: an explicit, named refusal, never a silent normalization of something dangerous.
///
/// Credentials and query strings are the ones that matter most: a client that forwards
/// `https://token@host/repo` or `https://host/repo?access_token=…` would put a secret into a
/// workspace-unique identity — and into every log line and every `project_alias` row that follows.
#[test]
fn credential_query_fragment_and_traversal_are_rejected() {
    let cases = [
        (
            remote("alice@git.com", None, "org/repo", RemoteTransport::Https),
            "raw URL userinfo in the host",
        ),
        (
            remote(
                "alice:secret@git.com",
                None,
                "org/repo",
                RemoteTransport::Ssh,
            ),
            "a password in the host",
        ),
        (
            remote(
                "git.com",
                None,
                "org/repo?token=abc",
                RemoteTransport::Https,
            ),
            "a query string",
        ),
        (
            remote("git.com", None, "org/repo#frag", RemoteTransport::Https),
            "a fragment",
        ),
        (
            remote("git.com", None, "org/../../etc/repo", RemoteTransport::Ssh),
            "a traversal segment",
        ),
        (
            remote("git.com", None, "org\\repo", RemoteTransport::Ssh),
            "a Windows separator",
        ),
        (
            remote("", None, "org/repo", RemoteTransport::Https),
            "an empty host",
        ),
        (
            remote("   ", None, "org/repo", RemoteTransport::Ssh),
            "a blank host",
        ),
        (
            remote("git.com", None, "   ", RemoteTransport::Https),
            "an empty path",
        ),
        (
            remote("git.com\n", None, "org/repo", RemoteTransport::Https),
            "a control character",
        ),
        (
            remote("git.com", None, "org\0repo", RemoteTransport::Https),
            "an embedded NUL",
        ),
        (
            remote("schema://git.com", None, "org/repo", RemoteTransport::Https),
            "a scheme smuggled into the host",
        ),
        (
            remote("git.com:443", None, "org/repo", RemoteTransport::Https),
            "a port smuggled into the host",
        ),
        (
            remote(
                "git.com",
                None,
                "https://git.com/org/repo",
                RemoteTransport::Https,
            ),
            "a whole URL in the path",
        ),
    ];
    for (remote, what) in cases {
        let outcome = normalize_remote(&remote);
        assert!(
            outcome.is_err(),
            "{what} was accepted as {:?} for {remote:?}",
            outcome.ok()
        );
    }
}

/// A caller that sends no host and an absolute path is sending a *local directory*, which the design
/// forbids as an identity (it is not stable across machines or worktrees). The refusal names that
/// case rather than reporting an empty field, because "host must not be empty" is not the fix.
#[test]
fn an_absolute_local_path_is_rejected_by_name() {
    let local = remote(
        "",
        None,
        "/home/anten/code/alexandria",
        RemoteTransport::Ssh,
    );
    let error = normalize_remote(&local).expect_err("a local path must not become an identity");
    assert!(
        matches!(error, ProjectIdentityError::LocalPath),
        "expected the named local-path refusal, got {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.to_lowercase().contains("local path"),
        "the refusal must say what was wrong: {message}"
    );
    assert!(
        !message.contains("/home/anten"),
        "and must not echo the submitted absolute path back: {message}"
    );
    // The same path *with* a host is an ordinary repository path on that server, and normalizes.
    assert_eq!(
        normalize_remote(&remote(
            "git.com",
            None,
            "/srv/git/alexandria",
            RemoteTransport::Ssh
        ))
        .expect("absolute server path"),
        key("git.com", None, "srv/git/alexandria")
    );
}

/// The alias the storage layer keys on. `project.normalized_alias`'s own assertion requires lowercase
/// and no surrounding whitespace, so the *rendering* has to satisfy it rather than the engine's
/// case-preserving key hoping the schema forgives it. Two tests, because both halves are real
/// constraints: the engine preserves path case, the stored alias is the case-folded form of it.
#[test]
fn the_canonical_key_renders_a_schema_legal_alias_that_round_trips() {
    let normalized = normalize_remote(&remote(
        "GitHub.com",
        Some(8443),
        "Alex/Andria.git",
        RemoteTransport::Https,
    ))
    .expect("mixed-case descriptor");
    assert_eq!(normalized.host, "GitHub.com".to_lowercase());
    assert_eq!(
        normalized.path, "Alex/Andria",
        "the key itself keeps the case"
    );
    let alias = normalized.to_alias();
    assert_eq!(
        alias, "github.com:8443/alex/andria",
        "the alias is the workspace-qualified identity: host, non-default port, folded path"
    );
    assert_eq!(
        alias,
        alias.to_lowercase().trim().to_string(),
        "`normalized_alias`'s assertion requires the stored form to equal its own lowercase"
    );
    // A default-port descriptor renders without the port at all, so `h/p` and `h:443/p` cannot be two
    // alias rows for one project.
    assert_eq!(
        normalize_remote(&https("git.com", "org/repo.git"))
            .expect("default")
            .to_alias(),
        "git.com/org/repo"
    );
}

/// Two identities that differ only in a trailing `.git`, a leading slash, host case, or the standard
/// port must render the *same* alias, and two that differ in path case or port must not. This is the
/// property `establish_project`'s idempotency rests on: same alias, one project.
#[test]
fn equivalent_descriptors_render_one_alias_and_different_ones_render_two() {
    let spellings = [
        ssh("GitHub.com", "anten/alexandria.git"),
        https("github.com", "/Anten/alexandria"),
        remote(
            "github.com",
            Some(443),
            "anten/alexandria",
            RemoteTransport::Https,
        ),
        remote(
            "github.com",
            Some(22),
            "anten/alexandria.git/",
            RemoteTransport::Ssh,
        ),
    ];
    let keys: Vec<CanonicalRepoKey> = spellings
        .iter()
        .map(|r| normalize_remote(r).expect("lawful descriptor").clone())
        .collect();
    let aliases: Vec<String> = keys.iter().map(CanonicalRepoKey::to_alias).collect();
    // The second spelling differs in *path case*, which the key keeps and the storage alias folds
    // (v009's own `normalized_alias` assertion). So the keys are two and the aliases are one, and the
    // pair of assertions below is the whole difference between "identity" and "indexed identity".
    assert_eq!(keys[0], keys[2], "port 443 on HTTPS is the default");
    assert_eq!(
        keys[0], keys[3],
        "22 on SSH is the default, and `/` plus `.git` are spellings"
    );
    assert_eq!(aliases[0], aliases[2]);
    assert_eq!(aliases[0], aliases[3]);
    assert_ne!(
        keys[0], keys[1],
        "spelling 1 differs in path case, which is identity"
    );
    assert_eq!(
        aliases[0], aliases[1],
        "and the alias folds exactly that case difference, per v009's own assertion"
    );
    let fork = https("gitlab.com", "someone-else/alexandria");
    assert_ne!(
        normalize_remote(&fork).expect("fork").to_alias(),
        aliases[0],
        "a different host or namespace is a different project, full stop"
    );
    assert_ne!(
        normalize_remote(&https("github.com", "alexandria"))
            .expect("shorter path")
            .to_alias(),
        aliases[0],
        "a different path depth is not a spelling of the same repository"
    );
}

/// IPv6 literals are **not** supported in a sanitized remote yet, and the refusal says so in the same
/// shape as any other impossible host.
///
/// The reason this is a test rather than a comment: brackets are only meaningful together with the `:`
/// they enclose, and `:` is not a host character. Accepting `[`/`]` while rejecting `:` would either
/// refuse the address by accident (which is what the shipped rule does, honestly) or — if someone later
/// "fixed" the character class — let a bracketed host reach [`CanonicalRepoKey`] and have its port folded
/// by a rule written for `host:port`, so two spellings of one IPv6 address would not compare equal.
/// Supporting IPv6 means deciding bracket grammar, address compression and the port split together.
#[test]
fn an_ipv6_host_is_refused_and_not_half_parsed() {
    for host in ["[::1]", "[2001:db8::1]", "[fe80::1%eth0]"] {
        let refused = normalize_remote(&https(host, "org/repo"));
        assert!(
            matches!(refused, Err(ProjectIdentityError::InvalidHost)),
            "a bracketed host {host:?} was accepted or refused as something else: {refused:?}"
        );
    }
    // The unbracketed form is refused for the same structural reason, and a bracket anywhere in an
    // otherwise valid host does not smuggle itself through.
    assert!(matches!(
        normalize_remote(&https("2001:db8::1", "org/repo")),
        Err(ProjectIdentityError::InvalidHost)
    ));
    assert!(matches!(
        normalize_remote(&https("git.com[", "org/repo")),
        Err(ProjectIdentityError::InvalidHost)
    ));
    // Positive control: the ordinary host this rule does support still normalizes, so the refusals above
    // are about the bracket form and not about a validator that rejects everything.
    assert!(normalize_remote(&https("git.com", "org/repo")).is_ok());
}
