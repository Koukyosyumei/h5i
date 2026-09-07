import { useEffect, useMemo, useState, type ReactElement } from "react";
import { HTMLTable, Tag } from "@blueprintjs/core";

import {
  api,
  type EndpointRow,
  type RequestRecord,
  type SessionDetail,
  type SessionRow,
} from "./api";

// Browser sessions in the console: which one wants a person, and what it did.
//
// The states are herdr's. `done` is the one a client clears by looking, so it
// is remembered in this browser and never written back.

export const ATTENTION_ORDER = ["blocked", "done", "working", "idle", "unknown"];

const ATTENTION_LABEL: Record<string, string> = {
  blocked: "waiting on you",
  done: "finished, unread",
  working: "working",
  idle: "idle",
  unknown: "unclassified",
};

/** Sessions whose `done` this browser has already looked at. */
const SEEN_KEY = "h5i.console.seen";

function loadSeen(): Record<string, string> {
  try {
    return JSON.parse(window.localStorage.getItem(SEEN_KEY) ?? "{}") as Record<
      string,
      string
    >;
  } catch {
    return {};
  }
}

/** What makes a `done` distinct: the ending, or the newest job that finished.
 *  Looking at one ending does not clear the next one. */
export function doneMark(s: SessionRow): string {
  const job = s.jobs.length > 0 ? s.jobs[s.jobs.length - 1] : null;
  return `${s.ended_at ?? ""}|${job?.id ?? ""}|${job?.ended_at ?? ""}`;
}

/** The state to show, after this client's own seen set. */
export function shownState(s: SessionRow, seen: Record<string, string>): string {
  if (s.attention.state === "done" && seen[s.id] === doneMark(s)) {
    return "idle";
  }
  return s.attention.state;
}

export function AttentionBar({
  sessions,
  boxes,
  filter,
  onFilter,
  onBoxFilter,
  seen,
}: {
  sessions: SessionRow[] | null;
  /** Boxes keep their own words. A refusal is the boundary working, which is
   *  not one of the five states, and folding it into them would be a lie for
   *  the sake of one row of chips. */
  boxes: { refusing: number; failing: number } | null;
  filter: string;
  onFilter: (f: string) => void;
  onBoxFilter: (f: string) => void;
  seen: Record<string, string>;
}) {
  const counts = useMemo(() => {
    const out: Record<string, number> = {};
    for (const s of sessions ?? []) {
      const state = shownState(s, seen);
      out[state] = (out[state] ?? 0) + 1;
    }
    return out;
  }, [sessions, seen]);

  const wanted =
    (counts.blocked ?? 0) + (counts.done ?? 0) + (boxes?.refusing ?? 0) + (boxes?.failing ?? 0);

  return (
    <div className="sbx-attention" role="group" aria-label="what wants a person">
      <span className="sbx-strip-label">attention</span>
      {wanted === 0 ? (
        <span className="sbx-attention-quiet">nothing is waiting on you</span>
      ) : null}
      {ATTENTION_ORDER.map((state) =>
        counts[state] ? (
          <button
            key={state}
            type="button"
            className={`sbx-attn sbx-attn-${state}${filter === state ? " is-on" : ""}`}
            onClick={() => onFilter(filter === state ? "all" : state)}
            title={`show only sessions that are ${ATTENTION_LABEL[state]}`}
          >
            <span className="sbx-attn-n">{counts[state]}</span>
            {ATTENTION_LABEL[state]}
          </button>
        ) : null,
      )}
      {boxes && (boxes.refusing > 0 || boxes.failing > 0) ? (
        <>
          <span className="sbx-attn-sep" aria-hidden />
          {boxes.refusing > 0 ? (
            <button
              type="button"
              className="sbx-attn sbx-attn-refusing"
              onClick={() => onBoxFilter("pressure")}
              title="boxes where the egress proxy refused something"
            >
              <span className="sbx-attn-n">{boxes.refusing}</span>
              boxes refused egress
            </button>
          ) : null}
          {boxes.failing > 0 ? (
            <button
              type="button"
              className="sbx-attn sbx-attn-failing"
              onClick={() => onBoxFilter("pressure")}
              title="boxes with a run that failed or timed out"
            >
              <span className="sbx-attn-n">{boxes.failing}</span>
              boxes with failures
            </button>
          ) : null}
        </>
      ) : null}
    </div>
  );
}

export function SessionList({
  sessions,
  total,
  seen,
  filter,
  selectedId,
  onSelect,
}: {
  sessions: SessionRow[] | null;
  /** How many records the registry holds, read or not. */
  total: number;
  seen: Record<string, string>;
  filter: string;
  selectedId: string | null;
  onSelect: (id: string) => void;
}) {
  if (sessions === null) {
    return <div className="sbx-pane-empty">reading the registry…</div>;
  }

  const shown = sessions.filter(
    (s) => filter === "all" || shownState(s, seen) === filter,
  );

  if (shown.length === 0) {
    return (
      <div className="sbx-pane-empty">
        {sessions.length === 0
          ? "no browser sessions on this machine. `h5i browser open <url> --capture` starts one"
          : "no session in that state"}
      </div>
    );
  }

  const unread = total - sessions.length;

  return (
    <div className="sbx-fleet-body">
      {unread > 0 ? (
        <div className="sbx-fleet-note">
          the newest {sessions.length} of {total} recorded sessions.{" "}
          {unread} older one{unread === 1 ? "" : "s"} not read
        </div>
      ) : null}
      {shown.map((s) => {
        const state = shownState(s, seen);
        return (
          <button
            type="button"
            key={s.id}
            className={`sbx-session-row${selectedId === s.id ? " is-selected" : ""}`}
            onClick={() => onSelect(s.id)}
          >
            <div className="sbx-session-top">
              <div className="sbx-env-id">
                <span className={`sbx-dot sbx-dot-${state}`} aria-hidden />
                {s.name ?? s.id}
              </div>
              <AttentionTag state={state} why={s.attention.why} />
            </div>
            <div className="sbx-env-sub" title={s.url}>
              {s.origins[0] ?? s.url}
            </div>
            <div className="sbx-session-counts">
              <Count n={s.requests} label="requests" />
              {s.denied > 0 ? <Count n={s.denied} label="refused" tone="deny" /> : null}
              {s.ledger ? (
                <Count n={s.ledger.confirmed} label="confirmed" tone="good" />
              ) : null}
              {s.captured !== null ? <Count n={s.captured} label="captured" /> : null}
            </div>
          </button>
        );
      })}
    </div>
  );
}

/** A command the reader can run. The console does not act, so the next step is
 *  always something you type; clicking copies it. */
function Cmd({ text, hint }: { text: string; hint?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <code
      className={`sbx-cmd sbx-cmd-click${copied ? " is-copied" : ""}`}
      title={hint ?? "click to copy"}
      onClick={(e) => {
        e.stopPropagation();
        void navigator.clipboard
          ?.writeText(text)
          .then(() => {
            setCopied(true);
            window.setTimeout(() => setCopied(false), 1200);
          })
          .catch(() => setCopied(false));
      }}
    >
      {copied ? "copied" : text}
    </code>
  );
}

function Count({
  n,
  label,
  tone,
}: {
  n: number;
  label: string;
  tone?: "deny" | "good";
}) {
  return (
    <span className={`sbx-count${tone ? ` sbx-count-${tone}` : ""}`}>
      <b>{n}</b> {label}
    </span>
  );
}

export function AttentionTag({ state, why }: { state: string; why: string }) {
  return (
    <Tag minimal className={`sbx-attn-tag sbx-attn-${state}`} title={why}>
      {ATTENTION_LABEL[state] ?? state}
    </Tag>
  );
}

type Tab = "requests" | "inventory" | "jobs";

export function SessionDetailPane({
  id,
  tick,
}: {
  id: string;
  tick: number;
}): ReactElement {
  const [detail, setDetail] = useState<SessionDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>("requests");

  useEffect(() => {
    let alive = true;
    api
      .session(id)
      .then((d) => alive && setDetail(d))
      .catch((e: Error) => alive && setError(e.message));
    return () => {
      alive = false;
    };
  }, [id, tick]);

  if (error) {
    return <div className="sbx-detail"><div className="sbx-pane-empty">{error}</div></div>;
  }
  if (!detail) {
    return <div className="sbx-detail"><div className="sbx-pane-empty">reading…</div></div>;
  }

  return (
    <div className="sbx-detail">
      <div className="sbx-detail-head sbx-session-head">
        <div className="sbx-detail-title">
          {detail.name ?? detail.id}
          <AttentionTag state={detail.attention.state} why={detail.attention.why} />
        </div>
        <div className="sbx-detail-why">{detail.attention.why}</div>
        <div className="sbx-detail-cmds">
          <Cmd
            text={`h5i browser audit --session ${detail.name ?? detail.id}`}
            hint="the whole timeline: verbs, fetches, handovers, ending"
          />
          <Cmd
            text={`h5i recon endpoints --session ${detail.name ?? detail.id} --state confirmed`}
            hint="the inventory, with the message that proves each row"
          />
        </div>
        <div className="sbx-detail-facts">
          <Fact label="state" value={detail.state} />
          <Fact label="placement" value={detail.placement} />
          <Fact label="lane" value={detail.lane} />
          <Fact label="identity" value={detail.identity} />
          <Fact
            label="capture"
            value={
              detail.captured === null
                ? "off"
                : `${detail.captured} message(s) stored`
            }
          />
        </div>
      </div>

      <div className="sbx-tabs">
        {(["requests", "inventory", "jobs"] as Tab[]).map((t) => (
          <button
            key={t}
            type="button"
            className={`sbx-tab${tab === t ? " is-on" : ""}`}
            onClick={() => setTab(t)}
          >
            {t}
            <span className="sbx-tab-n">
              {t === "requests"
                ? detail.requests
                : t === "inventory"
                  ? (detail.endpoints?.length ?? 0)
                  : detail.jobs.length}
            </span>
          </button>
        ))}
      </div>

      {tab === "requests" ? <Requests detail={detail} /> : null}
      {tab === "inventory" ? <Inventory detail={detail} /> : null}
      {tab === "jobs" ? <Jobs detail={detail} /> : null}
    </div>
  );
}

function Fact({ label, value }: { label: string; value: string }) {
  return (
    <span className="sbx-fact">
      <span className="sbx-strip-label">{label}</span>
      {value}
    </span>
  );
}

/** The request log, receipt by receipt. Counts, names and outcomes: the bytes
 *  are in the store, which is owner-only and read with `h5i websec show`. */
function Requests({ detail }: { detail: SessionDetail }) {
  const pairs = useMemo(() => fold(detail.requests_log ?? []), [detail]);
  if (pairs.length === 0) {
    return <div className="sbx-pane-empty">this session has fetched nothing</div>;
  }
  return (
    <div className="sbx-table-wrap">
      <HTMLTable compact striped className="sbx-table">
        <thead>
          <tr>
            <th>#</th>
            <th>method</th>
            <th>path</th>
            <th>status</th>
            <th>initiator</th>
            <th>ms</th>
            <th>read it</th>
          </tr>
        </thead>
        <tbody>
          {pairs.map((r) => (
            <tr key={r.seq} className={r.allowed ? undefined : "sbx-row-denied"}>
              <td>{r.seq}</td>
              <td>{r.method}</td>
              <td className="sbx-cell-path" title={r.url}>
                {pathOf(r.url)}
              </td>
              <td>
                {r.allowed ? (
                  (r.status ?? (r.error ? "error" : "…"))
                ) : (
                  <span className="sbx-deny" title={r.denied_reason ?? "refused"}>
                    refused
                  </span>
                )}
              </td>
              <td>{r.initiator}</td>
              <td>{r.duration_ms ?? ""}</td>
              <td>
                {detail.captured === null ? (
                  <span className="sbx-dim">
                    opened without --capture, so there are no bytes to read
                  </span>
                ) : (
                  <Cmd
                    text={`h5i websec show req_${r.seq} --session ${detail.name ?? detail.id}`}
                    hint="the bytes are in the store, which the console never renders"
                  />
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </HTMLTable>
    </div>
  );
}

/** Two receipt rows are one fetch: fold the response back onto its request. */
function fold(records: RequestRecord[]): RequestRecord[] {
  const by = new Map<number, RequestRecord>();
  for (const record of records) {
    const seen = by.get(record.seq);
    by.set(record.seq, seen ? { ...seen, ...record } : { ...record });
  }
  return [...by.values()].sort((a, b) => b.seq - a.seq);
}

function pathOf(url: string): string {
  try {
    const parsed = new URL(url);
    return parsed.pathname + parsed.search;
  } catch {
    return url;
  }
}

/** Rows recon asked for on purpose, to learn what "not there" looks like.
 *  They happened, so the ledger keeps them; they are h5i's own noise, so the
 *  reader is not shown them until they ask. */
function isCalibration(e: EndpointRow): boolean {
  return e.sources.length > 0 && e.sources.every((s) => s.from === "calibration");
}

/** The recon ledger: what is there, and how h5i knows. */
function Inventory({ detail }: { detail: SessionDetail }) {
  const all = detail.endpoints ?? [];
  const [showProbes, setShowProbes] = useState(false);
  const probes = all.filter(isCalibration).length;
  const endpoints = showProbes ? all : all.filter((e) => !isCalibration(e));
  if (all.length === 0) {
    return (
      <div className="sbx-pane-empty">
        no ledger yet. `h5i recon extract` reads what this session already
        fetched, and sends nothing
      </div>
    );
  }
  return (
    <div className="sbx-table-wrap">
      {probes > 0 ? (
        <div className="sbx-fleet-note">
          {probes} calibration probe{probes === 1 ? "" : "s"}{" "}
          {showProbes ? "shown" : "hidden"}: paths recon asked for because they
          should not exist, which is how triage learns what missing looks like.{" "}
          <button
            type="button"
            className="sbx-linkish"
            onClick={() => setShowProbes(!showProbes)}
          >
            {showProbes ? "hide them" : "show them"}
          </button>
        </div>
      ) : null}
      <HTMLTable compact striped className="sbx-table">
        <thead>
          <tr>
            <th>state</th>
            <th>method</th>
            <th>path</th>
            <th>identity</th>
            <th>status</th>
            <th>evidence</th>
          </tr>
        </thead>
        <tbody>
          {endpoints.map((e: EndpointRow) => (
            <tr key={e.id}>
              <td>
                <span className={`sbx-state sbx-state-${e.state}`}>{e.state}</span>
              </td>
              <td>{e.method}</td>
              <td className="sbx-cell-path" title={`${e.origin}${e.path}`}>
                {e.path}
                {e.params.length > 0 ? (
                  <span className="sbx-params">
                    ?{e.params.map((p) => p.name).join("&")}
                  </span>
                ) : null}
              </td>
              <td>{e.identity}</td>
              <td>{e.status ?? ""}</td>
              <td>
                {e.evidence.length > 0 ? (
                  <Cmd
                    text={`h5i websec show ${e.evidence[e.evidence.length - 1]} --session ${detail.name ?? detail.id}`}
                    hint={`${e.evidence[e.evidence.length - 1]} is the message that answered for this endpoint`}
                  />
                ) : (
                  <span className="sbx-dim">nothing has been there</span>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </HTMLTable>
    </div>
  );
}

/** Runs that spent requests, and how each of them ended. */
function Jobs({ detail }: { detail: SessionDetail }) {
  if (detail.jobs.length === 0) {
    return <div className="sbx-pane-empty">no run has spent a request here</div>;
  }
  return (
    <div className="sbx-table-wrap">
      <HTMLTable compact striped className="sbx-table">
        <thead>
          <tr>
            <th>run</th>
            <th>started</th>
            <th>requests</th>
            <th>rows</th>
            <th>ended</th>
          </tr>
        </thead>
        <tbody>
          {[...detail.jobs].reverse().map((j) => (
            <tr key={j.id} className={j.stopped ? "sbx-row-attn" : undefined}>
              <td className="sbx-cell-run">
                <div>{j.verb}</div>
                <Cmd
                  text={`h5i recon jobs resume ${j.id} --session ${detail.name ?? detail.id}`}
                  hint="runs the same parameters again, skipping what the ledger already answered"
                />
              </td>
              <td className="sbx-dim">{j.started_at.replace("T", " ").slice(0, 19)}</td>
              <td>{j.requests}</td>
              <td>{j.written}</td>
              <td>
                {j.ended_at === null ? (
                  <span className="sbx-dim">in flight</span>
                ) : j.stopped ? (
                  <span className="sbx-cell-why" title={j.stopped}>
                    {j.stopped}
                  </span>
                ) : (
                  "finished"
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </HTMLTable>
    </div>
  );
}

/** Remember that this browser has looked at a session's ending. */
export function markSeen(s: SessionRow): Record<string, string> {
  const seen = loadSeen();
  seen[s.id] = doneMark(s);
  try {
    window.localStorage.setItem(SEEN_KEY, JSON.stringify(seen));
  } catch {
    // A browser with storage blocked keeps its badges; nothing else breaks.
  }
  return seen;
}

export { loadSeen };
