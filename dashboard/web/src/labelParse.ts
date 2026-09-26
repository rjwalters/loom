/**
 * Wire JSON → `LabelsSnapshotRecord` (issue #9094). Same rules as `parse.ts`
 * and `queueParse.ts`: never throw, drop wrong-typed fields, and treat
 * anything but the exact string `"public"` as private.
 */

import type { LabelItem, LabelsSnapshotRecord } from "./labelTypes";

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function str(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function posInt(value: unknown): number | undefined {
  return typeof value === "number" && Number.isInteger(value) && value > 0 ? value : undefined;
}

export function parseLabelItem(value: unknown): LabelItem | undefined {
  if (!isObject(value)) return undefined;
  const type = value.type === "issue" || value.type === "pr" ? value.type : undefined;
  const number = posInt(value.number);
  if (type === undefined || number === undefined) return undefined;
  const labels = Array.isArray(value.labels)
    ? value.labels.filter((label): label is string => typeof label === "string" && label.length > 0)
    : [];
  const item: LabelItem = { type, number, labels };
  if (type === "pr" && Array.isArray(value.closes)) {
    const closes = value.closes.map(posInt).filter((issue): issue is number => issue !== undefined);
    if (closes.length > 0) item.closes = closes;
  }
  return item;
}

/** `undefined` when there is no repo or no parseable `taken_at`: a snapshot
 * that cannot be attributed or ordered cannot be diffed. */
export function parseLabelsSnapshot(value: unknown): LabelsSnapshotRecord | undefined {
  if (!isObject(value)) return undefined;
  const repo = str(value.repo);
  const takenAt = str(value.taken_at);
  if (repo === undefined || takenAt === undefined || Number.isNaN(Date.parse(takenAt))) return undefined;
  const record: LabelsSnapshotRecord = {
    kind: "labels.snapshot",
    repo,
    visibility: value.visibility === "public" ? "public" : "private",
    taken_at: takenAt,
    items: Array.isArray(value.items)
      ? value.items.map(parseLabelItem).filter((item): item is LabelItem => item !== undefined)
      : [],
  };
  const truncated = posInt(value.truncated);
  if (truncated !== undefined) record.truncated = truncated;
  return record;
}
