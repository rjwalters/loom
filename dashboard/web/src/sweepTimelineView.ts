/**
 * Per-sweep timeline view (issue #4750): renders the phase progression
 * (`curator` -> `builder` -> `judge` -> `doctor` -> `merge`) computed by
 * `SweepTimelineBuilder`, plus the terminal result / PR link once the sweep
 * completes. Framework-agnostic, plain-DOM rendering, matching
 * `liveFeedPanel.ts`.
 */

import type { SweepTimeline, TimelinePhaseEntry } from "./timelineBuilder.js";
import { SweepTimelineBuilder } from "./timelineBuilder.js";
import type { TelemetryRecord } from "./types.js";

function formatDuration(durationSec: number | undefined): string {
  if (durationSec === undefined) return "…";
  const total = Math.round(durationSec);
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  return minutes > 0 ? `${minutes}m ${seconds}s` : `${seconds}s`;
}

export function renderTimelinePhaseRow(entry: TimelinePhaseEntry): HTMLLIElement {
  const row = document.createElement("li");
  row.className = "sweep-timeline__phase";
  row.dataset.phase = entry.phase;
  if (entry.ongoing) row.classList.add("sweep-timeline__phase--ongoing");

  const name = document.createElement("span");
  name.className = "sweep-timeline__phase-name";
  name.textContent = entry.phase;

  const duration = document.createElement("span");
  duration.className = "sweep-timeline__phase-duration";
  duration.textContent = entry.ongoing ? "in progress" : formatDuration(entry.durationSec);

  row.append(name, duration);
  return row;
}

/** PR URL derived from `sweep.outcome`'s `pr_number`, when known. */
export function prLinkFor(timeline: SweepTimeline, repo?: string): string | undefined {
  if (timeline.prNumber === undefined) return undefined;
  if (!repo) return `#${timeline.prNumber}`;
  return `https://github.com/${repo}/pull/${timeline.prNumber}`;
}

export function renderSweepTimeline(container: HTMLElement, timeline: SweepTimeline, repo?: string): void {
  container.innerHTML = "";
  container.dataset.sweepId = timeline.sweepId;

  const list = document.createElement("ol");
  list.className = "sweep-timeline__phases";
  for (const entry of timeline.phases) {
    list.appendChild(renderTimelinePhaseRow(entry));
  }
  container.appendChild(list);

  if (timeline.result) {
    const result = document.createElement("div");
    result.className = "sweep-timeline__result";
    result.dataset.result = timeline.result;
    result.textContent = `Result: ${timeline.result}`;
    container.appendChild(result);

    const prLink = prLinkFor(timeline, repo);
    if (prLink) {
      const link = document.createElement("a");
      link.className = "sweep-timeline__pr-link";
      link.href = prLink;
      link.textContent = `PR #${timeline.prNumber}`;
      container.appendChild(link);
    }
  }
}

/**
 * Owns a `SweepTimelineBuilder` for one `sweepId` and re-renders into
 * `container` on every ingested record — whether streamed live (feed a
 * matching `LiveTailFrame.event.record`) or backfilled from
 * `GET /api/history?...&limit=...` for phases the live feed missed.
 */
export class SweepTimelineView {
  private readonly builder: SweepTimelineBuilder;
  private readonly container: HTMLElement;
  private readonly repo: string | undefined;

  constructor(sweepId: string, container: HTMLElement, repo?: string) {
    this.builder = new SweepTimelineBuilder(sweepId);
    this.container = container;
    this.repo = repo;
  }

  ingest(record: TelemetryRecord): void {
    this.builder.addRecord(record);
    this.render();
  }

  ingestAll(records: TelemetryRecord[]): void {
    this.builder.addRecords(records);
    this.render();
  }

  render(): void {
    renderSweepTimeline(this.container, this.builder.getTimeline(), this.repo);
  }
}
