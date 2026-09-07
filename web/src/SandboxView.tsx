import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactElement,
} from "react";
import {
  Callout,
  Code,
  HTMLTable,
  NonIdealState,
  Spinner,
  Tag,
} from "@blueprintjs/core";

import { BrowserTerminal } from "./BrowserTerminal";
import {
  AttentionBar,
  SessionDetailPane,
  SessionList,
  loadSeen,
  markSeen,
  shownState,
} from "./SessionsPane";
import {
  api,
  type BoxDetail,
  type BoxRow,
  type CapabilitiesReport,
  type EnforcedPolicy,
  type EnvEvent,
  type ExecRecord,
  type ServiceStatus,
  type ShareEvidence,
  type SharedNow,
  type SessionRow,
  type Signals,
  runtimeObserved,
  thirdPartyCanRead,
} from "./api";

// The box console: a read-only operator view of the h5i fleet.

const LANES: { key: LaneKey; label: string; hint: string }[] = [
  { key: "fs", label: "FS", hint: "filesystem reach" },
  { key: "net", label: "NET", hint: "network egress" },
  { key: "proc", label: "PROC", hint: "process / exit status" },
  { key: "res", label: "RES", hint: "resource limits" },
  { key: "browser", label: "PAGE", hint: "what the in-box browser saw" },
  // The kernel lane sits last because it is the newest and the narrowest, not
  // because it is the weakest — it is the only column here a box cannot write.
  { key: "kernel", label: "KERN", hint: "what an eBPF collector saw from the kernel" },
];

type LaneKey = "fs" | "net" | "proc" | "res" | "browser" | "kernel";

const POLL_MS = 8000;

/** Which half of the fleet the left column is showing. */
type Kind = "boxes" | "sessions";

export function SandboxView() {
  const [boxes, setBoxes] = useState<BoxRow[] | null>(null);
  const [sessions, setSessions] = useState<SessionRow[] | null>(null);
  // What the registry holds against what this poll read: a machine that has run
  // a thousand sessions should be told that, not shown a list that pretends to
  // be all of them.
  const [sessionTotal, setSessionTotal] = useState<{ total: number; live: number }>({
    total: 0,
    live: 0,
  });
  const [kind, setKind] = useState<Kind>("boxes");
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [attention, setAttention] = useState<string>("all");
  // What this browser has already looked at. Per client on purpose: the server
  // has no idea who has read what, and a console that wrote a "seen" flag back
  // would be a passive view with a side effect.
  const [seen, setSeen] = useState<Record<string, string>>(loadSeen);
  const [probe, setProbe] = useState<CapabilitiesReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [filter, setFilter] = useState<string>("all");
  // Bumped on every successful fleet poll, so the detail pane refreshes with it.
  const [tick, setTick] = useState(0);
  // Width of the fleet column. Read from storage once, on the initialiser, so
  // the first paint is already at the remembered width rather than jumping.
  const [split, setSplit] = useState<number>(loadSplit);

  useEffect(() => {
    try {
      window.localStorage.setItem(SPLIT_KEY, String(Math.round(split)));
    } catch {
      // Storage refused (private mode, disabled). The width still works for
      // this session; only the memory of it is lost.
    }
  }, [split]);

  // A window that shrinks below the saved width would otherwise leave no room
  // for the detail pane at all.
  useEffect(() => {
    const onResize = () => setSplit((w) => clampSplit(w));
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);

  const load = useCallback(() => {
    api
      .boxes()
      .then((b) => {
        setBoxes(b);
        setError(null);
        setTick((t) => t + 1);
      })
      .catch((e) => setError(String(e instanceof Error ? e.message : e)));
    // Sessions are their own registry: a session needs no box, and the two
    // lists fail independently. A fleet that cannot read one still shows the
    // other.
    api
      .sessions()
      .then((fleet) => {
        setSessions(fleet.sessions);
        setSessionTotal({ total: fleet.total, live: fleet.live });
      })
      .catch(() => setSessions([]));
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, POLL_MS);
    return () => clearInterval(t);
  }, [load]);

  // The probe shells out to `podman info`; it is host state, not fleet state,
  // so it is fetched once rather than on every poll.
  useEffect(() => {
    api.probe().then(setProbe).catch(() => setProbe(null));
  }, []);

  // Open on whichever half has something in it. A repository with no boxes
  // would otherwise greet its reader with an empty column while the sessions
  // they came to see sat behind a tab.
  const [chose, setChose] = useState(false);
  useEffect(() => {
    if (chose || boxes === null || sessions === null) return;
    if (boxes.length === 0 && sessions.length > 0) setKind("sessions");
    setChose(true);
  }, [boxes, sessions, chose]);

  // Keep a selection valid as the fleet refreshes; default to the most
  // pressing box, which the server already sorted to the top.
  useEffect(() => {
    if (!boxes) return;
    setSelectedId((prev) => {
      if (prev && boxes.some((b) => b.id === prev)) return prev;
      return boxes[0]?.id ?? null;
    });
  }, [boxes]);

  const filtered = useMemo(
    () => (boxes ? boxes.filter((b) => matchesFilter(b, filter)) : null),
    [boxes, filter],
  );

  const selected = useMemo(
    () => boxes?.find((b) => b.id === selectedId) ?? null,
    [boxes, selectedId],
  );

  // Looking at a session is what clears its `done`. Recorded here rather than
  // sent anywhere: each client keeps its own memory of what it has read.
  const selectSession = useCallback(
    (id: string) => {
      setSessionId(id);
      const row = sessions?.find((s) => s.id === id);
      if (row && shownState(row, seen) === "done") {
        setSeen(markSeen(row));
      }
    },
    [sessions, seen],
  );

  if (error) {
    return (
      <div className="sbx-shell">
        <NonIdealState
          icon="error"
          title="Could not read the fleet"
          description={error}
        />
      </div>
    );
  }

  return (
    <div className="sbx-shell">
      <TopStrip probe={probe} boxes={boxes} />
      <AttentionBar
        sessions={sessions}
        boxes={
          boxes
            ? {
                refusing: boxes.filter((b) => b.signals.egress_denied > 0).length,
                failing: boxes.filter(
                  (b) => b.signals.failed > 0 || b.signals.timed_out > 0,
                ).length,
              }
            : null
        }
        filter={attention}
        onFilter={(f) => {
          setAttention(f);
          if (f !== "all") setKind("sessions");
        }}
        onBoxFilter={(f) => {
          setKind("boxes");
          setFilter(f);
        }}
        seen={seen}
      />
      <div
        className="sbx-body"
        style={{ gridTemplateColumns: `${split}px 1px 1fr` }}
      >
        <div className="sbx-fleet">
          <div className="sbx-kinds">
            {(["boxes", "sessions"] as Kind[]).map((k) => (
              <button
                key={k}
                type="button"
                className={`sbx-kind${kind === k ? " is-on" : ""}`}
                onClick={() => setKind(k)}
                title={
                  k === "boxes"
                    ? "boxes of the repository this console was started in"
                    : `browser sessions on this machine: ${sessionTotal.live} live of ${sessionTotal.total} recorded, newest read first`
                }
              >
                {k}
                <span className="sbx-tab-n">
                  {k === "boxes"
                    ? (boxes?.length ?? 0)
                    : sessionTotal.total > (sessions?.length ?? 0)
                      ? `${sessions?.length ?? 0}/${sessionTotal.total}`
                      : (sessions?.length ?? 0)}
                </span>
              </button>
            ))}
          </div>
          {kind === "boxes" ? (
            <FleetPane
              boxes={filtered}
              total={boxes?.length ?? 0}
              filter={filter}
              onFilter={setFilter}
              selectedId={selectedId}
              onSelect={setSelectedId}
            />
          ) : (
            <SessionList
              sessions={sessions}
              total={sessionTotal.total}
              seen={seen}
              filter={attention}
              selectedId={sessionId}
              onSelect={selectSession}
            />
          )}
        </div>
        <Divider width={split} onWidth={setSplit} />
        {kind === "sessions" && sessionId ? (
          <SessionDetailPane id={sessionId} tick={tick} />
        ) : kind === "sessions" ? (
          <div className="sbx-detail">
            <div className="sbx-pane-empty">pick a session</div>
          </div>
        ) : (
          <DetailPane box={selected} tick={tick} />
        )}
      </div>
    </div>
  );
}

// ── the split ────────────────────────────────────────────────────────────────

/** Bounds on the fleet column, in pixels. Narrow enough to get out of the way
 *  of the browser terminal, wide enough that a box row is still readable. */
const SPLIT_MIN = 260;
const SPLIT_MAX = 900;
const SPLIT_DEFAULT = 380;
const SPLIT_KEY = "h5i.console.split";

/** Remembered across reloads, because re-dragging the same divider every time
 *  the page reloads is the kind of small friction that makes a tool feel
 *  disposable. `localStorage` can throw (private mode, storage disabled), and a
 *  console that fails to render because it could not save a pane width would be
 *  a bad trade — so both directions swallow. */
function loadSplit(): number {
  try {
    const raw = window.localStorage.getItem(SPLIT_KEY);
    const n = raw === null ? NaN : Number.parseInt(raw, 10);
    return Number.isFinite(n) ? clampSplit(n) : SPLIT_DEFAULT;
  } catch {
    return SPLIT_DEFAULT;
  }
}

function clampSplit(n: number): number {
  // Also bounded by the window, or a saved width from a wide monitor leaves the
  // detail pane invisible on a laptop.
  const ceiling = Math.min(SPLIT_MAX, Math.max(SPLIT_MIN, window.innerWidth - 360));
  return Math.min(ceiling, Math.max(SPLIT_MIN, n));
}

/**
 * The drag handle between the fleet and the detail pane.
 *
 * A `separator` with `aria-valuenow`, focusable, and movable with the arrow
 * keys: a divider that only responds to a mouse is one a keyboard user cannot
 * reach at all, and this one decides how much of the screen the browser
 * terminal gets.
 *
 * Pointer events rather than mouse events, so a trackpad, a pen and a touch
 * screen all work; `setPointerCapture` keeps the drag alive when the pointer
 * outruns the 1px handle, which at speed it always does.
 */
function Divider({
  width,
  onWidth,
}: {
  width: number;
  onWidth: (n: number) => void;
}) {
  const dragging = useRef(false);

  const onPointerDown = useCallback((e: React.PointerEvent<HTMLDivElement>) => {
    dragging.current = true;
    e.currentTarget.setPointerCapture(e.pointerId);
    // Without this the drag selects the fleet list's text as it crosses it.
    document.body.classList.add("sbx-dragging");
  }, []);

  const onPointerMove = useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      if (!dragging.current) return;
      // The x of the pointer *is* the new column width: the grid starts at the
      // body's left edge, so no offset bookkeeping is needed.
      const left = e.currentTarget.parentElement?.getBoundingClientRect().left ?? 0;
      onWidth(clampSplit(e.clientX - left));
    },
    [onWidth],
  );

  const onPointerUp = useCallback((e: React.PointerEvent<HTMLDivElement>) => {
    dragging.current = false;
    e.currentTarget.releasePointerCapture(e.pointerId);
    document.body.classList.remove("sbx-dragging");
  }, []);

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLDivElement>) => {
      const step = e.shiftKey ? 64 : 16;
      if (e.key === "ArrowLeft") onWidth(clampSplit(width - step));
      else if (e.key === "ArrowRight") onWidth(clampSplit(width + step));
      else if (e.key === "Home") onWidth(SPLIT_MIN);
      else if (e.key === "End") onWidth(clampSplit(SPLIT_MAX));
      else return;
      e.preventDefault();
    },
    [width, onWidth],
  );

  return (
    <div
      className="sbx-divider"
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize the fleet list"
      aria-valuenow={Math.round(width)}
      aria-valuemin={SPLIT_MIN}
      aria-valuemax={SPLIT_MAX}
      tabIndex={0}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      onKeyDown={onKeyDown}
      onDoubleClick={() => onWidth(SPLIT_DEFAULT)}
      title="Drag to resize · double-click to reset · arrow keys when focused"
    />
  );
}

// ── top strip: host readiness + fleet vitals ─────────────────────────────────

function TopStrip({
  probe,
  boxes,
}: {
  probe: CapabilitiesReport | null;
  boxes: BoxRow[] | null;
}) {
  const active =
    boxes?.filter((b) => ["running", "idle", "created"].includes(b.status))
      .length ?? 0;
  const proposed = boxes?.filter((b) => b.status === "proposed").length ?? 0;
  const denials =
    boxes?.reduce((n, b) => n + (b.signals.egress_denied > 0 ? 1 : 0), 0) ?? 0;
  const runs = boxes?.reduce((n, b) => n + b.signals.runs, 0) ?? 0;

  return (
    <div className="sbx-strip">
      <div className="sbx-strip-group">
        <span
          className="sbx-brand"
          title="h5i — read-only · loopback only · lifecycle verbs stay in the CLI"
        >
          h5i
        </span>
        <span className="sbx-strip-label">host</span>
        {probe ? (
          <>
            {probe.claims.map((c) => (
              <Tag
                key={c.claim}
                minimal
                // `satisfiable` says the policy resolves; `runnable` is the
                // functional exec self-test. A tier that resolves but will not
                // run is not a green tick.
                intent={
                  c.satisfiable && c.runnable !== false ? "success" : "none"
                }
                title={
                  c.note ??
                  (c.runnable === false
                    ? "policy resolves here, but a confined exec fails"
                    : undefined)
                }
              >
                {c.claim} {c.satisfiable && c.runnable !== false ? "✓" : "✗"}
              </Tag>
            ))}
            <Tag
              minimal
              intent={probe.egress_enforced ? "success" : "none"}
              title={
                probe.egress_enforced
                  ? "a domain allowlist can be enforced here"
                  : "no allowlist enforcement here — the kernel tiers deny all or allow all"
              }
            >
              egress {probe.egress_enforced ? "✓" : "✗"}
            </Tag>
            <Tag
              minimal
              intent={probe.resource_limits ? "success" : "none"}
              title={
                probe.memory_limit
                  ? "cpu / procs / wall / memory limits enforceable"
                  : "no memory cap on this host (see the Seatbelt resource note)"
              }
            >
              limits {probe.resource_limits ? "✓" : "✗"}
            </Tag>
            <Tag minimal title={`mechanism: ${probe.mechanism}`}>
              strongest: {probe.strongest_tier}
            </Tag>
          </>
        ) : (
          <Tag minimal>probing…</Tag>
        )}
      </div>
      <div className="sbx-strip-vitals">
        <Vital label="boxes" value={boxes?.length ?? 0} />
        <Vital label="active" value={active} />
        <Vital
          label="proposed"
          value={proposed}
          intent={proposed > 0 ? "primary" : undefined}
        />
        <Vital
          label="denials"
          value={denials}
          intent={denials > 0 ? "danger" : undefined}
        />
        <Vital label="runs" value={runs} />
      </div>
    </div>
  );
}

function Vital({
  label,
  value,
  intent,
}: {
  label: string;
  value: number;
  intent?: "primary" | "danger";
}) {
  const color =
    intent === "danger"
      ? "var(--bp-red)"
      : intent === "primary"
        ? "var(--bp-blue-hi)"
        : "var(--bp-text)";
  return (
    <span className="sbx-vital">
      <span className="sbx-vital-num" style={{ color }}>
        {value}
      </span>
      <span className="sbx-vital-label">{label}</span>
    </span>
  );
}

// ── left: the fleet ──────────────────────────────────────────────────────────

const FILTERS = [
  "all",
  "running",
  "proposed",
  "pressure",
  "drifted",
  "container",
  "supervised",
  "process",
  "workspace",
];

function matchesFilter(b: BoxRow, f: string): boolean {
  switch (f) {
    case "all":
      return true;
    case "running":
      return b.status === "running";
    case "proposed":
      return b.status === "proposed";
    case "pressure":
      return b.signals.verdict !== "clean";
    case "drifted":
      return b.drift !== "up-to-date" && b.drift !== "detached";
    default:
      return b.isolation_claim === f;
  }
}

function FleetPane(props: {
  boxes: BoxRow[] | null;
  total: number;
  filter: string;
  onFilter: (f: string) => void;
  selectedId: string | null;
  onSelect: (id: string) => void;
}) {
  const { boxes, total, filter, onFilter, selectedId, onSelect } = props;
  return (
    <div className="sbx-fleet">
      <div className="sbx-fleet-header">
        <span>Boxes</span>
        <Tag minimal round>
          {total}
        </Tag>
      </div>
      <div className="sbx-filters">
        {FILTERS.map((f) => (
          <button
            key={f}
            className={"sbx-chip" + (filter === f ? " active" : "")}
            onClick={() => onFilter(f)}
          >
            {f}
          </button>
        ))}
      </div>
      <div className="sbx-fleet-body">
        {!boxes ? (
          <NonIdealState icon={<Spinner size={20} />} title="Loading…" />
        ) : boxes.length === 0 ? (
          <NonIdealState
            icon="shield"
            title="No boxes"
            description={
              total === 0
                ? "Make one with `h5i box .`"
                : "None match this filter."
            }
          />
        ) : (
          <HTMLTable className="sbx-fleet-table" interactive compact>
            <thead>
              <tr>
                <th>Box</th>
                <th style={{ width: 90 }}>Isolation</th>
                <th style={{ width: 70 }}>Status</th>
                <th style={{ width: 96 }}>Signal</th>
              </tr>
            </thead>
            <tbody>
              {boxes.map((b) => (
                <tr
                  key={b.id}
                  className={b.id === selectedId ? "selected" : ""}
                  onClick={() => onSelect(b.id)}
                >
                  <td>
                    <div className="sbx-env-id">
                      {b.agent}/{b.slug}
                    </div>
                    <div className="sbx-env-sub">
                      {b.signals.runs} run{b.signals.runs === 1 ? "" : "s"}
                      {b.files_changed > 0 ? (
                        <span>
                          {" · "}
                          {b.files_changed}f +{b.insertions}/−{b.deletions}
                        </span>
                      ) : null}
                      {b.drift !== "up-to-date" && b.drift !== "detached" ? (
                        <span className="sbx-drift" title={b.drift_summary}>
                          {" · "}
                          {b.drift}
                        </span>
                      ) : null}
                      {b.shared_now ? (
                        <SharedNowChip shared={b.shared_now} />
                      ) : null}
                      {b.stale_running ? (
                        <span
                          className="sbx-drift"
                          title="status says running, but no live session holds it — a crash leftover"
                        >
                          {" · stale"}
                        </span>
                      ) : null}
                      {!b.has_workspace ? (
                        <span className="sbx-env-sub-dim"> · pulled</span>
                      ) : null}
                    </div>
                  </td>
                  <td>
                    <IsolationTag
                      isolation={b.isolation_claim}
                      weak={b.signals.weak_isolation}
                    />
                  </td>
                  <td>
                    <span className="sbx-status">{b.status}</span>
                    {b.live.length > 0 ? (
                      <div
                        className="sbx-env-sub"
                        title={b.live
                          .map((s) => `${s.kind} pid ${s.pid}`)
                          .join("\n")}
                      >
                        ● {b.live.length} live
                      </div>
                    ) : null}
                  </td>
                  <td>
                    <SignalBadge signals={b.signals} />
                  </td>
                </tr>
              ))}
            </tbody>
          </HTMLTable>
        )}
      </div>
    </div>
  );
}

function IsolationTag({
  isolation,
  weak,
}: {
  isolation: string;
  weak: boolean;
}) {
  // container / supervised are the confined tiers (green); process is
  // kernel-confined (blue); workspace confines nothing, and says so.
  const intent = weak
    ? "none"
    : isolation === "container" || isolation === "supervised"
      ? "success"
      : "primary";
  return (
    <Tag
      minimal
      intent={intent}
      title={weak ? "workspace tier — nothing was confined" : undefined}
    >
      {isolation}
    </Tag>
  );
}

/**
 * A share admitting somebody into this box right now.
 *
 * The one lane that lets a stranger *in*, and it used to be four dim grey
 * words in the drift line with every fact behind a hover. On a screen whose
 * job is saying what is pressing on a boundary, the reader should not have to
 * find the tooltip to learn which port is open and over what. The transport is
 * named on the row, because `p2p` and `tunnel` are different security claims
 * and only one of them is end to end.
 *
 * Not red: the fleet's red means enforcement fired. Nothing was refused here —
 * an operator opened a door on purpose, and it is standing open.
 */
function SharedNowChip({ shared }: { shared: SharedNow }) {
  const relayed = shared.transport !== "p2p";
  return (
    <span
      className={"sbx-shared" + (relayed ? " relayed" : "")}
      title={
        `somebody outside can reach port ${shared.port} inside this box right now, ` +
        `over ${shared.transport}, on ${shared.grants} live ticket(s).` +
        (relayed
          ? "\nThis transport is relayed: it is not end-to-end encrypted, and a third party terminates TLS."
          : "\nDirect peer to peer: no relay carries the application bytes.") +
        "\nThe receipt lands when the share ends."
      }
    >
      ⇄ shared :{shared.port} {shared.transport}
    </span>
  );
}

/** The one badge in the fleet table. Never a score — only what was recorded. */
function SignalBadge({ signals }: { signals: Signals }) {
  if (signals.verdict === "denial") {
    return (
      <span
        className="sbx-pressure critical"
        title={
          `${signals.egress_denied} egress request(s) refused` +
          (signals.denied_hosts.length
            ? `:\n${signals.denied_hosts.join("\n")}`
            : "")
        }
      >
        ⛔ blocked
      </span>
    );
  }
  if (signals.verdict === "attention") {
    const parts = [
      signals.failed ? `${signals.failed} failed` : null,
      signals.timed_out ? `${signals.timed_out} timed out` : null,
      signals.browser_issues ? `${signals.browser_issues} page issue(s)` : null,
      signals.kernel_alerts
        ? `${signals.kernel_alerts} kernel alert(s): ${(signals.kernel_rules ?? []).join(", ")}`
        : null,
    ].filter(Boolean);
    return (
      <span className="sbx-pressure warning" title={parts.join(", ")}>
        ⚠ {parts.length} kind{parts.length === 1 ? "" : "s"}
      </span>
    );
  }
  if (signals.runs === 0) {
    return <span className="sbx-pressure none">no runs</span>;
  }
  if (signals.box_claimed_only) {
    return (
      <span
        className="sbx-pressure weak"
        title="every record came from inside the box — nothing here was observed from the host"
      >
        ◌ box-claimed
      </span>
    );
  }
  return <span className="sbx-pressure clean">clean</span>;
}

// ── right: one box ───────────────────────────────────────────────────────────

type DetailView = "evidence" | "browser";

function DetailPane({ box, tick }: { box: BoxRow | null; tick: number }) {
  const [detail, setDetail] = useState<BoxDetail | null>(null);
  const [loading, setLoading] = useState(false);
  const [view, setView] = useState<DetailView>("evidence");

  const agent = box?.agent;
  const slug = box?.slug;

  // Selecting a different box returns to Evidence. Keeping the browser tab
  // across a switch would land the reader on a browser view of a box that may
  // not have one, and the empty result reads as a failure rather than as "you
  // are looking at the wrong box".
  useEffect(() => {
    setView("evidence");
  }, [agent, slug]);

  useEffect(() => {
    if (!agent || !slug) {
      setDetail(null);
      return;
    }
    let live = true;
    setLoading(true);
    api
      .box(agent, slug)
      .then((d) => {
        if (live) setDetail(d);
      })
      .catch(() => {
        if (live) setDetail(null);
      })
      .finally(() => {
        if (live) setLoading(false);
      });
    return () => {
      live = false;
    };
    // `tick` advances with the fleet poll: without it the detail pane fetched
    // once per selection while the row beside it kept updating, so an open box
    // drifted behind its own signal badge.
  }, [agent, slug, tick]);

  if (!box) {
    return (
      <div className="sbx-detail">
        <div className="sbx-pane-empty">
          Select a box to see what ran inside it.
        </div>
      </div>
    );
  }

  // Only a browser box has a browser terminal. Reading it off the profile
  // rather than off whether events have arrived: a box that has not browsed yet
  // still has the tab, and its panes say so, which is the honest empty state.
  const browserCapable = box.profile === "browser";

  return (
    <div className="sbx-detail">
      <div className="sbx-detail-head">
        <div>
          <span className="sbx-detail-title">
            {box.agent}/{box.slug}
          </span>
          <IsolationTag
            isolation={box.isolation_claim}
            weak={box.signals.weak_isolation}
          />
          <Tag minimal>{box.profile}</Tag>
          {box.pr ? <Tag minimal intent="primary">PR #{box.pr}</Tag> : null}
          <span
            className="sbx-detail-digest"
            title="sha256 of the policy that was actually enforced"
          >
            {box.policy_digest.slice(0, 12)}
          </span>
        </div>
        <SignalBadge signals={box.signals} />
      </div>

      {/* Two views of one box, because they answer different questions and
          want different shapes. Evidence is a scroll of what has already
          happened; the browser terminal is a live instrument that wants the
          whole pane. Wedging the second into the first gave it a few hundred
          pixels between Services and the timeline, which is not the density the
          panes were designed for. The tab strip only appears for a box that has
          a browser — every other box has one view, and a disabled tab is just a
          question a reader has to answer. */}
      {browserCapable ? (
        <div className="sbx-tabs" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={view === "evidence"}
            className={view === "evidence" ? "on" : undefined}
            onClick={() => setView("evidence")}
          >
            Evidence
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={view === "browser"}
            className={view === "browser" ? "on" : undefined}
            onClick={() => setView("browser")}
          >
            Browser
          </button>
        </div>
      ) : null}

      {loading && !detail ? (
        <NonIdealState icon={<Spinner size={20} />} title="Loading evidence…" />
      ) : !detail ? (
        <div className="sbx-pane-empty">No detail available.</div>
      ) : view === "browser" && browserCapable ? (
        <BrowserTerminal agent={box.agent} slug={box.slug} />
      ) : (
        <div className="sbx-detail-body">
          <SignalSummary box={box} />
          <Services services={detail.services} />
          <Timeline
            agent={box.agent}
            slug={box.slug}
            receipts={detail.receipts}
            folded={detail.receipts_folded}
            events={detail.events}
            policy={detail.policy ?? null}
          />
          <PolicyPanel policy={detail.policy ?? null} />
          <Diffstat text={detail.diffstat} drift={box.drift_summary} />
        </div>
      )}
    </div>
  );
}

function SignalSummary({ box }: { box: BoxRow }) {
  const s = box.signals;
  // Enforcement notes, and only those, decide whether the green "nothing to
  // report" line appears. Share notes sit outside that list: a box can have
  // been shared and still have run clean, and folding the two together would
  // have let an ingress session silently delete the summary of the runs.
  const notes: ReactElement[] = [];
  // A share admitting somebody *now* leads the pane; a record of shares that
  // have ended trails it, behind anything enforcement had to say.
  const live: ReactElement[] = [];
  const history: ReactElement[] = [];

  if (s.egress_denied > 0) {
    notes.push(
      <Callout
        key="denial"
        intent="danger"
        icon="ban-circle"
        className="sbx-callout"
      >
        The egress allowlist refused {s.egress_denied} request
        {s.egress_denied === 1 ? "" : "s"}
        {s.denied_hosts.length ? (
          <>
            {" "}
            to <Code>{s.denied_hosts.join(", ")}</Code>
          </>
        ) : null}
        . This is host-observed: the proxy recorded it, not the box.
      </Callout>,
    );
  }
  if (s.failed > 0 || s.timed_out > 0) {
    notes.push(
      <Callout key="fail" intent="warning" icon="warning-sign" className="sbx-callout">
        {s.failed} run{s.failed === 1 ? "" : "s"} exited non-zero
        {s.timed_out > 0
          ? `, ${s.timed_out} killed by the wall-clock limit`
          : ""}
        . A failing command is not a boundary trip — it is just a failing
        command.
      </Callout>,
    );
  }
  // First on the pane, ahead of every enforcement note: a door standing open
  // right now outranks a record of what already happened, and it is the only
  // fact here that can change while the reader is looking at it.
  if (box.shared_now) {
    const sn = box.shared_now;
    const relayed = sn.transport !== "p2p";
    live.push(
      <Callout
        key="shared-now"
        intent="warning"
        icon="globe-network"
        className="sbx-callout"
      >
        Somebody outside can reach port <Code>{sn.port}</Code> inside this box
        right now, over <Code>{sn.transport}</Code>, on {sn.grants} live ticket
        {sn.grants === 1 ? "" : "s"}.{" "}
        {relayed
          ? "This transport is relayed — a third party terminates TLS, so it is not end-to-end encrypted."
          : "Direct peer to peer — no relay carries the application bytes."}{" "}
        The receipt for it lands when the share ends.
      </Callout>,
    );
  }
  if (s.shares > 0) {
    history.push(
      <Callout key="shares" icon="exchange" className="sbx-callout">
        {s.shares} share session{s.shares === 1 ? "" : "s"} admitted{" "}
        {s.share_peers} peer{s.share_peers === 1 ? "" : "s"} into this box.{" "}
        {s.shares_third_party_readable > 0
          ? `${s.shares_third_party_readable} of them ran over a relayed transport, so a third party could read that traffic.`
          : "All of them ran peer to peer."}{" "}
        Each one is a receipt below, host-observed.
      </Callout>,
    );
  }
  if (s.weak_isolation) {
    notes.push(
      <Callout key="weak" icon="unlock" className="sbx-callout">
        Isolation tier is <Code>workspace</Code>: nothing was confined. The
        receipts below are a record of what ran, not evidence that anything was
        stopped.
      </Callout>,
    );
  }
  if (s.fs_overlap.length > 0) {
    notes.push(
      <Callout key="overlap" icon="link" className="sbx-callout">
        The last run recorded writable-path overlap with{" "}
        {s.fs_overlap.length} other box{s.fs_overlap.length === 1 ? "" : "es"}:{" "}
        <Code>{s.fs_overlap.join("; ")}</Code>. Cross-box influence is possible
        through a shared path. Boxes whose recorded grants are disjoint carry a
        machine-checked noninterference guarantee instead; this pair does not.
      </Callout>,
    );
  }
  if (s.box_claimed_only) {
    notes.push(
      <Callout key="claimed" icon="eye-off" className="sbx-callout">
        Every receipt here came from the in-box tee shim — the box's own
        account. Nothing on this screen was observed from the host.
      </Callout>,
    );
  }
  if (s.kernel_alerts) {
    notes.push(
      <Callout key="kernel" icon="pulse" className="sbx-callout">
        An eBPF collector in the kernel recorded {s.kernel_alerts} alert-level
        match{s.kernel_alerts === 1 ? "" : "es"}
        {s.kernel_rules?.length ? (
          <>
            {" "}
            (<Code>{s.kernel_rules.join(", ")}</Code>)
          </>
        ) : null}
        . Nothing was blocked — this lane observes and never denies. The
        receipts below carry what tripped each rule.
      </Callout>,
    );
  }
  if (s.kernel_unwatched) {
    notes.push(
      <Callout key="kernel-off" icon="eye-off" className="sbx-callout">
        {s.kernel_unwatched} run
        {s.kernel_unwatched === 1 ? "" : "s"} asked to be watched from the
        kernel and {s.kernel_unwatched === 1 ? "was" : "were"} not. Open the run
        below for the reason. An unwatched run is not a quiet one.
      </Callout>,
    );
  }
  if (s.kernel_events_lost) {
    notes.push(
      <Callout key="kernel-lost" icon="warning-sign" className="sbx-callout">
        {s.kernel_events_lost} kernel event
        {s.kernel_events_lost === 1 ? "" : "s"} were dropped before anything
        examined them, so every count from that lane is a lower bound.
      </Callout>,
    );
  }
  if (notes.length === 0) {
    notes.push(
      <Callout
        key="clean"
        intent="success"
        icon="tick-circle"
        className="sbx-callout"
      >
        {s.runs === 0
          ? "No runs recorded yet."
          : `${s.runs} run${s.runs === 1 ? "" : "s"}, all host-observed, no refused egress and no failures.`}
      </Callout>,
    );
  }
  return <div className="sbx-findings">{[...live, ...notes, ...history]}</div>;
}

function Services({ services }: { services: ServiceStatus[] }) {
  if (services.length === 0) return null;
  return (
    <div className="sbx-policy">
      <div className="sbx-policy-head">Services · declared in .h5i/env.toml</div>
      <div className="sbx-policy-grid">
        {services.map((s) => (
          <div key={s.name} className="sbx-policy-row">
            <span className="sbx-policy-key">
              {s.alive ? "●" : "○"} {s.name}
            </span>
            <span className="sbx-policy-val">
              pid {s.pid}
              {s.dynamic_port ? ` · :${s.dynamic_port}` : ""}
              {s.alive ? "" : " · not running"}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}

// ── the flight recorder ──────────────────────────────────────────────────────

/** One row: a receipt, or a mediated-commit refusal from the event log. */
type Row =
  | { kind: "run"; ts: string; receipt: ExecRecord }
  | { kind: "violation"; ts: string; event: EnvEvent };

function Timeline({
  agent,
  slug,
  receipts,
  folded,
  events,
  policy,
}: {
  agent: string;
  slug: string;
  receipts: ExecRecord[];
  folded: number;
  events: EnvEvent[];
  policy: EnforcedPolicy | null;
}) {
  const [openId, setOpenId] = useState<string | null>(null);
  const [renders, setRenders] = useState<Map<string, string>>(new Map());

  const rows = useMemo<Row[]>(() => {
    const out: Row[] = receipts.map((r) => ({
      kind: "run",
      ts: r.timestamp,
      receipt: r,
    }));
    // The output gate refusing a commit is boundary activity with no receipt
    // of its own, so it is folded in by timestamp rather than left out.
    for (const e of events) {
      if (e.event === "violation") out.push({ kind: "violation", ts: e.ts, event: e });
    }
    // RFC3339 UTC sorts lexically.
    return out.sort((a, b) => a.ts.localeCompare(b.ts));
  }, [receipts, events]);

  const toggle = useCallback(
    (id: string) => {
      setOpenId((prev) => (prev === id ? null : id));
      if (!renders.has(id)) {
        api
          .receipt(agent, slug, id)
          .then(({ render }) => setRenders((m) => new Map(m).set(id, render)))
          .catch(() =>
            setRenders((m) => new Map(m).set(id, "(failed to load this receipt)")),
          );
      }
    },
    [renders, agent, slug],
  );

  return (
    <div className="sbx-timeline">
      <div className="sbx-timeline-head">
        Flight recorder · {rows.length} record{rows.length === 1 ? "" : "s"}
        {folded > 0 ? ` · ${folded} older folded` : ""}
      </div>
      <div className="sbx-lane-grid">
        <div className="sbx-lane-row sbx-lane-policy">
          <div className="sbx-run-cell sbx-run-policy">policy allows →</div>
          {LANES.map((l) => (
            <div key={l.key} className="sbx-lane-cell" title={l.hint}>
              <div className="sbx-lane-name">{l.label}</div>
              <div className="sbx-lane-allow">{laneAllowance(l.key, policy)}</div>
            </div>
          ))}
        </div>
        {rows.length === 0 ? (
          <div className="sbx-lane-empty">
            No runs yet — {"`h5i box run <name> -- …`"}
          </div>
        ) : (
          rows.map((row, i) => (
            <TimelineRow
              key={row.kind === "run" ? row.receipt.id : `v${i}`}
              row={row}
              open={row.kind === "run" && openId === row.receipt.id}
              render={row.kind === "run" ? renders.get(row.receipt.id) : undefined}
              onToggle={row.kind === "run" ? () => toggle(row.receipt.id) : undefined}
            />
          ))
        )}
      </div>
    </div>
  );
}

function TimelineRow({
  row,
  open,
  render,
  onToggle,
}: {
  row: Row;
  open: boolean;
  render: string | undefined;
  onToggle?: () => void;
}) {
  if (row.kind === "violation") {
    return (
      <div className="sbx-lane-row">
        <div className="sbx-run-cell" title={row.event.detail ?? ""}>
          <div className="sbx-run-cmd">mediated commit refused</div>
          <div className="sbx-run-meta">
            output gate · {shortTime(row.ts)}
          </div>
        </div>
        <div className="sbx-lane-cell">
          <span className="sbx-verdict critical" title={row.event.detail ?? ""}>
            ⛔ refused
          </span>
        </div>
        {LANES.slice(1).map((l) => (
          <div key={l.key} className="sbx-lane-cell">
            <span className="sbx-verdict none">·</span>
          </div>
        ))}
      </div>
    );
  }

  const r = row.receipt;
  return (
    <>
      <div
        className={"sbx-lane-row" + (open ? " selected" : "")}
        onClick={onToggle}
      >
        <div className="sbx-run-cell" title={r.cmd ?? ""}>
          <div className="sbx-run-cmd">{r.cmd ?? "(no command recorded)"}</div>
          <div className="sbx-run-meta">
            <SourceChip source={r.source} />
            {r.exit_code != null ? ` exit ${r.exit_code}` : ""}
            {r.wall_ms != null ? ` · ${fmtMs(r.wall_ms)}` : ""}
            {" · "}
            {shortTime(r.timestamp)}
            {r.redactions && r.redactions.length > 0 ? " · redacted" : ""}
          </div>
          {r.share ? <ShareLine share={r.share} /> : null}
        </div>
        {LANES.map((l) => (
          <div key={l.key} className="sbx-lane-cell">
            <LaneVerdict lane={l.key} receipt={r} />
          </div>
        ))}
      </div>
      {open ? (
        <div className="sbx-render-row">
          {render === undefined ? (
            <Spinner size={16} />
          ) : (
            <pre className="sbx-render">{render}</pre>
          )}
        </div>
      ) : null}
    </>
  );
}

/**
 * What an ended share was, spelled out under its row.
 *
 * The transport is the security claim, so it is stated in words rather than
 * left to a colour or a hover: `tunnel` means a third party terminated TLS and
 * could read every byte, and a reader who has to guess that from a row is a
 * reader who will guess wrong. Read off the receipt's `share` field — the
 * command line beside it contains the box's name, and a box called `tunnel`
 * once made a plain peer-to-peer session read as relayed.
 */
function ShareLine({ share }: { share: ShareEvidence }) {
  const relayed = thirdPartyCanRead(share);
  return (
    <div className={"sbx-run-share" + (relayed ? " relayed" : "")}>
      ⇄ inbound · {share.transport} :{share.port} · {share.peers} peer
      {share.peers === 1 ? "" : "s"} · {fmtSecs(share.seconds)}
      {share.turned_away ? ` · ${share.turned_away} turned away` : ""} ·{" "}
      {relayed
        ? "a third party terminated TLS — not end-to-end encrypted"
        : "direct peer to peer"}
    </div>
  );
}

/** Who observed this run. The single most load-bearing label on the screen. */
function SourceChip({ source }: { source: string }) {
  const boxClaimed = source === "tee-shim";
  return (
    <span
      className={"sbx-source" + (boxClaimed ? " claimed" : " observed")}
      title={
        boxClaimed
          ? "recorded by the shim inside the box — the box's own account"
          : "observed by h5i from the host"
      }
    >
      {boxClaimed ? "box-claimed" : "host-observed"}
    </span>
  );
}

function LaneVerdict({ lane, receipt }: { lane: LaneKey; receipt: ExecRecord }) {
  switch (lane) {
    case "fs": {
      const n = receipt.files?.length ?? 0;
      return n > 0 ? (
        <span
          className="sbx-verdict info"
          title={(receipt.files ?? []).join("\n")}
        >
          {n} file{n === 1 ? "" : "s"}
        </span>
      ) : (
        <span className="sbx-verdict none">·</span>
      );
    }
    case "net": {
      // A share is network activity in the one direction this lane never had a
      // word for: inbound. It carries no egress summary, so the row for the
      // only session that let a stranger reach the box rendered as an empty
      // dot in every lane it has.
      const sh = receipt.share;
      if (sh) {
        const relayed = thirdPartyCanRead(sh);
        return (
          <span
            className={"sbx-verdict " + (relayed ? "warning" : "info")}
            title={
              `inbound: ${sh.peers} peer(s) admitted to port ${sh.port} over ${sh.transport}, ` +
              `${fmtSecs(sh.seconds)}` +
              (sh.turned_away ? `, ${sh.turned_away} connection(s) turned away` : "") +
              (relayed
                ? "\nRelayed transport: a third party terminated TLS, so this traffic was not end-to-end encrypted."
                : "\nDirect peer to peer: no relay carried the application bytes.")
            }
          >
            ⇄ {sh.peers} in
          </span>
        );
      }
      const e = receipt.egress;
      if (!e) return <span className="sbx-verdict none">·</span>;
      if (e.denied > 0) {
        return (
          <span
            className="sbx-verdict critical"
            title={(e.hosts ?? [])
              .filter((h) => h.denied > 0)
              .map((h) => `refused ${h.denied}× ${h.host}:${h.port}`)
              .join("\n")}
          >
            ⛔ {e.denied} refused
          </span>
        );
      }
      return e.allowed > 0 ? (
        <span className="sbx-verdict ok" title={`${e.allowed} request(s) allowed`}>
          ✓ {e.allowed}
        </span>
      ) : (
        <span className="sbx-verdict none">·</span>
      );
    }
    case "proc": {
      if (receipt.exit_code == null)
        return <span className="sbx-verdict none">·</span>;
      return receipt.exit_code === 0 ? (
        <span className="sbx-verdict ok">✓</span>
      ) : (
        <span className="sbx-verdict warning">⚠ exit {receipt.exit_code}</span>
      );
    }
    case "res": {
      if (receipt.timed_out)
        return (
          <span
            className="sbx-verdict critical"
            title="the wall-clock limit killed the process group"
          >
            ⛔ wall limit
          </span>
        );
      if (receipt.max_rss_kb != null)
        return (
          <span className="sbx-verdict info" title="peak resident set size">
            {fmtKb(receipt.max_rss_kb)}
          </span>
        );
      return <span className="sbx-verdict none">·</span>;
    }
    case "browser": {
      const b = receipt.browser;
      if (!b) return <span className="sbx-verdict none">·</span>;
      if (b.unavailable)
        return (
          <span
            className="sbx-verdict weak"
            title="the drain could not reach a browser — nothing was looked at"
          >
            ◌ none
          </span>
        );
      const issues =
        (b.console?.length ?? 0) +
        (b.errors?.length ?? 0) +
        (b.failed_requests?.length ?? 0);
      if (issues === 0)
        return (
          <span className="sbx-verdict ok" title={b.verb ? `${b.verb}: clean` : "clean"}>
            ✓
          </span>
        );
      return (
        <span
          className="sbx-verdict warning"
          title={[...(b.errors ?? []), ...(b.console ?? []), ...(b.failed_requests ?? [])].join(
            "\n",
          )}
        >
          ⚠ {issues}
        </span>
      );
    }
    case "kernel": {
      const rt = receipt.runtime;
      // No block at all: this run did not ask to be watched. A dot, not a
      // tick — a tick would claim a result nobody produced.
      if (!rt) return <span className="sbx-verdict none">·</span>;
      if (!runtimeObserved(rt))
        return (
          <span
            className="sbx-verdict weak"
            title={
              rt.unavailable ??
              rt.coverage_reason ??
              "the collector observed nothing for this run"
            }
          >
            ◌ none
          </span>
        );
      const dets = rt.detections ?? [];
      const alerts = dets.filter((d) => d.severity === "alert");
      const lost = rt.events_lost
        ? `\n${rt.events_lost} event(s) lost — this is a lower bound`
        : "";
      const partial =
        rt.coverage === "partial"
          ? `\npartial coverage: ${rt.coverage_reason ?? "some of the run was out of scope"}`
          : "";
      if (dets.length === 0)
        return (
          <span
            className="sbx-verdict ok"
            title={`${rt.events_seen ?? 0} kernel event(s), no signature fired${partial}${lost}`}
          >
            ✓
          </span>
        );
      const detail = dets
        .map((d) => `[${d.severity}] ${d.rule} ×${d.count} — ${d.title}`)
        .join("\n");
      return (
        <span
          className={alerts.length ? "sbx-verdict critical" : "sbx-verdict warning"}
          title={`${detail}${partial}${lost}`}
        >
          {alerts.length ? "⛔" : "⚠"} {dets.length}
        </span>
      );
    }
  }
}

function PolicyPanel({ policy }: { policy: EnforcedPolicy | null }) {
  if (!policy) {
    return (
      <div className="sbx-policy">
        <div className="sbx-policy-head">Enforced policy</div>
        <div className="sbx-pane-empty">
          policy.resolved.toml unavailable (a pulled or gc'd box).
        </div>
      </div>
    );
  }
  const rows: [string, string][] = [
    ["isolation", policy.isolation],
    ["net.mode", policy.net_mode],
    ["net.egress", policy.net_egress.length ? policy.net_egress.join(", ") : "—"],
    ["fs.write", policy.fs_write.length ? policy.fs_write.join(", ") : "$WORK"],
    ["fs.read", policy.fs_read.length ? policy.fs_read.join(", ") : "—"],
    ["fs.deny", policy.fs_deny.length ? policy.fs_deny.join(", ") : "—"],
    ["tools", policy.tools.length ? policy.tools.join(", ") : "(unrestricted)"],
    ["env.pass", policy.env_pass.length ? policy.env_pass.join(", ") : "—"],
    ["image", policy.image ?? "—"],
    ["wall", `${policy.wall_secs}s`],
    // A cap this host cannot apply is marked rather than shown alongside the
    // real ones — the panel's whole claim is "what was actually allowed", and
    // Darwin's kernel tiers impose neither of these.
    [
      "mem",
      policy.mem_bytes
        ? `${fmtBytes(policy.mem_bytes)}${policy.mem_enforced === false ? " (declared, not enforced here)" : ""}`
        : "—",
    ],
    [
      "procs",
      policy.max_procs != null
        ? `${policy.max_procs}${policy.procs_enforced === false ? " (declared, not enforced here)" : ""}`
        : "—",
    ],
    ["cpu", policy.cpu_secs != null ? `${policy.cpu_secs}s` : "—"],
    ["fsize", policy.fsize_bytes ? fmtBytes(policy.fsize_bytes) : "—"],
  ];
  return (
    <div className="sbx-policy">
      <div className="sbx-policy-head">
        Enforced policy · what was actually allowed
      </div>
      <div className="sbx-policy-grid">
        {rows.map(([k, v]) => (
          <div key={k} className="sbx-policy-row">
            <span className="sbx-policy-key">{k}</span>
            <span className="sbx-policy-val">{v}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

function Diffstat({ text, drift }: { text?: string; drift: string }) {
  if (!text || !text.trim()) return null;
  return (
    <div className="sbx-policy">
      <div className="sbx-policy-head">Work vs. pinned base · {drift}</div>
      <pre className="sbx-render">{text}</pre>
    </div>
  );
}

// ── helpers ──────────────────────────────────────────────────────────────────

function laneAllowance(lane: LaneKey, p: EnforcedPolicy | null): string {
  if (!p) return "—";
  switch (lane) {
    case "fs":
      return p.fs_write.length ? p.fs_write.join(",") : "$WORK rw";
    case "net":
      return p.net_egress.length ? `allow ${p.net_egress.length}` : p.net_mode;
    case "proc":
      return p.tools.length ? p.tools.join(",") : "any tool";
    case "res":
      return `wall ${p.wall_secs}s`;
    case "browser":
      return "in box";
    case "kernel":
      // The lane header says what the *policy* allows in every other column.
      // There is nothing to allow here: the collector grants nothing and
      // denies nothing, so the honest header is what it does.
      return "observe only";
  }
}

function shortTime(ts: string): string {
  // RFC3339 → HH:MM:SS (UTC), best-effort.
  const m = ts.match(/T(\d{2}:\d{2}:\d{2})/);
  return m ? m[1] : ts;
}

function fmtMs(ms: number): string {
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${ms}ms`;
}

function fmtSecs(s: number): string {
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m${s % 60 ? ` ${s % 60}s` : ""}`;
  return `${Math.floor(m / 60)}h${m % 60 ? ` ${m % 60}m` : ""}`;
}

function fmtKb(kb: number): string {
  return kb >= 1024 ? `${(kb / 1024).toFixed(0)}M` : `${kb}K`;
}

function fmtBytes(n: number): string {
  if (n >= 1 << 30) return `${(n / (1 << 30)).toFixed(1)}G`;
  if (n >= 1 << 20) return `${(n / (1 << 20)).toFixed(0)}M`;
  if (n >= 1 << 10) return `${(n / (1 << 10)).toFixed(0)}K`;
  return String(n);
}
