// The console's whole view of the server. Every route is a GET — the console
// watches boxes and never drives them (see crates/h5i-core/src/server.rs).
//
// Authorization is ambient: the page was loaded with `?token=…`, which the
// server traded for a SameSite=Strict cookie, so `fetch` needs nothing but
// `credentials: "same-origin"`.

// ── mirrors of the Rust types ────────────────────────────────────────────────

/** `h5i_core::env::EnvManifest`, flattened into every fleet row. */
export interface EnvManifest {
  id: string;
  agent: string;
  slug: string;
  base_commit: string;
  base_tree: string;
  parent_branch: string;
  branch: string;
  source: string;
  profile: string;
  policy_digest: string;
  isolation_claim: string;
  backend: string;
  created_at: string;
  updated_at: string;
  status: string;
  captures: string[];
  service_digest?: string;
  persona_digest?: string;
  pr?: number;
  pr_head_ref?: string;
}

export interface LiveSession {
  pid: number;
  kind: string;
  started_at: string;
  command?: string;
}

export interface EnvEvent {
  ts: string;
  env_id: string;
  agent: string;
  event: string;
  detail?: string;
  capture?: string;
}

export interface EgressHost {
  host: string;
  port: number;
  allowed: number;
  denied: number;
}

export interface EgressSummary {
  allowed: number;
  denied: number;
  hosts?: EgressHost[];
  hosts_truncated?: boolean;
  log?: string;
}

export interface BrowserEvidence {
  verb?: string;
  console?: string[];
  errors?: string[];
  failed_requests?: string[];
  truncated?: boolean;
  unavailable?: boolean;
}

/**
 * What an ingress session was — `h5i_core::receipt::ShareEvidence`. Present on
 * a receipt whose `source` is `share`, and on no other.
 *
 * Host-observed without qualification: h5i owned both ends of the bridge and
 * the box supplied none of it.
 */
export interface ShareEvidence {
  /** `p2p` or `tunnel`, as `h5i-share` recorded it. */
  transport: string;
  /** The port inside the box that was exposed. */
  port: number;
  /** Distinct peers admitted, including any past the receipt's record cap. */
  peers: number;
  /** How long the share ran, from the monotonic clock. */
  seconds: number;
  /** Connections refused before a ticket was weighed at all. */
  turned_away?: number;
}

/**
 * Can a third party read this share's traffic?
 *
 * The rule is `ShareEvidence::third_party_can_read` in `h5i-core`, mirrored
 * here for the per-row case; the fleet-wide counts in {@link Signals} are
 * computed by that method itself. Read off the transport *field* — recovering
 * it from the rendered command line is what once reported a plain P2P share of
 * a box named `tunnel` as Cloudflare-terminated. A transport this console does
 * not know is not a promise of end-to-end encryption, so it answers true.
 */
export function thirdPartyCanRead(share: ShareEvidence): boolean {
  return share.transport !== "p2p";
}

/**
 * One rule that fired, folded across every event that matched it —
 * `h5i_bpf::Detection`.
 */
export interface Detection {
  rule: string;
  family: string;
  severity: "info" | "notice" | "alert";
  title: string;
  count: number;
  first_ns: number;
  last_ns: number;
  examples?: string[];
  examples_truncated?: boolean;
}

/**
 * What the kernel-observed lane saw — `h5i_bpf::RuntimeEvidence`.
 *
 * The one thing a renderer must never do with this is treat an empty
 * `detections` list as a clean run. It is only clean if the run was watched,
 * which is `unavailable == null && coverage !== "none"`; anything else means
 * nobody looked. {@link runtimeObserved} is that test, in one place.
 */
export interface RuntimeEvidence {
  lane: string;
  scope: string;
  coverage: "full" | "partial" | "none";
  coverage_reason?: string;
  events_seen?: number;
  events_lost?: number;
  events_filtered?: number;
  detections?: Detection[];
  unavailable?: string;
}

/** Was this run actually watched? Mirrors `RuntimeEvidence::observed`. */
export function runtimeObserved(rt: RuntimeEvidence): boolean {
  return !rt.unavailable && rt.coverage !== "none";
}

/** One observed execution — `h5i_core::receipt::ExecRecord`. */
export interface ExecRecord {
  id: string;
  timestamp: string;
  env_id: string;
  policy_digest?: string;
  /** sha256 of policy.effective.json as this run enforced it. */
  effective_digest?: string;
  /** Boxes whose effective grants overlap this one's (`env/<id> via <path>`). */
  fs_overlap?: string[];
  /** Which lane observed this: `host-env-run`, `tee-shim`, `shell-egress`. */
  source: string;
  cmd?: string;
  cwd?: string;
  exit_code?: number;
  timed_out?: boolean;
  wall_ms?: number;
  cpu_ms?: number;
  max_rss_kb?: number;
  git_tree?: string;
  files?: string[];
  egress?: EgressSummary;
  browser?: BrowserEvidence;
  share?: ShareEvidence;
  /** What an eBPF collector saw from the kernel, when one was watching. */
  runtime?: RuntimeEvidence;
  redactions?: string[];
  raw_oid: string;
  raw_size: number;
  raw_lines: number;
  raw_truncated?: boolean;
}

export type Verdict = "denial" | "attention" | "clean";

/** Arithmetic over one box's receipts. Nothing here is a score. */
export interface Signals {
  runs: number;
  failed: number;
  timed_out: number;
  egress_allowed: number;
  egress_denied: number;
  denied_hosts: string[];
  browser_issues: number;
  host_observed: number;
  box_claimed: number;
  last_run_ts?: string;
  verdict: Verdict;
  weak_isolation: boolean;
  box_claimed_only: boolean;
  /** Runs an eBPF collector actually watched. */
  kernel_watched?: number;
  /** Runs that carry a runtime block which observed nothing, and why. */
  kernel_unwatched?: number;
  /** `alert`-severity matches, summed over watched runs. */
  kernel_alerts?: number;
  /** `notice`-severity matches. */
  kernel_notices?: number;
  /** Events dropped before anything could examine them. */
  kernel_events_lost?: number;
  /** Distinct rule ids that fired, capped. */
  kernel_rules?: string[];
  /** Ended share sessions on this log. Never folded into the verdict. */
  shares: number;
  /** Of those, the ones a third party could read. */
  shares_third_party_readable: number;
  /** Distinct peers admitted across every recorded share. */
  share_peers: number;
  /** Boxes overlapping this one's writable grants, per the newest run. */
  fs_overlap: string[];
}

/** A share serving this box right now. Absent when nobody is being let in. */
export interface SharedNow {
  transport: string;
  port: number;
  grants: number;
}

export interface BoxRow extends EnvManifest {
  drift: string;
  drift_summary: string;
  live: LiveSession[];
  stale_running: boolean;
  has_workspace: boolean;
  files_changed: number;
  insertions: number;
  deletions: number;
  last_event?: EnvEvent;
  signals: Signals;
  shared_now?: SharedNow | null;
}

export interface EnforcedPolicy {
  isolation: string;
  net_mode: string;
  net_egress: string[];
  fs_read: string[];
  fs_write: string[];
  fs_deny: string[];
  tools: string[];
  env_pass: string[];
  image?: string;
  mem_bytes?: number;
  max_procs?: number;
  /** False when this tier cannot apply mem_bytes on the serving host. */
  mem_enforced?: boolean;
  /** False when this tier cannot apply max_procs on the serving host. */
  procs_enforced?: boolean;
  wall_secs: number;
  cpu_secs?: number;
  fsize_bytes?: number;
}

export interface ServiceStatus {
  name: string;
  pid: number;
  command: string;
  started_at: string;
  port?: number;
  dynamic_port?: number;
  log: string;
  alive: boolean;
}

export interface BoxDetail {
  item: BoxRow;
  policy?: EnforcedPolicy;
  events: EnvEvent[];
  receipts: ExecRecord[];
  receipts_folded: number;
  services: ServiceStatus[];
  diffstat?: string;
}

export interface ClaimSupport {
  claim: string;
  satisfiable: boolean;
  runnable?: boolean;
  note?: string;
}

/** `h5i box capabilities --json`, verbatim. */
export interface CapabilitiesReport {
  os: string;
  landlock_abi?: number | null;
  userns: boolean;
  seccomp: boolean;
  seatbelt: boolean;
  mechanism: string;
  syscall_filter: boolean;
  memory_limit: boolean;
  container_runtime?: string | null;
  egress_enforced: boolean;
  resource_limits: boolean;
  claims: ClaimSupport[];
  strongest_tier: string;
}

// ── the browser terminal's stream (ROADMAP M11a) ─────────────────────────────

/**
 * Who observed an event. `host-observed` means h5i saw it from outside the box
 * and the box cannot edit it; `box-claimed` means the box said so.
 */
export type Lane = "host-observed" | "box-claimed";

/**
 * How complete the observation is. `fail-closed` means the record is a
 * precondition of the act — no record, no act. `best-effort` means it can miss
 * things without noticing. Orthogonal to {@link Lane}: the light engine's
 * request log is box-claimed *and* fail-closed.
 */
export type Grade = "fail-closed" | "best-effort";

export type ConsoleLevel = "warning" | "error" | "page-error";
export type Initiator = "navigation" | "subresource" | "redirect" | "other";

/** The envelope every row carries — `h5i_core::browser_events::ViewerEvent`. */
interface EventBase {
  id: number;
  /** When h5i *read* this. Not when the box produced it. */
  observed_at: string;
  lane: Lane;
  grade: Grade;
  /** Set only where the source carried the link. Never inferred. */
  caused_by?: number;
}

export type ViewerEvent = EventBase &
  (
    | { kind: "navigated"; url: string }
    | {
        kind: "request";
        seq: number;
        method: string;
        url: string;
        initiator: Initiator;
        allowed: boolean;
        denied_reason?: string;
      }
    | {
        kind: "response";
        seq: number;
        status?: number;
        bytes?: number;
        duration_ms?: number;
        error?: string;
      }
    | { kind: "console"; level: ConsoleLevel; text: string }
    | { kind: "agent-action"; action: string; forwarded: boolean }
    | { kind: "policy-verdict"; subject: string; reason: string }
    /**
     * A source restarted, so everything after this row belongs to a new
     * browser session. Rendered in every pane: it is a boundary for the whole
     * stream, not news about one lane.
     */
    | { kind: "session-reset"; source: string }
  );

export interface BrowserStream {
  events: ViewerEvent[];
  cursor: number;
  /** Events the cap discarded. Shown, never hidden. */
  dropped: number;
  /** The pinned engine, which is what the network pane's grade follows from. */
  engine?: string;
  /**
   * Whether a live view is being served inside the box right now. The console
   * does not relay those frames — the forward does, with its own token and the
   * control lock — so this only tells the page pane which state to describe.
   */
  live_view: boolean;
  /**
   * Sequence number of the newest frame the console holds. The page re-fetches
   * the image only when this changes, so a still page costs nothing.
   */
  frame_seq?: number;
  /** Why there is no picture, when a live view exists but no frame arrived. */
  frame_error?: string;
}

// ── browser sessions ─────────────────────────────────────────────────────────

/** `h5i_core::session_view::Attention`: how loudly a session is asking. */
export interface Attention {
  /** `blocked`, `working`, `done`, `idle`, `unknown`. */
  state: string;
  /** The evidence behind the state. A badge without this would be a score. */
  why: string;
}

/** A recon ledger, folded into counts by state. */
export interface LedgerCounts {
  candidate: number;
  observed: number;
  confirmed: number;
  refused: number;
  gone: number;
  cursor: number;
  unreadable: number;
  truncated: boolean;
}

/** One recorded recon run that spent requests. */
export interface JobRow {
  id: string;
  verb: string;
  started_at: string;
  ended_at: string | null;
  requests: number;
  written: number;
  stopped: string | null;
}

export interface SessionRow {
  id: string;
  name: string | null;
  state: string;
  placement: string;
  lane: string;
  identity: string;
  url: string;
  started_at: string;
  ended_at: string | null;
  held_by_human: boolean;
  requests: number;
  denied: number;
  origins: string[];
  last_request_at: string | null;
  /** How many messages the store holds, or null when capture was off. The
   *  bytes stay on disk: reading them is a CLI act. */
  captured: number | null;
  ledger: LedgerCounts | null;
  jobs: JobRow[];
  attention: Attention;
}

/** One endpoint in the recon ledger. */
export interface EndpointRow {
  id: string;
  origin: string;
  path: string;
  method: string;
  identity: string;
  state: string;
  /** How each row was learned: `{ from: "page" | "calibration" | … }`. */
  sources: { from: string }[];
  evidence: string[];
  params: { name: string; at: string }[];
  status: number | null;
  cluster: string | null;
  reason: string | null;
  first_seen: string;
  last_seen: string;
  line: number;
}

/** What `/api/sessions` answers: the newest sessions, and what did not fit. */
export interface SessionFleet {
  sessions: SessionRow[];
  total: number;
  live: number;
}

export interface SessionDetail extends SessionRow {
  /** Receipts, oldest first, both phases as the log holds them. */
  requests_log: RequestRecord[];
  endpoints: EndpointRow[];
}

/** One line of the request log. Counts and names, never values. */
export interface RequestRecord {
  seq: number;
  at: string;
  phase: string;
  initiator: string;
  method: string;
  url: string;
  allowed: boolean;
  denied_reason?: string;
  status?: number;
  bytes?: number;
  duration_ms?: number;
  error?: string;
}

// ── transport ────────────────────────────────────────────────────────────────

async function get<T>(path: string): Promise<T> {
  const res = await fetch(path, { credentials: "same-origin" });
  if (!res.ok) {
    // 401 here means the cookie is gone (or was never set): the page was
    // opened without the token h5i printed. Say that, rather than "500".
    if (res.status === 401) {
      throw new Error(
        "not authorized — reopen the URL `h5i ui` printed, token and all",
      );
    }
    throw new Error(`${res.status} ${res.statusText}`);
  }
  return (await res.json()) as T;
}

export const api = {
  boxes: () => get<BoxRow[]>("/api/boxes"),
  box: (agent: string, slug: string) =>
    get<BoxDetail>(`/api/box/${encodeURIComponent(agent)}/${encodeURIComponent(slug)}`),
  receipt: (agent: string, slug: string, id: string) =>
    get<{ render: string }>(
      `/api/box/${encodeURIComponent(agent)}/${encodeURIComponent(slug)}/receipts/${encodeURIComponent(id)}`,
    ),
  probe: () => get<CapabilitiesReport>("/api/probe"),
  sessions: () => get<SessionFleet>("/api/sessions"),
  session: (id: string) =>
    get<SessionDetail>(`/api/session/${encodeURIComponent(id)}`),
  browser: (agent: string, slug: string, since: number) =>
    get<BrowserStream>(
      `/api/box/${encodeURIComponent(agent)}/${encodeURIComponent(slug)}/browser?since=${since}`,
    ),
  /**
   * URL of the newest frame, for an `<img>` rather than a fetch — the browser
   * decodes the JPEG, so the bytes never become a string in this app. `seq` is
   * in the URL purely as a cache key: the same seq is the same picture, and a
   * changed seq is what makes the element reload.
   */
  browserFrameUrl: (agent: string, slug: string, seq: number) =>
    `/api/box/${encodeURIComponent(agent)}/${encodeURIComponent(slug)}/browser/frame?seq=${seq}`,
};
