// Cross-cutting mutable app state. Only the two values that are written by one
// surface and read by several others live here; per-surface state stays local to
// its feature module. A plain object (not `let` exports) so any module can mutate
// the shared fields through a stable binding.

import type { MonitorTopology, TailnetStatus } from "./types";

export const appState: {
  latestTailnetStatus: TailnetStatus | null;
  latestMonitorTopology: MonitorTopology | null;
} = {
  latestTailnetStatus: null,
  latestMonitorTopology: null,
};
