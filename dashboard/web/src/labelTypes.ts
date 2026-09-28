/**
 * The `labels.snapshot` record (issue #9094): one managed repo's open issues
 * and PRs that carry a `loom:*` label, with those labels, as one host's daemon
 * last listed them. Successive snapshots of the same repo are diffed
 * client-side (`labelTracker.ts`) into the label transitions the Live board
 * shows — the forge's label state machine is Loom's coordination layer, so
 * this is how a watcher sees an issue go `loom:issue` → `loom:building` and
 * its PR go `loom:review-requested` → `loom:pr`.
 *
 * On `/api/fleet-state` the newest snapshot per repo rides on each host as
 * `hosts[<id>].labels`; on the live tail it arrives as an ordinary frame.
 * `/public/*` never carries a private repo's snapshot.
 */

export interface LabelItem {
  type: "issue" | "pr";
  number: number;
  /** Only the `loom:*` labels, in the order the forge listed them. */
  labels: string[];
  /** For a PR: the issues it closes (its `Closes #N` links). */
  closes?: number[];
}

export interface LabelsSnapshotRecord {
  kind: "labels.snapshot";
  /** Forge `owner/repo`. */
  repo: string;
  visibility: "public" | "private";
  /** When the daemon finished listing (daemon clock). Snapshots of one repo
   * are ordered by this, so an older one arriving late is ignored. */
  taken_at: string;
  items: LabelItem[];
  /** Items the daemon listed but did not send (a per-record cap). A
   * truncated snapshot is still diffed, but an item missing from it is not
   * taken to have closed. */
  truncated?: number;
}
