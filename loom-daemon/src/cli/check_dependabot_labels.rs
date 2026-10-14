//! The existing Dependabot label/manifest gate, using the shared YAML reader.
use std::{collections::BTreeSet, fs, path::PathBuf, process::Command};

use anyhow::{bail, Context, Result};
use serde_json::Value;

#[derive(clap::Args)]
pub(crate) struct CheckDependabotLabelsArgs {
    /// Repository containing .github/dependabot.yml (defaults to cwd).
    #[arg(long)]
    repo_root: Option<PathBuf>,
}

impl CheckDependabotLabelsArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = self.repo_root.unwrap_or(std::env::current_dir()?);
        let config = root.join(".github/dependabot.yml");
        let text = match fs::read_to_string(&config) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!(
                    "check-dependabot-labels: no .github/dependabot.yml — nothing to check (ok)."
                );
                return Ok(());
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", config.display())),
        };
        let npm_dirs = entries(&text).context("check-dependabot-labels: invalid config")?;
        let output = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["ls-files", "-z", "--", "package.json", "*/package.json"])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .context("listing tracked npm manifests")?;
        if !output.status.success() {
            bail!(
                "could not list tracked npm manifests: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let tracked = String::from_utf8(output.stdout).context("non-UTF-8 manifest path")?;
        let manifests: BTreeSet<_> = tracked.split('\0').filter(|s| !s.is_empty()).collect();
        for manifest in &manifests {
            let path = root.join(manifest);
            let text = fs::read_to_string(&path).with_context(|| format!("reading {manifest}"))?;
            let has_dependencies =
                declares_dependencies(&text).with_context(|| format!("invalid {manifest}"))?;
            let relative = manifest
                .strip_suffix("package.json")
                .unwrap()
                .trim_end_matches('/');
            let directory = format!("/{relative}");
            if npm_dirs.contains(&directory) || !has_dependencies {
                continue;
            }
            let parent = path.parent().expect("manifest is beneath root");
            let locked = [
                "package-lock.json",
                "pnpm-lock.yaml",
                "yarn.lock",
                "npm-shrinkwrap.json",
            ]
            .iter()
            .any(|name| parent.join(name).is_file());
            if locked {
                bail!("{manifest} has a committed lockfile but no npm entry for directory {directory:?}");
            }
            println!("note: {manifest} declares dependencies and has no dependabot entry (no lockfile — treated as a scaffold, not an error).");
        }
        for dir in npm_dirs {
            let manifest = format!(
                "{}package.json",
                if dir == "/" {
                    String::new()
                } else {
                    format!("{}/", &dir[1..])
                }
            );
            if !manifests.contains(manifest.as_str()) {
                bail!("npm entry for directory {dir:?} has no tracked package.json");
            }
        }
        println!("check-dependabot-labels: all dependabot.yml entries carry 'loom:review-requested' and every dependency-bearing locked manifest is covered (ok).");
        Ok(())
    }
}

fn strings(value: &Value) -> Result<Vec<&str>> {
    let values = value
        .as_array()
        .context("expected a nonempty string list")?;
    if values.is_empty() {
        bail!("expected a nonempty string list");
    }
    values
        .iter()
        .map(|v| {
            v.as_str()
                .filter(|s| !s.is_empty())
                .context("expected a nonempty string")
        })
        .collect()
}

fn entries(text: &str) -> Result<BTreeSet<String>> {
    let config = loom_daemon::fleet_store::yaml::parse(text)?;
    let updates = config
        .get("updates")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty())
        .context("parsed zero entries (expected nonempty updates list)")?;
    let mut npm = BTreeSet::new();
    for entry in updates {
        let eco = entry
            .get("package-ecosystem")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .context("missing package-ecosystem")?;
        let dirs = match (entry.get("directory"), entry.get("directories")) {
            (Some(dir), None) => vec![dir.as_str().context("directory must be a string")?],
            (None, Some(dirs)) => strings(dirs)?,
            _ => bail!("entry {eco:?} must set exactly one of directory or directories"),
        };
        for dir in &dirs {
            if !dir.starts_with('/')
                || dir.contains(['*', '?', '[', '\\', '\t', '\n', '\r'])
                || (*dir != "/"
                    && dir[1..]
                        .split('/')
                        .any(|p| p.is_empty() || p == "." || p == ".."))
            {
                bail!("entry {eco:?} has unsupported directory {dir:?}; expected an exact absolute directory");
            }
        }
        let labels = strings(entry.get("labels").context("missing labels")?)?;
        if !labels.contains(&"loom:review-requested") {
            bail!("entry {eco:?} (directories {dirs:?}) does not list 'loom:review-requested' in labels");
        }
        if eco == "npm" {
            npm.extend(dirs.into_iter().map(str::to_owned));
        }
    }
    Ok(npm)
}

fn declares_dependencies(text: &str) -> Result<bool> {
    let manifest: Value = serde_json::from_str(text)?;
    let object = manifest
        .as_object()
        .context("package.json must be an object")?;
    let mut any = false;
    for name in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ] {
        if let Some(value) = object.get(name) {
            let deps = value
                .as_object()
                .with_context(|| format!("{name} must be an object"))?;
            if deps.values().any(|v| !v.is_string()) {
                bail!("{name} versions must be strings");
            }
            any |= !deps.is_empty();
        }
    }
    Ok(any)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(fields: &str) -> String {
        format!("version: 2\nupdates:\n  - package-ecosystem: npm\n{fields}\n")
    }
    #[test]
    fn both_directory_and_label_styles_preserve_each_directory() {
        let singular = config("    directory: /pkg\n    labels:\n      - loom:review-requested");
        assert_eq!(entries(&singular).unwrap(), BTreeSet::from(["/pkg".into()]));
        let plural = config("    directories:\n      - /pkg\n      - '/a space'\n    labels: [dependencies, 'loom:review-requested']");
        assert_eq!(entries(&plural).unwrap(), BTreeSet::from(["/pkg".into(), "/a space".into()]));
    }
    #[test]
    fn bad_or_ambiguous_declarations_fail_closed() {
        for fields in [
            "    directory: /pkg\n    directories: [/pkg]\n    labels: [loom:review-requested]",
            "    directories: []\n    labels: [loom:review-requested]",
            "    directories: [7]\n    labels: [loom:review-requested]",
            "    directory: ''\n    labels: [loom:review-requested]",
            "    directory: /../escape\n    labels: [loom:review-requested]",
            "    directory: /pkg\n    labels: []",
            "    directory: /pkg\n    labels: [dependencies]",
            "    directory: /pkg\n    labels: loom:review-requested",
            "    directory: /pkg\n    directory: /other\n    labels: [loom:review-requested]",
        ] {
            assert!(entries(&config(fields)).is_err(), "{fields}");
        }
        assert!(entries("version: 2\nupdates: []").is_err());
    }
    #[test]
    fn dependency_free_exemption_requires_a_valid_manifest() {
        assert!(!declares_dependencies(r#"{"name":"root","dependencies":{}}"#).unwrap());
        for name in [
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ] {
            assert!(declares_dependencies(&format!(r#"{{"{name}":{{"x":"1"}}}}"#)).unwrap());
        }
        for invalid in [
            "{",
            "null",
            "[]",
            r#"{"dependencies":null}"#,
            r#"{"devDependencies":{"x":7}}"#,
        ] {
            assert!(declares_dependencies(invalid).is_err(), "{invalid}");
        }
    }
}
