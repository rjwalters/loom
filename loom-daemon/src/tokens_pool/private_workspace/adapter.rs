//! Legacy adapter escape flags cannot turn private account ownership into a
//! host-direct model launch. The shell only locates owned profile paths; all
//! refusal policy lives here and is also applied before dispatch preparation.
use super::*;

fn set(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|v| !v.is_empty())
}

pub(super) fn check(profile: Option<&Path>) -> Result<()> {
    if set("LOOM_CODEX_NO_EXEC") {
        return Ok(());
    }
    let private = profile
        .filter(|path| path.is_dir())
        .map(|path| {
            let path = path.canonicalize()?;
            let owned = path.parent().is_some_and(|parent| {
                parent
                    .join(".private-sessions")
                    .join(path.file_name().unwrap())
                    .join("workspace.json")
                    .exists()
            });
            if owned {
                state_dir(&path)?;
            }
            Ok::<_, anyhow::Error>(owned)
        })
        .transpose()?
        .unwrap_or(false);
    if !private && !set("LOOM_PRIVATE_LEASE_FD") {
        return Ok(());
    }
    if std::env::var("LOOM_CODEX_SESSION_EXEC").as_deref() == Ok("0") {
        bail!(
            "private Codex account requires session-exec; LOOM_CODEX_SESSION_EXEC=0 is forbidden"
        );
    }
    if set("LOOM_SPAWN_NO_EXPORT") {
        bail!(
            "private Codex account requires profile resolution; LOOM_SPAWN_NO_EXPORT is forbidden"
        );
    }
    if private && !profile.unwrap().join(".session-managed.json").is_file() {
        bail!("private Codex account lacks its session marker; recover the owned session before dispatch");
    }
    Ok(())
}

pub(super) fn check_environment() -> Result<()> {
    check(None)?;
    let env_path = |key| {
        std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let ambient =
        env_path("CODEX_HOME").or_else(|| env_path("HOME").map(|home| home.join(".codex")));
    let requested = env_path("LOOM_CODEX_HOME")
        .or_else(|| env_path("CODEX_HOME"))
        .or_else(|| {
            env_path("LOOM_CODEX_PROFILE").and_then(|name| {
                // Match the direct adapter's explicit-name path even when an
                // empty profile-root override disables inventory discovery.
                env_path("LOOM_CODEX_PROFILE_ROOT")
                    .or_else(|| env_path("HOME").map(|home| home.join(".loom/codex-profiles")))
                    .map(|root| root.join(name))
            })
        })
        .or_else(|| ambient.clone());
    check(requested.as_deref())?;
    // NO_EXPORT leaves Codex's ambient home intact despite a Loom-only pin.
    if set("LOOM_SPAWN_NO_EXPORT") {
        check(ambient.as_deref())?;
    }
    Ok(())
}
