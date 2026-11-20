//! Read-only allocated-byte samples for managed private workspace volumes.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::{docker, MODE, ROOT, VOLUME_PREFIX};
use crate::telemetry::ops::MAX_LABEL_VALUE_BYTES;
use crate::tokens_pool::{account_registry::validate_name, session_lifecycle::container_name};

const SAMPLE_BUDGET: Duration = Duration::from_secs(60);
const MAX_SAMPLE_AGE: Duration = Duration::from_secs(15 * 60);
const METADATA_BUDGET: Duration = Duration::from_secs(3);
// The observed cold scan of a 39.8 GB seat took 32.815 s (#14678).
const MAX_DU_SECS: u64 = 50;
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

/// Samples one current seat per pass. Failed seats still advance the cursor.
#[derive(Default)]
pub(crate) struct SeatVolumeSamples {
    after: String,
    readings: BTreeMap<String, (Instant, i64)>,
}

impl SeatVolumeSamples {
    pub(crate) fn sample(&mut self) -> SeatVolumeUsage {
        self.sample_until(Instant::now() + SAMPLE_BUDGET, Instant::now)
    }

    fn sample_until(&mut self, deadline: Instant, clock: impl Fn() -> Instant) -> SeatVolumeUsage {
        let filter = format!("name={VOLUME_PREFIX}");
        let Some(inventory) = metadata(
            deadline,
            &clock,
            &["volume", "ls", "--filter", &filter, "--format", "{{.Name}}"],
        ) else {
            self.readings.clear();
            log::debug!("private workspace volume sampling skipped: inventory unavailable");
            return SeatVolumeUsage::default();
        };
        let roster: BTreeSet<_> = inventory
            .lines()
            .filter_map(|name| name.strip_prefix(VOLUME_PREFIX))
            .collect();
        self.readings
            .retain(|account, _| roster.contains(account.as_str()));
        let Some(account) = roster
            .iter()
            .copied()
            .find(|account| *account > self.after.as_str())
            .or_else(|| roster.first().copied())
        else {
            return SeatVolumeUsage::default();
        };
        self.after = account.to_owned();

        let mut seats = Vec::new();
        if account.len() <= MAX_LABEL_VALUE_BYTES && validate_name(account).is_ok() {
            let volume = super::workspace_volume_name(account);
            match seat_bytes(deadline, &clock, account, &volume) {
                Ok(bytes) => {
                    self.readings.insert(account.to_owned(), (clock(), bytes));
                    seats.push((account.to_owned(), bytes));
                }
                Err(reason) => {
                    self.readings.remove(account);
                    log::debug!(
                        "private workspace volume sampling skipped for {account}: {reason}"
                    );
                }
            }
        } else {
            log::debug!("private workspace volume sampling skipped: invalid account name");
        }

        let now = clock();
        self.readings
            .retain(|_, (measured, _)| now.duration_since(*measured) < MAX_SAMPLE_AGE);
        let total = if self.readings.len() == roster.len() {
            self.readings
                .values()
                .try_fold(0_i64, |total, (_, bytes)| total.checked_add(*bytes))
        } else {
            None
        };
        SeatVolumeUsage { seats, total }
    }
}

fn metadata(deadline: Instant, clock: &impl Fn() -> Instant, args: &[&str]) -> Option<String> {
    let budget = deadline
        .saturating_duration_since(clock())
        .min(METADATA_BUDGET);
    if budget.is_zero() {
        return None;
    }
    docker::command_with(args, budget).ok()
}

fn seat_bytes(
    deadline: Instant,
    clock: &impl Fn() -> Instant,
    account: &str,
    volume: &str,
) -> Result<i64, &'static str> {
    let volume_info =
        metadata(deadline, clock, &["volume", "inspect", "--format", VOLUME_FORMAT, volume])
            .ok_or("volume inspection failed")?;
    if !volume_info
        .trim_end_matches(['\r', '\n'])
        .split('\t')
        .eq([volume, "local", account, "0"])
    {
        return Err("unverified volume");
    }
    let container = container_name(account);
    let inspect = metadata(
        deadline,
        clock,
        &[
            "container",
            "inspect",
            "--format",
            CONTAINER_FORMAT,
            &container,
        ],
    )
    .ok_or("container inspection failed")?;
    let id = verified_container(&inspect, account, volume).ok_or("unverified running mount")?;

    // Killing a Docker client does not necessarily stop an in-container exec.
    // Bound the child too and leave room for timeout's kill grace and the CLI.
    let seconds = deadline
        .saturating_duration_since(clock())
        .as_secs()
        .saturating_sub(2)
        .min(MAX_DU_SECS);
    if seconds == 0 {
        return Err("read budget exhausted");
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
    .map_err(|_| "du failed or timed out")?;
    parse_du(&output).ok_or("unusable du measurement")
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
