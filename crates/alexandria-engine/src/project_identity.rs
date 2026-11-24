//! Project identity: what makes two repository remotes the same project, in pure Rust.
//!
//! The server owns this algorithm and nothing else may re-derive it. The design's reason is concrete:
//! a TypeScript copy in the pi companion and a shell copy in the Claude hook would each diverge from
//! this one the first time a rule was adjusted, and two clients that disagree about project identity
//! write one repository's memories into two partitions — silently, with no error anywhere to read. So
//! clients send a *descriptor*, this module decides, and storage persists the decision.
//!
//! ## What an identity is, and what it is not
//!
//! [`CanonicalRepoKey`] is a host, an optional non-default port, and a path. It deliberately carries
//! no transport: the same repository reachable over SSH and over HTTPS is one repository, and letting
//! the transport into the identity would fork a project every time a client switched remotes. It
//! equally carries no *user* (`git@` is a login, not a code location) and no credentials, which are
//! refused outright rather than stripped, because a secret that arrives inside an identity field is
//! headed for a workspace-unique index and every log line that reads it.
//!
//! Host case is a spelling and is folded. **Path case is not**, because `Org/Repo` and `org/repo` are
//! different repositories on a case-sensitive server, and merging them is the collapse of unrelated
//! identities the design rules out. That asymmetry has one consequence worth stating rather than
//! rediscovering: `project.normalized_alias` and `project_alias.alias` both assert that a stored alias
//! equals its own lowercase (v009, accepted), so the alias a project is keyed on is the case-folded
//! rendering of the key — see [`CanonicalRepoKey::to_alias`]. The key keeps the case; the
//! workspace-qualified uniqueness does not distinguish it. Normalizing here is therefore *finer* than
//! what the indexed column enforces, and finer on purpose: an identity rule must not be shaped by
//! whichever column a task chose to index, and a schema that later stores the case verbatim needs no
//! change to this module.
//!
//! Nothing here touches a database, an account or a workspace. Resolution and creation belong to
//! storage and MCP (`ScopedProjectRepo`, `ScopeService`); this module answers "what is this repository
//! called", never "does that name already exist".

use thiserror::Error;

/// How a remote was reached, as the client reports it.
///
/// Carried in the descriptor because it decides which port counts as *default* — and therefore which
/// ports are invisible in the identity — and because an `ssh://` URL has a path shape an HTTPS URL is
/// only spelled the same way by convention. It is not part of the resulting identity; that omission
/// is the whole point of normalizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoteTransport {
    Https,
    Ssh,
}

impl RemoteTransport {
    /// The port this transport means when none is written.
    const fn standard_port(self) -> u16 {
        match self {
            Self::Https => 443,
            Self::Ssh => 22,
        }
    }

    /// The closed wire spelling of each variant, as the MCP tool schema advertises it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Ssh => "ssh",
        }
    }

    /// Parses the transport label a client sent.
    ///
    /// Case-folded, because `HTTPS` is a spelling of a word and not of an identity. Anything else is
    /// `None` so the caller can name the allowed set in its own refusal: a transport this server has
    /// never heard of is not guessed at, and guessing is how a project ends up filed under a
    /// default its client never asked for.
    pub fn parse(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "https" => Some(Self::Https),
            "ssh" => Some(Self::Ssh),
            _ => None,
        }
    }
}

/// One repository, as sanitized by whoever looked at it.
///
/// A *descriptor*, not a URL: `host` and `path` are already separated, and [`normalize_remote`]
/// checks that nothing else has been smuggled into either. `path` needs no leading slash — `/org/repo`
/// (the URL form) and `org/repo` (the scp-like `host:org/repo` form) are one path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SanitizedRemote {
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub transport: RemoteTransport,
}

/// A repository's identity: host, port only when it is not the transport's own default, case-preserved
/// path.
///
/// `Eq`-wise, two values are the same project or they are not: there is no similarity measure and no
/// threshold to tune, deliberately. [`CanonicalRepoKey::to_alias`] is the one rendering storage keys
/// on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CanonicalRepoKey {
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
}

impl CanonicalRepoKey {
    /// The workspace-qualified alias string a `project` row is keyed and looked up by.
    ///
    /// `host[:port]/path`, with the host already lowercased by [`normalize_remote`] and the path
    /// lowercased *here*, because `project.normalized_alias` and `project_alias.alias` both assert that
    /// a stored value equals its own lowercase. A caller that needs the case-preserved form reads
    /// [`CanonicalRepoKey::path`] rather than parsing this string, so nothing has to reverse it.
    ///
    /// The lowercase is a folding of spelling *for the index*, not a claim that `Org/Repo` and
    /// `org/repo` are different repositories: the key above is what distinguishes them, and this is
    /// what a workspace can hold at most one of.
    pub fn to_alias(&self) -> String {
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        format!(
            "{host}{port}/{path}",
            host = self.host,
            path = self.path.to_lowercase()
        )
    }
}

/// Why a descriptor is not a project identity.
///
/// Every variant is a *refusal*, never a silent normalization: a descriptor that arrives carrying
/// credentials is not one with the credentials quietly stripped, because the stripping is wrong the
/// first time two differents collide, and the failure mode is one project's memories filed under
/// somebody else's name.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProjectIdentityError {
    /// No host was given, so what remains is a directory on the machine that sent it. Named for the
    /// mistake rather than for the empty field, because "host must not be empty" is not the fix: the
    /// client has to send a remote or an explicit project id. The submitted path is never echoed.
    #[error(
        "an absolute local path is not a project identity; submit a sanitized remote or an explicit project id"
    )]
    LocalPath,
    /// The host is empty and the path is not absolute either — nothing identifies a repository.
    #[error("host must not be empty")]
    EmptyHost,
    /// The path is empty, or was only a `.git` suffix.
    #[error("repository path must not be empty")]
    EmptyPath,
    /// Port 0: nothing listens there, and accepting it would put a junk key in a unique index.
    #[error("port must be between 1 and 65535")]
    InvalidPort,
    /// The host holds a character that cannot belong to a hostname: control, whitespace, a URL
    /// separator, a scheme, or a port in the wrong field.
    #[error("host contains a character that cannot belong to a hostname")]
    InvalidHost,
    /// The path holds a control character, a query/fragment separator, a backslash, an empty or
    /// traversal segment, or a scheme.
    #[error("repository path contains a character or segment that cannot belong to one")]
    InvalidPath,
}

/// Turns a sanitized descriptor into the identity storage keys on.
///
/// The rule set, each part with the reason it exists:
///
/// * **Standard ports vanish** (443 over HTTPS, 22 over SSH) and any other port is *kept*, because a
///   port identifies a server while the default port identifies an absence. 22 over HTTPS stays: it is
///   not that transport's default.
/// * **The host is lowercased, the path is not** — see the module docs.
/// * **One trailing `.git` is stripped**, and only one: `repo.git` is a repository called `repo`,
///   `repo.git.git` is one called `repo.git`.
/// * **Leading and trailing slashes are stripped** so the URL form and the scp-like form agree. That
///   equivalence is the reason this function exists at all.
/// * **Credentials, query strings, fragments, schemes, backslashes, traversal and control characters
///   are refused**, not stripped ([`ProjectIdentityError`]).
/// * **An empty host with an absolute path is refused by name**, because that is what it is — a local
///   directory, which the design excludes as an identity precisely because it is not stable across
///   machines or worktrees.
///
/// No fuzzy matching, no alias merging, no rename, no creation. Two descriptors either normalize to
/// one [`CanonicalRepoKey`] or they do not, and every case the rule set decides is pinned in
/// `tests/project_identity_test.rs`.
pub fn normalize_remote(
    remote: &SanitizedRemote,
) -> Result<CanonicalRepoKey, ProjectIdentityError> {
    let host = match validate_host(&remote.host) {
        // The distinction the design cares about: with no host, an *absolute* path is a local
        // directory, and that is the mistake worth naming. Anything else with no host is just
        // nothing, and says so.
        Err(ProjectIdentityError::EmptyHost) if remote.path.trim_start().starts_with('/') => {
            return Err(ProjectIdentityError::LocalPath);
        }
        other => other?,
    };
    let port = match remote.port {
        None => None,
        Some(0) => return Err(ProjectIdentityError::InvalidPort),
        Some(p) if p == remote.transport.standard_port() => None,
        Some(p) => Some(p),
    };
    let path = validate_path(&remote.path)?;
    Ok(CanonicalRepoKey { host, port, path })
}

/// Whether a character is one a hostname can contain.
///
/// Deliberately **no IPv6 support yet**: brackets are not allowed, because a bracketed literal is only
/// meaningful together with the `:` it encloses, and `:` is not a host character. So `https://[::1]/x`
/// refuses as `InvalidHost` rather than half-parsing a form this rule cannot canonicalize — a bracketed
/// host would otherwise reach [`CanonicalRepoKey`] with its port folded by a rule written for
/// `host:port`, and two spellings of one IPv6 address would not compare equal. Adding IPv6 means
/// deciding bracket grammar, address compression and the port split together, with tests; that is not a
/// comment fix.
fn is_host_char(ch: char) -> bool {
    matches!(ch, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '.')
}

/// The host, lowercased and trimmed, or a refusal.
///
/// Trimming is allowed (surrounding spaces are a copy-paste artifact, not identity) but a control
/// character *anywhere* in the submitted value is a refusal rather than something to trim away:
/// `git.com\n` is not a host, and treating it as one would let a delimiter survive into an index key.
fn validate_host(raw: &str) -> Result<String, ProjectIdentityError> {
    if raw.chars().any(char::is_control) {
        return Err(ProjectIdentityError::InvalidHost);
    }
    let host = raw.trim().to_ascii_lowercase();
    if host.is_empty() {
        return Err(ProjectIdentityError::EmptyHost);
    }
    if !host.chars().all(is_host_char) {
        return Err(ProjectIdentityError::InvalidHost);
    }
    Ok(host)
}

/// The repository path, case-preserved, with spellings normalized away.
fn validate_path(raw: &str) -> Result<String, ProjectIdentityError> {
    if raw.chars().any(char::is_control) {
        return Err(ProjectIdentityError::InvalidPath);
    }
    let mut path = raw.trim();
    while let Some(rest) = path.strip_prefix('/') {
        path = rest;
    }
    path = path.strip_suffix('/').unwrap_or(path);
    // Exactly one `.git`, at the end, and not the whole of the name.
    let stripped = path.strip_suffix(".git").unwrap_or(path);
    if stripped.trim().is_empty() {
        return Err(ProjectIdentityError::EmptyPath);
    }
    for segment in stripped.split('/') {
        // An empty segment is a `//`, `.`/`..` are traversal, and a segment carrying `@ : ? # \`
        // is a URL or a Windows path rather than a repository path.
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(ProjectIdentityError::InvalidPath);
        }
        if segment
            .chars()
            .any(|c| matches!(c, '?' | '#' | '\\' | '@' | ':' | '%'))
        {
            return Err(ProjectIdentityError::InvalidPath);
        }
    }
    Ok(stripped.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(
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

    /// `RemoteTransport`'s own contract, which the identity rules lean on: the label set is closed,
    /// the standard ports are what this module claims they are, and an unknown label resolves to
    /// nothing rather than to a default.
    #[test]
    fn transports_have_closed_labels_and_known_defaults() {
        assert_eq!(RemoteTransport::Https.standard_port(), 443);
        assert_eq!(RemoteTransport::Ssh.standard_port(), 22);
        for (label, expected) in [
            ("https", Some(RemoteTransport::Https)),
            ("HTTPS", Some(RemoteTransport::Https)),
            (" ssh ", Some(RemoteTransport::Ssh)),
            ("git", None),
            ("", None),
        ] {
            assert_eq!(
                RemoteTransport::parse(label),
                expected,
                "label {label:?} resolved the wrong way"
            );
        }
        assert_eq!(RemoteTransport::Https.as_str(), "https");
        assert_eq!(RemoteTransport::Ssh.as_str(), "ssh");
        // `as_str` and `parse` are inverses, so a descriptor round-trips through its wire form.
        for transport in [RemoteTransport::Https, RemoteTransport::Ssh] {
            assert_eq!(RemoteTransport::parse(transport.as_str()), Some(transport));
        }
    }

    /// The standard port of *one* transport is a foreign port on the other, and stays in the key.
    #[test]
    fn a_standard_port_is_only_standard_for_its_own_transport() {
        let ssh_default = normalize_remote(&descriptor(
            "h.example",
            Some(22),
            "a/b",
            RemoteTransport::Ssh,
        ))
        .unwrap();
        let https_with_22 = normalize_remote(&descriptor(
            "h.example",
            Some(22),
            "a/b",
            RemoteTransport::Https,
        ))
        .unwrap();
        assert_eq!(ssh_default.port, None);
        assert_eq!(https_with_22.port, Some(22));
        assert_ne!(ssh_default, https_with_22);
    }

    /// `to_alias` is what a `project.normalized_alias` row is written under, so the property is
    /// checked against the same expression v009's `ASSERT` uses rather than a paraphrase of it.
    #[test]
    fn an_alias_satisfies_the_schema_assertion_it_is_written_under() {
        let key = normalize_remote(&descriptor(
            "GitHub.Example.com",
            Some(8443),
            "/Org/Repo.git/",
            RemoteTransport::Https,
        ))
        .unwrap();
        assert_eq!(key.path, "Org/Repo", "the key preserves path case");
        let alias = key.to_alias();
        assert_eq!(alias, "github.example.com:8443/org/repo");
        assert_eq!(alias, alias.trim().to_lowercase());
        assert!(!alias.is_empty());
        // The default port leaves no trace and the case is folded, so one repository has one alias
        // however its client typed the host, the path case or the port.
        let same = normalize_remote(&descriptor(
            "GitHub.Example.com",
            Some(443),
            "ORG/Repo",
            RemoteTransport::Https,
        ))
        .unwrap();
        assert_eq!(same.path, "ORG/Repo", "the key keeps the case");
        assert_eq!(
            same.to_alias(),
            "github.example.com/org/repo",
            "and the alias is the folded identity"
        );
        // A non-default port is *not* a spelling, so it must survive into the alias.
        assert_ne!(alias, same.to_alias(), "8443 is another server than 443");
    }

    /// A newline in a host is refused, not trimmed: an accepted `git.com\n` would reach a unique
    /// index as a key nobody can type back.
    #[test]
    fn a_control_character_in_either_field_is_refused_not_trimmed() {
        assert_eq!(
            normalize_remote(&descriptor(
                "git.com\n",
                None,
                "a/b",
                RemoteTransport::Https
            )),
            Err(ProjectIdentityError::InvalidHost)
        );
        assert_eq!(
            normalize_remote(&descriptor(
                "git.com",
                None,
                "a/b\0c",
                RemoteTransport::Https
            )),
            Err(ProjectIdentityError::InvalidPath)
        );
        // Surrounding spaces, by contrast, are a copy-paste artifact and normalize away.
        assert!(
            normalize_remote(&descriptor(
                "  git.com  ",
                None,
                "a/b",
                RemoteTransport::Https
            ))
            .is_ok()
        );
    }
}
