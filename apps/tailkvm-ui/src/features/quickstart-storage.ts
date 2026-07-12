// Quick Start persistence + settings: the peer-attach (which local monitor edge
// the peer is pinned to), the per-host peer-screen-size cache, and the KVM
// pointer-gain. Pure storage/lookup — no rendering, no IPC — so both the
// monitor map (quickstart-map) and the flow wiring (quickstart) share one
// validated source of truth.

export const EDGE_LABEL: Record<string, string> = {
  top: "上",
  bottom: "下",
  left: "左",
  right: "右",
};

export type KvmEdge = "top" | "bottom" | "left" | "right";

// The peer (peer-pc) is pinned to one edge of one specific local monitor,
// identified by that monitor's physical-pixel rect so the backend can match it.
export type PeerAttach = {
  rect: [number, number, number, number];
  edge: KvmEdge;
  // Peer screen's virtual rect (position + real resolution) for multi-edge
  // crossing. Optional for back-compat with values saved before this field.
  peerRect?: [number, number, number, number];
};

const PEER_ATTACH_KEY = "tailkvm.peerAttach.v1";

export function getPeerAttach(): PeerAttach | null {
  try {
    const raw = localStorage.getItem(PEER_ATTACH_KEY);
    if (!raw) return null;
    const v = JSON.parse(raw) as PeerAttach;
    if (
      Array.isArray(v.rect) &&
      v.rect.length === 4 &&
      v.rect.every((n) => Number.isFinite(n)) &&
      ["top", "bottom", "left", "right"].includes(v.edge)
    ) {
      return v;
    }
  } catch {
    // ignore malformed storage
  }
  return null;
}

export function savePeerAttach(attach: PeerAttach) {
  localStorage.setItem(PEER_ATTACH_KEY, JSON.stringify(attach));
}

// Per-host cache of each peer's real virtual-screen size, learned from a live
// connection (get_peer_screen_size). Lets the position editor draw the remote
// at its true resolution even before/without a connection.
const PEER_SCREENS_KEY = "tailkvm.peerScreens.v1";
let lastPeerScreen: [number, number] | null = null;

function getPeerScreens(): Record<string, [number, number]> {
  try {
    const raw = localStorage.getItem(PEER_SCREENS_KEY);
    if (!raw) return {};
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return {};
    // Keep only entries that are a [w, h] pair of finite positive numbers, so
    // a corrupted store degrades to "unknown size" instead of NaN geometry.
    const valid: Record<string, [number, number]> = {};
    for (const [host, size] of Object.entries(parsed as Record<string, unknown>)) {
      if (
        Array.isArray(size) &&
        size.length === 2 &&
        size.every((n) => typeof n === "number" && Number.isFinite(n) && n > 0)
      ) {
        valid[host] = [size[0], size[1]];
      }
    }
    return valid;
  } catch {
    // ignore malformed storage
  }
  return {};
}

export function savePeerScreen(host: string, w: number, h: number) {
  if (!host || !(w > 0) || !(h > 0)) return;
  const all = getPeerScreens();
  all[host] = [w, h];
  localStorage.setItem(PEER_SCREENS_KEY, JSON.stringify(all));
  lastPeerScreen = [w, h];
}

export function getPeerScreenForHost(host: string): [number, number] | null {
  const cached = host ? getPeerScreens()[host] : undefined;
  if (cached && cached[0] > 0 && cached[1] > 0) return cached;
  return lastPeerScreen;
}

export function getKvmEdge(): KvmEdge {
  return getPeerAttach()?.edge ?? "bottom";
}

// KVM pointer-speed (gain): the backend scales raw mouse deltas by this so
// controlling the remote doesn't feel slow next to the local cursor.
export const KVM_GAIN_KEY = "tailkvm.kvmGain";

export function getKvmGain(): number {
  const fromInput = Number(document.querySelector<HTMLInputElement>("#qs-kvm-gain")?.value);
  const stored = Number(localStorage.getItem(KVM_GAIN_KEY));
  const g = fromInput || stored || 1.8;
  return Math.min(4, Math.max(0.5, g));
}
