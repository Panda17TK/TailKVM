// Shared domain / IPC types for the TailKVM UI. These shapes mirror the Rust
// command return values (Tauri `invoke`) and must stay in sync with the backend.

export type TailnetNode = {
  id: string;
  host_name: string;
  dns_name?: string | null;
  os?: string | null;
  online: boolean;
  active?: boolean | null;
  tailscale_ips: string[];
  user?: string | null;
  relay?: string | null;
  cur_addr?: string | null;
  last_seen?: string | null;
  tx_bytes?: number | null;
  rx_bytes?: number | null;
};

export type TailnetStatus = {
  backend_state: string;
  self_node?: TailnetNode | null;
  peers: TailnetNode[];
  raw_peer_count: number;
};

export type RectI32 = {
  left: number;
  top: number;
  right: number;
  bottom: number;
  width: number;
  height: number;
};

export type MonitorInfo = {
  id: string;
  name: string;
  rect_physical_px: RectI32;
  work_area_physical_px: RectI32;
  dpi_x: number;
  dpi_y: number;
  scale_factor: number;
  is_primary: boolean;
};

export type MonitorTopology = {
  virtual_screen: RectI32;
  monitors: MonitorInfo[];
};

export type KeyboardLayoutInfo = {
  hkl: number;
  language_id: number;
  primary_language: number;
  is_japanese_locale: boolean;
  keyboard_type: number;
  keyboard_subtype: number;
  function_keys: number;
  is_jis_keyboard: boolean;
  label: string;
};

export type TcpSessionSnapshot = {
  role: string;
  listening: boolean;
  listen_addr?: string | null;
  connected: boolean;
  peer_addr?: string | null;
  peer_name?: string | null;
  heartbeat_seq: number;
  last_heartbeat_ms?: number | null;
  last_event: string;
  local_keyboard_layout?: string | null;
  peer_keyboard_layout?: string | null;
  keyboard_layout_warning?: string | null;
  ime_mode?: string;
  /** Backend-synthesized truth: this machine is currently capturing/forwarding
   *  local input (any capture loop, low-level hook, or router is live). */
  capture_active?: boolean;
};

export type LayoutRect = {
  x: number;
  y: number;
  width: number;
  height: number;
};

export type SavedDisplayLayout = {
  targetPeerIp: string;
  targetPeerHost: string;
  remoteRect: LayoutRect;
  switchEdge: "left" | "right" | "top" | "bottom";
};
