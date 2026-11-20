//! Read-only allocated-byte samples for managed private workspace volumes.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::{docker, ROOT, VOLUME_PREFIX};
use crate::session_reconcile::skew::DaemonVersion;
use crate::telemetry::ops::MAX_LABEL_VALUE_BYTES;
use crate::tokens_pool::account_registry::validate_name;
use crate::tokens_pool::session_lifecycle::DEFAULT_SESSION_IMAGE;

pub(crate) const SEAT_VOLUME_SAMPLE_INTERVAL: Duration = Duration::from_secs(180);
const MAX_READER_CANDIDATES: usize = 4;
const METADATA_BUDGET: Duration = Duration::from_secs(3);
// The observed cold scans ran from 32.815 to 147.2 seconds (#14678).
const MAX_DU_SECS: u64 = 175;
// Return the ownership witness and backing path without inspecting credentials
// or exposing a local driver's option values.
const VOLUME_FORMAT: &str = concat!(
    r#"{{.Name}}{{"\t"}}{{.Driver}}{{"\t"}}{{index .Labels "loom.account"}}"#,
    r#"{{"\t"}}{{if .Options}}1{{else}}0{{end}}{{"\t"}}{{.Mountpoint}}{{"\t"}}{{.CreatedAt}}"#,
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
    /// Measured time, bytes and the volume identity line the bytes describe.
    readings: BTreeMap<String, (Instant, i64, String)>,
}

impl SeatVolumeSamples {
    /// `reader_image` is resolved lazily, only once a seat is about to be
    /// measured, and must return an image already verified as trusted.
    pub(crate) fn sample(
        &mut self,
        reader_image: impl FnOnce() -> Option<String>,
    ) -> SeatVolumeUsage {
        self.sample_until(Instant::now() + SEAT_VOLUME_SAMPLE_INTERVAL, Instant::now, reader_image)
    }

    fn sample_until(
        &mut self,
        deadline: Instant,
        clock: impl Fn() -> Instant,
        reader_image: impl FnOnce() -> Option<String>,
    ) -> SeatVolumeUsage {
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
        // A volume replaced since it was measured no longer backs its reading.
        let volumes: Vec<String> = roster
            .iter()
            .map(|account| super::workspace_volume_name(account))
            .collect();
        if !volumes.is_empty() && !self.readings.is_empty() {
            let mut args = vec!["volume", "inspect", "--format", VOLUME_FORMAT];
            args.extend(volumes.iter().map(String::as_str));
            let Some(identities) = metadata(deadline, &clock, &args) else {
                self.readings.clear();
                log::debug!("private workspace volume sampling skipped: identities unavailable");
                return SeatVolumeUsage::default();
            };
            let current: BTreeMap<&str, &str> = identities
                .lines()
                .filter_map(|line| {
                    let name = line.split('\t').next()?;
                    Some((name.strip_prefix(VOLUME_PREFIX)?, line))
                })
                .collect();
            self.readings.retain(|account, (_, _, identity)| {
                current.get(account.as_str()) == Some(&identity.as_str())
            });
        }
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
            let measured = match reader_image() {
                Some(image) => seat_bytes(deadline, &clock, account, &volume, &image),
                None => Err("no verified local reader image"),
            };
            match measured {
                Ok((bytes, identity)) => {
                    self.readings
                        .insert(account.to_owned(), (clock(), bytes, identity));
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
        // Budget for a complete roster rotation even when more seats are added.
        let maximum_age = SEAT_VOLUME_SAMPLE_INTERVAL
            .saturating_mul(u32::try_from(roster.len()).unwrap_or(u32::MAX));
        self.readings
            .retain(|_, (measured, _, _)| now.duration_since(*measured) < maximum_age);
        let total = if self.readings.len() == roster.len() {
            self.readings
                .values()
                .try_fold(0_i64, |total, (_, bytes, _)| total.checked_add(*bytes))
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
    image: &str,
) -> Result<(i64, String), &'static str> {
    let volume_args = ["volume", "inspect", "--format", VOLUME_FORMAT, volume];
    let before = metadata(deadline, clock, &volume_args).ok_or("volume inspection failed")?;
    let root = metadata(deadline, clock, &["info", "--format", "{{.DockerRootDir}}"])
        .ok_or("Docker root unavailable")?;
    let backing_path =
        verified_volume(&before, volume, account, &root).ok_or("unverified volume")?;
    let mount = format!("type=bind,src={backing_path},dst={ROOT},readonly,bind-recursive=disabled");

    // The same isolated reader works whether the seat has a session or not.
    // A bind mount of the verified backing directory cannot implicitly create
    // a replacement Docker volume if the inspected one disappears.
    let seconds = deadline
        .saturating_duration_since(clock())
        .as_secs()
        .saturating_sub(METADATA_BUDGET.as_secs() + 2)
        .min(MAX_DU_SECS);
    if seconds == 0 {
        return Err("read budget exhausted");
    }
    let duration = format!("{seconds}s");
    let output = docker::command_with(
        &[
            "run",
            "--rm",
            "--pull",
            "never",
            "--network",
            "none",
            "--user",
            "1000:1000",
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--log-driver",
            "none",
            "--workdir",
            "/",
            "--mount",
            &mount,
            "--entrypoint",
            "/usr/bin/timeout",
            "--",
            image,
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
    if metadata(deadline, clock, &volume_args).as_deref() != Some(before.as_str()) {
        return Err("volume identity changed during read");
    }
    let bytes = parse_du(&output).ok_or("unusable du measurement")?;
    Ok((bytes, before.trim_end_matches(['\r', '\n']).to_owned()))
}

/// The image the reader runs from: a configured override, else the newest
/// local stock session image. Either must already be local (never pulled) and
/// carry this daemon's sealed control bundle, the same proof private-session
/// creation demands before an image may see a workspace.
pub(crate) fn reader_image(configured: Option<&str>) -> Option<String> {
    let candidates = match configured {
        Some(image) => vec![image.to_owned()],
        None => local_stock_images(),
    };
    candidates.into_iter().find(|image| {
        docker::image_present(image).is_ok()
            && docker::verify_image(image)
                .map_err(|error| log::debug!("reader image {image} rejected: {error:#}"))
                .is_ok()
    })
}

/// Local stock session images, `:latest` first and then versioned tags newest
/// first, bounded so a crowded image store cannot stall a pass.
fn local_stock_images() -> Vec<String> {
    let repo = DEFAULT_SESSION_IMAGE
        .rsplit_once(':')
        .map_or(DEFAULT_SESSION_IMAGE, |(r, _)| r);
    let args = ["image", "ls", "--format", "{{.Repository}}:{{.Tag}}", repo];
    let Ok(listing) = docker::command_with(&args, METADATA_BUDGET) else {
        return Vec::new();
    };
    let mut images: Vec<(Option<DaemonVersion>, String)> = listing
        .lines()
        .filter(|image| {
            image
                .strip_prefix(repo)
                .is_some_and(|tag| tag.starts_with(':'))
        })
        .filter(|image| {
            *image == DEFAULT_SESSION_IMAGE || DaemonVersion::from_image(image).is_some()
        })
        .map(|image| (DaemonVersion::from_image(image), image.to_owned()))
        .collect();
    images.sort_by(|a, b| {
        (b.1 == DEFAULT_SESSION_IMAGE)
            .cmp(&(a.1 == DEFAULT_SESSION_IMAGE))
            .then_with(|| b.0.cmp(&a.0))
    });
    images.dedup_by(|a, b| a.1 == b.1);
    images
        .into_iter()
        .map(|(_, image)| image)
        .take(MAX_READER_CANDIDATES)
        .collect()
}

fn verified_volume<'a>(
    inspect: &'a str,
    volume: &str,
    account: &str,
    root: &str,
) -> Option<&'a str> {
    let mut fields = inspect.trim_end_matches(['\r', '\n']).split('\t');
    for expected in [volume, "local", account, "0"] {
        if fields.next()? != expected {
            return None;
        }
    }
    let backing_path = fields.next()?;
    let created = fields.next()?;
    if created.is_empty() || created.contains(['\r', '\n']) || fields.next().is_some() {
        return None;
    }
    let root = root.trim_end_matches(['\r', '\n']);
    let components = root.strip_prefix('/')?.split('/');
    if !components.into_iter().all(|part| {
        !matches!(part, "" | "." | "..")
            && part
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'.' | b'-' | b'_'))
    }) {
        return None;
    }
    (backing_path == format!("{root}/volumes/{volume}/_data")).then_some(backing_path)
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
