/**
 * Live event feed panel (issue #4750). A thin, framework-agnostic DOM
 * renderer over `LiveFeedClient` — deliberately built with plain DOM APIs
 * (no framework dependency yet) so it can be dropped into whatever
 * scaffold the sibling Fleet-overview issue (#4749) lands with.
 */

import type { LiveTailFrame } from "./types.js";
import { LiveFeedClient, type LiveFeedClientOptions } from "./sseFeedClient.js";

/** Client-side filter over a frame's `record` — server doesn't filter model/result on the live tail. */
export interface LiveFeedFilter {
  model?: string;
  result?: string;
}

function matchesFilter(frame: LiveTailFrame, filter: LiveFeedFilter): boolean {
  const record = frame.event.record as Record<string, unknown>;
  if (filter.model && record.model !== filter.model) return false;
  if (filter.result && record.result !== filter.result) return false;
  return true;
}

export function renderLiveFeedRow(frame: LiveTailFrame): HTMLLIElement {
  const row = document.createElement("li");
  row.className = "live-feed-row";
  row.dataset.topic = frame.topic;

  const time = document.createElement("span");
  time.className = "live-feed-row__time";
  time.textContent = frame.event.emittedAt;

  const host = document.createElement("span");
  host.className = "live-feed-row__host";
  host.textContent = frame.event.hostId;

  const topic = document.createElement("span");
  topic.className = "live-feed-row__topic";
  topic.textContent = frame.topic;

  row.append(time, host, topic);
  return row;
}

export interface LiveFeedPanelOptions extends Omit<LiveFeedClientOptions, "onEvent"> {
  container: HTMLElement;
  maxRows?: number;
  filter?: LiveFeedFilter;
}

/**
 * Owns a `LiveFeedClient` and renders each accepted frame as a row,
 * newest-first, capped at `maxRows`. Filtering is client-side only — the
 * live-tail endpoint only supports `host`/`repo` server-side (see
 * `dashboard/docs/query-api.md`'s "Not implemented here" section).
 */
export class LiveFeedPanel {
  private readonly container: HTMLElement;
  private readonly maxRows: number;
  private readonly client: LiveFeedClient;
  private filter: LiveFeedFilter;

  constructor(options: LiveFeedPanelOptions) {
    this.container = options.container;
    this.maxRows = options.maxRows ?? 200;
    this.filter = options.filter ?? {};

    const { container: _container, maxRows: _maxRows, filter: _filter, ...clientOptions } = options;
    this.client = new LiveFeedClient({
      ...clientOptions,
      onEvent: (frame) => this.handleEvent(frame),
    });
  }

  start(): void {
    this.client.start();
  }

  stop(): void {
    this.client.stop();
  }

  setFilter(filter: LiveFeedFilter): void {
    this.filter = filter;
  }

  private handleEvent(frame: LiveTailFrame): void {
    if (!matchesFilter(frame, this.filter)) return;

    const row = renderLiveFeedRow(frame);
    this.container.insertBefore(row, this.container.firstChild);

    while (this.container.childElementCount > this.maxRows) {
      this.container.lastElementChild?.remove();
    }
  }
}
