use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Resolves the machine-wide local data directory embarch-core owns outright:
/// `%ProgramData%\embarch` on Windows, `/var/lib/embarch` on Linux/macOS. The
/// token file below and `study.rs`'s `study_results/<study_id>/` tree both
/// live under this same root — the one place that convention is decided.
#[cfg(windows)]
pub fn local_data_dir() -> Result<PathBuf> {
    let program_data =
        std::env::var("ProgramData").context("ProgramData environment variable is not set")?;
    Ok(PathBuf::from(program_data).join("embarch"))
}

#[cfg(unix)]
pub fn local_data_dir() -> Result<PathBuf> {
    Ok(PathBuf::from("/var/lib/embarch"))
}

/// The agent's machine-wide token file: `local_data_dir()/token`. The one
/// file every existing caller discovers (`embarch-token.md` §2).
fn token_file_path() -> Result<PathBuf> {
    Ok(local_data_dir()?.join("token"))
}

/// The person's token file, beside the agent's: `local_data_dir()/person-token`.
fn person_token_file_path() -> Result<PathBuf> {
    Ok(local_data_dir()?.join("person-token"))
}

/// Which of the two tokens authenticated a request (decision 109). Core is
/// unattested today, so nothing is refused by scope: it is recorded, for a
/// later handler to stamp onto a note, an adoption or a publish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Agent,
    Person,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Agent => "agent",
            Scope::Person => "person",
        }
    }
}

/// A handler takes `scope: Scope` to read what `auth_middleware` recorded.
/// A request that reached a handler has been authenticated, so the extension
/// is always present; its absence is a router wired without the middleware.
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Scope {
    type Rejection = (axum::http::StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        parts.extensions.get::<Scope>().copied().ok_or((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "request carries no scope: auth_middleware did not run",
        ))
    }
}

/// The two tokens Core authenticates with.
#[derive(Clone, Debug)]
pub struct Tokens {
    pub agent: String,
    pub person: String,
}

impl Tokens {
    /// The scope a bearer value matches, or `None`. The agent is checked
    /// first; `resolve_tokens` refuses two equal tokens, so the order never
    /// decides a real request.
    pub fn scope_of(&self, bearer: &str) -> Option<Scope> {
        if bearer == self.agent {
            Some(Scope::Agent)
        } else if bearer == self.person {
            Some(Scope::Person)
        } else {
            None
        }
    }
}

/// Resolves both tokens. The agent's: `EMBARCH_TOKEN` if set (file left
/// untouched), else the machine-wide file, generated on first use. The
/// person's: `EMBARCH_PERSON_TOKEN` if set, else its own file beside the
/// agent's, generated on first use with the same permissions. Equal tokens
/// are refused, since a scope that cannot be told apart is no scope.
pub fn resolve_tokens() -> Result<Tokens> {
    let agent = match std::env::var("EMBARCH_TOKEN") {
        Ok(t) => t,
        Err(_) => resolve_token_at_path(&token_file_path()?)?,
    };
    let person = match std::env::var("EMBARCH_PERSON_TOKEN") {
        Ok(t) => t,
        Err(_) => resolve_token_at_path(&person_token_file_path()?)?,
    };
    anyhow::ensure!(
        agent != person,
        "the agent and person tokens are equal; a request's scope could not be told apart"
    );
    Ok(Tokens { agent, person })
}

fn resolve_token_at_path(path: &Path) -> Result<String> {
    if path.exists() {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read token file at {}", path.display()))?;
        let token = contents.trim().to_string();
        tracing::info!(path = %path.display(), "reusing existing machine-wide token");
        return Ok(token);
    }

    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    std::fs::write(path, &token)
        .with_context(|| format!("failed to write token file at {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod 600 token file at {}", path.display()))?;
    }

    #[cfg(windows)]
    {
        restrict_token_file_permissions(path)?;
    }

    tracing::info!(path = %path.display(), "generated new machine-wide token");
    Ok(token)
}

/// Restricts the newly-created token file's ACL to the account that created
/// it plus `Administrators`/`SYSTEM`, since `%ProgramData%` is more
/// permissive by default than a user-profile directory. Only called on the
/// freshly-generated-file path — an existing file's ACL was already set when
/// it was created.
///
/// Shells out to `icacls` rather than pulling in a crate: `windows-acl`
/// (the main crate candidate) is a thin, infrequently-updated wrapper around
/// `SetNamedSecurityInfoW` with a narrow user base, which is a worse bet for
/// a security-relevant, currently-untestable-on-this-machine operation than
/// well-documented, stable OS tooling.
///
/// Grants are by SID (`*S-...`), not by account/group name. Granting by the
/// bare `%USERNAME%` string was tried first and found broken on a real
/// machine whose hostname equals the username (`gabriel\gabriel`): `icacls`
/// resolved the bare name to an unrelated/unresolvable principal instead of
/// the actual account, silently locking Core out of the token file it had
/// just created (confirmed by a same-session restart failing to re-read the
/// file `icacls` claimed to have just granted it access to). `SYSTEM` and
/// `Administrators` are passed as their well-known constant SIDs for the
/// same reason — no name resolution left for any of the three principals to
/// get wrong.
#[cfg(windows)]
pub(crate) fn restrict_token_file_permissions(path: &Path) -> Result<()> {
    const SID_SYSTEM: &str = "S-1-5-18";
    const SID_ADMINISTRATORS: &str = "S-1-5-32-544";

    let user_sid = current_user_sid()?;
    let path_str = path
        .to_str()
        .context("token file path is not valid UTF-8")?;

    let status = std::process::Command::new("icacls")
        .arg(path_str)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("*{user_sid}:F"))
        .arg("/grant:r")
        .arg(format!("*{SID_SYSTEM}:F"))
        .arg("/grant:r")
        .arg(format!("*{SID_ADMINISTRATORS}:F"))
        .status()
        .context("failed to invoke icacls")?;

    if !status.success() {
        anyhow::bail!("icacls exited with status {status}");
    }

    Ok(())
}

/// Resolves the SID of the account running this process via `whoami /user`,
/// rather than trusting `%USERNAME%` to resolve unambiguously through
/// `icacls`'s own name lookup (see `restrict_token_file_permissions`'s doc
/// comment for why that broke). CSV output (`/fo csv /nh`) is parsed instead
/// of the default table so field order is stable regardless of the system's
/// display language.
#[cfg(windows)]
fn current_user_sid() -> Result<String> {
    let output = std::process::Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .context("failed to invoke whoami /user")?;

    if !output.status.success() {
        anyhow::bail!("whoami /user exited with status {}", output.status);
    }

    let stdout =
        String::from_utf8(output.stdout).context("whoami /user output was not valid UTF-8")?;
    stdout
        .trim()
        .rsplit(',')
        .next()
        .map(|s| s.trim_matches('"').trim().to_string())
        .filter(|s| !s.is_empty())
        .context("could not parse SID from whoami /user output")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("embarch-token-store-test-{name}-{}", std::process::id()))
    }

    #[test]
    fn generates_and_reuses_token() {
        let dir = temp_path("generates-and-reuses");
        let path = dir.join("token");
        let _ = std::fs::remove_dir_all(&dir);

        let generated = resolve_token_at_path(&path).expect("should generate a new token");
        assert_eq!(generated.len(), 64, "32 random bytes hex-encoded is 64 chars");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let reused = resolve_token_at_path(&path).expect("should reuse the existing token");
        assert_eq!(generated, reused);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn two_files_hold_two_distinct_tokens_and_a_restart_reuses_both() {
        let dir = temp_path("pair");
        let _ = std::fs::remove_dir_all(&dir);
        let agent_path = dir.join("token");
        let person_path = dir.join("person-token");

        let a = resolve_token_at_path(&agent_path).unwrap();
        let p = resolve_token_at_path(&person_path).unwrap();
        assert_ne!(a, p);
        assert!(agent_path.exists() && person_path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&person_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        assert_eq!(resolve_token_at_path(&agent_path).unwrap(), a);
        assert_eq!(resolve_token_at_path(&person_path).unwrap(), p);

        let t = Tokens { agent: a.clone(), person: p.clone() };
        assert_eq!(t.scope_of(&a), Some(Scope::Agent));
        assert_eq!(t.scope_of(&p), Some(Scope::Person));
        assert_eq!(t.scope_of("other"), None);

        std::fs::remove_dir_all(&dir).ok();
    }
}
