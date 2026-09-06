//! The engine's command line.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use crate::engine::{Page, PageFactory, PageOptions};
use crate::broker::Broker;
use crate::policy::Policy;
use crate::receipt::{JsonlSink, MemorySink, Sink};
use crate::{fonts, Capabilities};
use h5i_error::H5iError;
use url::Url;

/// The environment variable h5i uses to hand a box its egress proxy.
const EGRESS_PROXY_VAR: &str = "H5I_EGRESS_PROXY";

/// Origins h5i granted this box, as a comma-separated list. Read as a default
/// for `--allow` so a box inherits its own `net.egress` without the agent
/// having to restate it (and without it being able to widen it by omission).
const ALLOW_VAR: &str = "H5I_BROWSER_ALLOW";

/// Where h5i wants the request log. Read as a default for `--receipts`, which
/// is what puts the fail-closed guarantee under h5i's control rather than the
/// caller's: no writable log, no fetch.
const RECEIPTS_VAR: &str = "H5I_BROWSER_RECEIPTS";

/// Where h5i wants `serve` to advertise its port, so `h5i box view` finds it
/// without being told this engine exists.
const STREAM_FILE_VAR: &str = "H5I_BROWSER_STREAM_FILE";

/// Where to advertise the session's control port. Optional: without it the
/// control file sits beside the stream file.
const CONTROL_FILE_VAR: &str = "H5I_BROWSER_CONTROL_FILE";

/// Where a session's Unix control socket is. Set by h5i for a session it placed
/// in a box, where a port cannot be reached across the per-run network
/// namespace; unset everywhere else, where the port is simpler.
const CONTROL_SOCKET_VAR: &str = "H5I_BROWSER_CONTROL_SOCKET";

/// Where h5i wants the agent's verbs recorded, so the console's agent-actions
/// pane has a source on an engine that has no mediated socket in front of it.
const ACTIONS_VAR: &str = "H5I_BROWSER_ACTIONS";

/// Where h5i wants the messages themselves kept. Unset in an ordinary session,
/// which stores no header and no body anywhere. See [`crate::capture`].
const CAPTURE_VAR: &str = "H5I_BROWSER_CAPTURE";

#[derive(Parser)]
#[command(
    name = "h5i __engine",
    version,
    about = "A lightweight visual browser for coding agents: every request is policy-checked and receipted before it reaches the wire."
)]
struct Cli {
    /// This process is the renderer half; its broker is on standard input.
    ///
    /// Hidden because it is not an interface. Nobody types it: the broker
    /// spawns the renderer with it, and `h5i browser open` is unchanged. See
    /// [`crate::ipc`] for what the two halves are and why.
    #[arg(long = "brokered", hide = true)]
    brokered: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load a page, then report what it says and what it tried to reach.
    Open {
        /// One or more URLs, or paths to local HTML files.
        ///
        /// Several share one browser: one connection pool, one cookie jar and
        /// one font set across the batch, so a run over twenty pages does not
        /// re-read the font files twenty times or throw away every keep-alive
        /// connection between them. They are read in order, and a page that
        /// fails does not stop the ones after it.
        #[arg(required = true, num_args = 1..)]
        targets: Vec<String>,

        #[command(flatten)]
        net: NetArgs,

        #[command(flatten)]
        view: ViewArgs,

        /// Write a PNG of the viewport here.
        #[arg(long, value_name = "PATH")]
        screenshot: Option<PathBuf>,

        /// Print the page's prose instead of its outline.
        #[arg(long)]
        text: bool,

        /// Emit one JSON object instead of human output.
        #[arg(long)]
        json: bool,
    },

    /// Serve a live view of a page over WebSocket.
    ///
    /// Speaks the format h5i's viewers already use, so `h5i box view` and
    /// `h5i box view --term` attach to this engine unchanged.
    Serve {
        /// A URL, or a path to a local HTML file.
        target: String,

        #[command(flatten)]
        net: NetArgs,

        #[command(flatten)]
        view: ViewArgs,

        /// Address to listen on. Port 0 picks a free one.
        #[arg(long, default_value = "127.0.0.1:0")]
        addr: String,

        /// JPEG quality for frames.
        #[arg(long, default_value_t = 80)]
        quality: u8,

        /// Advertise the bound port here. h5i's viewers look for
        /// `<env>/tmp/agent-browser/*.stream`.
        #[arg(long, value_name = "PATH")]
        stream_file: Option<PathBuf>,

        /// Advertise the session's control port here, for the verbs that drive
        /// it (`snapshot`, `navigate`, `click`). Defaults to the stream file
        /// with a `.control` extension, so a box that sets one gets both.
        #[arg(long, value_name = "PATH")]
        control_file: Option<PathBuf>,

        /// Also take control connections on a Unix socket here. Unix only.
        ///
        /// For a session inside an h5i box. Every `h5i box run` gets its own
        /// network namespace, so a verb carried in afterwards has a loopback of
        /// its own and cannot reach the port this session bound. A path can be
        /// reached because the box's filesystem is one filesystem across every
        /// run in it. Defaults to $H5I_BROWSER_CONTROL_SOCKET.
        #[arg(long, value_name = "PATH")]
        control_socket: Option<PathBuf>,

        /// Record the verbs an agent asks for here, as JSON lines. Defaults to
        /// $H5I_BROWSER_ACTIONS. With one set, a verb that cannot be recorded
        /// is refused rather than performed unseen.
        #[arg(long, value_name = "PATH")]
        actions: Option<PathBuf>,

        /// Serve one viewer, then exit.
        #[arg(long)]
        once: bool,
    },

    /// Drive the resident session a `serve` is holding open.
    ///
    /// This is the agent-facing half of the engine. `open` renders its own page
    /// and exits, so two `open`s share nothing, no history, no cookies, and
    /// nothing a viewer can watch. These verbs act on the page `serve` is
    /// holding, which is the page `h5i box view` is showing.
    #[command(subcommand)]
    Session(SessionVerb),

    /// Run a recorded script against the session a `serve` is holding open.
    Replay {
        /// The script, as written by `session script --save`.
        script: PathBuf,
        /// Keep going after a step fails, and report how many did.
        ///
        /// Off by default: a replay is a sequence, and a step that acts on a
        /// page the previous step failed to reach is acting somewhere the
        /// script never described.
        #[arg(long)]
        keep_going: bool,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Report what this engine can and cannot do, as JSON.
    ///
    /// h5i reads this to decide what to route here rather than inferring it
    /// from a version number.
    Capabilities {
        /// Report what this engine can do with `--script` on.
        #[arg(long)]
        script: bool,
    },

    /// List, show or check a browser identity.
    ///
    /// An identity is who a session says it is. On the wire and in the page,
    /// from one source. `list` names the built-ins, `show` prints one as the
    /// TOML you would edit, and `check` says whether this engine can stand
    /// behind it, and what it does not cover either way.
    #[cfg(feature = "identity")]
    Identity {
        #[command(subcommand)]
        verb: IdentityVerb,
    },

    /// Report the environment: fonts, proxy, and what the policy would allow.
    Doctor {
        #[command(flatten)]
        net: NetArgs,
    },
}

#[cfg(feature = "identity")]
#[derive(Subcommand)]
enum IdentityVerb {
    /// Name every identity that ships with the engine.
    List {
        /// Emit JSON instead of human output.
        #[arg(long)]
        json: bool,
    },
    /// Print an identity as the TOML you would edit to make your own.
    Show {
        /// A built-in name, or a path to a TOML file.
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Say whether this engine can stand behind an identity, and what it does
    /// not cover either way.
    Check {
        /// A built-in name, or a path to a TOML file.
        name: String,
        /// Check as a session that runs page script. Most of what an identity
        /// declares is only readable from script, so this changes the answer.
        #[arg(long)]
        script: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SessionVerb {
    /// What the session is on right now.
    Status {
        #[command(flatten)]
        at: SessionArgs,
    },
    /// The page as a model should read it: the fenced outline, with `@ref`
    /// handles for the things that can be acted on.
    Snapshot {
        /// Report only what changed since the last snapshot.
        ///
        /// Three hundred lines re-read after every click, of which four are
        /// new, is the wrong shape for an agent loop. When the page changed too
        /// much for a difference to be the shorter answer (a navigation, or a
        /// page that replaced its own body) the full outline is sent instead
        /// and the reply says which it is.
        #[arg(long)]
        delta: bool,
        /// Go here first, then read.
        ///
        /// One round trip instead of `navigate` followed by this verb, which
        /// costs an agent a whole turn through a model to read a reply it only
        /// uses to send the next request. Relative to the current page, like
        /// `navigate`. The reply carries the URL it ended up on, so a redirect
        /// is not silent.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },
    /// Hand the page to the human at the live view for as long as a login takes.
    Login {
        /// End login mode and make the page readable again.
        #[arg(long, conflicts_with = "on")]
        off: bool,
        /// Begin login mode. The default.
        #[arg(long)]
        on: bool,
        #[command(flatten)]
        at: SessionArgs,
    },
    /// Go to a URL, resolved against the current page like a click would be.
    Navigate {
        url: String,
        #[command(flatten)]
        at: SessionArgs,
    },
    /// Scroll the page. Negative scrolls up.
    ///
    /// With `--script`, the page's own `scroll` handlers run and its
    /// intersection observers are re-checked at the new offset, so a page that
    /// loads more as you go has loaded it before this replies.
    Scroll {
        /// Pixels to scroll by.
        #[arg(allow_negative_numbers = true)]
        by: f64,
        #[command(flatten)]
        at: SessionArgs,
    },
    /// Put text into a field, replacing what was there.
    Type {
        /// `e3` or `@e3` from a `snapshot`, then the text. With `--selector`,
        /// pass the text alone: the selector is the handle.
        ///
        /// Both positionals are optional to clap and checked in code, because
        /// clap refuses an optional positional before a required one, and with
        /// `--selector` the ref is genuinely absent. The check also gives a
        /// better message: it can say which of the two forms was half-used.
        #[arg(value_name = "REF|TEXT")]
        reference: Option<String>,
        #[arg(value_name = "TEXT")]
        text: Option<String>,
        /// A CSS selector instead of a `@ref`, which is what a `snapshot`'s `refs` carry beside
        /// each one.
        #[arg(long, value_name = "CSS")]
        selector: Option<String>,
        /// Address it by role instead, the way the outline names it.
        ///
        /// `--role button --name "Sign in"` matches the snapshot line
        /// `- button "Sign in"`, through the same computation that printed it.
        /// More stable than a selector against generated markup, where the
        /// class names change every build and the button is still called
        /// "Sign in". Refused when it matches more than one, with the list.
        #[arg(long, value_name = "ROLE", conflicts_with = "selector")]
        role: Option<String>,
        /// The accessible name to go with `--role`. Matched exactly.
        #[arg(long, value_name = "TEXT", requires = "role")]
        name: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },
    /// Submit the form containing a `@ref`.
    Submit {
        /// Any `@ref` inside the form. The submit button, or a field.
        reference: Option<String>,
        /// A CSS selector instead, which is what a `snapshot`'s `refs` carry
        /// beside each `@ref`.
        ///
        /// The durable handle. A `@ref` is a position in the reading that
        /// minted it and is checked against that reading; a selector names
        /// whatever it matches now, which is what makes it survive a
        /// navigation and what makes a recorded session replayable.
        #[arg(long, value_name = "CSS", conflicts_with = "reference")]
        selector: Option<String>,
        /// Address it by role instead, the way the outline names it.
        ///
        /// `--role button --name "Sign in"` matches the snapshot line
        /// `- button "Sign in"`, through the same computation that printed it.
        /// More stable than a selector against generated markup, where the
        /// class names change every build and the button is still called
        /// "Sign in". Refused when it matches more than one, with the list.
        #[arg(long, value_name = "ROLE", conflicts_with = "selector")]
        role: Option<String>,
        /// The accessible name to go with `--role`. Matched exactly.
        #[arg(long, value_name = "TEXT", requires = "role")]
        name: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },
    /// Wait until something is on the page, or until nothing can put it there.
    ///
    /// Three answers, and the third is the point: a page that runs no script,
    /// or a scripted page that has gone quiet, cannot grow the thing you are
    /// waiting for, so that comes back immediately rather than after a budget
    /// spent proving it.
    WaitFor {
        // No `--role` here, unlike the action verbs. Their `--selector` names
        // *a handle on one element*; this one is a *condition*, "wait until
        // something matches", and the two happen to share a spelling. A role
        // locator here would read as the first and behave as the second.
        /// A CSS selector that must match at least one element.
        #[arg(long, value_name = "CSS")]
        selector: Option<String>,
        /// Text that must appear in the outline a reader would see.
        #[arg(long, value_name = "TEXT", conflicts_with = "selector")]
        text: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Wait until a page expression is true.
    ///
    /// Needs a session started with `--script`. A condition that throws counts
    /// as *not yet* rather than as an error, because a page mid-build throws on
    /// the way to values it has not made.
    WaitForScript {
        /// The expression, evaluated in the page's realm.
        expr: String,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Pull structured data out of the page by selector.
    ///
    /// The schema is an object of field names to selector specs: `"h1"` for the
    /// first match's text, `["a"]` for every match, `{"selector":"a",
    /// "attr":"href"}` for an attribute, `[{"selector":"a","attr":"href"}]` for that
    /// attribute of every match, and `[{"selector":"li","fields":{…}}]` for one
    /// object per match with sub-selectors scoped to it.
    ///
    /// An empty array is a result. A schema where nothing matched is an error,
    /// because an object full of nulls looks like an answer.
    Extract {
        /// The schema, as JSON.
        schema: String,
        /// Go here first, then read.
        ///
        /// One round trip instead of `navigate` followed by this verb, which
        /// costs an agent a whole turn through a model to read a reply it only
        /// uses to send the next request. Relative to the current page, like
        /// `navigate`. The reply carries the URL it ended up on, so a redirect
        /// is not silent.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// The page as markdown: what a reader would read, without the handles.
    Markdown {
        /// Stop after this many bytes. Truncation is always announced.
        #[arg(long, value_name = "BYTES")]
        max_bytes: Option<usize>,
        /// Go here first, then read.
        ///
        /// One round trip instead of `navigate` followed by this verb, which
        /// costs an agent a whole turn through a model to read a reply it only
        /// uses to send the next request. Relative to the current page, like
        /// `navigate`. The reply carries the URL it ended up on, so a redirect
        /// is not silent.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// What this session did, as something that can be run again.
    ///
    /// Made of verified CSS selectors rather than `@ref` ordinals, because an
    /// ordinal names a position in the reading that minted it and a replay happens
    /// against a later page. Steps whose element had no verifiable selector are
    /// dropped and counted rather than written down wrongly.
    ///
    /// Reads are not in it: a replay exists to reach a state, and a snapshot changes
    /// nothing. `type` records the placeholder it was given, never a credential.
    Script {
        /// Write the steps here as JSON, for `replay`.
        #[arg(long, value_name = "PATH")]
        save: Option<PathBuf>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Set a checkbox or radio to a state, rather than toggling it.
    ///
    /// Prefer this to `click` on a checkbox. A click *toggles*, so a recorded
    /// session that clicks one reaches a different state depending on what the
    /// page was serving; setting a state is idempotent and replays to the same
    /// place. Fires `input` and `change` like a real edit, and turns off the
    /// rest of a radio group.
    SetChecked {
        /// `e3` or `@e3` from a `snapshot`, then `true` or `false`.
        ///
        /// With `--selector`, pass the state alone.
        #[arg(value_name = "REF|STATE")]
        reference: Option<String>,
        #[arg(value_name = "STATE")]
        checked: Option<String>,
        /// A CSS selector instead of a `@ref`.
        ///
        /// No `conflicts_with`, unlike `click` and `submit`: with a selector
        /// the remaining positional carries the *value*, so the two are used
        /// together rather than instead of each other.
        #[arg(long, value_name = "CSS")]
        selector: Option<String>,
        /// Address it by role instead, the way the outline names it.
        ///
        /// `--role button --name "Sign in"` matches the snapshot line
        /// `- button "Sign in"`, through the same computation that printed it.
        /// More stable than a selector against generated markup, where the
        /// class names change every build and the button is still called
        /// "Sign in". Refused when it matches more than one, with the list.
        #[arg(long, value_name = "ROLE", conflicts_with = "selector")]
        role: Option<String>,
        /// The accessible name to go with `--role`. Matched exactly.
        #[arg(long, value_name = "TEXT", requires = "role")]
        name: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Choose an option in a `<select>`, by its value or the text it shows.
    ///
    /// Value first, then text: an agent reading a snapshot has the text, and a
    /// recorded script should carry the value, because that is what the form
    /// submits and what survives a re-render.
    Select {
        /// `e3` or `@e3` from a `snapshot`, then the option.
        ///
        /// With `--selector`, pass the option alone.
        #[arg(value_name = "REF|OPTION")]
        reference: Option<String>,
        #[arg(value_name = "OPTION")]
        option: Option<String>,
        /// A CSS selector instead of a `@ref`.
        ///
        /// No `conflicts_with`, unlike `click` and `submit`: with a selector
        /// the remaining positional carries the *value*, so the two are used
        /// together rather than instead of each other.
        #[arg(long, value_name = "CSS")]
        selector: Option<String>,
        /// Address it by role instead, the way the outline names it.
        ///
        /// `--role button --name "Sign in"` matches the snapshot line
        /// `- button "Sign in"`, through the same computation that printed it.
        /// More stable than a selector against generated markup, where the
        /// class names change every build and the button is still called
        /// "Sign in". Refused when it matches more than one, with the list.
        #[arg(long, value_name = "ROLE", conflicts_with = "selector")]
        role: Option<String>,
        /// The accessible name to go with `--role`. Matched exactly.
        #[arg(long, value_name = "TEXT", requires = "role")]
        name: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Send a key that does something: Enter, Escape, Tab, ArrowDown.
    ///
    /// Not typing. `type` enters text; this sends the keys a page *listens*
    /// for, and merging them would make one verb whose meaning depended on its
    /// argument. Fires keydown, keypress and keyup, because a page may be
    /// waiting on any of the three.
    Press {
        /// `e3` or `@e3` from a `snapshot`, then the key.
        ///
        /// With `--selector`, pass the key alone. Spelled as
        /// `KeyboardEvent.key` spells it: `Enter`, `Escape`, `Tab`.
        #[arg(value_name = "REF|KEY")]
        reference: Option<String>,
        #[arg(value_name = "KEY")]
        key: Option<String>,
        /// A CSS selector instead of a `@ref`.
        ///
        /// No `conflicts_with`, unlike `click` and `submit`: with a selector
        /// the remaining positional carries the *value*, so the two are used
        /// together rather than instead of each other.
        #[arg(long, value_name = "CSS")]
        selector: Option<String>,
        /// Address it by role instead, the way the outline names it.
        ///
        /// `--role button --name "Sign in"` matches the snapshot line
        /// `- button "Sign in"`, through the same computation that printed it.
        /// More stable than a selector against generated markup, where the
        /// class names change every build and the button is still called
        /// "Sign in". Refused when it matches more than one, with the list.
        #[arg(long, value_name = "ROLE", conflicts_with = "selector")]
        role: Option<String>,
        /// The accessible name to go with `--role`. Matched exactly.
        #[arg(long, value_name = "TEXT", requires = "role")]
        name: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Locate elements by role and name, the way the outline names them.
    Find {
        /// `button`, `link`, `textbox`, `checkbox`, `radio`, `combobox`,
        /// `image`, `heading`, `paragraph`, `listitem`, `cell`.
        #[arg(long, value_name = "ROLE")]
        role: String,
        /// The accessible name, matched exactly after collapsing whitespace.
        ///
        /// Exact rather than a substring: `--name Save` matching "Save as
        /// draft" and "Discard without saving" would hand back three elements
        /// where one was asked for.
        #[arg(long, value_name = "TEXT")]
        name: Option<String>,
        /// Go here first, then look.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Write a PNG of the page as it is right now.
    ///
    /// The path is required and is not defaulted here: h5i names every artifact
    /// a session produces, and an engine that picked its own filename would be
    /// the one place that rule did not hold. `h5i browser screenshot` is what
    /// supplies it.
    Screenshot {
        /// Where to write the PNG. Named by the caller, always.
        #[arg(long, value_name = "PATH")]
        path: PathBuf,
        /// Go here first, then paint.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Fetch the current URL again.
    ///
    /// Takes no URL: a reload that went somewhere else would be `navigate`
    /// wearing a name that says it is not going anywhere. After a redirect it
    /// re-fetches where the session actually is, not the hop that got it there.
    Reload {
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Open a WebSocket, send frames, and report what came back.
    Socket {
        /// The `ws://` or `wss://` endpoint.
        #[arg(value_name = "URL")]
        url: String,
        /// A text frame to send. Repeatable, sent in order.
        #[arg(long = "send", value_name = "TEXT")]
        send: Vec<String>,
        /// How long to listen after the last frame, in milliseconds.
        #[arg(long = "wait-ms", value_name = "MS")]
        wait_ms: Option<u64>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// What the page publishes about itself: JSON-LD, OpenGraph, `<meta>`.
    ///
    /// The cheapest read there is. An outline is the page's content and costs
    /// hundreds of lines; this is a few hundred bytes the page already wrote down
    /// for the purpose, and the one read where the answer is the page's own words
    /// rather than something inferred from them. A page with no metadata is a
    /// result, not an error.
    Structured {
        /// Go here first, then read.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// What the page's media says: `<track>` captions, fetched and parsed.
    Transcript {
        /// Go here first, then read.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// Prefer this language, for the words and for the outline alike.
        /// Prefix-matched against `srclang`, so `en` finds `en-GB`.
        #[arg(long, value_name = "LANG")]
        lang: Option<String>,
        /// The ceiling on caption text carried out of one track.
        #[arg(long, value_name = "BYTES")]
        max_bytes: Option<usize>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Which credentials this session can use, by name.
    ///
    /// Names only. No verb in this engine returns a credential's value: the
    /// model names one, the engine resolves it on the way into the field, and
    /// the reply echoes the placeholder. Only the `H5I_SECRET_` namespace is
    /// reachable, so h5i's own configuration is not.
    Env {
        #[command(flatten)]
        at: SessionArgs,
    },

    /// The request log: what this session asked for, and what was refused.
    ///
    /// The engine *is* the HTTP client here, so this is the decision record the
    /// broker wrote before the bytes moved, not an observation of the network
    /// made from beside it. If a request is not in this list, it did not
    /// happen.
    Requests {
        /// Only what happened after this sequence number.
        ///
        /// Pass back the `cursor` from a previous answer to see just what is
        /// new, the way `snapshot --delta` works and for the same reason.
        #[arg(long, value_name = "SEQ")]
        since: Option<u64>,
        /// Only this method.
        #[arg(long, value_name = "METHOD")]
        method: Option<String>,
        /// Only rows whose URL contains this.
        #[arg(long, value_name = "TEXT")]
        url_contains: Option<String>,
        /// Only responses with this status.
        #[arg(long, value_name = "CODE")]
        status: Option<u16>,
        /// Only `navigation`, `subresource`, `frame`, `redirect` or `replay`.
        #[arg(long, value_name = "KIND")]
        initiator: Option<String>,
        /// Only what policy refused.
        #[arg(long)]
        denied_only: bool,
        /// At most this many rows, newest last.
        #[arg(long, value_name = "N")]
        limit: Option<u64>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Send a stored request again, with changes.
    ///
    /// Needs a session opened with `--capture`: a request nobody stored cannot
    /// be sent again. The edits are applied in the broker, which is where the
    /// stored request's credentials already are, so this process never holds
    /// them.
    Resend {
        /// The sequence number to send again, as `requests` lists them.
        ///
        /// Not needed with `--request`, which carries the whole message from
        /// somewhere else. One or the other, never both.
        #[arg(long, value_name = "SEQ", required_unless_present = "request")]
        from: Option<u64>,
        /// `target=value`, repeatable, applied in order.
        #[arg(long = "set", value_name = "TARGET=VALUE")]
        set: Vec<String>,
        /// `target=path`: the value is the file's bytes, whatever they are.
        ///
        /// Applied after every `--set`. These are the edits that cannot be
        /// written on a command line: a real image, a polyglot, anything a
        /// magic-number check will look at.
        #[arg(long = "set-file", value_name = "TARGET=PATH")]
        set_file: Vec<String>,
        /// A target to remove.
        #[arg(long = "unset", value_name = "TARGET")]
        unset: Vec<String>,
        /// Add a target that is not there rather than refusing.
        #[arg(long)]
        create: bool,
        /// Send it this many times, and report each send's clock.
        #[arg(long, default_value_t = 1, value_name = "N")]
        repeat: u32,
        /// Release the sends together rather than one after another.
        #[arg(long)]
        together: bool,
        /// Stop at the first redirect and report it, rather than following it.
        #[arg(long)]
        no_follow: bool,
        /// Start the page's network allowance again before sending.
        #[arg(long)]
        reset_budget: bool,
        /// A whole request, as JSON, instead of one from this session's store.
        ///
        /// `{"method":…,"url":…,"headers":[[name,value],…],"body_base64":…}`.
        /// What `h5i browser resend --as` sends when the message came from
        /// another session.
        #[arg(long, value_name = "JSON", conflicts_with = "from")]
        request: Option<String>,
        /// Write this request-target without URL normalization.
        #[arg(long = "raw-target", value_name = "TARGET")]
        raw_target: Option<String>,
        /// Write a complete base64-encoded request unchanged.
        #[arg(long = "raw-request", value_name = "BASE64")]
        raw_request: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },

    /// Follow a `@ref` from the last snapshot.
    Click {
        /// `e3` or `@e3`, from a `snapshot`.
        reference: Option<String>,
        /// A CSS selector instead, which is what a `snapshot`'s `refs` carry
        /// beside each `@ref`.
        ///
        /// The durable handle. A `@ref` is a position in the reading that
        /// minted it and is checked against that reading; a selector names
        /// whatever it matches now, which is what makes it survive a
        /// navigation and what makes a recorded session replayable.
        #[arg(long, value_name = "CSS", conflicts_with = "reference")]
        selector: Option<String>,
        /// Address it by role instead, the way the outline names it.
        ///
        /// `--role button --name "Sign in"` matches the snapshot line
        /// `- button "Sign in"`, through the same computation that printed it.
        /// More stable than a selector against generated markup, where the
        /// class names change every build and the button is still called
        /// "Sign in". Refused when it matches more than one, with the list.
        #[arg(long, value_name = "ROLE", conflicts_with = "selector")]
        role: Option<String>,
        /// The accessible name to go with `--role`. Matched exactly.
        #[arg(long, value_name = "TEXT", requires = "role")]
        name: Option<String>,
        #[command(flatten)]
        at: SessionArgs,
    },
}

#[derive(Args, Clone)]
struct SessionArgs {
    /// The file a `serve` wrote its control port into. Defaults to
    /// $H5I_BROWSER_CONTROL_FILE, then to the control file beside
    /// $H5I_BROWSER_STREAM_FILE, so inside a box these verbs need no flags.
    #[arg(long, value_name = "PATH")]
    control_file: Option<PathBuf>,

    /// The control port directly, when there is no file to read it from.
    #[arg(long, conflicts_with = "control_file")]
    port: Option<u16>,

    /// The session's Unix control socket, when it has one. Unix only.
    ///
    /// Preferred over a port whenever it is set, because the arrangement that
    /// needs it, a session in a box, is the one where a port cannot work.
    /// Defaults to $H5I_BROWSER_CONTROL_SOCKET.
    #[arg(long, value_name = "PATH")]
    control_socket: Option<PathBuf>,

    /// Print the session's raw JSON answer instead of human output.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Clone)]
struct NetArgs {
    /// Grant an origin. Repeatable. Without any, nothing remote is reachable.
    #[arg(long = "allow", value_name = "ORIGIN")]
    allow: Vec<String>,

    /// Refuse loopback too (it is reachable by default: it is the dev server).
    #[arg(long)]
    no_loopback: bool,

    /// Grant every remote origin.
    #[arg(long)]
    allow_any_remote: bool,

    /// Let this session's pages send credentials cross-origin as a browser
    /// does: `mode: "no-cors"` with `credentials: "include"`.
    ///
    /// Refused by default: an opaque response cannot be checked. That refusal
    /// is also the classic POST-CSRF vector, so it stopped h5i acting as the
    /// victim in a CSRF test.
    #[arg(long = "permissive-cors")]
    permissive_cors: bool,

    /// Append the request log here as JSON lines.
    #[arg(long, value_name = "PATH")]
    receipts: Option<PathBuf>,

    /// Mirror the cookie jar to a file, and read it at start.
    #[arg(long, value_name = "PATH")]
    cookie_jar: Option<PathBuf>,

    /// Keep the messages themselves here: headers and bodies, both directions.
    ///
    /// Off unless asked for, and separate from `--receipts` on purpose. The
    /// receipt is the account and is safe to paste anywhere; this is the
    /// evidence, and it holds session cookies and `Authorization` headers in
    /// full. Defaults to $H5I_BROWSER_CAPTURE.
    #[arg(long, value_name = "DIR")]
    capture: Option<PathBuf>,

    /// The egress proxy to route through. Defaults to $H5I_EGRESS_PROXY.
    #[arg(long, value_name = "URL")]
    proxy: Option<String>,

    /// Who this session says it is: a built-in name, or a path to a TOML file.
    #[cfg(feature = "identity")]
    #[arg(long, value_name = "NAME|PATH", default_value = "native")]
    identity: String,

    /// Refuse a response larger than this many bytes.
    #[arg(long, default_value_t = 8 * 1024 * 1024, value_name = "BYTES")]
    max_response_bytes: u64,

    /// How many redirect hops to follow. Every hop is policy-checked.
    #[arg(long, default_value_t = 5)]
    max_redirects: usize,

    /// How many requests one page may make before the rest are refused.
    ///
    /// Every other limit here is per *request*: a size cap, a redirect count, a
    /// timeout. None of them bounds a page that makes many, and a script in a
    /// loop is exactly that. Resets when the agent navigates, because a fresh
    /// page is a fresh decision by the agent and the ceiling exists to bound
    /// untrusted page code rather than the principal driving the engine.
    #[arg(long, default_value_t = 500, value_name = "N")]
    max_requests: u64,

    /// How many bytes one page may pull across the wire, in total.
    #[arg(long, default_value_t = 64 * 1024 * 1024, value_name = "BYTES")]
    max_wire_bytes: u64,

    /// How many seconds one page may spend waiting on the network, summed.
    ///
    /// Not a per-request timeout: a hundred requests each well inside the
    /// 30-second limit are together minutes an agent is waiting.
    #[arg(long, default_value_t = 60, value_name = "SECONDS")]
    max_network_seconds: u64,
}

#[derive(Args, Clone)]
struct ViewArgs {
    /// How long one navigation may take, first byte to last.
    ///
    /// The bound the per-phase budgets could not give. A request timeout bounds
    /// a request and the script-phase budget bounds the script; a page inside
    /// both can still take the better part of a minute.
    #[arg(long, default_value_t = 45, value_name = "SECONDS")]
    navigation_seconds: u64,

    /// How long a page's script may run, in seconds.
    #[arg(long, default_value_t = 0, value_name = "SECONDS")]
    script_seconds: u64,

    /// Install the WebIDL member decoration: enumerable interface members, and the
    /// brand check that makes an accessor reached on a prototype throw.
    ///
    /// For instruments. `idlharness` checks both on every member of every interface;
    /// a page reads `el.href` and never asks whether the descriptor is enumerable.
    /// Installing it rebuilds every descriptor of every interface prototype, which
    /// measured 15 ms of the 83 ms a script realm cost, on every page, for something
    /// one harness looks at.
    #[arg(long)]
    webidl_conformance: bool,

    #[arg(long, default_value_t = 1280)]
    width: u32,

    #[arg(long, default_value_t = 720)]
    height: u32,

    #[arg(long, default_value_t = 1.0)]
    scale: f32,

    /// A font file to register. Repeatable, and never subject to the scan cap.
    #[arg(long = "font-file", value_name = "PATH")]
    font_files: Vec<PathBuf>,

    /// A directory to scan for fonts. Repeatable. Replaces the defaults.
    #[arg(long = "font-dir", value_name = "PATH")]
    font_dirs: Vec<PathBuf>,

    /// Most lines of outline to emit.
    #[arg(long, default_value_t = 500)]
    max_snapshot_lines: usize,

    /// Run the page's own JavaScript. *Limited preview*. See the README for
    /// what is and is not implemented.
    ///
    /// Off by default on purpose. With script off, page-borne prompt injection
    /// has no delivery channel at all because no engine is running; turning it
    /// on spends that, and it is a decision rather than a default (ROADMAP
    /// §12.5).
    #[arg(long)]
    script: bool,
}

/// Run the engine's CLI over `args`, which must include the program name.
///
/// Exits the process on failure rather than returning, because the caller is a
/// `main` whose only remaining job would be to do the same thing. The prefix on
/// the error names the engine, not h5i: a page that failed to load is the
/// engine's answer, and attributing it to the caller would send someone looking
/// in the wrong place.
pub fn main<I, T>(args: I) -> !
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    match run_on_a_deep_stack(args) {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("h5i browser engine: {error}");
            std::process::exit(1);
        }
    }
}

/// How much stack the engine gives itself.
const ENGINE_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Run the engine on a thread whose stack this process chose.
///
/// A thread rather than `main` itself, because a process's main stack size is
/// set by the loader from `RLIMIT_STACK` and cannot be raised from inside it.
/// Everything the engine does happens on this thread: `Page` is `!Send` and is
/// created here, so nothing crosses back.
///
/// If the thread cannot be started, the work happens here instead: a host too
/// short of resources to spawn a thread should still be able to read a page.
fn run_on_a_deep_stack<I, T>(args: I) -> Result<(), H5iError>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let argv: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
    let here = argv.clone();
    let spawned = std::thread::Builder::new()
        .name("h5i-engine".to_string())
        .stack_size(ENGINE_STACK_BYTES)
        .spawn(move || run(argv));
    match spawned {
        Ok(handle) => match handle.join() {
            Ok(result) => result,
            // The thread panicked and the panic hook has already printed it.
            // Reported as a failure rather than swallowed into a zero exit.
            Err(_) => Err(H5iError::Metadata(
                "the engine thread ended unexpectedly".to_string(),
            )),
        },
        Err(error) => {
            eprintln!(
                "h5i browser engine: could not start on a stack of its own ({error}); running \
                 on this one, where a deeply nested page has whatever `ulimit -s` allows."
            );
            run(here)
        }
    }
}

/// Which half of the engine this process is.
///
/// Two processes by default: a broker that decides and records, and a renderer
/// that parses the page. `Whole` is the shape the engine had before the split,
/// kept because a host where the second process cannot be started is a host
/// that should still be able to read a page, and because being able to run
/// both shapes is what makes them comparable. See [`crate::ipc`].
#[cfg_attr(not(unix), allow(dead_code))]
enum Half {
    /// One process, brokering its own requests.
    Whole,
    /// This process renders. Everything that decides or records is on the
    /// other end of this.
    Renderer(Arc<crate::ipc::BrokerClient>),
}

fn run<I, T>(args: I) -> Result<(), H5iError>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let argv: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
    let cli = Cli::parse_from(&argv);
    // May not return: a process that becomes the broker spends the rest of its
    // life answering the renderer, and exits with the renderer's status.
    let half = half_for(&cli, &argv)?;
    dispatch(cli.command, &half)
}

/// Decide which half this process is, and, when it is the broker, never come
/// back.
fn half_for(cli: &Cli, argv: &[std::ffi::OsString]) -> Result<Half, H5iError> {
    if cli.brokered {
        return renderer_half();
    }
    // Only the two commands that load a page have a broker to split from. The
    // rest (`capabilities`, `doctor`, the session verbs, `replay`) either
    // answer from this process or talk to a session that already exists.
    let Some(net) = cli.command.net() else {
        return Ok(Half::Whole);
    };
    if !crate::ipc::splitting() {
        return Ok(Half::Whole);
    }
    become_broker(net, argv)
}

/// This process is the renderer: its broker is on the descriptor it was handed.
#[cfg(unix)]
fn renderer_half() -> Result<Half, H5iError> {
    Ok(Half::Renderer(crate::ipc::BrokerClient::on_stdin()?))
}

/// There is no renderer half where there is no split.
///
/// The transport is a socket pair a child inherits, which is a Unix
/// arrangement, so everywhere else the engine runs as one process. The way it
/// always did. A `--brokered` here is a flag h5i itself would never pass on
/// this platform, and saying so beats adopting whatever standard input happens
/// to be.
#[cfg(not(unix))]
fn renderer_half() -> Result<Half, H5iError> {
    Err(H5iError::Metadata(
        "`--brokered` is how the broker starts the renderer, and this platform does not run \
         the engine as two processes."
            .to_string(),
    ))
}

/// Build the broker, start the renderer under it, and serve until it exits.
///
/// Returns only when the renderer could not be started at all, and then the
/// engine runs as one process and *says so*. A sandbox nobody can see is
/// indistinguishable from one that was never applied, and the same is true of
/// a split.
#[cfg(unix)]
fn become_broker(net: &NetArgs, argv: &[std::ffi::OsString]) -> Result<Half, H5iError> {
    let broker = local_broker(net)?;
    let (mut renderer, socket) = match crate::ipc::spawn_renderer(argv) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!(
                "h5i-browser: {error}. Running as one process: the policy, the receipts, \
                 the cookie jar and the secrets are in the same process as the page's parsers."
            );
            return Ok(Half::Whole);
        }
    };

    crate::ipc::serve(broker, socket);

    // The renderer closed the socket, which is what exiting looks like from
    // here. Its status is this process's status: `h5i browser` is waiting on
    // one child and must not be told a page loaded because the broker's own
    // work went fine.
    let status = renderer
        .wait()
        .map_err(|e| H5iError::Metadata(format!("the renderer could not be waited for: {e}")))?;
    std::process::exit(exit_code(&status));
}

#[cfg(not(unix))]
fn become_broker(_net: &NetArgs, _argv: &[std::ffi::OsString]) -> Result<Half, H5iError> {
    Ok(Half::Whole)
}

/// A child's status as an exit code, including the one a signal produced.
///
/// A renderer killed by the kernel's OOM killer is the case worth naming: it
/// exits with no code at all, and reporting `0` there would say the page loaded.
#[cfg(unix)]
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

fn dispatch(command: Command, half: &Half) -> Result<(), H5iError> {
    match command {
        Command::Capabilities { script } => {
            // Reported for the configuration asked about, because what h5i
            // routes on is whether *this* invocation runs script.
            println!(
                "{}",
                serde_json::to_string_pretty(&Capabilities::with_script(script))?
            );
            Ok(())
        }
        #[cfg(feature = "identity")]
        Command::Identity { verb } => identity_verb(verb),
        Command::Doctor { net } => doctor(&net),
        Command::Session(verb) => session(verb),
        Command::Replay { script, keep_going, at } => replay(&script, keep_going, &at),
        Command::Open {
            targets,
            net,
            view,
            screenshot,
            text,
            json,
        } => open(half, &targets, &net, &view, screenshot, text, json),
        Command::Serve {
            target,
            net,
            view,
            addr,
            quality,
            stream_file,
            control_file,
            control_socket,
            actions,
            once,
        } => serve(
            half,
            &target,
            &net,
            &view,
            addr,
            quality,
            stream_file,
            control_file,
            control_socket,
            actions,
            once,
        ),
    }
}

impl Command {
    /// The network arguments this command brokers with, when it brokers at all.
    ///
    /// The two that load a page have one; the rest either answer from this
    /// process or speak to a session that already exists. It is also the test
    /// for whether this invocation splits into two processes, and those are the
    /// same question: a command with no broker has no halves.
    fn net(&self) -> Option<&NetArgs> {
        match self {
            Command::Open { net, .. } | Command::Serve { net, .. } => Some(net),
            #[cfg(feature = "identity")]
            Command::Identity { .. } => None,
            Command::Capabilities { .. }
            | Command::Doctor { .. }
            | Command::Session(_)
            | Command::Replay { .. } => None,
        }
    }
}

/// Build the factory and load the first page, shared by `open` and `serve`.
#[cfg(feature = "identity")]
fn broker_for(
    policy: Policy,
    sink: Arc<dyn Sink>,
    proxy: Option<&str>,
    limits: crate::budget::Limits,
    net: &NetArgs,
) -> Result<Arc<crate::net::LocalBroker>, H5iError> {
    crate::net::LocalBroker::with_identity(
        policy,
        sink,
        proxy,
        limits,
        Arc::new(identity_of(net)?),
        capture_store(net)?,
    )
}

#[cfg(not(feature = "identity"))]
fn broker_for(
    policy: Policy,
    sink: Arc<dyn Sink>,
    proxy: Option<&str>,
    limits: crate::budget::Limits,
    net: &NetArgs,
) -> Result<Arc<crate::net::LocalBroker>, H5iError> {
    crate::net::LocalBroker::with_limits(policy, sink, proxy, limits, capture_store(net)?)
}

#[cfg(feature = "identity")]
fn identity_of(net: &NetArgs) -> Result<crate::identity::Identity, H5iError> {
    let identity = crate::identity::Identity::resolve(&net.identity)?;
    let found = identity.incoherences();
    if !found.is_empty() {
        let lines: Vec<String> = found.iter().map(|f| format!("  {f}")).collect();
        return Err(H5iError::Metadata(format!(
            "the browser identity `{}` contradicts itself:\n{}",
            identity.name,
            lines.join("\n")
        )));
    }
    Ok(identity)
}

fn local_broker(net: &NetArgs) -> Result<Arc<crate::net::LocalBroker>, H5iError> {
    let broker = broker_for(
        build_policy(net),
        receipts_sink(net)?,
        proxy_of(net).as_deref(),
        crate::budget::Limits {
            max_requests: net.max_requests,
            max_wire_bytes: net.max_wire_bytes,
            // The decoded ceiling follows the wire one rather than being its
            // own flag: it exists to bound what compression expands into, so
            // tying it to the wire limit keeps the two from being set
            // inconsistently.
            max_decoded_bytes: net.max_wire_bytes.saturating_mul(4),
            max_network_time: std::time::Duration::from_secs(net.max_network_seconds),
        },
        net,
    )?;

    // The jar, if h5i named a file for it. Done here rather than in `serve`
    // because this is the function both halves reach: in split mode `serve`
    // runs in the renderer, which holds no jar at all.
    //
    // A failure here stops the session. The alternative is a browser that came
    // up logged out and said so in a warning nobody reads, which is the exact
    // shape of the defect this feature was written to remove.
    if let Some(path) = &net.cookie_jar {
        let loaded = broker.jar().persist_to(path).map_err(|reason| {
            H5iError::Metadata(format!(
                "the cookie jar at `{}` could not be used: {reason}",
                path.display()
            ))
        })?;
        if loaded.loaded > 0 {
            eprintln!(
                "h5i-browser: restored {} cookie(s) from {}",
                loaded.loaded,
                path.display()
            );
        }
        // Said, not swallowed. A row the jar refused is one no server could
        // have set (a cookie widened to a public suffix, a `__Host-` name
        // without the flags that name means) and a login that is missing
        // because of one should say so rather than look like a login that was
        // never saved.
        if loaded.refused > 0 {
            eprintln!(
                "h5i-browser: {} cookie(s) in {} were refused: they claim a scope or a \
                 name prefix no server could have set, so they were not restored",
                loaded.refused,
                path.display()
            );
        }
    }
    Ok(broker)
}

fn factory_for(half: &Half, net: &NetArgs, view: &ViewArgs) -> Result<PageFactory, H5iError> {
    // The identity's door, and it is here because here is where the two halves
    // of the question meet: the identity comes from `net`, and whether this
    // engine can stand behind it depends on `--script`, which comes from
    // `view`. Refused rather than trimmed to fit. An agent string claiming
    // Chrome in front of an engine with no client hints is more detectable than
    // no claim at all, so a partial application would be worse than nothing.
    #[cfg(feature = "identity")]
    {
        let identity = identity_of(net)?;
        identity.admit(&crate::Capabilities::with_script(view.script))?;
        // And the one contradiction that only exists once the identity meets
        // the run: a window cannot be larger than the screen it says it is on.
        if let Some(over) = identity.check_viewport(view.width, view.height) {
            return Err(H5iError::Metadata(format!(
                "the browser identity `{}` cannot be used at this size:\n  {over}",
                identity.name
            )));
        }
    }

    let broker: Arc<dyn Broker> = match half {
        Half::Whole => local_broker(net)?,
        // Nothing is built here: no policy, no sink, no jar, no secrets. This
        // process asks, and the answers come from a process the page cannot
        // reach.
        Half::Renderer(client) => client.clone(),
    };
    let font_setup = load_fonts(view);
    if font_setup.is_empty() {
        eprintln!("h5i-browser: no fonts registered; text will not be drawn.");
        eprintln!("      pass --font-file <path.ttf> or --font-dir <dir>.");
    }

    let factory = PageFactory::new(
        broker,
        font_setup.sources.clone(),
        PageOptions {
            width: view.width,
            height: view.height,
            scale: view.scale,
            max_snapshot_lines: view.max_snapshot_lines,
            script: view.script,
            script_budget: (view.script_seconds > 0)
                .then(|| std::time::Duration::from_secs(view.script_seconds)),
            webidl_conformance: view.webidl_conformance,
            navigation_budget: std::time::Duration::from_secs(view.navigation_seconds),
        },
    );
    Ok(factory)
}

/// One page, through a factory that already exists.
fn open_target(factory: &PageFactory, target: &str) -> Result<Page, H5iError> {
    Ok(match parse_target(target)? {
        Target::Remote(url) => factory.open(&url)?,
        Target::Local(path) => {
            // Bytes rather than `read_to_string`, so a local file gets the same
            // encoding treatment a fetched one does. `read_to_string` also
            // *refuses* a file that is not valid UTF-8, which is exactly the
            // file this path most needs to be able to open.
            let bytes = std::fs::read(&path).map_err(|e| H5iError::with_path(e, &path))?;
            factory.from_bytes(&bytes, None, &local_base(&path)?)
        }
    })
}

fn load(
    half: &Half,
    target: &str,
    net: &NetArgs,
    view: &ViewArgs,
) -> Result<(PageFactory, Page), H5iError> {
    let factory = factory_for(half, net, view)?;
    let page = open_target(&factory, target)?;
    Ok((factory, page))
}

#[allow(clippy::too_many_arguments)]
fn serve(
    half: &Half,
    target: &str,
    net: &NetArgs,
    view: &ViewArgs,
    addr: String,
    quality: u8,
    stream_file: Option<PathBuf>,
    control_file: Option<PathBuf>,
    control_socket: Option<PathBuf>,
    action_log: Option<PathBuf>,
    once: bool,
) -> Result<(), H5iError> {
    let (factory, page) = load(half, target, net, view)?;
    let control_socket =
        control_socket.or_else(|| std::env::var(CONTROL_SOCKET_VAR).ok().map(PathBuf::from));
    let stream_file = stream_file.or_else(|| std::env::var(STREAM_FILE_VAR).ok().map(PathBuf::from));
    let chosen = control_file
        .or_else(|| std::env::var(CONTROL_FILE_VAR).ok().map(PathBuf::from))
        .or_else(|| stream_file.as_deref().map(control_file_beside));
    // The other half of `session_port`'s default. Without this the two
    // disagree: the verbs would look somewhere `serve` never wrote.
    let defaulted = chosen.is_none();
    let control_file = chosen.or_else(default_control_file);

    // Created 0700 before anything is advertised into it, but *only* the
    // directory this binary chose. `session_port` already exempts a path
    // someone typed on the same principle, and applying the check here anyway
    // broke a documented invocation: SKILL.md tells an agent to give each
    // concurrent session its own `--control-file`, and
    // `serve --control-file /tmp/a.control` then aborted on `/tmp` being
    // mode 1777 before opening anything.
    if defaulted
        && let Some(path) = &control_file
        && let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        make_private_dir(parent)?;
    }
    let action_log = action_log.or_else(|| std::env::var(ACTIONS_VAR).ok().map(PathBuf::from));
    crate::stream::serve(
        factory,
        page,
        crate::stream::ServeOptions {
            addr,
            quality,
            stream_file,
            control_file,
            control_socket,
            action_log,
            once,
        },
    )
}


/// Drive the resident session: send a recorded script's steps through the
/// control channel, in order.
///
/// Deliberately not a second execution engine. Every step is an ordinary verb
/// request, so a replay is subject to the same policy checks, produces the same
/// receipts, and lands in the same action log as the session it was recorded
/// from. A replay that could bypass any of those would be a way to do things the
/// audited path refuses.
fn replay(script: &Path, keep_going: bool, at: &SessionArgs) -> Result<(), H5iError> {
    let text = std::fs::read_to_string(script).map_err(|e| H5iError::with_path(e, script))?;
    let recording: crate::replay::Recording = serde_json::from_str(&text)
        .map_err(|e| H5iError::Metadata(format!("{} is not a script: {e}", script.display())))?;

    if recording.dropped > 0 {
        // Said before anything runs, not after. Whoever is about to trust this
        // replay should know it is not the whole session.
        eprintln!(
            "note: this script is missing {} action(s) from the session it was recorded \
             from — no durable selector could be verified for them",
            recording.dropped
        );
    }

    let port = session_port(at)?;
    let mut ran = 0usize;
    let mut failed = 0usize;

    for (at_step, step) in recording.steps.iter().enumerate() {
        let reply = crate::stream::ask(port, &step.request())?;
        let ok = reply.get("ok").and_then(serde_json::Value::as_bool) == Some(true);
        if ok {
            ran += 1;
            if !at_json(at) {
                println!("{:>3}. {}", at_step + 1, step.render());
            }
            continue;
        }

        failed += 1;
        let reason = reply
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("the session refused, without saying why");
        eprintln!("{:>3}. {} — FAILED: {reason}", at_step + 1, step.render());
        if !keep_going {
            // A sequence, not a set. Carrying on would act on a page the
            // previous step failed to reach.
            return Err(H5iError::Metadata(format!(
                "step {} of {} failed: {reason}. The replay stopped there; pass \
                 --keep-going to run the rest anyway.",
                at_step + 1,
                recording.steps.len()
            )));
        }
    }

    if failed > 0 {
        return Err(H5iError::Metadata(format!(
            "{ran} of {} step(s) replayed; {failed} failed",
            recording.steps.len()
        )));
    }
    eprintln!("replayed {ran} step(s)");
    Ok(())
}

/// Whether this invocation asked for machine output.
fn at_json(at: &SessionArgs) -> bool {
    at.json
}

/// Resolve the "`@ref` and a value, or a locator and the value" shape.
fn two_positionals(
    verb: &str,
    what: &str,
    located: bool,
    first: &Option<String>,
    second: &Option<String>,
) -> Result<(Option<String>, String), H5iError> {
    match (located, first, second) {
        (true, Some(value), None) => Ok((None, value.clone())),
        (true, _, Some(_)) => Err(H5iError::Metadata(format!(
            "with `--selector` or `--role`, pass only {what}: the locator is the handle."
        ))),
        (false, Some(reference), Some(value)) => Ok((Some(reference.clone()), value.clone())),
        _ => Err(H5iError::Metadata(format!(
            "`{verb}` needs a `@ref` and {what}, or `--selector <css>` / `--role <role>` \
             and {what}."
        ))),
    }
}

#[cfg(test)]
mod positional_tests {
    use super::two_positionals;

    /// `--role` had to stop conflicting with the positional the value arrives
    /// in. Before that, `set-checked --role checkbox true` was rejected by clap
    /// and the flag existed on three verbs without being usable on any of them.
    #[test]
    fn a_locator_takes_the_value_in_the_first_positional() {
        let value = Some("true".to_string());
        let (reference, state) =
            two_positionals("set-checked", "a state", true, &value, &None).expect("the role form");
        assert_eq!(reference, None, "a locator is the handle; there is no ref");
        assert_eq!(state, "true");
    }

    #[test]
    fn a_ref_takes_both_positionals() {
        let (reference, state) = two_positionals(
            "set-checked",
            "a state",
            false,
            &Some("@e1".to_string()),
            &Some("false".to_string()),
        )
        .expect("the ref form");
        assert_eq!(reference.as_deref(), Some("@e1"));
        assert_eq!(state, "false");
    }

    #[test]
    fn half_of_either_form_says_which_forms_there_are() {
        let only_a_ref = two_positionals("select", "the option", false, &Some("@e1".into()), &None)
            .unwrap_err()
            .to_string();
        assert!(only_a_ref.contains("--role"), "{only_a_ref}");
        assert!(only_a_ref.contains("--selector"), "{only_a_ref}");

        // A locator plus both positionals is a caller using two handles.
        let both = two_positionals(
            "select",
            "the option",
            true,
            &Some("@e1".into()),
            &Some("x".into()),
        )
        .unwrap_err()
        .to_string();
        assert!(both.contains("the locator is the handle"), "{both}");
    }
}

fn session(verb: SessionVerb) -> Result<(), H5iError> {
    // Every name comes from `Verb`, so the CLI cannot ask for a verb the session
    // does not have. This used to be eight string literals that happened to
    // match eight others in `stream.rs`, with nothing enforcing the agreement.
    use crate::verbs::Verb;
    let (at, request) = match &verb {
        SessionVerb::Status { at } => (at, serde_json::json!({"verb": Verb::Status.name()})),
        SessionVerb::Snapshot { delta, url, at } => (
            at,
            serde_json::json!({"verb": Verb::Snapshot.name(), "delta": delta, "url": url}),
        ),
        SessionVerb::Login { off, on: _, at } => (
            at,
            serde_json::json!({"verb": Verb::Login.name(), "on": !off}),
        ),
        SessionVerb::Navigate { url, at } => (
            at,
            serde_json::json!({"verb": Verb::Navigate.name(), "url": url}),
        ),
        SessionVerb::Scroll { by, at } => (
            at,
            serde_json::json!({"verb": Verb::Scroll.name(), "by": by}),
        ),
        SessionVerb::Type { reference, text, selector, role, name, at } => {
            let (reference, text) =
                two_positionals("type", "the text", selector.is_some() || role.is_some(), reference, text)?;
            (
                at,
                serde_json::json!({
                    "verb": Verb::Type.name(),
                    "ref": reference,
                    "selector": selector,
                    "role": role,
                    "name": name,
                    "text": text,
                }),
            )
        }
        SessionVerb::Submit { reference, selector, role, name, at } => (
            at,
            serde_json::json!({
                "verb": Verb::Submit.name(), "ref": reference, "selector": selector,
                "role": role, "name": name,
            }),
        ),
        SessionVerb::Click { reference, selector, role, name, at } => (
            at,
            serde_json::json!({
                "verb": Verb::Click.name(), "ref": reference, "selector": selector,
                "role": role, "name": name,
            }),
        ),
        SessionVerb::Requests {
            since,
            method,
            url_contains,
            status,
            initiator,
            denied_only,
            limit,
            at,
        } => (
            at,
            serde_json::json!({
                "verb": Verb::Requests.name(),
                "since": since,
                "method": method,
                "url_contains": url_contains,
                "status": status,
                "initiator": initiator,
                "denied_only": denied_only,
                "limit": limit,
            }),
        ),
        SessionVerb::Resend {
            from,
            set,
            set_file,
            unset,
            create,
            repeat,
            together,
            no_follow,
            reset_budget,
            request,
            raw_target,
            raw_request,
            at,
        } => {
            let composed: Option<serde_json::Value> = match request {
                None => None,
                Some(text) => match serde_json::from_str(text) {
                    Ok(value) => Some(value),
                    Err(e) => {
                        return Err(H5iError::Metadata(format!(
                            "`--request` is not JSON: {e}"
                        )));
                    }
                },
            };
            // Read here and carried as base64: this hop is a JSON control
            // message, and the point of the flag is bytes JSON cannot hold.
            let mut from_files: Vec<(String, String)> = Vec::new();
            for spec in set_file {
                let (target, path) = spec.split_once('=').ok_or_else(|| {
                    H5iError::Metadata(format!(
                        "`--set-file {spec}` is `target=path`, and this has no `=`"
                    ))
                })?;
                let bytes = read_a_payload(target, path)?;
                use base64::Engine as _;
                from_files.push((
                    target.to_string(),
                    base64::engine::general_purpose::STANDARD.encode(&bytes),
                ));
            }
            (
                at,
                serde_json::json!({
                    "verb": Verb::Resend.name(),
                    "from": from,
                    "set": set,
                    "set_file": from_files,
                    "unset": unset,
                    "create": create,
                    "repeat": repeat,
                    "together": together,
                    "no_follow": no_follow,
                    "reset_budget": reset_budget,
                    "request": composed,
                    "raw_target": raw_target,
                    "raw_request": raw_request,
                }),
            )
        }
        SessionVerb::WaitFor {
            selector,
            text,
            at,
        } => (
            at,
            serde_json::json!({
                "verb": Verb::WaitFor.name(),
                "selector": selector,
                "text": text,
            }),
        ),
        SessionVerb::WaitForScript { expr, at } => (
            at,
            serde_json::json!({"verb": Verb::WaitForScript.name(), "expr": expr}),
        ),
        SessionVerb::Extract { schema, url, at } => {
            // Parsed here so a typo is a message from the CLI rather than a
            // refusal from the far end of a socket.
            let parsed: serde_json::Value = serde_json::from_str(schema).map_err(|e| {
                H5iError::Metadata(format!("the schema is not valid JSON: {e}"))
            })?;
            (
                at,
                serde_json::json!({
                    "verb": Verb::Extract.name(), "schema": parsed, "url": url,
                }),
            )
        }
        SessionVerb::Markdown { max_bytes, url, at } => (
            at,
            serde_json::json!({
                "verb": Verb::Markdown.name(), "max_bytes": max_bytes, "url": url,
            }),
        ),
        SessionVerb::Env { at } => (at, serde_json::json!({"verb": Verb::Env.name()})),
        SessionVerb::SetChecked { reference, checked, selector, role, name, at } => {
            let (reference, state) =
                two_positionals("set-checked", "`true` or `false`", selector.is_some() || role.is_some(), reference, checked)?;
            // Parsed here rather than by clap, because the positional had to be
            // a string for the reasons `two_positionals` explains, and a
            // typo'd state should say what it should have been rather than
            // silently meaning `false`.
            let checked = match state.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => true,
                "false" | "no" | "off" | "0" => false,
                other => {
                    return Err(H5iError::Metadata(format!(
                        "`{other}` is not a state. `set-checked` takes `true` or `false`: it \
                         sets a state rather than toggling one, which is what makes it \
                         replayable."
                    )))
                }
            };
            (
                at,
                serde_json::json!({
                    "verb": Verb::SetChecked.name(),
                    "ref": reference,
                    "selector": selector,
                    "role": role,
                    "name": name,
                    "checked": checked,
                }),
            )
        }
        SessionVerb::Select { reference, option, selector, role, name, at } => {
            let (reference, option) =
                two_positionals("select", "the option", selector.is_some() || role.is_some(), reference, option)?;
            (
                at,
                serde_json::json!({
                    "verb": Verb::Select.name(),
                    "ref": reference,
                    "selector": selector,
                    "role": role,
                    "name": name,
                    "option": option,
                }),
            )
        }
        SessionVerb::Press { reference, key, selector, role, name, at } => {
            let (reference, key) =
                two_positionals("press", "the key", selector.is_some() || role.is_some(), reference, key)?;
            (
                at,
                serde_json::json!({
                    "verb": Verb::Press.name(),
                    "ref": reference,
                    "selector": selector,
                    "role": role,
                    "name": name,
                    "key": key,
                }),
            )
        }
        SessionVerb::Find { role, name, url, at } => (
            at,
            serde_json::json!({
                "verb": Verb::Find.name(), "role": role, "name": name, "url": url,
            }),
        ),
        SessionVerb::Screenshot { path, url, at } => (
            at,
            serde_json::json!({
                "verb": Verb::Screenshot.name(),
                "path": path.display().to_string(),
                "url": url,
            }),
        ),
        SessionVerb::Reload { at } => (at, serde_json::json!({"verb": Verb::Reload.name()})),
        SessionVerb::Socket {
            url,
            send,
            wait_ms,
            at,
        } => (
            at,
            serde_json::json!({
                "verb": Verb::Socket.name(),
                "url": url,
                "send": send,
                "wait_ms": wait_ms,
            }),
        ),
        SessionVerb::Structured { url, at } => (
            at,
            serde_json::json!({"verb": Verb::Structured.name(), "url": url}),
        ),
        SessionVerb::Transcript {
            url,
            lang,
            max_bytes,
            at,
        } => (
            at,
            serde_json::json!({
                "verb": Verb::Transcript.name(),
                "url": url,
                "lang": lang,
                "max_bytes": max_bytes,
            }),
        ),
        SessionVerb::Script { save: _, at } => {
            (at, serde_json::json!({"verb": Verb::Script.name()}))
        }
    };

    // The socket wins when there is one. It is only ever set deliberately,
    // by a flag or by h5i inside a box, and in a box it is the only channel
    // that reaches the session at all.
    let reply = match session_socket(at) {
        #[cfg(unix)]
        Some(path) => crate::stream::ask_unix(&path, &request)?,
        // Refused rather than quietly falling back to the port. A caller who
        // named a socket named it for a reason, and answering from somewhere
        // else is worse than not answering.
        #[cfg(not(unix))]
        Some(path) => {
            return Err(H5iError::Metadata(format!(
                "a Unix control socket ({}) is not available on this platform. \
                 Pass `--control-file` or `--port` instead.",
                path.display()
            )))
        }
        None => crate::stream::ask(session_port(at)?, &request)?,
    };

    if at.json {
        println!("{reply}");
        // A refusal is still an answer, and `--json` promised the answer. The
        // exit code carries the verdict so a script does not have to parse it.
        return exit_status(&reply);
    }

    if reply.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let text = reply
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("the session refused, without saying why");
        return Err(H5iError::Metadata(text.to_string()));
    }

    // A recording asked to be kept. Written as the step list rather than the
    // rendered form, because `replay` reads this back and a comment is not a
    // step.
    if let SessionVerb::Script { save: Some(path), .. } = &verb {
        let steps = reply.get("steps").cloned().unwrap_or(serde_json::json!([]));
        let document = serde_json::json!({
            "start_url": reply.get("start_url").cloned().unwrap_or(serde_json::Value::Null),
            "steps": steps,
            "dropped": reply.get("dropped").cloned().unwrap_or(serde_json::json!(0)),
        });
        let text = serde_json::to_string_pretty(&document)
            .map_err(|e| H5iError::Metadata(format!("could not write the script: {e}")))?;
        std::fs::write(path, text).map_err(|e| H5iError::with_path(e, path))?;
        let dropped = reply.get("dropped").and_then(serde_json::Value::as_u64).unwrap_or(0);
        let count = reply
            .get("steps")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        eprintln!("wrote {count} step(s) to {}", path.display());
        if dropped > 0 {
            // Never silent. A script shorter than the session it came from is
            // the fact the person about to run it most needs.
            eprintln!(
                "note: {dropped} action(s) are not in it — no durable selector could be \
                 verified for what they acted on"
            );
        }
        return Ok(());
    }

    // The snapshot is the whole point of the verb; everything else is a line.
    if let Some(text) = reply.get("text").and_then(serde_json::Value::as_str) {
        // Why the full outline arrived when a difference was asked for. Said
        // rather than left to be inferred from the length.
        if let Some(reason) = reply.get("reason").and_then(serde_json::Value::as_str) {
            eprintln!("note: {reason}");
        }
        println!("{text}");
    } else if let Some(message) = reply.get("message").and_then(serde_json::Value::as_str) {
        println!("{message}");
    } else if let Some(moved) = reply.get("moved").and_then(serde_json::Value::as_bool) {
        let offset = reply.get("offset").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
        let height = reply
            .get("content_height")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        // "did not move" is the answer an agent needs to stop scrolling, so it
        // is said rather than left to be inferred from an unchanged number.
        println!(
            "{} at {offset:.0} of {height:.0}",
            if moved { "scrolled" } else { "already at the end —" }
        );
    } else if let Some(url) = reply.get("url").and_then(serde_json::Value::as_str) {
        println!("url: {url}");
    } else if let Some(reference) = reply.get("ref").and_then(serde_json::Value::as_str) {
        // A verb that printed nothing read as a verb that did nothing. The
        // typed text is deliberately not echoed: it may be a password, and the
        // engine's whole posture is that a credential does not travel back out
        // through a surface an agent or a log can read.
        println!("typed into {reference}");
    }
    Ok(())
}

fn exit_status(reply: &serde_json::Value) -> Result<(), H5iError> {
    if reply.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

/// Where the session is listening. The fallback chain ends at the stream file
/// because that is the one thing h5i already sets in a box: an agent that has to
/// be told a port is an agent that has to be told this engine exists.
///
/// The Unix control socket to use, if one was named. Never guessed: a socket is
/// either passed or put in the environment by whatever started the session.
/// Guessing a path would mean a verb silently talking to a different session
/// that happened to leave a socket behind.
fn session_socket(at: &SessionArgs) -> Option<PathBuf> {
    at.control_socket
        .clone()
        .or_else(|| std::env::var(CONTROL_SOCKET_VAR).ok().map(PathBuf::from))
}

fn session_port(at: &SessionArgs) -> Result<u16, H5iError> {
    if let Some(port) = at.port {
        return Ok(port);
    }
    let explicit = at
        .control_file
        .clone()
        .or_else(|| std::env::var(CONTROL_FILE_VAR).ok().map(PathBuf::from))
        .or_else(|| {
            std::env::var(STREAM_FILE_VAR)
                .ok()
                .map(|s| control_file_beside(Path::new(&s)))
        });

    let explicit_none = explicit.is_none();
    let path = match explicit {
        // Named by the caller or by h5i, which is a deliberate act; the
        // directory check below is for the path nobody named.
        Some(path) => path,
        // Nothing said where the session is, which on a bare host is the
        // ordinary case rather than a mistake: h5i sets those variables inside
        // a box and nothing sets them outside one. `serve` writes here by
        // default, so the documented no-flags path works with no h5i anywhere.
        None => default_control_file().ok_or_else(|| {
            H5iError::Metadata(
                "no session to talk to, and no per-user runtime directory to look in. \
                 Pass --control-file or --port, or set $XDG_RUNTIME_DIR or $HOME."
                    .into(),
            )
        })?,
    };

    // Absence first, and privacy second. The other order made the *first* thing
    // a new standalone user ever sees, running a verb before `serve`, a
    // warning about credentials being redirected to somebody else's listener,
    // when the real answer is "nothing is running yet". A missing directory is
    // not a suspicious one.
    if !path.exists() {
        return Err(H5iError::Metadata(format!(
            "no session is listening ({} does not exist). Open one with \
             `h5i browser open <url>` — it holds a page open for these verbs to drive — \
             or point at a running one with --control-file, --control-socket or --port.",
            path.display()
        )));
    }

    // Only for the default. A path someone typed is a path someone chose.
    if explicit_none
        && let Some(parent) = path.parent()
        && let Err(why) = check_private_dir(parent)
    {
        return Err(H5iError::Metadata(format!(
            "refusing to read a session port from {}: {why}. A port number there is enough \
             to point `session type` — carrying a substituted credential — at somebody \
             else's listener. Pass --control-file or --port to use it anyway.",
            path.display()
        )));
    }
    crate::stream::read_port_file(&path)
}

/// Where a session advertises itself when nothing else says.
///
/// Per-user, and never a shared directory. The file holds a port number, and a
/// port number is enough to point `session type`, with a substituted credential
/// in it, at somebody else's listener. On a multi-user host a default under
/// `/tmp` would make that a one-line attack, so there is no fallback to one:
/// `$XDG_RUNTIME_DIR` first, then a directory under `$HOME`, then nothing rather
/// than somewhere writable by strangers.
fn default_control_file() -> Option<PathBuf> {
    default_session_dir().map(|dir| dir.join("session.control"))
}

/// Whether a directory is ours alone.
#[cfg(unix)]
fn check_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::metadata(dir).map_err(|e| format!("cannot inspect {}: {e}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    // Safe: the only caller is this process reading its own runtime dir.
    let me = unsafe { libc_getuid() };
    if meta.uid() != me {
        return Err(format!(
            "{} is owned by uid {} and this process is uid {me}",
            dir.display(),
            meta.uid()
        ));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is writable by group or other (mode {:o})",
            dir.display(),
            meta.mode() & 0o777
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private_dir(_dir: &Path) -> Result<(), String> {
    Ok(())
}

/// `getuid`, without taking a dependency on `libc` for one call.
#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

/// Create the session directory, private to this user.
///
/// `serve` calls this before advertising. 0700 at creation rather than a
/// chmod afterwards, so there is no window in which it exists and is readable.
#[cfg(unix)]
fn make_private_dir(dir: &Path) -> Result<(), H5iError> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.exists() {
        return check_private_dir(dir).map_err(H5iError::Metadata);
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| H5iError::with_path(e, dir))
}

#[cfg(not(unix))]
fn make_private_dir(dir: &Path) -> Result<(), H5iError> {
    std::fs::create_dir_all(dir).map_err(|e| H5iError::with_path(e, dir))
}

fn default_session_dir() -> Option<PathBuf> {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR")
        && !runtime.trim().is_empty()
    {
        return Some(PathBuf::from(runtime).join("h5i-browser"));
    }
    // `LOCALAPPDATA` on Windows, `HOME` elsewhere. Both are per-user.
    let home = std::env::var("LOCALAPPDATA")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
        .filter(|h| !h.trim().is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join("h5i-browser"),
    )
}

/// The control file that belongs to a given stream file.
///
/// Derived rather than configured so that a box which sets only
/// `H5I_BROWSER_STREAM_FILE`, which is what h5i injects today, still gets a
/// drivable session. A second variable h5i would also have to learn to set is a
/// second thing that can be forgotten, and the failure would be a session that
/// looks live and cannot be driven.
fn control_file_beside(stream_file: &Path) -> PathBuf {
    stream_file.with_extension("control")
}

fn build_policy(net: &NetArgs) -> Policy {
    // Flags first, then whatever h5i granted the box, and the two are a *union*.
    let from_env: Vec<String> = std::env::var(ALLOW_VAR)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Policy::new()
        .allow_all_of(&net.allow)
        .allow_all_of(&from_env)
        .set_allow_loopback(!net.no_loopback)
        .set_any_remote(net.allow_any_remote)
        .set_max_redirects(net.max_redirects)
        .set_max_response_bytes(net.max_response_bytes)
        .set_cross_site_credentials(net.permissive_cors)
}

fn proxy_of(net: &NetArgs) -> Option<String> {
    net.proxy
        .clone()
        .or_else(|| std::env::var(EGRESS_PROXY_VAR).ok())
        .filter(|value| !value.trim().is_empty())
}

/// The durable half of the record, when one was asked for.
///
/// The other half is never optional: the broker keeps every record in memory
/// whatever this returns, which is what `h5i browser open` prints and what the
/// renderer can ask for. This is the file, and it is the one that can refuse,
/// which is what makes `--receipts` the flag that puts the fail-closed rule
/// under h5i's control rather than the caller's.
fn receipts_sink(net: &NetArgs) -> Result<Arc<dyn Sink>, H5iError> {
    let receipts = net
        .receipts
        .clone()
        .or_else(|| std::env::var(RECEIPTS_VAR).ok().map(PathBuf::from));
    match &receipts {
        None => Ok(Arc::new(crate::receipt::NullSink)),
        Some(path) => Ok(Arc::new(JsonlSink::create(path)?)),
    }
}

/// The message store, when a session was opened with one.
///
/// Opening fails loudly and writing does not, and the two are different
/// questions. A directory that cannot be created means h5i asked for capture and
/// the session cannot provide it, which the caller should hear about before the
/// first page rather than discover as an empty store afterwards. A single
/// message that cannot be written later is a gap in the evidence, counted and
/// reported, and not a reason to fail the fetch that produced it.
fn capture_store(net: &NetArgs) -> Result<Option<Arc<crate::capture::Capture>>, H5iError> {
    let dir = net
        .capture
        .clone()
        .or_else(|| std::env::var(CAPTURE_VAR).ok().filter(|v| !v.trim().is_empty()).map(PathBuf::from));
    match dir {
        None => Ok(None),
        Some(dir) => Ok(Some(Arc::new(crate::capture::Capture::open(&dir)?))),
    }
}

fn load_fonts(view: &ViewArgs) -> fonts::FontSetup {
    let dirs = if view.font_dirs.is_empty() {
        fonts::default_font_dirs()
    } else {
        view.font_dirs.clone()
    };
    fonts::load(&view.font_files, &dirs, None)
}

/// `identity list|show|check`.
///
/// `check` is the verb that matters, and what it prints is the design: not a
/// score, and not a promise. It says which layers this identity reaches, which
/// it does not, and, when the engine cannot back it, exactly which declared
/// requirement is missing and why. A caller who wanted "am I undetectable"
/// gets, instead, the two lists that let them answer it themselves.
#[cfg(feature = "identity")]
fn identity_verb(verb: IdentityVerb) -> Result<(), H5iError> {
    use crate::identity::Identity;
    match verb {
        IdentityVerb::List { json } => {
            let builtins = crate::identity::builtins();
            if json {
                println!("{}", serde_json::to_string_pretty(&builtins)?);
                return Ok(());
            }
            for identity in &builtins {
                println!(
                    "{:<20} {:<11} {}",
                    identity.name,
                    identity.mode.as_str(),
                    identity.browser.user_agent
                );
            }
            Ok(())
        }
        IdentityVerb::Show { name, json } => {
            let identity = Identity::resolve(&name)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&identity)?);
            } else {
                println!(
                    "{}",
                    toml::to_string_pretty(&identity).map_err(|e| H5iError::Metadata(
                        format!("could not render `{name}` as TOML: {e}")
                    ))?
                );
            }
            Ok(())
        }
        IdentityVerb::Check { name, script, json } => {
            let identity = Identity::resolve(&name)?;
            let caps = Capabilities::with_script(script);
            let incoherences = identity.incoherences();
            let unmet = identity.unmet(&caps);
            let admitted = incoherences.is_empty() && unmet.is_empty();

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "name": identity.name,
                        "mode": identity.mode.as_str(),
                        "digest": identity.digest(),
                        "admitted": admitted,
                        "contradicts": incoherences
                            .iter()
                            .map(|f| serde_json::json!({ "field": f.field, "says": f.says }))
                            .collect::<Vec<_>>(),
                        "unmet": unmet
                            .iter()
                            .map(|need| serde_json::json!({
                                "requires": need.as_str(),
                                "why": need.why_unmet(),
                            }))
                            .collect::<Vec<_>>(),
                        "covers": Identity::COVERS,
                        "does_not_cover": Identity::DOES_NOT_COVER,
                    }))?
                );
                // Refused is not an error to *ask* about, so `--json` exits 0
                // and the caller reads `admitted`. It is an error to open a
                // session with, which is where the refusal actually bites.
                return Ok(());
            }

            println!("{}  ({}, {})", identity.name, identity.mode.as_str(), identity.digest());
            println!("  agent   {}", identity.browser.user_agent);
            println!("  accepts {}", identity.locale.accept_language());
            println!();
            if admitted {
                println!("✓ this engine can present it{}", if script { "" } else { " with script off" });
            } else {
                println!("✗ refused");
                for found in &incoherences {
                    println!("    contradicts itself: {found}");
                }
                for need in &unmet {
                    println!("    needs {}: {}", need.as_str(), need.why_unmet());
                }
                if !unmet.is_empty() {
                    let family = identity.browser.family.as_str();
                    println!("    Not applied in part: an agent string claiming {family} in front");
                    println!("    of an engine missing these is louder than no claim at all.");
                }
            }
            println!();
            println!("  covers");
            for line in Identity::COVERS {
                println!("    {line}");
            }
            println!("  does not cover");
            for line in Identity::DOES_NOT_COVER {
                println!("    {line}");
            }
            Ok(())
        }
    }
}

fn doctor(net: &NetArgs) -> Result<(), H5iError> {
    let policy = build_policy(net);
    let proxy = proxy_of(net);
    let font_setup = fonts::load(&[], &fonts::default_font_dirs(), None);

    println!("engine     : h5i-browser {}", env!("CARGO_PKG_VERSION"));
    println!("fonts      : {}", font_setup.summary());
    if font_setup.is_empty() {
        println!(
            "             (pass --font-file to name one; without fonts, pages render but text does not)"
        );
    }
    match &proxy {
        Some(url) => println!("egress     : via {url}"),
        None => println!(
            "egress     : direct (no {EGRESS_PROXY_VAR}; outside a box there is no proxy to route through)"
        ),
    }
    println!(
        "loopback   : {}",
        if net.no_loopback {
            "refused"
        } else {
            "allowed (the dev server)"
        }
    );

    let origins: Vec<_> = policy.origins().collect();
    if policy.allows_any_remote() {
        // Said first and said plainly. A mode this wide reported as a long
        // allowlist would read as a careful configuration.
        println!(
            "allowlist  : ANY REMOTE ORIGIN (--allow-any-remote; instrument mode) — \
             internal addresses are still refused"
        );
        if !origins.is_empty() {
            println!("             (also granted by name: {})", origins.join(", "));
        }
    } else if origins.is_empty() {
        println!("allowlist  : empty — nothing remote is reachable");
    } else {
        println!("allowlist  : {}", origins.join(", "));
    }
    println!("script     : not linked in this tier");
    if policy.allows_cross_site_credentials() {
        println!(
            "cors       : PERMISSIVE (--permissive-cors) — a page may send this session's \
             credentials cross-origin with `no-cors`, as a browser does"
        );
    }

    // Prove the client can actually be built with these settings rather than
    // reporting a configuration that fails at the first fetch.
    crate::net::LocalBroker::new(policy, Arc::new(MemorySink::new()), proxy.as_deref())?;
    println!("client     : ok");
    Ok(())
}

fn open(
    half: &Half,
    targets: &[String],
    net: &NetArgs,
    view: &ViewArgs,
    screenshot: Option<PathBuf>,
    as_text: bool,
    as_json: bool,
) -> Result<(), H5iError> {
    // One screenshot path cannot name several images, and picking one page to
    // apply it to would be an arbitrary choice made silently. Refused with the
    // reason instead.
    if targets.len() > 1 && screenshot.is_some() {
        return Err(H5iError::Metadata(
            "`--screenshot` names one file, so it cannot be used with several targets. \
             Open them one at a time, or drop the flag."
                .to_string(),
        ));
    }

    let factory = factory_for(half, net, view)?;
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut failures = 0usize;

    for (at, target) in targets.iter().enumerate() {
        // Where this page's own records start. The sink is shared across the
        // batch, that sharing is the point, so a per-page view has to be cut
        // out of it rather than read whole, or every page after the first would
        // report the requests of the ones before it.
        let seen_before = factory.broker().records().len();

        let page = match open_target(&factory, target) {
            Ok(page) => page,
            // A page that fails does not stop the ones after it: a batch read
            // is worth having precisely when some of it is going to fail, and
            // stopping would throw away the pages that worked.
            Err(error) => {
                failures += 1;
                if as_json {
                    results.push(serde_json::json!({
                        "url": target,
                        "ok": false,
                        "error": error.to_string(),
                    }));
                } else {
                    eprintln!("{target}: {error}");
                }
                continue;
            }
        };

        if targets.len() > 1 && !as_json && at > 0 {
            println!();
        }
        let page_records = factory.broker().records().split_off(seen_before);
        match one_page(
            page,
            page_records,
            screenshot.clone(),
            as_text,
            as_json,
            targets.len() > 1,
        ) {
            Ok(Some(payload)) => results.push(payload),
            Ok(None) => {}
            Err(error) => {
                failures += 1;
                eprintln!("{target}: {error}");
            }
        }
    }

    if as_json {
        if targets.len() == 1 {
            // One target keeps the shape it always had, so nothing driving
            // this has to learn a new envelope to read one page.
            println!(
                "{}",
                serde_json::to_string_pretty(&results.into_iter().next().unwrap_or_default())?
            );
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "results": results }))?
            );
        }
    }

    if failures > 0 {
        return Err(H5iError::Metadata(format!(
            "{failures} of {} page(s) could not be read",
            targets.len()
        )));
    }
    Ok(())
}

/// Read one page and report it. Returns the JSON payload when asked for one.
fn one_page(
    mut page: Page,
    records: Vec<crate::receipt::RequestRecord>,
    screenshot: Option<PathBuf>,
    as_text: bool,
    as_json: bool,
    label: bool,
) -> Result<Option<serde_json::Value>, H5iError> {
    let snapshot = page.snapshot();

    let screenshot_bytes = match &screenshot {
        Some(path) => {
            let png = page.screenshot_png()?;
            std::fs::write(path, &png).map_err(|e| H5iError::with_path(e, path))?;
            Some((path.clone(), png.len()))
        }
        None => None,
    };

    if as_json {
        let payload = serde_json::json!({
            "url": snapshot.url,
            "title": snapshot.title,
            "snapshot": snapshot,
            "text": page.text(),
            "requests": records,
            // Machine-readable forms of what the snapshot says in prose, so a
            // caller aggregating across many pages does not have to parse
            // sentences back out of the outline.
            "unsupported": page
                .unsupported()
                .into_iter()
                .map(|(name, count)| serde_json::json!({ "api": name, "calls": count }))
                .collect::<Vec<_>>(),
            "console": page
                .console()
                .into_iter()
                .map(|line| {
                    serde_json::json!({
                        "level": line.level,
                        "text": line.text,
                        // Which of the two is talking. "the site reported an
                        // error" and "the browser could not do something" call
                        // for different responses and were indistinguishable.
                        "source": line.source,
                        "repeats": line.repeats,
                    })
                })
                .collect::<Vec<_>>(),
            "settled": page.settled().map(|s| s.render()),
            "script": page.has_script(),
            "screenshot": screenshot_bytes.as_ref().map(|(path, len)| serde_json::json!({
                "path": path.display().to_string(),
                "bytes": len,
            })),
        });
        return Ok(Some(payload));
    }

    // Which page this is, when there is more than one. Without it a batch of
    // outlines runs together and the second page looks like more of the first.
    if label {
        println!("=== {}", snapshot.url);
    }
    if as_text {
        // Fenced like every other read of a page.
        println!("{}", crate::snapshot::fenced(&page.text()));
    } else {
        print!("{}", snapshot.render());
    }

    // The request log is the point of this engine, so it is printed by default
    // rather than hidden behind a flag.
    if !records.is_empty() {
        eprintln!("\nrequests:");
        for record in records
            .iter()
            .filter(|r| r.phase == crate::receipt::Phase::Response)
        {
            eprintln!("  {}", record.render());
        }
    }

    if let Some((path, len)) = screenshot_bytes {
        eprintln!("\nscreenshot: {} ({len} bytes)", path.display());
    }
    Ok(None)
}

#[derive(Debug)]
enum Target {
    Remote(Url),
    Local(PathBuf),
}

/// Decide whether the caller named a URL or a file.
fn local_base(path: &Path) -> Result<Url, H5iError> {
    let absolute = match path.canonicalize() {
        Ok(resolved) => resolved,
        // The canonicalize error is the informative one, it says *why* the
        // walk failed, so it is what surfaces if even this cannot produce an
        // absolute path.
        Err(error) => std::path::absolute(path).map_err(|_| H5iError::with_path(error, path))?,
    };
    Url::from_file_path(&absolute).map_err(|_| {
        H5iError::InvalidPath(format!(
            "{} cannot be expressed as a file:// base",
            absolute.display()
        ))
    })
}

/// Most a `--set-file` payload may be. Generous, because a large upload is a
/// real test; bounded, because the file is read whole and base64'd through a
/// control message.
const MAX_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// One `--set-file` payload, read whole and bounded.
///
/// The shape check is the one that matters: `fs::read` on `/dev/zero` grows a
/// `Vec` until the machine gives out, and a fifo blocks forever.
fn read_a_payload(target: &str, path: &str) -> Result<Vec<u8>, H5iError> {
    let meta = std::fs::metadata(path).map_err(|e| {
        H5iError::Metadata(format!("`--set-file {target}`: {path} could not be read: {e}"))
    })?;
    if !meta.is_file() {
        return Err(H5iError::Metadata(format!(
            "`--set-file {target}`: {path} is not a regular file, so it has no bytes to \
             send — a device or a pipe is read until it stops, and these do not stop"
        )));
    }
    if meta.len() > MAX_PAYLOAD_BYTES {
        return Err(H5iError::Metadata(format!(
            "`--set-file {target}`: {path} is {} bytes, past the {MAX_PAYLOAD_BYTES} byte \
             limit on one payload. It is read whole, encoded and carried through a control \
             message, so this would cost several times that in memory",
            meta.len()
        )));
    }
    std::fs::read(path).map_err(|e| {
        H5iError::Metadata(format!("`--set-file {target}`: {path} could not be read: {e}"))
    })
}

fn parse_target(target: &str) -> Result<Target, H5iError> {
    if let Ok(url) = Url::parse(target) {
        match url.scheme() {
            "http" | "https" => return Ok(Target::Remote(url)),
            "file" => {
                let path = url
                    .to_file_path()
                    .map_err(|_| H5iError::InvalidPath(target.to_string()))?;
                return Ok(Target::Local(path));
            }
            other => {
                return Err(H5iError::InvalidPath(format!(
                    "`{other}:` is not something this engine opens (try http, https, or a path)"
                )));
            }
        }
    }
    Ok(Target::Local(PathBuf::from(target)))
}

#[cfg(test)]
mod tests {

    /// The default session path is per-user, or absent.
    ///
    /// Guarded because the failure is silent and serious: a shared directory
    /// would let any local user publish a port and receive the next
    /// `session type`, including one carrying a substituted credential.
    #[test]
    fn the_default_session_directory_is_never_shared() {
        // Nothing to go on: no path at all rather than a guess.
        temp_env(&[("XDG_RUNTIME_DIR", None), ("HOME", None), ("LOCALAPPDATA", None)], || {
            assert!(default_control_file().is_none());
        });

        // A runtime dir is preferred, because it is per-user and 0700.
        temp_env(&[("XDG_RUNTIME_DIR", Some("/run/user/1000")), ("HOME", Some("/home/a"))], || {
            let path = default_control_file().expect("a path");
            assert!(
                path.starts_with("/run/user/1000"),
                "{}",
                path.display()
            );
        });

        // Falling back to HOME, still per-user.
        temp_env(&[("XDG_RUNTIME_DIR", Some("")), ("HOME", Some("/home/a"))], || {
            let path = default_control_file().expect("a path");
            assert!(path.starts_with("/home/a"), "{}", path.display());
        });

    }

    #[cfg(unix)]
    #[test]
    fn a_session_directory_somebody_else_can_write_is_refused() {
        // The rule that replaced a blacklist. Setting `HOME=/tmp`, which real
        // daemons do, put the default under a world-writable parent, and no
        // list of bad paths would have caught it. This checks the directory
        // instead, which does not have that class of hole.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod");
        assert!(check_private_dir(dir.path()).is_ok());

        for mode in [0o777, 0o770, 0o707, 0o722] {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode))
                .expect("chmod");
            let why = check_private_dir(dir.path())
                .expect_err(&format!("mode {mode:o} should be refused"));
            assert!(why.contains("writable"), "{why}");
        }

        // And a path that is not a directory at all.
        let file = dir.path().join("f");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod");
        std::fs::write(&file, "1").expect("write");
        assert!(check_private_dir(&file).is_err());
    }

    #[test]
    fn a_control_file_the_caller_named_is_not_second_guessed() {
        // SKILL.md tells an agent to give each concurrent session its own
        // `--control-file`. Applying the private-directory rule to a path somebody
        // typed made `serve --control-file /tmp/a.control` abort on `/tmp` being
        // mode 1777, before it opened anything: a documented invocation refused by a
        // guard meant for the path nobody chose. `session_port` already drew that
        // line; this is the same line on the serving side.
        assert!(
            check_private_dir(std::path::Path::new("/tmp")).is_err(),
            "the fixture assumes /tmp is world-writable"
        );
        // The rule still applies to a default, and default_control_file never
        // points at a shared directory in the first place.
        temp_env(&[("XDG_RUNTIME_DIR", Some("/run/user/1000"))], || {
            let path = default_control_file().expect("a path");
            assert!(!path.starts_with("/tmp"), "{}", path.display());
        });
    }

    #[cfg(unix)]
    #[test]
    fn serve_creates_its_session_directory_private() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().expect("tempdir");
        let dir = base.path().join("nested").join("session");
        make_private_dir(&dir).expect("created");
        let mode = std::fs::metadata(&dir).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "created {:o}", mode & 0o777);
    }

    #[test]
    fn serve_and_the_verbs_agree_on_where_a_session_lives() {
        // The two halves have to name the same file or the verbs look
        // somewhere `serve` never wrote, which reads as "no session running"
        // on a host where one is.
        temp_env(&[("XDG_RUNTIME_DIR", Some("/run/user/4242"))], || {
            let path = default_control_file().expect("a path");
            assert_eq!(
                path,
                std::path::PathBuf::from("/run/user/4242/h5i-browser/session.control")
            );
        });
    }

    /// Set some environment variables, run, and put them back.
    ///
    /// Serialised on a mutex: these tests write process-global state, and the
    /// test harness runs them on threads.
    fn temp_env(vars: &[(&str, Option<&str>)], body: impl FnOnce()) {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(name, _)| (name.to_string(), std::env::var(name).ok()))
            .collect();
        for (name, value) in vars {
            match value {
                Some(v) => unsafe { std::env::set_var(name, v) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
        body();
        for (name, value) in saved {
            match value {
                Some(v) => unsafe { std::env::set_var(&name, v) },
                None => unsafe { std::env::remove_var(&name) },
            }
        }
    }

    use super::*;

    #[test]
    fn http_targets_are_remote_and_paths_are_local() {
        assert!(matches!(
            parse_target("https://example.com/").unwrap(),
            Target::Remote(_)
        ));
        assert!(matches!(
            parse_target("./page.html").unwrap(),
            Target::Local(_)
        ));
        assert!(matches!(
            parse_target("/tmp/page.html").unwrap(),
            Target::Local(_)
        ));
    }

    #[test]
    fn an_unopenable_scheme_says_so_rather_than_being_read_as_a_filename() {
        // `data:...` as a navigation target would otherwise become a file
        // named "data:..." and fail with a confusing not-found.
        let error = parse_target("ftp://example.com/x").unwrap_err();
        assert!(error.to_string().contains("ftp"));
    }

    #[test]
    fn a_relative_target_gets_an_absolute_file_base() {
        let base = local_base(Path::new("./page.html")).expect("a relative path has a base");
        assert_eq!(base.scheme(), "file");
        assert!(
            base.path().ends_with("/page.html"),
            "the base keeps the file it was built from: {base}"
        );
        assert!(
            !base.path().starts_with("/./"),
            "the cwd was joined rather than pasted: {base}"
        );
    }

    #[test]
    fn a_path_that_cannot_be_walked_still_yields_a_base() {
        // The regression this exists for: inside a box the supervised tier
        // redirects `/tmp`, so a cwd underneath it reads fine through the
        // shell's fd and fails to resolve by name. `canonicalize` fails on a
        // path that does not resolve, and the old fallback handed
        // `from_file_path` a relative path, which it refuses. Turning a
        // readable page into "invalid path". Nothing here may depend on the
        // path existing.
        let missing = Path::new("./no-such-dir-b7f1/page.html");
        assert!(missing.canonicalize().is_err(), "the premise of this test");

        let base = local_base(missing).expect("an unwalkable path still has a base");
        assert_eq!(base.scheme(), "file");
        assert!(base.path().ends_with("/no-such-dir-b7f1/page.html"), "{base}");
    }

    #[test]
    fn the_control_file_sits_beside_the_stream_file() {
        // The whole reason the path is derived: h5i injects
        // H5I_BROWSER_STREAM_FILE and nothing else, so a session must be
        // drivable without h5i learning a second variable.
        assert_eq!(
            control_file_beside(Path::new("/tmp/agent-browser/h5i-light.stream")),
            PathBuf::from("/tmp/agent-browser/h5i-light.control")
        );
    }

    #[test]
    fn session_verbs_parse_the_way_an_agent_would_type_them() {
        for argv in [
            vec!["h5i-browser", "session", "status"],
            vec!["h5i-browser", "session", "snapshot", "--json"],
            vec!["h5i-browser", "session", "navigate", "/docs"],
            vec!["h5i-browser", "session", "click", "@e3"],
            vec!["h5i-browser", "session", "click", "e3", "--port", "9000"],
            // Both handles, on every verb that takes one. `type` is the awkward
            // shape: with `--selector` the remaining positional is the *text*,
            // so the two do not conflict there and do on the others.
            vec!["h5i-browser", "session", "type", "@e1", "alice"],
            vec!["h5i-browser", "session", "type", "--selector", "#user", "alice"],
            vec!["h5i-browser", "session", "submit", "@e2"],
            vec!["h5i-browser", "session", "submit", "--selector", "#go"],
            vec!["h5i-browser", "session", "click", "--selector", "a.next"],
            vec!["h5i-browser", "session", "structured"],
            vec!["h5i-browser", "session", "transcript"],
            vec!["h5i-browser", "session", "transcript", "--lang", "en"],
            vec!["h5i-browser", "session", "markdown", "--url", "https://x.test/"],
            vec!["h5i-browser", "session", "script", "--save", "/tmp/s.json"],
            vec!["h5i-browser", "replay", "/tmp/s.json"],
            vec!["h5i-browser", "open", "a.html", "b.html"],
            vec!["h5i-browser", "session", "set-checked", "@e1", "true"],
            vec!["h5i-browser", "session", "set-checked", "--selector", "#opt", "false"],
            vec!["h5i-browser", "session", "select", "@e2", "Express"],
            vec!["h5i-browser", "session", "select", "--selector", "#ship", "Express"],
            vec!["h5i-browser", "session", "press", "@e3", "Enter"],
            vec!["h5i-browser", "session", "press", "--selector", "#q", "Escape"],
        ] {
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?} should parse: {e}"));
        }

        // Two ways to name the same session is a way to name two different
        // ones by accident.
        assert!(
            Cli::try_parse_from([
                "h5i-browser",
                "session",
                "status",
                "--port",
                "9000",
                "--control-file",
                "/tmp/x.control",
            ])
            .is_err(),
            "--port and --control-file must not be given together"
        );

        // A selector *replaces* the ref on the verbs whose positional is the
        // ref, so naming both is a request whose author did not know which one
        // they meant.
        for argv in [
            vec!["h5i-browser", "session", "click", "@e1", "--selector", "#go"],
            vec!["h5i-browser", "session", "submit", "@e1", "--selector", "#go"],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "{argv:?} names the same element twice and should be refused"
            );
        }
    }

    /// clap asserts its own invariants at parser-construction time, and one of
    /// them is that an optional positional may not precede a required one.
    /// Teaching `type` a `--selector` made its ref optional while its text was
    /// still required, so *every* `session type` invocation panicked before
    /// parsing anything, in a debug build, which is what the tests run.
    ///
    /// Nothing caught it: the verb tests drive `control_verb` directly, and the
    /// parse test above happened not to list `type`. This builds the whole
    /// command tree, which is what runs clap's assertions.
    #[test]
    fn the_command_tree_satisfies_claps_own_invariants() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn script_is_opt_in_at_the_command_line() {
        // The gate roadmap-history.md §B3.3 asks for: script is a decision someone
        // makes, never a default they inherit.
        for argv in [
            vec!["h5i-browser", "open", "https://x.example/", "--script"],
            vec!["h5i-browser", "serve", "https://x.example/", "--script"],
            vec!["h5i-browser", "capabilities", "--script"],
        ] {
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
    }

    #[test]
    fn capabilities_report_the_configuration_asked_about() {
        // What h5i routes on is whether *this* invocation runs script, not what
        // the binary could do if asked differently.
        assert!(!Capabilities::current().javascript, "off unless asked");
        assert!(!Capabilities::with_script(false).javascript);
        assert!(Capabilities::with_script(true).javascript);

        // The rest is a property of the engine either way, and stays honest.
        let with = Capabilities::with_script(true);
        assert!(with.fail_closed_receipts);
        assert!(with.snapshot && with.screenshot && with.live_view);
        assert!(!with.video && !with.webgl, "still absent, and still said so");
    }

    #[test]
    fn cli_parses_the_shapes_the_docs_promise() {
        // Cheap guard against a flag rename breaking the documented usage.
        Cli::try_parse_from([
            "h5i-browser",
            "open",
            "https://example.com",
            "--allow",
            "example.com",
            "--screenshot",
            "/tmp/x.png",
            "--receipts",
            "/tmp/r.jsonl",
        ])
        .expect("documented open invocation parses");

        Cli::try_parse_from(["h5i-browser", "capabilities"]).expect("capabilities parses");
        Cli::try_parse_from(["h5i-browser", "doctor"]).expect("doctor parses");
    }
}
