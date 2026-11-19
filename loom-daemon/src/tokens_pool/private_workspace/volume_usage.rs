//! Read-only allocated-byte samples for managed private workspace volumes.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use super::{docker, MODE, ROOT};
use crate::telemetry::ops::MAX_LABEL_VALUE_BYTES;
use crate::tokens_pool::{account_registry::validate_name, session_lifecycle::container_name};

const VOLUME_PREFIX: &str = "loom-codex-workspace-";
const SAMPLE_BUDGET: Duration = Duration::from_secs(45);
const METADATA_BUDGET: Duration = Duration::from_secs(3);
const MAX_DU_SECS: u64 = 8;
// Docker may serialize an ordinary local volume's options as null. Use the
// template's truth test and report a bit without disclosing option values.
const VOLUME_FORMAT: &str = r#"{{.Name}}{{"\t"}}{{.Driver}}{{"\t"}}{{index .Labels "loom.account"}}{{"\t"}}{{if .Options}}1{{else}}0{{end}}"#;
// Keep inspect scoped to mount identities and state. Full inspect includes env
// and profile metadata that the disk sampler does not need.
const CONTAINER_FORMAT: &str = concat!(
    r#"{{.Id}}{{"\t"}}{{.State.Running}}{{"\t"}}{{.State.Paused}}"#,
    r#"{{"\t"}}{{index .Config.Labels "loom.workspace-mode"}}{{"\t"}}{{index .Config.Labels "loom.account"}}"#,
    r#"{{range .Mounts}}{{"\n"}}{{json .Destination}}{{"\t"}}{{.Type}}{{"\t"}}{{if eq .Type "volume"}}{{.Name}}{{end}}{{end}}"#,
);

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SeatVolumeUsage {
    pub(crate) seats: Vec<(String, i64)>,
    /// A complete sample of the discovered set, or unknown.
    pub(crate) total: Option<i64>,
}

pub(crate) fn sample_seat_volumes() -> SeatVolumeUsage {
    sample_until(Instant::now() + SAMPLE_BUDGET)
}

fn sample_until(deadline: Instant) -> SeatVolumeUsage {
    let filter = format!("name={VOLUME_PREFIX}");
    let Some(inventory) =
        metadata(deadline, &["volume", "ls", "--filter", &filter, "--format", "{{.Name}}"])
    else {
        return SeatVolumeUsage::default();
    };
    let volumes: BTreeSet<_> = inventory
        .lines()
        .filter(|name| name.starts_with(VOLUME_PREFIX))
        .collect();
    let seats: Vec<_> = volumes
        .iter()
        .filter_map(|name| {
            let account = name.strip_prefix(VOLUME_PREFIX)?;
            if account.len() > MAX_LABEL_VALUE_BYTES {
                return None;
            }
            validate_name(account).ok()?;
            Some((account.to_owned(), seat_bytes(deadline, account, name)?))
        })
        .collect();
    let total = if !seats.is_empty() && seats.len() == volumes.len() {
        seats
            .iter()
            .try_fold(0_i64, |total, (_, size)| total.checked_add(*size))
    } else {
        None
    };
    SeatVolumeUsage { seats, total }
}

fn metadata(deadline: Instant, args: &[&str]) -> Option<String> {
    let budget = deadline
        .saturating_duration_since(Instant::now())
        .min(METADATA_BUDGET);
    if budget.is_zero() {
        return None;
    }
    docker::command_with(args, budget).ok()
}

fn seat_bytes(deadline: Instant, account: &str, volume: &str) -> Option<i64> {
    let volume_info =
        metadata(deadline, &["volume", "inspect", "--format", VOLUME_FORMAT, volume])?;
    if !volume_info
        .trim_end_matches(['\r', '\n'])
        .split('\t')
        .eq([volume, "local", account, "0"])
    {
        return None;
    }
    let container = container_name(account);
    let inspect = metadata(
        deadline,
        &[
            "container",
            "inspect",
            "--format",
            CONTAINER_FORMAT,
            &container,
        ],
    )?;
    let id = verified_container(&inspect, account, volume)?;

    // Killing a Docker client does not necessarily stop an in-container exec.
    // Bound the child too and leave room for timeout's kill grace and the CLI.
    let seconds = deadline
        .saturating_duration_since(Instant::now())
        .as_secs()
        .saturating_sub(2)
        .min(MAX_DU_SECS);
    if seconds == 0 {
        return None;
    }
    let duration = format!("{seconds}s");
    let output = docker::command_with(
        &[
            "exec",
            "--user",
            "1000:1000",
            id,
            "/usr/bin/timeout",
            "--kill-after=1s",
            &duration,
            "/usr/bin/du",
            "-skx",
            "--",
            ROOT,
        ],
        Duration::from_secs(seconds + 2),
    )
    .ok()?;
    parse_du(&output)
}

fn verified_container<'a>(inspect: &'a str, account: &str, volume: &str) -> Option<&'a str> {
    let mut lines = inspect.lines();
    let mut identity = lines.next()?.split('\t');
    let id = identity.next()?;
    if id.len() != 64
        || !id.bytes().all(|b| b.is_ascii_hexdigit())
        || !identity.eq(["true", "false", MODE, account])
    {
        return None;
    }
    let mut mounted = false;
    for line in lines {
        let mut fields = line.split('\t');
        let path: String = serde_json::from_str(fields.next()?).ok()?;
        if path == ROOT {
            if mounted || !fields.eq(["volume", volume]) {
                return None;
            }
            mounted = true;
        } else if path
            .strip_prefix(ROOT)
            .is_some_and(|suffix| suffix.starts_with('/'))
        {
            // Same-filesystem bind mounts can slip past `du -x`.
            return None;
        }
    }
    mounted.then_some(id)
}

fn parse_du(output: &str) -> Option<i64> {
    let (blocks, path) = output.trim_end_matches(['\r', '\n']).split_once('\t')?;
    if path != ROOT {
        return None;
    }
    let bytes = blocks.parse::<u64>().ok()?.checked_mul(1024)?;
    i64::try_from(bytes).ok()
}

#[cfg(test)]
mod tests;
