//! `h5i browser`: the front door.
//!
//! One noun an agent learns: a *session*. `open` makes one, points the default
//! at it, and every later verb follows that pointer.
//!
//! ```text
//! h5i browser open https://example.com
//! h5i browser snapshot
//! h5i browser click @e3
//! ```
//!
//! The opaque id (`br_7k2xqa`) is what `--json` and the receipts carry, since a
//! durable reference must survive a rename, and is not what anyone types.
//! `--session <name>` runs several at once; a name is reusable once its session
//! ends, which is why the id is what gets written down.
//!
//! Containment is a placement, not a product. With no flags the session runs in
//! this user's process space like any other headless browser, and what it does
//! that others do not is *record*. `--in <box>` moves the same session into a
//! box, changing only who saw the network: the lane goes from engine-claimed to
//! host-observed ([`h5i_core::browser_session::Lane`]).
//!
//! Verbs are carried in over `h5i box run` rather than dialled, because a
//! supervised box's netns puts the engine's loopback control port out of the
//! host's reach. Two things fall out, both wanted: every verb gets a receipt,
//! and the control lock is checked on the host, outside the box.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::Subcommand;
use console::style;
use serde_json::Value;

use h5i_core::browser_session as bs;
use h5i_core::ui::SUCCESS;

/// How long `start` waits for the engine to advertise its control file.
///
/// Generous, because the first thing a session does is fetch and render the URL
/// it was given, and a cold font scan is not instant. A start that gives up
/// says what it was waiting for and leaves the engine's own log behind.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// Refuse an identity that lives in a file when the session runs in a box.
#[cfg(feature = "identity")]
fn refuse_a_file_identity_in_a_box(in_box: Option<&str>, selector: &str) -> anyhow::Result<()> {
    let is_builtin = h5i_browser::identity::builtin(selector).is_some();
    if in_box.is_none() || is_builtin || !std::path::Path::new(selector).exists() {
        return Ok(());
    }
    anyhow::bail!(
        "`--identity {selector}` names a file on this machine, and this runs in a box.\n\n  \
         A box has its own filesystem, so a path beside you is not a path it has. Use one \
         of the built-in identities, which travel by name:\n    \
         h5i browser identity list\n\n  \
         Or put the file where the box can read it and name it by the path it has in there."
    )
}

/// The identity a session presents unless one is named.
///
/// The honest one, and the same word the engine's own `--identity` defaults to.
/// Two defaults that could drift would make the session record say `native`
/// while the engine presented something else.
pub const DEFAULT_IDENTITY: &str = "native";

#[derive(Subcommand)]
pub enum BrowserCommands {
    /// Open a URL, making a session if there is not one already.
    ///
    /// With no `--session`, this uses the default session and points the
    /// default at whatever it makes, so the verbs that follow need no id.
    /// Without `--in`, the session runs here with no containment beyond the
    /// engine itself; with `--in <box>`, the same session runs inside that box.
    Open {
        /// A URL, or a path to a local HTML file.
        url: String,

        /// Name this session, so several can run at once.
        ///
        /// A name is not an identity: it can be reused once the session it
        /// named has ended. The opaque id in `--json` and in receipts is what
        /// cannot be, and is what to keep when you need a durable reference.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,

        /// Make a new session even if one is already open.
        ///
        /// Without this, `open` navigates the session it finds, because that is
        /// what opening a URL means when a browser is already up.
        #[arg(long)]
        new: bool,

        /// Run the session inside this box instead of on this machine.
        #[arg(long = "in", value_name = "BOX")]
        in_box: Option<String>,

        /// Grant an origin. Repeatable. Without any, nothing remote is
        /// reachable except the URL's own origin.
        #[arg(long = "allow", value_name = "ORIGIN")]
        allow: Vec<String>,

        /// Refuse loopback too. It is reachable by default: it is the dev
        /// server.
        #[arg(long)]
        no_loopback: bool,

        /// Run the page's own JavaScript. Off by default: with script off,
        /// page-borne prompt injection has no delivery channel at all.
        #[arg(long)]
        script: bool,

        /// Let this session's pages send credentials cross-origin as a browser
        /// does: `mode: "no-cors"` with `credentials: "include"`.
        ///
        /// Refused by default, which is right for containing an agent: an
        /// opaque response cannot be read, so nothing can check the server
        /// agreed. It is also the classic POST-CSRF vector, so the refusal
        /// stopped h5i acting as the victim in a CSRF test. Fixed at creation,
        /// in the policy digest, and named in `h5i browser status`.
        #[arg(long)]
        permissive_cors: bool,

        /// Run the engine unconfined.
        ///
        /// By default a session on this machine runs in a process-tier sandbox:
        /// it may read the system and its own directory, write only its own
        /// directory, execute nothing, and see only the secrets named with
        /// `--secret`. That contains what a parser bug could *do*; it does not
        /// contain the network, which needs a boundary outside the engine.
        #[arg(long)]
        no_sandbox: bool,

        /// Let this session substitute `$H5I_SECRET_<NAME>`.
        #[arg(long = "secret", value_name = "NAME")]
        secrets: Vec<String>,

        /// Who this session says it is: a built-in name, or a path to a TOML file.
        #[cfg(feature = "identity")]
        #[arg(long, value_name = "NAME|PATH", default_value = "native")]
        identity: String,

        /// Viewport width.
        #[arg(long, default_value_t = 1280)]
        width: u32,

        /// Viewport height.
        #[arg(long, default_value_t = 720)]
        height: u32,

        /// End the session automatically after this many seconds.
        ///
        /// Recorded as an ending on the session's record when it passes, not as
        /// a disappearance: `h5i browser status` still answers afterwards, and
        /// says it expired.
        #[arg(long, value_name = "SECONDS")]
        expires_in: Option<u64>,

        /// Seed this session's storage from one that has ended.
        ///
        /// A restore is a new session with a new id, and the inheritance is
        /// written into its record. Nothing resurrects an id: an agent holding
        /// a stale one always gets a refusal, never a different session wearing
        /// the same name. Takes an id, not a name, because a name can be reused
        /// and storage has to come from one definite session.
        #[arg(long, value_name = "SESSION_ID")]
        restore: Option<String>,

        /// Keep every request and response this session makes: headers and
        /// bodies, both directions.
        ///
        /// Off by default, and a different thing from the request log, which is
        /// always on. The log records decisions in a form that is safe to paste
        /// into a bug report; this records the messages, `Authorization` header
        /// and session cookie included, which is what an HTTP workbench needs
        /// and what an audit trail must not carry. Stored `0700` inside the
        /// session directory, bounded, and never copied by `--restore`.
        #[arg(long)]
        capture: bool,

        /// Print the session record as JSON instead of a summary line.
        #[arg(long)]
        json: bool,
    },

    /// List, show or check a browser identity.
    #[cfg(feature = "identity")]
    Identity {
        /// `list`, `show <name>`, or `check <name>`.
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Read one page, or a batch of them, and leave no session behind.
    Read {
        /// URLs, or paths to local HTML files.
        #[arg(required = true, num_args = 1..)]
        targets: Vec<String>,

        /// Read inside this box.
        #[arg(long = "in", value_name = "BOX")]
        in_box: Option<String>,

        /// Grant an origin besides the target's own. Repeatable.
        ///
        /// Naming the URL is what grants its origin, and for most reads that is
        /// the whole allowlist. A page written in a library served from a CDN
        /// is the case where it is not: without the grant the script is
        /// refused, and the page an agent reads is the one the library never
        /// ran on.
        #[arg(long = "allow", value_name = "ORIGIN")]
        allow: Vec<String>,

        /// Print the page's prose instead of its outline.
        #[arg(long)]
        text: bool,

        /// Run the page's own JavaScript.
        #[arg(long)]
        script: bool,

        /// Read unconfined.
        #[arg(long)]
        no_sandbox: bool,

        /// Who this read says it is: a built-in name, or a path to a TOML file.
        ///
        /// The same identities a session takes, and refused on the same terms:
        /// see `h5i browser identity`. It belongs here as much as on a session.
        /// A read is a fetch, and a fetch has an agent string.
        #[cfg(feature = "identity")]
        #[arg(long, value_name = "NAME|PATH", default_value = DEFAULT_IDENTITY)]
        identity: String,

        #[arg(long)]
        json: bool,
    },

    /// List the browser sessions on this machine.
    List {
        /// Include sessions that have ended. They are kept: the record of how a
        /// session ended is the part a reviewer needs.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },

    /// What a session is, where it runs, and who saw its network.
    Status {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// End a session. Its record stays.
    Close {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// End every live session on this machine.
        #[arg(long, conflicts_with = "session")]
        all: bool,
        /// Delete the captured messages as well as ending the session.
        ///
        /// The store holds whole bodies and `Cookie` and `Authorization` in
        /// full. The record and the request log stay either way.
        #[arg(long = "capture-drop")]
        capture_drop: bool,
        #[arg(long)]
        json: bool,
    },

    /// The page as a model should read it: the outline, with `@ref` handles.
    Snapshot {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// Report only what changed since the last snapshot.
        #[arg(long)]
        delta: bool,
        /// Go here first, then read. One round trip where `navigate` and then
        /// this would be two, and the reply still names the URL it ended up on
        /// so a redirect is not silent.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Go to a URL, resolved against the current page like a click would be.
    Navigate {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        url: String,
        #[arg(long)]
        json: bool,
    },

    /// Write a PNG of the page this session is on.
    ///
    /// `open --screenshot` could always do this for a page it rendered and
    /// exited; a resident session could not, so an agent that had just clicked
    /// something had no way to look at the result. The live view is the human's
    /// channel and is not an answer to a verb.
    ///
    /// Refused while `login` is on: a password is pixels before it is anything
    /// else, and handing those to the agent is the transfer that mode stops.
    Screenshot {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// Where to write it, on *this* machine. Defaults to a host-named
        /// file in the session's own artifacts directory.
        ///
        /// The engine paints into a directory it is allowed to write and h5i
        /// moves the file here afterwards, so this is any path you can write,
        /// not a path the confined engine can. For a session in a box that
        /// means the file is carried out of the box, which needs a tier whose
        /// `/tmp` this machine shares, and says so when it does not.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Go here first, then paint.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Open a WebSocket, send frames, and report what came back.
    ///
    /// The workbench verb for the other protocol. `resend` bends one HTTP
    /// message; this one says something on a socket and reports the reply,
    /// through the same policy, the same budget and the same receipts.
    ///
    /// Needs no `--script`: the socket is the engine's, not the page's, which
    /// is the whole point. An application whose commands travel over a
    /// WebSocket was previously one this workbench could watch connect and
    /// never speak to.
    ///
    /// One exchange, not a resident connection: it opens, sends what it was
    /// given, listens for `--wait-ms`, and closes.
    Socket {
        /// The `ws://` or `wss://` endpoint to open.
        #[arg(value_name = "URL")]
        url: String,
        /// A text frame to send. Repeatable, and sent in the order given.
        #[arg(long = "send", value_name = "TEXT")]
        send: Vec<String>,
        /// How long to listen for replies, in milliseconds.
        ///
        /// A server that answers by doing something first — running a command,
        /// say — answers late, and a socket that says nothing at all is a
        /// result rather than an error.
        #[arg(long = "wait-ms", value_name = "MS", default_value = "5000")]
        wait_ms: u64,
        /// Which session, when more than one is open.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Fetch the current URL again.
    ///
    /// Takes no URL. After a redirect it re-fetches where the session actually
    /// is, which is the thing `navigate` to a remembered URL gets wrong.
    Reload {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Follow a `@ref` from the last snapshot.
    Click {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// `e3` or `@e3`, from a `snapshot`. Omit when using `--selector` or
        /// `--role`.
        reference: Option<String>,
        /// A CSS selector instead, which survives a navigation where a `@ref`
        /// does not.
        #[arg(long, value_name = "CSS", conflicts_with = "reference")]
        selector: Option<String>,
        /// Find it by what it is called, the way a person would say it:
        /// `--role button --name "Sign in"`.
        #[arg(long, value_name = "ROLE", conflicts_with = "selector")]
        role: Option<String>,
        #[arg(long, value_name = "TEXT", requires = "role")]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Put text into a field, replacing what was there.
    Type {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// `e3` or `@e3`, from a `snapshot`. Omit when using `--selector` or
        /// `--role`, and give the text alone.
        #[arg(value_name = "REF|TEXT")]
        reference: Option<String>,
        /// What to type.
        #[arg(value_name = "TEXT")]
        text: Option<String>,
        /// A CSS selector instead.
        ///
        /// No conflict with the positional, unlike `click`'s: with a locator
        /// given, the one positional left is the *text*, and the engine shifts
        /// it (`two_positionals`). Declaring a conflict here made
        /// `type --selector '#password' hunter2` a usage error, which is the
        /// obvious way to type it.
        #[arg(long, value_name = "CSS")]
        selector: Option<String>,
        /// Find the field by what it is called: `--role textbox --name Email`.
        #[arg(long, value_name = "ROLE", conflicts_with = "selector")]
        role: Option<String>,
        #[arg(long, value_name = "TEXT", requires = "role")]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Submit the form containing a `@ref`.
    ///
    /// Not always needed: clicking a submit button submits its form, the way a
    /// browser does with script switched off.
    Submit {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// `e3` or `@e3`, from a `snapshot`. Omit when using `--selector` or
        /// `--role`.
        reference: Option<String>,
        /// A CSS selector instead.
        #[arg(long, value_name = "CSS", conflicts_with = "reference")]
        selector: Option<String>,
        /// Find a control in the form by what it is called.
        #[arg(long, value_name = "ROLE", conflicts_with = "selector")]
        role: Option<String>,
        #[arg(long, value_name = "TEXT", requires = "role")]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Scroll the page. Negative scrolls up.
    ///
    /// With `--script`, the page's own `scroll` handlers run and its
    /// intersection observers are re-checked at the new offset, so a page that
    /// loads more as you go has loaded it before this replies.
    Scroll {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(allow_negative_numbers = true)]
        by: f64,
        #[arg(long)]
        json: bool,
    },

    /// Wait until something is on the page, or until nothing can put it there.
    WaitFor {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(long, value_name = "CSS")]
        selector: Option<String>,
        #[arg(long, value_name = "TEXT", conflicts_with = "selector")]
        text: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Wait until a page expression is true. Needs `open --script`.
    WaitForScript {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        expr: String,
        #[arg(long)]
        json: bool,
    },

    /// Pull structured data out of the page by selector.
    Extract {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// The schema, as JSON.
        schema: String,
        /// Go here first, then read. One round trip where `navigate` and then
        /// this would be two, and the reply still names the URL it ended up on
        /// so a redirect is not silent.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// The page as markdown: what a reader would read, without the handles.
    Markdown {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(long, value_name = "BYTES")]
        max_bytes: Option<usize>,
        /// Go here first, then read. One round trip where `navigate` and then
        /// this would be two, and the reply still names the URL it ended up on
        /// so a redirect is not silent.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Everything recorded about this session, in one ordered timeline.
    ///
    /// What the agent asked for, what the engine decided about each fetch, who
    /// was driving, and how the session ended. Each row says which lane it came
    /// from: the action and request logs are the engine's own account, the
    /// handovers and the lifecycle are h5i's, written from outside.
    ///
    /// `requests` is the network layer of this on its own, and stays the verb
    /// to reach for in a loop. This is the one to read afterwards.
    Audit {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// The helper runs that belong to no session.
        ///
        /// `h5i browser transcript --via yt-dlp --url <URL>` needs no page and
        /// so opens no session; the run is still recorded, and this is where.
        /// Kept out of a session's own timeline on purpose: a run that was not
        /// part of a session must not appear inside it.
        #[arg(long = "no-session", conflicts_with = "session")]
        no_session: bool,
        #[arg(long)]
        json: bool,
    },

    /// What the page publishes *about itself*: JSON-LD, OpenGraph, `<meta>`,
    /// `<link rel>`.
    ///
    /// The cheapest read there is. A few hundred bytes where a snapshot is a
    /// few hundred lines. Try it first on an article, a product, or anything
    /// with a canonical URL. A page with no metadata answers `empty`, which is
    /// a fact about the page rather than a failed read.
    Structured {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// Go here first, then read. One round trip where `navigate` and then
        /// this would be two, and the reply still names the URL it ended up on
        /// so a redirect is not silent.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// What the page's media *says*: `<track>` captions, fetched and parsed.
    Transcript {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// Go here first, then read. One round trip where `navigate` and then
        /// this would be two, and the reply still names the URL it ended up on
        /// so a redirect is not silent.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// Prefer this language, for the words and for the outline alike.
        ///
        /// Prefix-matched against `srclang`, so `en` finds `en-GB`. Falls back
        /// to the track the page marked `default`, then to the first. Every
        /// track is listed either way.
        #[arg(long, value_name = "LANG")]
        lang: Option<String>,
        /// The ceiling on caption text carried out of *one* track.
        ///
        /// Per track rather than per reply, so a media element with both words
        /// and an outline can carry up to twice this.
        #[arg(long, value_name = "BYTES")]
        max_bytes: Option<usize>,
        /// Read it with an outside program instead of from the page's markup.
        #[arg(long, value_name = "HELPER")]
        via: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Set a checkbox or radio to a state, rather than toggling it.
    ///
    /// Prefer this to clicking one. A click *toggles*, so where it lands
    /// depends on what the page was serving; setting a state is idempotent, and
    /// that is the difference between a session that replays to the same place
    /// and one that does not.
    SetChecked {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// `e3` or `@e3`, from a `snapshot`. Omit when using `--selector` or
        /// `--role`, and give the state alone.
        #[arg(value_name = "REF|STATE")]
        reference: Option<String>,
        /// `true` or `false`.
        #[arg(value_name = "STATE")]
        checked: Option<String>,
        #[arg(long, value_name = "CSS")]
        selector: Option<String>,
        /// Find the control by what it is called, the way a person would.
        #[arg(long, value_name = "ROLE", conflicts_with = "selector")]
        role: Option<String>,
        #[arg(long, value_name = "TEXT", requires = "role")]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Choose an option in a `<select>`, by its value or by the text it shows.
    ///
    /// The reply carries the *value*, because that is what the form submits and
    /// what survives a re-render. The text is what you read.
    Select {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// `e3` or `@e3`. Omit when using `--selector` or `--role`.
        #[arg(value_name = "REF|OPTION")]
        reference: Option<String>,
        /// The option's value, or the text it shows, in that order.
        #[arg(value_name = "OPTION")]
        option: Option<String>,
        #[arg(long, value_name = "CSS")]
        selector: Option<String>,
        #[arg(long, value_name = "ROLE", conflicts_with = "selector")]
        role: Option<String>,
        #[arg(long, value_name = "TEXT", requires = "role")]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Press a key that *does* something: Enter, Escape, Tab, ArrowDown.
    ///
    /// To enter text use `type`. Merging the two would make one verb whose
    /// meaning depended on its argument.
    Press {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// `e3` or `@e3`. Omit when using `--selector` or `--role`.
        #[arg(value_name = "REF|KEY")]
        reference: Option<String>,
        /// The key name.
        #[arg(value_name = "KEY")]
        key: Option<String>,
        #[arg(long, value_name = "CSS")]
        selector: Option<String>,
        #[arg(long, value_name = "ROLE", conflicts_with = "selector")]
        role: Option<String>,
        #[arg(long, value_name = "TEXT", requires = "role")]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Find an element by what it is called, rather than by where it sits.
    ///
    /// A role and, usually, its accessible name. The way a person would
    /// describe it out loud. Survives a re-render that moves everything, which
    /// a `@ref` from an older reading does not.
    Find {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// `button`, `link`, `textbox`, `checkbox`, …
        #[arg(long, value_name = "ROLE")]
        role: String,
        /// The accessible name, matched whole: case is ignored and whitespace
        /// collapsed, but half a name finds nothing.
        #[arg(long, value_name = "TEXT")]
        name: Option<String>,
        /// Go here first, then read. One round trip where `navigate` and then
        /// this would be two, and the reply still names the URL it ended up on
        /// so a redirect is not silent.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Record what this session did as a replayable script.
    ///
    /// The steps are verified CSS selectors, not `@ref` handles, so the script
    /// outlives the reading it was recorded from. `h5i browser replay` sends
    /// each one back through the same control channel an agent uses, so the
    /// policy, the receipts and the action log see a replay exactly as they see
    /// a live session.
    Script {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// Write the script here instead of printing it.
        #[arg(long, value_name = "PATH")]
        save: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },

    /// The request log: what this session asked for, and what was refused.
    ///
    /// The engine is the HTTP client, so this is the decision record written
    /// before the bytes moved, not an observation made from beside the network.
    /// If a request is not in this list, it did not happen.
    Requests {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// Only what happened after this sequence number.
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
        #[arg(long)]
        json: bool,
    },

    /// Run a multi-step flow: send, extract, send again with what was found.
    ///
    /// What a CSRF-protected application needs. A single `resend` cannot test
    /// an endpoint whose token is minted by the request before it, and hand-
    /// carrying the token between two shell commands is where the mistakes
    /// happen. Steps run in order and stop at the first failure, because a step
    /// acting on a token the step before it failed to produce is acting on a
    /// state the file never described.
    ///
    /// The file is JSON:
    ///
    /// ```json
    /// {"steps": [
    ///   {"resend": 3, "extract": {"csrf": "regex:value=\"([^\"]+)\""}},
    ///   {"resend": 5, "set": ["header.X-CSRF-Token=${csrf}", "json.role=admin"]}
    /// ]}
    /// ```
    Sequence {
        /// The sequence file.
        #[arg(value_name = "FILE")]
        file: PathBuf,
        /// Bind a name before the first step, as `name=value`. Repeatable.
        #[arg(long = "var", value_name = "NAME=VALUE")]
        vars: Vec<String>,
        /// Run every step even after one fails, to read a whole file's failures
        /// at once.
        #[arg(long)]
        keep_going: bool,
        /// Which session, when more than one is open.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// What this session reached, as a tree of origins and endpoints.
    ///
    /// The request log folded into the shape a person asks it questions in:
    /// which paths, by which methods, answering which statuses, taking which
    /// parameters. Built from the receipts, so it holds what the session
    /// actually reached and nothing it merely read about. A URL scraped out of
    /// a JavaScript bundle was not visited, and blurring the two would answer
    /// "what did this session reach" with a guess.
    Sitemap {
        /// Which session, when more than one is open.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// One stored message, as it went out or as it came back.
    ///
    /// Needs a session opened with `--capture`. This is the verb that shows the
    /// bytes: headers in full, `Authorization` and `Cookie` included, which the
    /// request log deliberately never holds. `--raw` prints it as an HTTP
    /// message; the default summarises it.
    Message {
        /// The sequence number, as `requests` lists them.
        #[arg(value_name = "SEQ")]
        seq: u64,
        /// Which half. Both by default.
        #[arg(long, value_name = "HALF", value_parser = ["request", "response", "both"])]
        part: Option<String>,
        /// Print it as an HTTP message rather than a summary.
        ///
        /// Wins over `--json`: a wire message is bytes, and bytes wrapped in a
        /// JSON string are no longer the message.
        #[arg(long)]
        raw: bool,
        /// Write the body to this file, exactly as it came back.
        ///
        /// The store already holds the bytes; this is the way to get them out.
        /// A response is not always something to read — a database backup left
        /// in an open bucket, an image, an archive — and the next step is
        /// usually a tool that wants a file.
        ///
        /// With `--part both`, the response body if there is one, otherwise
        /// the request's.
        #[arg(long = "body-to", value_name = "PATH")]
        body_to: Option<PathBuf>,
        /// Which session, when more than one is open.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// How two of this session's responses differ.
    ///
    /// Status, headers and body, with the clock headers left out so that two
    /// identical answers read as identical. A JSON body is compared field by
    /// field, so a re-ordered object is not a difference; anything else is
    /// compared by line. The reply carries a similarity number for the loop
    /// that has to decide "same page or not" a few hundred times.
    Diff {
        /// The response to compare from.
        #[arg(value_name = "SEQ")]
        left: u64,
        /// The response to compare to.
        #[arg(value_name = "SEQ")]
        right: u64,
        /// Which session, when more than one is open.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Ask a stored response a question, for a script to branch on.
    ///
    /// Every condition has to hold. Exits 0 when they all do and 1 when they do
    /// not, the way `grep` does, so a loop reads `if h5i browser match ...`
    /// without reaching for `jq`. A condition that could not be *evaluated* (a
    /// pattern that does not compile, a body that was never stored) is an
    /// error, not a "no": those two answers must never look the same.
    ///
    /// What it captures is the other half. A regex hands back its groups and
    /// `--json` hands back a JSON path's value, which is how a CSRF token or a
    /// session id gets from one response into the next request.
    Match {
        /// The response to ask about.
        #[arg(value_name = "SEQ")]
        seq: u64,
        /// A regular expression over the body. Capture groups come back.
        #[arg(long, value_name = "PATTERN")]
        regex: Option<String>,
        /// A literal substring of the body. Needs no escaping, which matters
        /// when the thing being looked for is a payload.
        #[arg(long, value_name = "TEXT")]
        contains: Option<String>,
        /// A dotted path into a JSON body (`session.token`), or `path=value`.
        #[arg(long = "json-path", value_name = "PATH[=VALUE]")]
        json_path: Option<String>,
        /// A header by name, or `name=value`.
        #[arg(long, value_name = "NAME[=VALUE]")]
        header: Option<String>,
        /// The status code.
        #[arg(long, value_name = "CODE")]
        status: Option<u16>,
        /// The body is longer than this many bytes.
        #[arg(long, value_name = "BYTES")]
        longer_than: Option<u64>,
        /// The body is shorter than this many bytes.
        #[arg(long, value_name = "BYTES")]
        shorter_than: Option<u64>,
        /// Which session, when more than one is open.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Send one of this session's own requests again, with changes.
    ///
    /// The workbench verb. `requests` names what the session sent; this sends
    /// one of them again with a parameter, header, cookie or body field bent,
    /// through the same policy, the same jar and the same receipts. Needs a
    /// session opened with `--capture`, because a request nobody stored cannot
    /// be sent again.
    ///
    /// A replay is a request like any other: it gets its own sequence number
    /// and its own stored message, so a replay is itself replayable and the
    /// whole chain is in the audit.
    Resend {
        /// The sequence number to send again, as `requests` lists them.
        #[arg(value_name = "SEQ")]
        from: u64,
        /// Change one part of it: `query.id=456`, `header.X-Real-IP=127.0.0.1`,
        /// `cookie.session=forged`, `json.role=admin`, `form.user=admin`,
        /// `path=/admin`, `method=POST`, `body.raw=<bytes>`. Repeatable, and
        /// applied in the order given.
        ///
        /// The value is everything after the first `=`, so a payload full of
        /// `=` needs no escaping.
        #[arg(long = "set", value_name = "TARGET=VALUE")]
        set: Vec<String>,
        /// Set one part of it from a file: `multipart.userfile=./payload.jpg`.
        ///
        /// The value is the file's bytes, unaltered. `--set` cannot carry them:
        /// a command line is text and a JPEG begins `ff d8`, which is not text
        /// in any encoding. A magic-number check is a filter worth testing, so
        /// there has to be a way to send a real one.
        ///
        /// Applied after every `--set`, in the order given.
        #[arg(long = "set-file", value_name = "TARGET=PATH")]
        set_file: Vec<String>,
        /// Remove one part of it, by the same names.
        #[arg(long = "unset", value_name = "TARGET")]
        unset: Vec<String>,
        /// Add a target that is not there rather than refusing.
        ///
        /// Off by default: a parameter that does not exist is usually a typo,
        /// and a typo that silently succeeds costs a whole turn spent reading a
        /// response that was never going to differ.
        #[arg(long)]
        create: bool,
        /// Send it this many times and report the clock, for a timing test.
        ///
        /// Repeated inside the engine rather than by a shell loop: the thing
        /// being measured is milliseconds and starting a process costs tens of
        /// them, so a loop out here would measure the loop. The reply carries
        /// every sample plus a median and a median absolute deviation, which is
        /// the pair that survives one scheduling hiccup where a mean does not.
        #[arg(long, default_value_t = 1, value_name = "N")]
        repeat: u32,
        /// Release the repeated sends together, for a race.
        ///
        /// The test for check-then-act: an application that reads a balance,
        /// decides, and writes it back has a window between the read and the
        /// write, and `--repeat 20 --race` is what finds out how wide it is.
        ///
        /// A burst, and named as one: the sends leave from twenty threads that
        /// meet at a barrier first. It is not a single-packet attack, which
        /// needs each request split across two writes with the last byte held
        /// back. Ordinary check-then-act windows do not need that.
        ///
        /// Every send is a receipt and a stored message like any other, so a
        /// race that reproduces is a race somebody else can read afterwards.
        #[arg(long, requires = "repeat")]
        race: bool,
        /// Stop at the first redirect and report it, rather than following it.
        ///
        /// A browser follows a `Location`; a test usually wants the 302 itself.
        /// It is where an authentication flow says who you are, where an open
        /// redirect proves it accepts anything, and where the `Set-Cookie` that
        /// logs you in actually rides. The reply carries the status and the
        /// headers with nothing followed.
        #[arg(long)]
        no_follow: bool,
        /// Start this page's network allowance again before sending.
        ///
        /// The budget exists to bound *page* code: a script in a loop is the
        /// untrusted thing it stops. A blind extraction is the opposite, an
        /// agent deliberately sending hundreds of requests it composed, and
        /// without this it stops partway through with "this page has spent its
        /// allowance" and every later answer reads as a negative result.
        ///
        /// Grants nothing new: navigating resets the same allowance, so this is
        /// the same act without throwing away the page.
        #[arg(long)]
        reset_budget: bool,
        /// Send it from another session instead of this one.
        ///
        /// The authorization test, in one flag: take the request one logged-in
        /// session made and send it from a different logged-in session. The
        /// message comes from `--session`; the cookies, identity, policy and
        /// receipts are the named session's.
        ///
        /// The source session's own credentials are dropped, because carrying
        /// them would send a request that is neither user's and whose answer
        /// means nothing. `--keep-credentials` sends them anyway, for the test
        /// where that is the question.
        #[arg(long = "as", value_name = "SESSION")]
        as_session: Option<String>,
        /// Carry the source session's `Cookie` and `Authorization` headers into
        /// the other session. Only meaningful with `--as`.
        #[arg(long, requires = "as_session")]
        keep_credentials: bool,
        /// Send this request-target unchanged, without URL normalization.
        ///
        /// Policy, budget, and receipt checks still apply. The sender computes
        /// `Host` and `Content-Length`; use `--raw-request` to control framing.
        #[arg(long = "raw-target", value_name = "TARGET")]
        raw_target: Option<String>,
        /// Send a complete request unchanged from this file; `-` reads stdin.
        ///
        /// The stored URL supplies the authority for policy checks and dialing.
        #[arg(long = "raw-request", value_name = "PATH")]
        raw_request: Option<String>,
        /// Which session, when more than one is open.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Which credentials this session can use, by name. Never their values.
    Env {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Hand the page to the human at the live view for as long as a login takes.
    Login {
        /// Which session, when more than one is open. A name from
        /// `--session` at open time, or an opaque id. Defaults to
        /// $H5I_BROWSER_SESSION, then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// End login mode and make the page readable again.
        #[arg(long)]
        off: bool,
        #[arg(long)]
        json: bool,
    },

    /// Take control as a human. The agent's automation pauses at its next verb.
    Take {
        /// Which session, when more than one is open.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
    },

    /// Hand control back to the agent. It must re-snapshot before acting,
    /// because the page moved while you were driving.
    Release {
        /// Which session, when more than one is open.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
    },

    /// Watch this session's page, and take the controls.
    ///
    /// Draws the page in this terminal and drives it by keyboard; `?` lists the
    /// keys. `--web` serves it to your own browser instead.
    ///
    /// The status line says less about a host session than a boxed one, and
    /// honestly so: its engine is on loopback with no boundary outside it, so
    /// what the page may reach rests on the engine's word. `--in` is what makes
    /// that checkable.
    View {
        /// Which session, when more than one is open. A name from `--session`
        /// at open time, or an opaque id. Defaults to $H5I_BROWSER_SESSION,
        /// then to the session `open` last made.
        #[arg(long, short = 's', value_name = "NAME")]
        session: Option<String>,
        /// Serve the page to a browser over loopback instead of drawing it here.
        ///
        /// For a terminal that cannot draw images, and for a page the keyboard
        /// cannot reach.
        #[arg(long)]
        web: bool,
        /// Port for `--web`. 0 picks a free one.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Frame-rate ceiling asked of the session.
        #[arg(long, default_value_t = 10, value_name = "N")]
        fps: u32,
        /// Skip the graphics probe and render anyway, for a terminal the probe
        /// gets wrong.
        #[arg(long)]
        assume_graphics: bool,
    },

    /// Print the loopback viewer URL for a box, token included. `h5i box view`
    /// is what actually serves it; this is for pasting into a browser when a
    /// forward is already running.
    Url {
        /// A box name.
        name: String,
        #[arg(long, default_value_t = 7331)]
        port: u16,
    },
}

pub fn run(action: BrowserCommands) -> anyhow::Result<()> {
    let root = bs::root()?;
    // Cheap, and it means every entry point sweeps: there is no daemon to hold
    // a timer, so expiry happens the next time anyone looks.
    let _ = bs::expire_due(&root);

    match action {
        BrowserCommands::Open {
            url,
            session,
            new,
            in_box,
            allow,
            no_loopback,
            script,
            permissive_cors,
            no_sandbox,
            secrets,
            #[cfg(feature = "identity")]
            identity,
            width,
            height,
            expires_in,
            restore,
            capture,
            json,
        } => open(
            &root,
            session,
            new,
            StartOptions {
                url,
                in_box,
                allow,
                no_loopback,
                script,
                permissive_cors,
                no_sandbox,
                secrets,
                #[cfg(feature = "identity")]
                identity,
                width,
                height,
                expires_in,
                restore,
                capture,
            },
            json,
        ),
        #[cfg(feature = "identity")]
        BrowserCommands::Identity { args } => identity(args),
        BrowserCommands::Read {
            targets,
            in_box,
            allow,
            text,
            script,
            no_sandbox,
            #[cfg(feature = "identity")]
            identity,
            json,
        } => read(
            targets,
            in_box,
            allow,
            text,
            script,
            no_sandbox,
            #[cfg(feature = "identity")]
            identity,
            json,
        ),
        BrowserCommands::List { all, json } => list(&root, all, json),
        BrowserCommands::Status { session, json } => status(&root, session.as_deref(), json),
        BrowserCommands::Close {
            session,
            all,
            capture_drop,
            json,
        } => close(&root, session.as_deref(), all, capture_drop, json),

        BrowserCommands::Snapshot {
            session,
            delta,
            url,
            json,
        } => {
            let mut argv = vec!["snapshot".to_string()];
            if delta {
                argv.push("--delta".into());
            }
            argv.extend(url_arg(url));
            // Reading is not mutating, but a `--url` moves the page, and the
            // lock exists so a human at the wheel is not steered from under.
            let moves_the_page = argv.iter().any(|a| a == "--url");
            verb(&root, session.as_deref(), argv, moves_the_page, json)
        }
        BrowserCommands::Navigate { session, url, json } => {
            verb(&root, session.as_deref(), vec!["navigate".into(), url], true, json)
        }
        BrowserCommands::Screenshot {
            session,
            out,
            url,
            json,
        } => screenshot(&root, session.as_deref(), out, url, json),
        BrowserCommands::Reload { session, json } => {
            verb(&root, session.as_deref(), vec!["reload".into()], true, json)
        }
        BrowserCommands::Socket {
            url,
            send,
            wait_ms,
            session,
            json,
        } => {
            let mut argv = vec!["socket".to_string(), url];
            for frame in send {
                argv.push("--send".into());
                argv.push(frame);
            }
            argv.push("--wait-ms".into());
            argv.push(wait_ms.to_string());
            verb(&root, session.as_deref(), argv, false, json)
        }
        BrowserCommands::Click {
            session,
            reference,
            selector,
            role,
            name,
            json,
        } => {
            let mut argv = vec!["click".to_string()];
            argv.extend(reference);
            argv.extend(locator(selector, role, name));
            verb(&root, session.as_deref(), argv, true, json)
        }
        BrowserCommands::Type {
            session,
            reference,
            text,
            selector,
            role,
            name,
            json,
        } => {
            let mut argv = vec!["type".to_string()];
            argv.extend(reference);
            argv.extend(text);
            argv.extend(locator(selector, role, name));
            verb(&root, session.as_deref(), argv, true, json)
        }
        BrowserCommands::Submit {
            session,
            reference,
            selector,
            role,
            name,
            json,
        } => {
            let mut argv = vec!["submit".to_string()];
            argv.extend(reference);
            argv.extend(locator(selector, role, name));
            verb(&root, session.as_deref(), argv, true, json)
        }
        BrowserCommands::Scroll { session, by, json } => verb(
            &root,
            session.as_deref(),
            vec!["scroll".into(), by.to_string()],
            true,
            json,
        ),
        BrowserCommands::WaitFor {
            session,
            selector,
            text,
            json,
        } => {
            let mut argv = vec!["wait-for".to_string()];
            if let Some(selector) = selector {
                argv.push("--selector".into());
                argv.push(selector);
            }
            if let Some(text) = text {
                argv.push("--text".into());
                argv.push(text);
            }
            verb(&root, session.as_deref(), argv, false, json)
        }
        BrowserCommands::WaitForScript {
            session,
            expr,
            json,
        } => verb(
            &root,
            session.as_deref(),
            vec!["wait-for-script".into(), expr],
            false,
            json,
        ),
        BrowserCommands::Extract {
            session,
            schema,
            url,
            json,
        } => {
            let mut argv = vec!["extract".to_string(), schema];
            argv.extend(url_arg(url));
            let moves_the_page = argv.iter().any(|a| a == "--url");
            verb(&root, session.as_deref(), argv, moves_the_page, json)
        }
        BrowserCommands::Markdown {
            session,
            max_bytes,
            url,
            json,
        } => {
            let mut argv = vec!["markdown".to_string()];
            if let Some(max) = max_bytes {
                argv.push("--max-bytes".into());
                argv.push(max.to_string());
            }
            argv.extend(url_arg(url));
            let moves_the_page = argv.iter().any(|a| a == "--url");
            verb(&root, session.as_deref(), argv, moves_the_page, json)
        }
        BrowserCommands::Structured {
            session,
            url,
            json,
        } => {
            let mut argv = vec!["structured".to_string()];
            argv.extend(url_arg(url));
            let moves_the_page = argv.iter().any(|a| a == "--url");
            verb(&root, session.as_deref(), argv, moves_the_page, json)
        }
        BrowserCommands::Transcript {
            session,
            url,
            lang,
            max_bytes,
            via,
            json,
        } => {
            if let Some(helper) = via {
                return via_helper(
                    &root,
                    session.as_deref(),
                    &helper,
                    url,
                    lang,
                    max_bytes,
                    json,
                );
            }
            let mut argv = vec!["transcript".to_string()];
            argv.extend(url_arg(url));
            if let Some(lang) = lang {
                argv.push("--lang".into());
                argv.push(lang);
            }
            if let Some(max) = max_bytes {
                argv.push("--max-bytes".into());
                argv.push(max.to_string());
            }
            let moves_the_page = argv.iter().any(|a| a == "--url");
            verb(&root, session.as_deref(), argv, moves_the_page, json)
        }
        BrowserCommands::SetChecked {
            session,
            reference,
            checked,
            selector,
            role,
            name,
            json,
        } => {
            let mut argv = vec!["set-checked".to_string()];
            argv.extend(reference);
            argv.extend(checked);
            argv.extend(locator(selector, role, name));
            verb(&root, session.as_deref(), argv, true, json)
        }
        BrowserCommands::Select {
            session,
            reference,
            option,
            selector,
            role,
            name,
            json,
        } => {
            let mut argv = vec!["select".to_string()];
            argv.extend(reference);
            argv.extend(option);
            argv.extend(locator(selector, role, name));
            verb(&root, session.as_deref(), argv, true, json)
        }
        BrowserCommands::Press {
            session,
            reference,
            key,
            selector,
            role,
            name,
            json,
        } => {
            let mut argv = vec!["press".to_string()];
            argv.extend(reference);
            argv.extend(key);
            argv.extend(locator(selector, role, name));
            verb(&root, session.as_deref(), argv, true, json)
        }
        BrowserCommands::Find {
            session,
            role,
            name,
            url,
            json,
        } => {
            let mut argv = vec!["find".to_string(), "--role".into(), role];
            if let Some(name) = name {
                argv.push("--name".into());
                argv.push(name);
            }
            argv.extend(url_arg(url));
            let moves_the_page = argv.iter().any(|a| a == "--url");
            verb(&root, session.as_deref(), argv, moves_the_page, json)
        }
        BrowserCommands::Script {
            session,
            save,
            json,
        } => {
            let mut argv = vec!["script".to_string()];
            if let Some(path) = save {
                argv.push("--save".into());
                argv.push(path.display().to_string());
            }
            verb(&root, session.as_deref(), argv, false, json)
        }
        BrowserCommands::Requests {
            session,
            since,
            method,
            url_contains,
            status,
            initiator,
            denied_only,
            limit,
            json,
        } => {
            let mut argv = vec!["requests".to_string()];
            let mut flag = |name: &str, value: Option<String>| {
                if let Some(value) = value {
                    argv.push(format!("--{name}"));
                    argv.push(value);
                }
            };
            flag("since", since.map(|s| s.to_string()));
            flag("method", method);
            flag("url-contains", url_contains);
            flag("status", status.map(|s| s.to_string()));
            flag("initiator", initiator);
            flag("limit", limit.map(|s| s.to_string()));
            if denied_only {
                argv.push("--denied-only".into());
            }
            verb(&root, session.as_deref(), argv, false, json)
        }
        BrowserCommands::Sequence {
            file,
            vars,
            keep_going,
            session,
            json,
        } => {
            let bindings: Vec<(String, String)> = vars
                .into_iter()
                .map(|spec| match spec.split_once('=') {
                    Some((name, value)) => (name.to_string(), value.to_string()),
                    None => (spec, String::new()),
                })
                .collect();
            super::websec::sequence(
                &root,
                session.as_deref(),
                &file,
                &bindings,
                keep_going,
                json,
            )
        }
        BrowserCommands::Sitemap { session, json } => {
            super::websec::sitemap(&root, session.as_deref(), json)
        }
        BrowserCommands::Message {
            seq,
            part,
            raw,
            body_to,
            session,
            json,
        } => {
            let part = match part.as_deref() {
                Some("request") => super::websec::Part::Request,
                Some("response") => super::websec::Part::Response,
                _ => super::websec::Part::Both,
            };
            super::websec::show(
                &root,
                session.as_deref(),
                seq,
                part,
                raw,
                body_to.as_deref(),
                json,
            )
        }
        BrowserCommands::Diff {
            left,
            right,
            session,
            json,
        } => super::websec::diff(&root, session.as_deref(), left, right, json),
        BrowserCommands::Match {
            seq,
            regex,
            contains,
            json_path,
            header,
            status,
            longer_than,
            shorter_than,
            session,
            json,
        } => {
            use super::websec::Condition;
            // `name=value` splits on the first `=`, like an edit does, so a
            // value containing one needs no escaping.
            let split = |spec: String| -> (String, Option<String>) {
                match spec.split_once('=') {
                    Some((name, value)) => (name.to_string(), Some(value.to_string())),
                    None => (spec, None),
                }
            };
            let mut conditions = Vec::new();
            if let Some(pattern) = regex {
                conditions.push(Condition::Regex(pattern));
            }
            if let Some(text) = contains {
                conditions.push(Condition::Contains(text));
            }
            if let Some(spec) = json_path {
                let (path, value) = split(spec);
                conditions.push(Condition::Json { path, value });
            }
            if let Some(spec) = header {
                let (name, value) = split(spec);
                conditions.push(Condition::Header { name, value });
            }
            if let Some(code) = status {
                conditions.push(Condition::Status(code));
            }
            if let Some(bytes) = longer_than {
                conditions.push(Condition::LongerThan(bytes));
            }
            if let Some(bytes) = shorter_than {
                conditions.push(Condition::ShorterThan(bytes));
            }
            super::websec::matches(&root, session.as_deref(), seq, &conditions, json)
        }
        BrowserCommands::Resend {
            from,
            set,
            set_file,
            unset,
            create,
            repeat,
            race,
            no_follow,
            reset_budget,
            as_session,
            keep_credentials,
            raw_target,
            raw_request,
            session,
            json,
        } => {
            // With `--as`, the message is read here and handed to the other
            // session whole. h5i reads the store rather than asking the source
            // session for it, for the reason `websec` exists: the stored
            // request holds credentials, and a verb's reply would carry them
            // out through a renderer.
            let mut argv = vec!["resend".to_string()];
            // In the reply as well as on the terminal: `--json` is how an
            // agent reads this verb, and a silent strip is a different request.
            let mut dropped: Vec<String> = Vec::new();
            match &as_session {
                None => {
                    argv.push("--from".into());
                    argv.push(from.to_string());
                }
                Some(_) => {
                    let (request, left_behind) = super::websec::carry(
                        &root,
                        session.as_deref(),
                        from,
                        keep_credentials,
                    )?;
                    dropped = left_behind;
                    if !dropped.is_empty() && !json {
                        println!(
                            "  note     : {} left behind, so this is the other session's \
                             own request",
                            dropped.join(", ")
                        );
                    }
                    argv.push("--request".into());
                    argv.push(serde_json::to_string(&request)?);
                }
            }
            for spec in set {
                argv.push("--set".into());
                argv.push(spec);
            }
            for spec in set_file {
                argv.push("--set-file".into());
                argv.push(spec);
            }
            for spec in unset {
                argv.push("--unset".into());
                argv.push(spec);
            }
            if create {
                argv.push("--create".into());
            }
            if repeat > 1 {
                argv.push("--repeat".into());
                argv.push(repeat.to_string());
            }
            if race {
                argv.push("--together".into());
            }
            if no_follow {
                argv.push("--no-follow".into());
            }
            if reset_budget {
                argv.push("--reset-budget".into());
            }
            if let Some(target) = raw_target {
                argv.push("--raw-target".into());
                argv.push(target);
            }
            // Encode arbitrary request bytes for the JSON control channel.
            if let Some(path) = raw_request {
                let bytes = if path == "-" {
                    use std::io::Read as _;
                    let mut buf = Vec::new();
                    std::io::stdin().read_to_end(&mut buf).map_err(|e| {
                        anyhow::anyhow!("--raw-request -: stdin could not be read: {e}")
                    })?;
                    buf
                } else {
                    std::fs::read(&path).map_err(|e| {
                        anyhow::anyhow!("--raw-request: {path} could not be read: {e}")
                    })?
                };
                use base64::Engine as _;
                argv.push("--raw-request".into());
                argv.push(base64::engine::general_purpose::STANDARD.encode(&bytes));
            }
            // Mutating: it puts bytes on the wire under this session's
            // identity, which is exactly what the control lock exists to stop a
            // second driver from doing while a human is at the wheel.
            //
            // Delivered to `--as` when there is one: that is the session whose
            // cookies and receipts this request becomes part of.
            let target = as_session.as_deref().or(session.as_deref());
            // With one send there is nothing to summarise and the reply is the
            // answer. With several, the medians go in beside the samples.
            verb_then(&root, target, argv, true, json, |answer, _refused| {
                // On a refusal too: that is when the count matters most.
                if let Some(samples) = answer.get("samples").and_then(Value::as_array)
                    && let Some(summary) = super::websec::timing_summary(samples)
                {
                    answer["timing"] = summary;
                }
                if !dropped.is_empty() {
                    answer["credentials_dropped"] = serde_json::json!(dropped);
                }
                Ok(())
            })
        }
        BrowserCommands::Audit {
            session,
            no_session,
            json,
        } => {
            if no_session {
                sessionless_audit(&root, json)
            } else {
                audit(&root, session.as_deref(), json)
            }
        }
        BrowserCommands::Env { session, json } => {
            verb(&root, session.as_deref(), vec!["env".into()], false, json)
        }
        BrowserCommands::Login { session, off, json } => {
            let mut argv = vec!["login".to_string()];
            argv.push(if off { "--off".into() } else { "--on".into() });
            // Not mutating in the lock's sense: `login` is how a human takes the
            // keyboard, so refusing it while a human holds control would refuse
            // the very thing they are here to do.
            verb(&root, session.as_deref(), argv, false, json)
        }

        BrowserCommands::Take { session } => take(&root, session.as_deref()),
        BrowserCommands::Release { session } => release(&root, session.as_deref()),
        BrowserCommands::View {
            session,
            web,
            port,
            fps,
            assume_graphics,
        } => view(&root, session.as_deref(), web, port, fps, assume_graphics),
        BrowserCommands::Url { name, port } => viewer_url(&name, port),
    }
}

struct StartOptions {
    url: String,
    in_box: Option<String>,
    allow: Vec<String>,
    no_loopback: bool,
    script: bool,
    /// Behave like a browser about cross-site credentials. See the flag.
    permissive_cors: bool,
    no_sandbox: bool,
    secrets: Vec<String>,
    #[cfg(feature = "identity")]
    identity: String,
    width: u32,
    height: u32,
    expires_in: Option<u64>,
    restore: Option<String>,
    /// Keep the messages themselves, not only the record of them.
    capture: bool,
}

/// Open a URL: navigate the session that is already there, or make one.
fn open(
    root: &Path,
    selector: Option<String>,
    force_new: bool,
    opts: StartOptions,
    json: bool,
) -> anyhow::Result<()> {
    if !force_new
        && let Ok(existing) = bs::resolve(root, selector.as_deref())
    {
        let creation_only = creation_flags(&opts);
        if !creation_only.is_empty() {
            anyhow::bail!(
                "browser session {} is already open, and its policy was fixed when its \
                 engine started, so {} cannot apply now.\n\n  \
                 Open a second session with `--new`, or end this one with \
                 `h5i browser close` first.",
                label(&existing),
                creation_only.join(" and ")
            );
        }
        let dir = bs::dir(root, &existing.id);
        let mut answer = deliver(&existing, &dir, vec!["navigate".into(), opts.url.clone()])?;
        bs::scrub(&mut answer);
        if answer.get("ok").and_then(Value::as_bool) == Some(false) {
            // The session is still on whatever it was on. Saying otherwise
            // would leave an agent acting on a page it never reached.
            if json {
                println!("{}", serde_json::to_string_pretty(&answer)?);
            }
            anyhow::bail!("{} did not go to {}: {}", label(&existing), opts.url, refusal(&answer));
        }
        // The record follows the page. `url` is what this session was last told
        // to open, so `h5i browser list` keeps naming something true.
        let mut moved = existing.clone();
        moved.url = opts.url.clone();
        let _ = bs::write(root, &moved);
        if json {
            let mut record = serde_json::to_value(&moved)?;
            record["navigated"] = answer;
            println!("{}", serde_json::to_string_pretty(&record)?);
        } else {
            println!("{} {} is now on {}", SUCCESS, label(&moved), opts.url);
        }
        return Ok(());
    }
    start(root, selector, opts, json)
}

/// Which creation-only flags the caller set, named the way they typed them.
fn creation_flags(opts: &StartOptions) -> Vec<&'static str> {
    let mut set = Vec::new();
    if !opts.allow.is_empty() {
        set.push("`--allow`");
    }
    if opts.in_box.is_some() {
        set.push("`--in`");
    }
    if opts.script {
        set.push("`--script`");
    }
    if opts.permissive_cors {
        set.push("`--permissive-cors`");
    }
    if opts.no_loopback {
        set.push("`--no-loopback`");
    }
    if opts.expires_in.is_some() {
        set.push("`--expires-in`");
    }
    if opts.restore.is_some() {
        set.push("`--restore`");
    }
    if opts.no_sandbox {
        set.push("`--no-sandbox`");
    }
    if !opts.secrets.is_empty() {
        set.push("`--secret`");
    }
    // The store is opened when the broker is built, and a session that began
    // recording halfway through would hold evidence with a hole in it that
    // nothing in the store could describe. Refused rather than ignored, like
    // every other flag here: silently dropping it would leave an agent
    // believing it had a record of the login it just performed.
    if opts.capture {
        set.push("`--capture`");
    }
    // Creation-only for a stronger reason than most of these. The agent string
    // is handed to the HTTP client when the client is built, and a session that
    // could change identity while it ran would be a browser whose user agent
    // rotated between two requests of one page, which is louder than any value
    // it could have rotated to.
    #[cfg(feature = "identity")]
    if opts.identity != DEFAULT_IDENTITY {
        set.push("`--identity`");
    }
    set
}

/// How to refer to a session in a sentence: the name if it has one, and the id
/// otherwise, because the id is the only thing every session has.
fn label(session: &bs::Session) -> String {
    match &session.name {
        Some(name) => format!("`{name}` ({})", session.id),
        None => session.id.clone(),
    }
}

fn start(
    root: &Path,
    name: Option<String>,
    opts: StartOptions,
    json: bool,
) -> anyhow::Result<()> {
    // The box h5i is standing in, if it is standing in one. Read once, here,
    // rather than at each use: the three markers are the host's, and a process
    // does not move between boxes while it runs.
    let enclosing_box = h5i_core::env::in_env_box()
        .then(|| std::env::var(h5i_core::env::H5I_ENV_ID_VAR).ok())
        .flatten();

    if let (Some(target), Some(here)) = (&opts.in_box, &enclosing_box) {
        // `--in` means "put this session in a box I am outside of", and that is
        // the whole reason it can promise an enforced takeover and a lane the
        // engine did not claim for itself. From in here neither is true, and a
        // box inside a box is not a thing this product has.
        anyhow::bail!(
            "`--in {target}` cannot run from inside a box. This process is already in \
             `{here}`.\n\n  \
             Open the session without `--in`: it runs beside you, in this box, and its \
             record says so. To place a session in a box from outside one, run \
             `h5i browser open --in {target}` on the host."
        );
    }

    if let (Some(target), false) = (&opts.in_box, opts.secrets.is_empty()) {
        // A box already has a place to say this, and it is checked in: two ways to
        // declare one grant is how one of them rots. `--secret` reaches the profile
        // h5i builds for a *host* session, the placement with no repository and
        // therefore no `env.toml` to write it in. A session placed in a box
        // inherits that box's policy, so the grant belongs there.
        //
        // An error rather than a warning because the flag did nothing at all here:
        // `spawn_in_box` never read it, so every session started this way ran
        // without the credential it was told to carry and said nothing.
        anyhow::bail!(
            "`--secret` does not apply to a session placed in a box. `{target}` gets its \
             credentials from its own policy.\n\n  \
             Declare the grant in `.h5i/env.toml`, under the profile the box was created \
             with:\n    \
             [profile.browser]\n    \
             secrets = [\"H5I_SECRET_ACME_PASS\"]\n\n  \
             `--secret` is for a session on this host, which has no repository and so no \
             `env.toml` to declare anything in."
        );
    }

    let placement = match &opts.in_box {
        None => bs::Placement::Host,
        Some(name) => bs::Placement::Box { name: name.clone() },
    };

    // The identity, resolved and admitted before anything is spawned, for the
    // same reason `--restore` is: a session refused after its engine started is
    // a process to clean up and a record to explain.
    //
    // The engine checks it again, and that is not redundancy for its own sake.
    // The engine is the enforcement point, it is the half that writes the
    // headers, and this is the half that can say so in a sentence before a
    // process exists. A `--in <box>` session's engine is behind a boundary this
    // command cannot read an error out of at all.
    #[cfg(feature = "identity")]
    let identity = {
        // A path cannot travel into a box, and this is the one place that can say so clearly.
        refuse_a_file_identity_in_a_box(opts.in_box.as_deref(), &opts.identity)?;
        let identity = h5i_browser::identity::Identity::resolve(&opts.identity)
            .map_err(anyhow::Error::from)?;
        identity
            .admit(&h5i_browser::Capabilities::with_script(opts.script))
            .map_err(anyhow::Error::from)?;
        if let Some(over) = identity.check_viewport(opts.width, opts.height) {
            anyhow::bail!(
                "the browser identity `{}` cannot be used at this size:\n  {over}\n\n\
                 Open it at a size that fits, or use an identity that declares no display.",
                identity.name
            );
        }
        identity
    };

    // The inheritance is resolved before anything is spawned, so a bad
    // `--restore` fails before there is a session to clean up.
    let restored_from = match &opts.restore {
        None => None,
        Some(id) => {
            let previous = bs::read(root, id)?;
            Some(previous.id)
        }
    };

    let id = bs::new_id(root)?;
    let dir = bs::dir(root, &id);

    let mut spawned = match &placement {
        bs::Placement::Host => spawn_on_host(root, &dir, &opts, enclosing_box.is_some())?,
        bs::Placement::Box { name } => spawn_in_box(name, &dir, &opts)?,
    };
    let lane = bs::Session::lane_for(&placement, spawned.boundary_enforced);

    if let Some(from) = &restored_from {
        seed_storage(root, from, &dir)?;
    }

    let session = bs::Session {
        id: id.clone(),
        name: name.clone(),
        engine: bs::Engine::H5iLight,
        lane,
        placement,
        url: opts.url.clone(),
        // Microseconds, to match the rest of the record: these stamps interleave
        // with the engine's in an audit, and a whole agent loop fits inside one
        // second.
        started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        expires_at: opts.expires_in.map(|secs| {
            (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
        }),
        storage: bs::Storage::Ephemeral,
        policy_digest: spawned.policy_digest.clone(),
        // Empty in a build without identities, which is what those sessions
        // are: not "presented nothing", but "not recorded". The same thing an
        // older record says, and what `#[serde(default)]` gives it.
        #[cfg(feature = "identity")]
        identity: identity.name.clone(),
        #[cfg(not(feature = "identity"))]
        identity: String::new(),
        #[cfg(feature = "identity")]
        identity_digest: identity.digest(),
        #[cfg(not(feature = "identity"))]
        identity_digest: String::new(),
        restored_from,
        state: bs::State::Live,
        ended_at: None,
        end_reason: None,
        enclosing_box,
        confinement: spawned.confinement.clone(),
        control: bs::Control {
            channel: spawned.channel,
            file: Some(spawned.control_in_engine_view.clone()),
            witness: spawned.control_on_host.clone(),
            pid: spawned.pid,
        },
        logs: spawned.logs.clone(),
        permissive_cors: opts.permissive_cors,
    };
    bs::write(root, &session)?;
    // The default follows the newest session whether or not it was named, so a
    // `--session auth` that is the only thing running is still what a bare
    // `h5i browser snapshot` acts on. A name adds a way to address it; it does
    // not take away the ordinary one.
    let _ = bs::set_default(root, &session.id);

    // Wait for the engine to say it is up, and record the death if it is not.
    if let Err(e) = await_control(&mut spawned, &dir) {
        // Stop whatever did start. A timeout that leaves an engine running is a
        // session nothing owns: its record says died, its process is serving,
        // and the next start in the same box collides with it.
        (spawned.stop)();
        let mut dead = session.clone();
        bs::end(root, &mut dead, bs::State::Died, &e);
        anyhow::bail!("{e}\n\n  The session is recorded as `{}`, died.", dead.id);
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&session)?);
    } else {
        match &session.name {
            Some(name) => println!(
                "{} browser session {} ({})",
                SUCCESS,
                style(name).cyan(),
                style(&session.id).dim()
            ),
            None => println!("{} browser session {}", SUCCESS, style(&session.id).dim()),
        }
        print_summary(&session);
        // The next command, spelled the way it is actually used. Printing the
        // id here and expecting it back would teach the id as the interface.
        let sel = match &session.name {
            Some(name) => format!(" --session {name}"),
            None => String::new(),
        };
        println!("\n  next     : {}", style(format!("h5i browser snapshot{sel}")).dim());
    }
    Ok(())
}

/// What a spawn produced, in both views of the filesystem it has to be seen in.
struct Spawned {
    /// Which channel the engine is listening on.
    channel: bs::Channel,
    /// Ask whether the engine is still on its way up. `Some(reason)` means it is
    /// not, and the reason is what the user is told.
    ///
    /// A closure rather than a pid, because the honest answer differs by placement
    /// and a pid cannot carry that. On the host it owns the `Child` and asks
    /// `try_wait`: a child nobody waits on is a zombie, and a zombie answers
    /// `kill(pid, 0)`, so polling the pid would wait the full timeout on an engine
    /// that exited immediately. In a box it asks the service registry, which knows
    /// that a microvm's pid is a guest pid and not this machine's to signal.
    alive: Box<dyn FnMut() -> Option<String>>,
    /// The process this machine can signal, when there is one. `None` for a
    /// boxed session: at the microvm tier the service's pid belongs to the
    /// guest, and a host `kill` on that number would be aimed at whatever
    /// unrelated process happens to hold it.
    pid: Option<u32>,
    /// The control file's path as the engine sees it. Inside a box that is a
    /// box path; on the host it is a host path. Never mixed: binding one and
    /// reading the other is how enforcement goes silently missing.
    control_in_engine_view: PathBuf,
    /// The same file as this machine sees it, when it can.
    control_on_host: Option<PathBuf>,
    policy_digest: String,
    /// Where this machine can read the session's logs, when it can.
    logs: bs::Logs,
    /// What is holding the engine, as it turned out rather than as it was asked
    /// for: a host without Landlock answers `None` with the reason.
    confinement: h5i_core::browser_sandbox::Confinement,
    /// Whether something outside the engine actually decides what may leave.
    /// See [`bs::Session::lane_for`]. This is the input to the one claim the
    /// product makes that a reader can check.
    boundary_enforced: bool,
    /// How to end it. `close` calls this before recording the ending.
    stop: Box<dyn FnOnce()>,
}

fn spawn_on_host(
    root: &Path,
    dir: &Path,
    opts: &StartOptions,
    in_a_box: bool,
) -> anyhow::Result<Spawned> {
    // A port on a bare host, a socket inside a box.
    //
    // The port is the simpler channel and the session directory can be
    // anywhere, which matters: a socket address is capped near 100 bytes and a
    // registry under a long temp path would exceed it. Inside a box a port is
    // not merely awkward, it does not work. The netns may have no usable
    // loopback at all, and `net.mode = deny` leaves nothing to dial. The
    // registry inside a box lives under the box's own tmp, which is short.
    let channel = if in_a_box {
        bs::Channel::Socket
    } else {
        bs::Channel::Port
    };
    let control = match channel {
        bs::Channel::Port => dir.join(bs::CONTROL_FILE),
        bs::Channel::Socket => dir.join(bs::CONTROL_FILE).with_extension("sock"),
    };
    let mut argv: Vec<String> = vec![
        ENGINE_SUBCOMMAND.into(),
        "serve".into(),
        opts.url.clone(),
        "--stream-file".into(),
        dir.join(bs::STREAM_FILE).display().to_string(),
        channel.flag().into(),
        control.display().to_string(),
        "--receipts".into(),
        dir.join(bs::RECEIPTS_FILE).display().to_string(),
        "--actions".into(),
        dir.join(bs::ACTIONS_FILE).display().to_string(),
        // The jar h5i names, and the file `--restore` reads back. h5i chooses
        // the path; the engine only chooses the bytes, which is the same rule
        // every other session artifact follows.
        "--cookie-jar".into(),
        dir.join(bs::COOKIES_FILE).display().to_string(),
        "--width".into(),
        opts.width.to_string(),
        "--height".into(),
        opts.height.to_string(),
    ];
    argv.extend(net_args(opts));
    // h5i names the directory, as it does for every other session artifact, so
    // where a session's evidence lands is not the engine caller's to choose.
    if opts.capture {
        argv.push("--capture".into());
        argv.push(dir.join(bs::MESSAGES_DIR).display().to_string());
    }
    if opts.script {
        argv.push("--script".into());
    }

    let log_path = dir.join("engine.log");

    // The sandbox, unless the caller declined it or the host cannot.
    //
    // The engine's own binary has to be readable, because a confined `execve`
    // needs it and nothing under a development checkout, or under `~/.cargo`,
    // is granted by the defaults. This is the same wall a box hits when the
    // engine is somewhere it may read and not run.
    let engine = engine_binary()?;
    let reads = vec![engine.clone()];
    let secrets = secret_variables(&opts.secrets)?;
    let wants = h5i_core::browser_sandbox::Wants {
        session_dir: dir,
        reads: &reads,
        secrets: &secrets,
        // A session is resident and is bounded by `--expires-in`, not a signal.
        wall_secs: 0,
    };
    let confined = if opts.no_sandbox {
        None
    } else {
        h5i_core::browser_sandbox::resolve_for(&wants)?
    };

    // The credentials this session was told to carry, resolved before anything is spawned and
    // delivered to this child alone.
    let brokered = if secrets.is_empty() {
        None
    } else {
        Some(
            h5i_core::secrets_broker::broker(
                &h5i_core::browser_sandbox::grants_for(&secrets),
                &secret_dir(root),
                false,
                false,
                &h5i_core::secrets_broker::fingerprint_key(root)?,
            )
            .map_err(unresolved_credential)?,
        )
    };
    let granted: Vec<(String, String)> = brokered
        .as_ref()
        .map(|b| b.env.clone())
        .unwrap_or_default();

    // Inside the sandbox the environment is cleared, so the engine's own
    // `$HOME`-based font discovery finds nothing. The grant and the search path
    // come from one list so they cannot disagree, and `--font-dir` *replaces*
    // the engine's defaults, which is why the system directories travel with
    // the personal ones rather than being left implicit.
    if let Some(c) = &confined {
        for dir in &c.fonts {
            argv.push("--font-dir".into());
            argv.push(dir.display().to_string());
        }
        if !c.dropped_fonts.is_empty() {
            eprintln!(
                "  {}     {} personal font director{} could not be granted, so a page may \
                 render with different faces here than outside. The likeliest cause is a \
                 symlink that resolves somewhere the policy denies.",
                style("note").yellow(),
                c.dropped_fonts.len(),
                if c.dropped_fonts.len() == 1 { "y" } else { "ies" }
            );
        }
    }

    let (pid, confinement) = match &confined {
        Some(c) => {
            // `spawn_background` is the same call `h5i box service start` makes:
            // a confined child that outlives the command that started it, with
            // no pid namespace, so the pid it returns is the engine itself.
            // argv[0] is the program, and `spawn_background` execs it directly:
            // `__engine` alone is a subcommand, not a binary.
            let mut confined_argv = vec![engine.display().to_string()];
            confined_argv.extend(argv.iter().cloned());
            // The environment is cleared and rebuilt from the profile, so
            // nothing here inherits by accident: `--secret` is a policy
            // statement rather than a shell habit, and this is where the
            // statement is made good.
            let mut injected = granted.clone();
            // The engine's own single-process switch, which is here because a
            // debugging hatch that silently does nothing in the default
            // arrangement is worse than no hatch: it is documented as running
            // the engine as one process, and inside the sandbox the variable
            // would otherwise never arrive. It grants nothing and reveals
            // nothing.
            injected.extend(single_process_switch());

            let handle = h5i_core::sandbox::spawn_background(
                &c.policy,
                dir,
                &confined_argv,
                &injected,
                &log_path,
                "browser-session",
            )?;
            (handle.pid, h5i_core::browser_sandbox::Confinement::Process)
        }
        None => {
            let log = std::fs::File::create(&log_path)?;
            let mut command = Command::new(engine_binary()?);
            command
                .args(&argv)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log));
            // Named credentials, set explicitly even though this child inherits
            // the whole environment anyway. What it buys is not secrecy (there
            // is none to buy on this path, and the summary line says so) it is
            // that `--secret` means one thing in both shapes: named, resolved,
            // and refused if it is not there.
            command.envs(granted.clone());
            detach(&mut command);
            let child = command
                .spawn()
                .map_err(|e| anyhow::anyhow!("could not start the browser engine ({e})"))?;
            let why = if opts.no_sandbox {
                "started with --no-sandbox".to_string()
            } else {
                h5i_core::browser_sandbox::unavailable_reason(&h5i_core::browser_sandbox::caps())
            };
            (child.id(), h5i_core::browser_sandbox::Confinement::None { why })
        }
    };

    if let h5i_core::browser_sandbox::Confinement::None { why } = &confinement
        && !opts.no_sandbox
    {
        // Loud, because a sandbox nobody can see is indistinguishable from one
        // that was never applied. The record says it too; this is for whoever
        // is watching the terminal.
        eprintln!(
            "  {}     no sandbox: {why}. The session still records every request; \
             what it does not have is containment of what a parser bug could do.",
            style("note").yellow()
        );
    }

    Ok(Spawned {
        channel,
        // `waitpid(WNOHANG)` rather than a signal probe: the confined spawn
        // hands back a pid and drops the `Child`, so a dead engine is an
        // unreaped zombie and `kill(pid, 0)` would call it alive, which is the
        // bug that made a start wait its whole timeout on an engine that exited
        // immediately.
        alive: Box::new(move || reap(pid)),
        pid: Some(pid),
        control_in_engine_view: control.clone(),
        control_on_host: Some(control),
        policy_digest: host_policy_digest(opts),
        logs: bs::Logs {
            actions: Some(dir.join(bs::ACTIONS_FILE)),
            requests: Some(dir.join(bs::RECEIPTS_FILE)),
        },
        confinement,
        // Nothing outside the engine is deciding anything here. That is what
        // "no containment beyond the engine" means, and the lane says so.
        boundary_enforced: false,
        stop: Box::new(move || kill(pid)),
    })
}

/// Everything about a box that has to be true before an engine is started in it,
/// checked before anything is started.
///
/// Written as a preflight rather than discovered by the 30-second start timeout
/// because all three of these failures look identical from outside, the engine
/// never advertises, and each of them has a different fix. A timeout that
/// says "did not come up" for a box that could never have run it is the kind of
/// error that costs an afternoon.
fn preflight_box(
    name: &str,
    h5i_root: &Path,
    manifest: &h5i_core::env::EnvManifest,
) -> anyhow::Result<()> {
    let policy = h5i_core::env::load_policy(h5i_root, manifest)?;

    // 1. The tier has to be able to hold a long-lived process at all. A
    //    resident browser is a service, and services are a workspace/process/
    //    microvm capability today: the supervised and container tiers cannot
    //    spawn one (h5i-sandbox's `spawn_background`, "Idea 3.5").
    let claim = policy.claim;
    let holds_a_service = matches!(
        claim,
        h5i_core::sandbox::IsolationClaim::Workspace
            | h5i_core::sandbox::IsolationClaim::Process
            | h5i_core::sandbox::IsolationClaim::Microvm
    );
    if !holds_a_service {
        anyhow::bail!(
            "box `{name}` is at isolation `{}`, which cannot hold a resident process yet, so \
             it cannot hold a browser session.\n\n  \
             The tiers that can are workspace, process and microvm. Note the standing \
             trade-off: the `browser` profile's egress allowlist needs supervised or \
             container, and those are exactly the tiers that cannot hold a service — so on \
             Linux today the only tier that does both is microvm.\n\n  \
             Run the session on this machine instead (drop `--in`), which records every \
             request the same way and claims no containment for it.",
            claim.as_str()
        );
    }

    // 2.
    let probe = Command::new(std::env::current_exe()?)
        .arg("box")
        .arg("run")
        .arg(name)
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(format!(
            "{} {ENGINE_SUBCOMMAND} capabilities >/dev/null 2>&1",
            h5i_in_box()
        ))
        .output()?;
    let binary = h5i_in_box();
    match probe.status.code() {
        Some(0) => Ok(()),
        // Found and not executable. Almost always the h5i on `PATH` being a
        // `cargo install` one, so the fix is named rather than described.
        Some(126) => anyhow::bail!(
            "box `{name}` cannot execute `{binary}`.\n\n  \
             A box grants `~/.cargo/bin` and `~/.local/bin` **read** and not **exec**, so an \
             h5i installed there is visible inside the box and cannot be run. Install one \
             where the box may execute it:\n    \
             sudo install -m755 $(command -v h5i) /usr/local/bin/h5i\n\n  \
             Or set $H5I_IN_BOX to a path inside the box that is executable there."
        ),
        Some(127) => anyhow::bail!(
            "there is no `{binary}` in box `{name}`.\n\n  \
             `--in` carries every verb into the box by running h5i there, so the box needs \
             one on its `PATH`. Install it at a system path, or set $H5I_IN_BOX to where it \
             actually is inside the box."
        ),
        _ => anyhow::bail!(
            "the `{binary}` inside box `{name}` has no browser engine in it.\n\n  \
             The engine is part of the h5i binary, so the box needs an h5i new enough to \
             carry one, built with the `browser` feature. Check what it has:\n    \
             h5i box run {name} -- {binary} --version\n\n  \
             Set $H5I_IN_BOX to point at a different one inside the box."
        ),
    }
}

fn spawn_in_box(name: &str, dir: &Path, opts: &StartOptions) -> anyhow::Result<Spawned> {
    let repo = super::discover_repo("h5i browser --in")?;
    let h5i_root = h5i_core::storage::h5i_root_for_repo(&repo)?;
    let manifest = h5i_core::env::find(&h5i_root, name)?;
    preflight_box(name, &h5i_root, &manifest)?;

    // What this box actually enforces at its boundary, read from the policy it
    // was created with rather than assumed from the fact that it is a box. Box
    // creation is fail-closed on the combination, a profile that declares an
    // egress allowlist cannot be created at a tier that cannot enforce one, so
    // a declared allowlist here is an enforced one.
    let boundary_enforced = match h5i_core::env::load_policy(&h5i_root, &manifest) {
        Ok(policy) => {
            !policy.profile.net_egress.is_empty()
                || policy.profile.net_mode == h5i_core::sandbox::NetMode::Deny
        }
        Err(_) => false,
    };

    // Both views of the same file, named here rather than inherited from the
    // box's environment. The `browser` profile does set `H5I_BROWSER_STREAM_FILE`
    // and the engine would derive a control file beside it, but relying on that
    // would tie `--in` to one profile, and a session in a box is not a
    // property of the profile, it is a property of the placement.
    let files = h5i_core::env::box_tmp_file(&h5i_root, &manifest, BROWSER_SERVICE);
    let (control_in_box, control_on_host) = match &files {
        // The box always has a path, and it is always the one `box_tmp_file`
        // built. Qualified where the box's `/tmp` is shared with the host's.
        // Only the host's view of it can be missing: an image-backed tier keeps
        // that `/tmp` inside the image, and a tier whose mapping h5i cannot
        // name is refused rather than guessed at. Inventing a bare
        // `/tmp/h5i-browser.sock` here, as this used to, put the socket back on
        // the shared unqualified name two boxes would fight over.
        Some((in_box, on_host)) => (
            in_box.with_extension("sock"),
            on_host.as_ref().map(|p| p.with_extension("sock")),
        ),
        None => (
            PathBuf::from("/tmp")
                .join(BROWSER_SERVICE)
                .with_extension("sock"),
            None,
        ),
    };
    let in_box_base = control_in_box.with_extension("");

    // `sockaddr_un.sun_path` is 108 bytes on Linux and 104 on macOS, and a bind
    // past it fails with a message about the address family rather than about
    // the length. The path is h5i's own, so the failure is h5i's to explain, and
    // to explain now rather than at the first verb.
    const SUN_LEN: usize = 100;
    if control_in_box.as_os_str().len() > SUN_LEN {
        anyhow::bail!(
            "the control socket for a session in `{name}` would be {} bytes, and a Unix socket \
             path cannot exceed about {SUN_LEN}:\n    {}\n\n  \
             The path comes from the box's own /tmp. Create the box under a shorter directory, \
             or run the session on this machine (drop `--in`).",
            control_in_box.as_os_str().len(),
            control_in_box.display()
        );
    }

    if control_on_host.is_none() {
        // Not fatal: an image-backed tier has a `/tmp` the host cannot read, and
        // a session there is still a session. What is lost is only the ability
        // to answer "is it alive" without sending a verb, and `probe` says so
        // by declining to guess rather than by reporting a death.
        eprintln!(
            "  {}     `{name}` keeps its /tmp inside its image, so `h5i browser list` cannot \
             see whether this session is still up without sending it a verb",
            style("note").yellow()
        );
    }

    let mut argv: Vec<String> = vec![
        h5i_in_box(),
        ENGINE_SUBCOMMAND.into(),
        "serve".into(),
        opts.url.clone(),
        // A socket, not a port. Every `h5i box run` gets its own network
        // namespace, so a verb carried in later has a loopback of its own and
        // the port this session binds is not on it. The connection fails with
        // ENETUNREACH, which reads exactly like a session that is not running.
        // The box's filesystem is one filesystem across every run in it, so a
        // path is the address that survives.
        "--control-socket".into(),
        control_in_box.display().to_string(),
        "--stream-file".into(),
        in_box_base.with_extension("stream").display().to_string(),
        "--receipts".into(),
        in_box_base.with_extension("requests.jsonl").display().to_string(),
        "--actions".into(),
        in_box_base.with_extension("actions.jsonl").display().to_string(),
        // Inside the box, beside the receipts, for the same reason: this is the
        // filesystem the engine has. Whether this machine can read it back
        // afterwards is the `control_on_host` question above, and a tier that
        // keeps its /tmp in an image is a tier whose jar does not survive the
        // box, which is a narrowing of `--restore`, not of the session.
        "--cookie-jar".into(),
        in_box_base.with_extension("cookies.json").display().to_string(),
        "--width".into(),
        opts.width.to_string(),
        "--height".into(),
        opts.height.to_string(),
    ];
    argv.extend(net_args(opts));
    // Inside the box, beside the receipts, for the same reason the jar is: this
    // is the filesystem the engine has.
    if opts.capture {
        argv.push("--capture".into());
        argv.push(in_box_base.with_extension("messages").display().to_string());
    }
    if opts.script {
        argv.push("--script".into());
    }

    // A service, not a run. `h5i box run` takes the box's exclusive writer
    // lock and holds it for the life of the command, so a resident engine
    // started that way locks every later verb out of its own box. The failure
    // this path was rewritten to fix. A service takes the service lock instead,
    // which is what lets a brief `box run` carry a verb in while the engine
    // keeps serving.
    let def = h5i_core::env::ServiceDef {
        command: shell_join(&argv),
        port: None,
        restart: None,
        logs: true,
    };
    let record = h5i_core::env::service_start_with_def(
        &repo,
        &h5i_root,
        &manifest,
        BROWSER_SERVICE,
        &def,
    )
    .map_err(|e| {
        let detail = e.to_string();
        // Two failures reach here and they want different next steps, so the
        // hint is chosen rather than a paragraph covering both.
        let hint = if detail.contains("services are not supported at isolation") {
            format!(
                "A resident browser is a long-lived process in the box, and `{name}` is on a \
                 tier that cannot hold one. Make the box at a tier that can:\n    \
                 h5i box --profile browser --engine h5i --isolation process --name {name}"
            )
        } else {
            format!(
                "The box needs `h5i` on its own PATH. Check with:\n    \
                 h5i box run {name} -- command -v h5i"
            )
        };
        anyhow::anyhow!("could not start the browser engine in `{name}`: {detail}\n\n  {hint}")
    })?;

    let log = PathBuf::from(record.log.clone());
    if let Ok(mut sink) = std::fs::File::create(dir.join("engine.log")) {
        use std::io::Write;
        let _ = writeln!(sink, "the engine's own log is in the box, at {}", log.display());
    }

    let alive_root = h5i_root.clone();
    let alive_manifest = manifest.clone();
    let stop_root = h5i_root.clone();
    let stop_manifest = manifest.clone();
    let stop_repo_path = repo.path().to_path_buf();

    Ok(Spawned {
        alive: Box::new(move || {
            let running = h5i_core::env::service_status(&alive_root, &alive_manifest)
                .into_iter()
                .find(|s| s.record.name == BROWSER_SERVICE)
                .map(|s| s.alive)
                .unwrap_or(false);
            (!running).then(|| "the browser engine exited inside the box".to_string())
        }),
        // The service's pid is the box's, not this machine's to signal.
        pid: None,
        // The box's own view. `serve` with no `--control-file` derives it from
        // `$H5I_BROWSER_STREAM_FILE`, which the box's environment sets.
        control_in_engine_view: control_in_box,
        control_on_host,
        channel: bs::Channel::Socket,
        // A boxed session is confined by its box, whose tier the record already
        // carries as `placement`. Saying "process" here would name the wrong
        // thing.
        confinement: h5i_core::browser_sandbox::Confinement::None {
            why: "confined by its box, not by a session sandbox".into(),
        },
        policy_digest: manifest.policy_digest.clone(),
        // The box's own logs, as this machine sees them. `None` on a tier whose
        // /tmp lives in an image, and an audit then says `unavailable` rather
        // than rendering an empty list that looks like a quiet session.
        logs: bs::Logs {
            actions: files
                .as_ref()
                .and_then(|(_, on_host)| on_host.as_ref())
                .map(|p| p.with_extension("actions.jsonl")),
            requests: files
                .as_ref()
                .and_then(|(_, on_host)| on_host.as_ref())
                .map(|p| p.with_extension("requests.jsonl")),
        },
        boundary_enforced,
        stop: Box::new(move || {
            if let Ok(repo) = git2::Repository::open(&stop_repo_path) {
                let _ = h5i_core::env::service_stop(
                    &repo,
                    &stop_root,
                    &stop_manifest,
                    BROWSER_SERVICE,
                );
            }
        }),
    })
}

/// The service name a boxed browser session runs under.
///
/// One per box, because one box holds one resident engine: the box's
/// environment names a single stream file, and two engines writing it would be
/// two sessions the viewers could not tell apart.
const BROWSER_SERVICE: &str = "h5i-browser";

/// Quote an argv into the single shell string a service definition carries.
///
/// A service's command goes through `sh -c`, so a URL with a `&` in it, or a
/// path with a space, is a command that does something other than what was
/// asked. Single quotes with the usual escape, applied to every word rather
/// than to the ones that look dangerous.
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|word| format!("'{}'", word.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The hidden subcommand `h5i` becomes when it is the engine.
///
/// The engine used to be a second binary, and finding it was a problem with
/// two halves that both bit: on the host it might be an older install earlier
/// on `$PATH`, and inside a box it might sit in `~/.cargo/bin`, which Landlock
/// makes readable and *not executable*, so `command -v` found it and `exec`
/// refused it. Neither can happen now: the engine is this binary, and a box
/// that can run `h5i` at all can run it.
const ENGINE_SUBCOMMAND: &str = "__engine";

/// The outer bound on a confined read, in seconds.
///
/// Generous on purpose. The engine already bounds each navigation; this only
/// catches the case where it stops making progress at all.
const READ_WALL_SECS: u64 = 300;

/// How to invoke h5i inside a box.
///
/// Bare `h5i`, not a path: a box's `PATH` is the thing that knows where the
/// system install is, and it is already the binary `h5i box run` executes.
///
/// `$H5I_IN_BOX` overrides it, for the two cases where the name is not enough:
/// a box whose h5i is somewhere its `PATH` does not cover, and a working copy
/// newer than the system install, which is the ordinary state of anyone
/// developing h5i itself.
fn h5i_in_box() -> String {
    std::env::var("H5I_IN_BOX").unwrap_or_else(|_| "h5i".to_string())
}

/// `--url X`, or nothing.
fn url_arg(url: Option<String>) -> Vec<String> {
    match url {
        Some(url) => vec!["--url".to_string(), url],
        None => Vec::new(),
    }
}

/// The `--selector` / `--role` / `--name` way of naming an element, when a
/// `@ref` is not what the caller has.
///
/// A role and its accessible name survive a re-render that moves everything; a
/// `@ref` from an older reading does not, and is refused rather than resolved
/// against whatever now sits in that position.
fn locator(
    selector: Option<String>,
    role: Option<String>,
    name: Option<String>,
) -> Vec<String> {
    let mut argv = Vec::new();
    if let Some(selector) = selector {
        argv.push("--selector".to_string());
        argv.push(selector);
    }
    if let Some(role) = role {
        argv.push("--role".to_string());
        argv.push(role);
    }
    if let Some(name) = name {
        argv.push("--name".to_string());
        argv.push(name);
    }
    argv
}

fn net_args(opts: &StartOptions) -> Vec<String> {
    let mut argv = Vec::new();
    for origin in granted_origins(opts) {
        argv.push("--allow".to_string());
        argv.push(origin);
    }
    if opts.no_loopback {
        argv.push("--no-loopback".into());
    }
    if opts.permissive_cors {
        argv.push("--permissive-cors".into());
    }
    // Only when it differs from the default, which is what every other flag here already does,
    // and the reason is not tidiness.
    #[cfg(feature = "identity")]
    if opts.identity != DEFAULT_IDENTITY {
        argv.push("--identity".into());
        argv.push(opts.identity.clone());
    }
    argv
}

/// What this session is allowed to reach: the origins the caller named, and the page it asked
/// to open.
fn granted_origins(opts: &StartOptions) -> Vec<String> {
    let mut origins = opts.allow.clone();
    if is_web_url(&opts.url) && !origins.contains(&opts.url) {
        origins.push(opts.url.clone());
    }
    origins
}

/// Whether a target is fetched over the network, judged by its scheme alone.
fn is_web_url(target: &str) -> bool {
    let target = target.trim_start();
    ["http://", "https://"]
        .iter()
        .any(|scheme| target.len() > scheme.len() && target[..scheme.len()].eq_ignore_ascii_case(scheme))
}

/// The digest of what a host session was allowed to do.
fn host_policy_digest(opts: &StartOptions) -> String {
    use sha2::{Digest, Sha256};
    let mut allow = granted_origins(opts);
    allow.sort();
    let material = format!(
        "host\nallow={}\nloopback={}\nscript={}\npermissive_cors={}\n",
        allow.join(","),
        !opts.no_loopback,
        opts.script,
        opts.permissive_cors
    );
    format!("sha256:{:x}", Sha256::digest(material.as_bytes()))
}

/// Wait until the engine advertises its control file, or until it is clear it
/// never will.
fn await_control(spawned: &mut Spawned, dir: &Path) -> Result<(), String> {
    let Some(witness) = spawned.control_on_host.clone() else {
        // Nothing on this side to watch. The first verb finds out.
        return Ok(());
    };
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if witness.exists() {
            return Ok(());
        }
        if let Some(reason) = (spawned.alive)() {
            return Err(format!(
                "{reason} before it served a page. Its own output:\n{}",
                tail_of(&dir.join("engine.log"))
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the browser engine did not come up within {}s (see {})",
                START_TIMEOUT.as_secs(),
                dir.join("engine.log").display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The last few lines of the engine's own output, scrubbed.
///
/// Quoted into an error because the useful half of a failed start is almost
/// always one line the engine already printed (a URL the box cannot see, an
/// engine not on its `PATH`) and telling someone to go and read a file is one
/// step more than they need. Scrubbed like any other answer: this text came
/// from a process that was rendering a page.
fn tail_of(log: &Path) -> String {
    let body = std::fs::read_to_string(log).unwrap_or_default();
    let tail: Vec<&str> = body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(6)
        .collect();
    tail.into_iter()
        .rev()
        .map(|line| format!("    {}", bs::scrub_text(line)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Carry a previous session's cookie jar into a new session's directory.
fn seed_storage(root: &Path, from: &str, into: &Path) -> anyhow::Result<()> {
    let source = bs::dir(root, from).join(bs::COOKIES_FILE);
    if !source.exists() {
        anyhow::bail!(
            "session {from} left no cookie jar at {}, so there is nothing for `--restore` to \
             carry forward.\n\n  \
             A session writes one only while it is running, and only when it is placed \
             somewhere this machine can read — a box that keeps its /tmp inside its image \
             does not qualify. A session that stored no cookies leaves an empty jar, not no \
             jar, so a missing file means the session predates this or ran out of reach.\n\n  \
             Drop `--restore` to start a fresh session, and log in once more in this one.",
            source.display()
        );
    }
    std::fs::copy(&source, into.join(bs::COOKIES_FILE))?;
    Ok(())
}

/// Take a PNG of the page, into a file *h5i names*.
fn screenshot(
    root: &Path,
    selector: Option<&str>,
    out: Option<PathBuf>,
    url: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let session = match bs::resolve(root, selector) {
        Ok(session) => session,
        Err(gone) => {
            eprintln!("{}", gone);
            std::process::exit(bs::EXIT_SESSION_GONE);
        }
    };

    // Milliseconds, so two shots in one second are two files. A screenshot that
    // silently replaced the previous one would lose the before-and-after that is
    // most of why an agent takes two.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("screenshot-{stamp}.png");

    // The engine always paints into a directory it may write, and h5i moves
    // the file afterwards. Handing `--out` straight to the engine is what this
    // used to do, and a confined session may write only its own directory, so
    // `--out ~/shot.png` came back as a bare `Permission denied`: h5i asking a
    // sandboxed process to write somewhere h5i itself could have written, then
    // reporting the sandbox's refusal as though the path were at fault. Same
    // rule as the cookie jar: h5i chooses the path, the engine the bytes.
    let (painted, host_view) = match &session.placement {
        bs::Placement::Host => {
            let path = bs::artifact_path(root, &session.id, &name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            (path.clone(), Some(path))
        }
        bs::Placement::Box { name: box_name } => {
            // The box's own /tmp, via the socket the record already holds.
            // Nothing here can create the directory: it is on the far side of
            // the boundary, and it exists because the socket is bound in it.
            let control = session.control.file.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "this session in `{box_name}` names no control socket, so there is no \
                     path inside the box to paint into. Reopen the session, or take the \
                     screenshot from a session on this machine."
                )
            })?;
            let in_box = control
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(bs::safe_name(&name));
            // Where this machine sees that same file, when it can see it at
            // all. An image-backed tier keeps its `/tmp` inside the image, and
            // then the file exists and is unreachable from here, which is a
            // fact to report rather than to paper over.
            let on_host = session
                .control
                .witness
                .as_ref()
                .and_then(|p| p.parent())
                .map(|dir| dir.join(bs::safe_name(&name)));
            (in_box, on_host)
        }
    };

    let mut argv = vec![
        "screenshot".to_string(),
        "--path".to_string(),
        painted.display().to_string(),
    ];
    argv.extend(url_arg(url));
    // Painting does not move the page. Going somewhere first does, and the
    // control lock exists so a human at the wheel is not steered from under.
    let moves_the_page = argv.iter().any(|a| a == "--url");

    verb_then(root, selector, argv, moves_the_page, json, |answer, refused| {
        // Nothing was painted, so there is nothing to move.
        if refused {
            return Ok(());
        }
        let Some(out) = out else {
            return Ok(());
        };
        deliver_file(&painted, host_view.as_deref(), &out, &session)?;
        // The reply names where the file *is*. An answer still pointing into
        // the session's artifacts would send a caller to the copy h5i just
        // moved away.
        answer["path"] = Value::String(out.display().to_string());
        Ok(())
    })
}

/// Move the file the engine painted to where the caller asked for it.
///
/// The bytes are never lost. A copy that fails leaves the shot where the engine
/// put it and says both paths, because "the screenshot could not be written" is
/// false when it was written and only the second step failed.
fn deliver_file(
    painted: &Path,
    host_view: Option<&Path>,
    out: &Path,
    session: &bs::Session,
) -> anyhow::Result<()> {
    let Some(source) = host_view else {
        anyhow::bail!(
            "the screenshot was painted inside the box, at {}, and this machine cannot see \
             that filesystem — the box keeps its /tmp inside its image — so h5i cannot \
             bring it out to {}.\n\n  \
             Take it without `--out` and read it from inside the box, or place the session \
             on a tier whose /tmp this machine shares.",
            painted.display(),
            out.display()
        );
    };
    if source == out {
        return Ok(());
    }
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| {
            anyhow::anyhow!(
                "the screenshot is at {}, and {} could not be created for it: {e}",
                source.display(),
                parent.display()
            )
        })?;
    }
    // Copy then remove, rather than `rename`: the two paths are routinely on
    // different filesystems (a box's /tmp, a scratch mount), and a `rename`
    // across one fails with `EXDEV` for a reason that has nothing to do with
    // what the caller asked.
    std::fs::copy(source, out).map_err(|e| {
        anyhow::anyhow!(
            "the screenshot was taken and is at {}, but it could not be copied to {}: {e}",
            source.display(),
            out.display()
        )
    })?;
    // Best effort: the file is where it was asked for, and a leftover in the
    // session's own directory is not worth failing a screenshot over. Kept for
    // a boxed session, where the file is the box's and h5i took a copy.
    if matches!(session.placement, bs::Placement::Host) {
        let _ = std::fs::remove_file(source);
    }
    Ok(())
}

/// Send one verb to a session and print what came back.
fn verb(
    root: &Path,
    selector: Option<&str>,
    argv: Vec<String>,
    mutating: bool,
    json: bool,
) -> anyhow::Result<()> {
    verb_then(root, selector, argv, mutating, json, |_, _| Ok(()))
}

/// The same, with something to do to the answer before it is printed.
///
/// One caller: `screenshot --out`, which has to move a file the confined engine
/// wrote and then say where it ended up. It runs only on an answer that was not
/// a refusal, and it runs *before* the printing, because an answer naming a
/// path the file is no longer at would be worse than no answer.
/// Run one step of a sequence, and hand back the answer.
///
/// Typed rather than argv, because a sequence step and a typed verb have to end
/// at the same place: this assembles the same command line `BrowserCommands::
/// Resend` does, so the control lock, the receipts and the policy see a
/// sequence exactly as they see somebody typing the steps one at a time.
pub(crate) fn resend_step(
    root: &Path,
    selector: Option<&str>,
    step: &super::websec::Sending<'_>,
) -> anyhow::Result<Value> {
    let mut argv = vec!["resend".to_string()];
    match step.as_session {
        None => {
            argv.push("--from".into());
            argv.push(step.from.to_string());
        }
        Some(_) => {
            let (request, _) =
                super::websec::carry(root, selector, step.from, step.keep_credentials)?;
            argv.push("--request".into());
            argv.push(serde_json::to_string(&request)?);
        }
    }
    for spec in step.set {
        argv.push("--set".into());
        argv.push(spec.clone());
    }
    for spec in step.unset {
        argv.push("--unset".into());
        argv.push(spec.clone());
    }
    if step.create {
        argv.push("--create".into());
    }
    // The session the request becomes part of, which is the other one when
    // there is one.
    let target = step.as_session.or(selector);
    ask_session(root, target, argv, true)
}

/// Send a verb and hand back the answer, without printing it.
///
/// The half of [`verb_then`] a caller that is *composing* verbs needs: a
/// sequence runs several and reads each answer to decide the next, so a
/// function that prints and returns nothing is the wrong shape for it. Same
/// resolution, same control-lock check, same scrub, because a composed run must
/// not be able to reach a session by a path a typed verb could not.
pub(crate) fn ask_session(
    root: &Path,
    selector: Option<&str>,
    argv: Vec<String>,
    mutating: bool,
) -> anyhow::Result<Value> {
    let session = match bs::resolve(root, selector) {
        Ok(session) => session,
        Err(gone) => {
            eprintln!("{}", gone);
            std::process::exit(bs::EXIT_SESSION_GONE);
        }
    };
    let dir = bs::dir(root, &session.id);
    if let Some(explanation) = h5i_core::control::check(&dir, mutating).explain() {
        anyhow::bail!("{explanation}");
    }
    let mut answer = deliver(&session, &dir, argv)?;
    bs::scrub(&mut answer);
    Ok(answer)
}

fn verb_then(
    root: &Path,
    selector: Option<&str>,
    argv: Vec<String>,
    mutating: bool,
    json: bool,
    after: impl FnOnce(&mut Value, bool) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let session = match bs::resolve(root, selector) {
        Ok(session) => session,
        Err(gone) => {
            eprintln!("{}", gone);
            std::process::exit(bs::EXIT_SESSION_GONE);
        }
    };
    let dir = bs::dir(root, &session.id);

    if let Some(explanation) = h5i_core::control::check(&dir, mutating).explain() {
        anyhow::bail!("{explanation}");
    }

    let is_snapshot = argv.first().map(String::as_str) == Some("snapshot");
    let clock = std::time::Instant::now();
    let mut answer = deliver(&session, &dir, argv)?;
    let t_deliver = clock.elapsed();

    // A completed snapshot is what clears the stale-ref flag a human takeover
    // set. It has to happen here, after the answer came back, because the flag
    // means "the agent has not seen the page since it moved" and only a
    // delivered reading changes that. Clearing it on request rather than on
    // answer would clear it for a snapshot that failed.
    if is_snapshot && answer.get("ok").and_then(Value::as_bool) != Some(false) {
        let _ = h5i_core::control::snapshotted(&dir);
    }
    bs::scrub(&mut answer);
    let t_scrub = clock.elapsed();
    // The client half of `H5I_BROWSER_TIMING`, whose server half puts a
    // `timing_ms` object in the reply. Together they say which side of the
    // socket a verb spent its time on, which is a question that has been
    // answered wrongly by inspection several times: `scrub` walks every string
    // in the reply and allocates one per value, which reads like the expensive
    // thing and measures at a third of a millisecond.
    if std::env::var_os("H5I_BROWSER_TIMING").is_some() {
        eprintln!(
            "client timing: deliver {:.2} ms (includes the session's own work), \
             scrub {:.2} ms",
            t_deliver.as_secs_f64() * 1000.0,
            (t_scrub - t_deliver).as_secs_f64() * 1000.0,
        );
    }

    // A refusal is an answer, and `--json` promised the answer, so it is
    // printed either way. What must not happen is printing it and exiting 0: a
    // script that checks the status code would read "denied by policy" as
    // success, which is the failure this whole design is arranged against.
    let refused = answer.get("ok").and_then(Value::as_bool) == Some(false);
    // The hook runs either way and is told which it is. Skipping it on a
    // refusal is right for one that moves a file and wrong for one that only
    // annotates: a burst stopped halfway came back with samples and no summary.
    after(&mut answer, refused)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&answer)?);
    } else if refused {
        anyhow::bail!("{}", refusal(&answer));
    } else {
        print_answer(&answer);
    }
    if refused {
        std::process::exit(1);
    }
    Ok(())
}

/// `transcript --via <helper>`: the lane that is not the engine.
///
/// Kept apart from [`verb`] on purpose. `verb` carries a request to the engine
/// and everything it does (the control lock, the stale-ref clearing, the
/// refusal-is-still-an-answer exit code) is about that conversation. This
/// talks to a different program entirely, and folding it into the same function
/// would be the first step toward the two being hard to tell apart, which is
/// the one property this lane must never lose.
#[cfg(feature = "ytdlp")]
#[allow(clippy::too_many_arguments)]
fn via_helper(
    root: &Path,
    selector: Option<&str>,
    helper: &str,
    url: Option<String>,
    lang: Option<String>,
    max_bytes: Option<usize>,
    json: bool,
) -> anyhow::Result<()> {
    use crate::cli::helper;

    // Named rather than matched loosely: `--via ytdlp` and `--via youtube-dl`
    // are both somebody expecting a lane that is not there, and running the one
    // that is would be answering a question nobody asked.
    if helper != helper::NAME {
        anyhow::bail!(
            "`--via {helper}` names no helper this build has. The only one is `--via {}`.",
            helper::NAME
        );
    }

    // A session when there is one, and none when there is not.
    //
    // Only when the caller named neither a session nor a URL is a missing
    // session an error. `--url` names the media and this lane renders no page,
    // so a session would contribute nothing but a placement; asking for one
    // anyway is what made `h5i browser transcript --via yt-dlp --url …` answer
    // with the closing note of an unrelated session. A `--session` that names
    // something gone is still an error, because running somewhere else would
    // move the lane to a boundary the caller did not choose.
    let session = match bs::resolve(root, selector) {
        Ok(session) => Some(session),
        Err(gone) if selector.is_some() || url.is_none() => {
            eprintln!("{gone}");
            std::process::exit(bs::EXIT_SESSION_GONE);
        }
        Err(_) => None,
    };

    // Not a mutation: nothing here touches the page. The lock is still asked,
    // because a human at the controls of a session is a human whose box should
    // not have a second program started against it behind their back. A run
    // with no session steers nothing and asks nobody.
    if let Some(session) = &session
        && let Some(explanation) =
            h5i_core::control::check(&bs::dir(root, &session.id), false).explain()
    {
        anyhow::bail!("{explanation}");
    }

    // The page the session is actually on, when the caller did not name a URL.
    // Asked rather than read off the record: `session.url` is what `open` was
    // *told*, and a redirect or a click has moved it since.
    let target = match (&url, &session) {
        (Some(named), _) => named.clone(),
        // Unreachable by the arms above: a caller with no URL and no session
        // has already been sent away with the session's own explanation.
        (None, None) => anyhow::bail!(
            "there is no session to read a URL from and none was named. \
             Name one with `--url`."
        ),
        (None, Some(session)) => {
            let dir = bs::dir(root, &session.id);
            let status = deliver(session, &dir, vec!["status".to_string()])?;
            status
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "this session did not say what page it is on, so there is no URL to hand \
                         the helper. Name one with `--url`."
                    )
                })?
        }
    };

    let site = match &session {
        Some(session) => helper::Site::Session(session),
        None => helper::Site::sessionless(),
    };

    let outcome = helper::transcript(
        root,
        &site,
        &target,
        lang.as_deref(),
        max_bytes.unwrap_or(h5i_browser::transcript::DEFAULT_MAX_BYTES),
    )?;

    if json {
        println!("{}", serde_json::to_string_pretty(&outcome.reply)?);
    } else {
        print_answer(&outcome.reply);
    }

    // Judged by what arrived, not by the helper's exit code.
    if !outcome.answered && outcome.status.is_some_and(|code| code != 0) {
        std::process::exit(1);
    }
    Ok(())
}

/// The same entry point in a build with no helper lane compiled in.
///
/// An error rather than a missing flag, because the flag is in the help text
/// either way and a caller who typed it deserves to be told which of the two
/// things is missing. The build, or the program.
#[cfg(not(feature = "ytdlp"))]
#[allow(clippy::too_many_arguments)]
fn via_helper(
    _root: &Path,
    _selector: Option<&str>,
    helper: &str,
    _url: Option<String>,
    _lang: Option<String>,
    _max_bytes: Option<usize>,
    _json: bool,
) -> anyhow::Result<()> {
    anyhow::bail!(
        "this build has no helper lane, so `--via {helper}` cannot run: it was compiled \
         without the `ytdlp` feature, and it has no path to exec a helper at all. Drop \
         `--via` to read the captions the page itself declares."
    )
}

/// What a session said when it refused, or a stand-in when it said nothing.
fn refusal(answer: &Value) -> String {
    answer
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("the session refused, without saying why")
        .to_string()
}

/// `h5i browser identity ...`, answered by the engine.
#[cfg(feature = "identity")]
fn identity(args: Vec<String>) -> anyhow::Result<()> {
    let status = Command::new(engine_binary()?)
        .arg(ENGINE_SUBCOMMAND)
        .arg("identity")
        .args(&args)
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        // The engine has already said why, on its own stderr. Adding a sentence
        // here would be this command's opinion about an answer it did not form.
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Carry a verb to wherever the session actually is.
fn deliver(session: &bs::Session, dir: &Path, argv: Vec<String>) -> anyhow::Result<Value> {
    let output = match &session.placement {
        bs::Placement::Host => {
            // Whatever the start recorded, not a second derivation of it: two
            // places that have to agree about an address are two places that
            // can stop agreeing.
            let control = session.control.file.clone().unwrap_or(dir.join(bs::CONTROL_FILE));
            let mut command = Command::new(engine_binary()?);
            command
                .arg(ENGINE_SUBCOMMAND)
                .arg("session")
                .args(&argv)
                .arg(session.control.channel.flag())
                .arg(&control)
                .arg("--json");
            command.output()?
        }
        bs::Placement::Box { name } => {
            // The control file as the box sees it, straight from the record
            // the start wrote. Deriving it here instead would be a second place
            // that has to agree with the first.
            let control = session
                .control
                .file
                .clone()
                .ok_or_else(|| anyhow::anyhow!("this session's record names no control socket"))?;
            let mut command = Command::new(std::env::current_exe()?);
            command
                .arg("box")
                .arg("run")
                .arg("--json")
                .arg(name)
                .arg("--")
                .arg(h5i_in_box())
                .arg(ENGINE_SUBCOMMAND)
                .arg("session")
                .args(&argv)
                .arg(session.control.channel.flag())
                .arg(&control)
                .arg("--json");
            command.output()?
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() && stdout.trim().is_empty() {
        let stderr = bs::scrub_text(&String::from_utf8_lossy(&output.stderr));
        anyhow::bail!("the session refused the verb: {}", stderr.trim());
    }

    let parsed: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        anyhow::anyhow!(
            "could not read the session's answer ({e}): {}",
            bs::scrub_text(stdout.trim())
        )
    })?;

    // A boxed verb comes back wrapped in `h5i box run --json`'s envelope, whose
    // `output` field is the engine's own answer as the receipt recorded it.
    // Unwrapping here rather than at the call site keeps every verb's answer the
    // same shape whatever the placement, which is the promise `--in` makes.
    if session.placement.box_name().is_some() {
        let inner = parsed
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        return serde_json::from_str(&inner).map_err(|e| {
            anyhow::anyhow!(
                "the box ran the verb but its answer was unreadable ({e}): {}",
                bs::scrub_text(&inner)
            )
        });
    }
    Ok(parsed)
}

/// One page, or a batch, with nothing left behind.
#[allow(clippy::too_many_arguments)]
fn read(
    targets: Vec<String>,
    in_box: Option<String>,
    allow: Vec<String>,
    text: bool,
    script: bool,
    no_sandbox: bool,
    #[cfg(feature = "identity")] identity: String,
    json: bool,
) -> anyhow::Result<()> {
    let mut engine_args: Vec<String> = vec![ENGINE_SUBCOMMAND.into(), "open".into()];
    engine_args.extend(targets.iter().cloned());
    for origin in origins_of(&targets).into_iter().chain(allow) {
        engine_args.push("--allow".into());
        engine_args.push(origin);
    }
    if text {
        engine_args.push("--text".into());
    }
    if script {
        engine_args.push("--script".into());
    }
    if json {
        engine_args.push("--json".into());
    }
    // The same boundary `open` refuses at, and it belongs here too: this lane
    // builds its own argv, so the fix that only touched `open` left a path
    // going into a box from here.
    #[cfg(feature = "identity")]
    refuse_a_file_identity_in_a_box(in_box.as_deref(), &identity)?;
    // Only when it differs from the default. See `net_args`: an in-box engine
    // may be older than this one, and a flag it has never heard of is a usage
    // error rather than a read.
    #[cfg(feature = "identity")]
    if identity != DEFAULT_IDENTITY {
        engine_args.push("--identity".into());
        engine_args.push(identity);
    }

    if let Some(name) = &in_box {
        return read_in_box(name, &engine_args, json);
    }
    read_here(&engine_args, no_sandbox, json)
}

/// The origins the caller named, by naming the URLs.
///
/// The targets are handed back verbatim: the engine's `--allow` already accepts
/// a full URL and normalizes it with the same code that later checks a request,
/// so there is nothing to parse here and no second notion of "origin" to drift
/// from the first. A local file path normalizes to no entry at all, which is
/// right. A page opened from disk needs no grant.
///
/// Deduplicated so a batch produces a stable argv.
fn origins_of(targets: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for t in targets {
        if !seen.contains(t) {
            seen.push(t.clone());
        }
    }
    seen
}

/// Carry the read into a box, through the same `box run` a person would type.
///
/// Nothing here re-implements the tier: `box run` already resolves the box's
/// pinned policy, dispatches to the backend it names, and writes a receipt. A
/// read is run-to-completion, which is why it fits a tier a session does not.
fn read_in_box(name: &str, engine_args: &[String], json: bool) -> anyhow::Result<()> {
    // `box run --json` rather than its streamed output: the envelope carries the
    // policy digest that was enforced, which is the reason to read inside a
    // box at all and the thing a hand-built policy could never hand back. It
    // also keeps `box run`'s own framing out of the middle of a page.
    let out = Command::new(std::env::current_exe()?)
        .arg("box")
        .arg("run")
        .arg("--json")
        .arg(name)
        .arg("--")
        .arg(h5i_in_box())
        .args(engine_args)
        .output()?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let envelope: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        anyhow::anyhow!(
            "could not read the box's answer ({e}): {}",
            bs::scrub_text(&String::from_utf8_lossy(&out.stderr))
        )
    })?;

    let digest = envelope["policy_digest"].as_str().unwrap_or("unknown");
    let exit = envelope["exit_code"].as_i64().unwrap_or(1);
    let body = envelope["output"].as_str().unwrap_or_default();
    let confinement = serde_json::json!({"kind": "box", "box": name, "policy_digest": digest});

    // A receipt has one output field, so `box run` folded the two streams into
    // it behind a banner. Unfold them: the page belongs on stdout and the
    // request log on stderr, the same as a read that ran here, and a caller
    // parsing `--json` should not have to find the JSON inside a transcript.
    let (out, err) = match body.split_once(h5i_core::env::STDERR_BANNER) {
        Some((out, err)) => (out, err),
        None => (body, ""),
    };

    if !json {
        println!("  confined : box {name}, policy {digest}");
        println!();
    }
    relay(out.as_bytes(), err.as_bytes(), json, confinement);
    if exit != 0 {
        std::process::exit(exit as i32);
    }
    Ok(())
}

/// Read on this machine, under the session sandbox.
fn read_here(engine_args: &[String], no_sandbox: bool, json: bool) -> anyhow::Result<()> {
    let engine = engine_binary()?;
    // A scratch directory is `$WORK`: a read that left state behind would be a
    // session with the word "read" on it.
    let scratch = tempfile::tempdir()?;

    let wants = h5i_core::browser_sandbox::Wants {
        session_dir: scratch.path(),
        reads: std::slice::from_ref(&engine),
        secrets: &[],
        // A backstop, not a policy: the engine's per-navigation budgets are what
        // actually bound a fetch. Leaving this at a session's `0` would hand
        // `sandbox::run` a deadline that has already passed.
        wall_secs: READ_WALL_SECS,
    };
    let confined = if no_sandbox {
        None
    } else {
        h5i_core::browser_sandbox::resolve_for(&wants)?
    };

    let out = match &confined {
        Some(c) => {
            let mut argv = vec![engine.display().to_string()];
            argv.extend(engine_args.iter().cloned());
            for dir in &c.fonts {
                argv.push("--font-dir".into());
                argv.push(dir.display().to_string());
            }
            let outcome = h5i_core::sandbox::run(&c.policy, scratch.path(), &argv)?;
            (outcome.stdout, outcome.stderr, outcome.exit_code)
        }
        None => {
            let out = Command::new(&engine).args(engine_args).output()?;
            (out.stdout, out.stderr, out.status.code())
        }
    };

    let confinement = match &confined {
        Some(_) => serde_json::json!({"kind": "process"}),
        None => serde_json::json!({"kind": "none"}),
    };
    if !json {
        println!(
            "  confined : {}",
            match &confined {
                Some(_) => "process (files and environment; the origin allowlist is the engine's)",
                None => "nothing — `--in <box>` is where a tier-enforced allowlist comes from",
            }
        );
        println!();
    }
    relay(&out.0, &out.1, json, confinement);
    if out.2 != Some(0) {
        std::process::exit(out.2.unwrap_or(1));
    }
    Ok(())
}

/// Print what the engine said, scrubbed. Everything here was composed by a page.
/// Print what the engine produced, with what held it attached.
///
/// `confinement` is carried *into* the JSON rather than printed beside it: a
/// machine reading a read has no other way to learn what was holding the
/// engine, and a request log without the thing that was containing the requests
/// is half a receipt. In text mode it is a line above the page, for the reader.
fn relay(stdout: &[u8], stderr: &[u8], json: bool, confinement: Value) {
    let mut body: Value = match serde_json::from_slice(stdout) {
        Ok(v) => v,
        Err(_) => Value::String(String::from_utf8_lossy(stdout).to_string()),
    };
    bs::scrub(&mut body);
    if json {
        // An object gains a field; anything else (a `--text` read, or a box's
        // merged output) becomes one, so the key is in the same place either way.
        let answer = match body {
            Value::Object(mut map) => {
                map.insert("confinement".into(), confinement);
                Value::Object(map)
            }
            other => serde_json::json!({"confinement": confinement, "read": other}),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&answer).unwrap_or_else(|_| answer.to_string())
        );
    } else {
        match body.as_str() {
            Some(t) => println!("{t}"),
            None => println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string())
            ),
        }
    }
    let err = bs::scrub_text(&String::from_utf8_lossy(stderr));
    if !err.trim().is_empty() {
        eprintln!("{}", err.trim());
    }
}

fn list(root: &Path, all: bool, json: bool) -> anyhow::Result<()> {
    let sessions: Vec<bs::Session> = bs::list(root)?
        .into_iter()
        .filter(|s| all || s.state.is_live())
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }
    if sessions.is_empty() {
        println!(
            "  no browser sessions{}. Open one with `h5i browser open <url>`.",
            if all { "" } else { " running" }
        );
        return Ok(());
    }
    let default = bs::read_default(root);
    println!(
        "  {:<14}  {:<10}  {:<8}  {:<9}  {:<15}  URL",
        "SESSION", "ID", "STATE", "PLACED", "LANE"
    );
    for session in sessions {
        let state = match session.state {
            bs::State::Live => style(session.state.as_str()).green(),
            bs::State::Closed => style(session.state.as_str()).dim(),
            _ => style(session.state.as_str()).yellow(),
        };
        // The default is marked rather than hidden: an agent reading this
        // needs to know which row a bare verb will land on.
        let is_default = default.as_deref() == Some(session.id.as_str());
        let shown = match (&session.name, is_default) {
            (Some(name), true) => format!("{name} *"),
            (Some(name), false) => name.clone(),
            (None, true) => "(default) *".to_string(),
            (None, false) => "-".to_string(),
        };
        println!(
            "  {:<14}  {:<10}  {:<8}  {:<9}  {:<15}  {}",
            style(shown).cyan(),
            style(&session.id).dim(),
            state,
            session.placement.as_str(),
            session.lane.as_str(),
            session.url
        );
    }
    Ok(())
}

fn status(root: &Path, selector: Option<&str>, json: bool) -> anyhow::Result<()> {
    // Not `resolve`: a status on a session that has ended is exactly the
    // question worth answering, so this reads the record rather than refusing.
    let mut session = resolve_for_reading(root, selector)?;
    let id = &session.id.clone();
    // Reading status is the moment to notice a death and write it down.
    if session.state.is_live() && !session.probe() {
        bs::end(
            root,
            &mut session,
            bs::State::Died,
            "the engine stopped answering",
        );
    }
    // What the engine knows and the record cannot. `errors` is the only signal
    // that the stored evidence has a hole in it. Best-effort and live-only,
    // because `status` answers about ended sessions too.
    let health = session
        .state
        .is_live()
        .then(|| deliver(&session, &bs::dir(root, id), vec!["status".to_string()]).ok())
        .flatten()
        .and_then(|reply| reply.get("capture").cloned())
        .filter(|capture| !capture.is_null());

    if json {
        let control = h5i_core::control::read(&bs::dir(root, id));
        let mut value = serde_json::to_value(&session)?;
        value["control_lock"] = serde_json::to_value(&control)?;
        if let Some(health) = health {
            value["capture"] = health;
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    match &session.name {
        Some(name) => println!(
            "  session  : {} ({})",
            style(name).cyan(),
            style(&session.id).dim()
        ),
        None => println!("  session  : {}", style(&session.id).cyan()),
    }
    print_summary(&session);
    let lock = h5i_core::control::read(&bs::dir(root, id));
    println!(
        "  control  : {} (since {})",
        style(lock.holder.as_str()).cyan(),
        lock.since
    );
    if lock.needs_resnapshot {
        println!(
            "  {}    the agent's @refs are stale — re-snapshot before acting \
             (`h5i browser snapshot`)",
            style("stale").yellow()
        );
    }
    if let Some(health) = &health {
        let number = |key: &str| health.get(key).and_then(Value::as_u64).unwrap_or(0);
        let errors = number("errors");
        let line = format!(
            "{} messages, {} bytes",
            number("messages"),
            number("bytes")
        );
        if errors > 0 {
            println!(
                "  captured : {line}, and {} it could not write — the store has a hole in it",
                style(errors).red()
            );
        } else {
            println!("  captured : {line}");
        }
    }
    if let Some(reason) = &session.end_reason {
        println!("  ended    : {} — {}", session.state.as_str(), reason);
    }
    Ok(())
}

fn print_summary(session: &bs::Session) {
    println!("  url      : {}", session.url);
    // One sentence, from the record, so the summary and the audit cannot say
    // different things about the same session.
    let placed = session.where_it_ran();
    match (&session.placement, session.confinement.is_confined()) {
        (bs::Placement::Box { .. }, _) => println!("  placed   : {}", style(&placed).cyan()),
        (_, true) => println!("  placed   : {}", style(&placed).green()),
        (_, false) => println!("  placed   : {}", style(&placed).yellow()),
    }
    // The honest half of the product, printed every time rather than claimed
    // once in a README: what this session's network record actually is.
    let lane = match session.lane {
        bs::Lane::EngineClaimed => style("engine-claimed").yellow(),
        bs::Lane::HostObserved => style("host-observed").green(),
    };
    println!(
        "  requests : {} ({})",
        lane,
        match session.lane {
            bs::Lane::EngineClaimed =>
                "fail-closed, and the engine's own account of what it fetched",
            bs::Lane::HostObserved => "also seen at the box's boundary, outside the engine",
        }
    );
    println!("  policy   : {}", session.policy_digest);
    // Named, not left to the digest: a digest says two sessions differ, and
    // this is the difference that changes what a finding means.
    if session.permissive_cors {
        println!(
            "  cors     : {} — a page here may send this session's credentials \
             cross-origin with `no-cors`, as a browser does",
            style("permissive (--permissive-cors)").yellow()
        );
    }
    if let Some(from) = &session.restored_from {
        println!("  storage  : inherited from {from}");
    }
    if let Some(expires) = &session.expires_at {
        println!("  expires  : {expires}");
    }
}

fn close(
    root: &Path,
    selector: Option<&str>,
    all: bool,
    capture_drop: bool,
    json: bool,
) -> anyhow::Result<()> {
    let targets: Vec<bs::Session> = if all {
        bs::list(root)?
            .into_iter()
            .filter(|s| s.state.is_live())
            .collect()
    } else {
        match bs::resolve(root, selector) {
            Ok(session) => vec![session],
            // Closing something already closed is the state `close` wanted, so
            // it reports rather than fails. Only "no such session" is an error.
            Err(bs::SessionGone::Ended { id, .. }) => vec![bs::read(root, &id)?],
            Err(gone) => anyhow::bail!("{gone}"),
        }
    };

    if targets.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("  no browser session is open.");
        }
        return Ok(());
    }

    let mut closed = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for mut session in targets {
        if session.state.is_live() {
            stop_engine(&session)?;
            bs::end(root, &mut session, bs::State::Closed, "closed by the user");
        }
        // After the engine has stopped, so nothing is still writing there.
        if capture_drop {
            let store = bs::dir(root, &session.id).join(bs::MESSAGES_DIR);
            match std::fs::remove_dir_all(&store) {
                Ok(()) => dropped.push(session.id.clone()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => anyhow::bail!("{} could not be removed: {e}", store.display()),
            }
        }
        if !json {
            println!(
                "{} browser session {} {}. Its record stays at {}.",
                SUCCESS,
                label(&session),
                session.state.describe(),
                bs::dir(root, &session.id).display()
            );
            if dropped.last() == Some(&session.id) {
                println!("  dropped  : its captured messages. The request log stays.");
            }
        }
        closed.push(session);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&closed)?);
    }
    Ok(())
}

/// The whole record of one session, merged and ordered.
///
/// Reads the record rather than requiring a live session: the session a
/// reviewer most wants to audit is usually the one that has already ended.
/// The session a *reader* means, live or not.
///
/// `resolve` answers for the verbs that act, so a name only finds a live
/// session. The readers — `status`, `audit`, and the websec surface — work on a
/// record and a store that outlive the engine, and asking by name answered "no
/// such session" for evidence `close` had just promised was still there.
pub(crate) fn resolve_for_reading(
    root: &Path,
    selector: Option<&str>,
) -> anyhow::Result<bs::Session> {
    match bs::resolve(root, selector) {
        Ok(session) => Ok(session),
        Err(bs::SessionGone::Ended { id, .. }) => Ok(bs::read(root, &id)?),
        Err(gone) => match selector.and_then(|name| bs::find_ended_by_name(root, name)) {
            Some(ended) => Ok(ended),
            None => anyhow::bail!("{gone}"),
        },
    }
}

fn audit(root: &Path, selector: Option<&str>, json: bool) -> anyhow::Result<()> {
    let session = resolve_for_reading(root, selector)?;
    let audit = bs::audit(root, &session);

    if json {
        println!("{}", serde_json::to_string_pretty(&audit)?);
        return Ok(());
    }

    println!("  session  : {}", label(&session));
    print_summary(&session);

    // What could and could not be read, before the rows. A reader has to know
    // whether an empty timeline means a quiet session or a log h5i cannot see.
    let src = &audit.sources;
    // Named only when there *is* a helper log.
    let helpers = match src.helpers {
        bs::Availability::Empty => String::new(),
        other => format!(" · helpers {}", availability(other)),
    };
    // The same rule for the message store: silent on the ordinary session,
    // which was not opened with `--capture` and has nothing to report.
    let messages = match src.messages {
        bs::Availability::Empty => String::new(),
        other => format!(" · messages {}", availability(other)),
    };
    println!(
        "  sources  : actions {} · requests {} · control {}{helpers}{messages}",
        availability(src.actions),
        availability(src.requests),
        availability(src.control)
    );
    if audit.dropped > 0 {
        println!(
            "  {}  {} older rows were dropped by the cap",
            style("capped").yellow(),
            audit.dropped
        );
    }
    // Said once rather than on every row: the engine's stamps are its own
    // claim about its own clock, which nothing outside the box can check.
    println!(
        "  {}     engine rows are ordered by the engine's own clock, which h5i cannot verify",
        style("note").dim()
    );
    println!();

    for event in &audit.events {
        // The lane on every row, because the two are not the same kind of
        // claim: `host` is what h5i saw from outside, `engine` is the engine's
        // own account of itself.
        let lane = match event.lane {
            h5i_core::browser_events::Lane::HostObserved => style("host  ").green(),
            _ => style("engine").yellow(),
        };
        println!("  {lane}  {}", render_event(&event.kind));
    }
    if audit.events.is_empty() {
        println!("  nothing recorded for this session yet.");
    }
    Ok(())
}

/// The helper runs that belong to no session.
///
/// A separate command rather than a section of the timeline above, because
/// these runs were not part of any session and putting them in one session's
/// audit would be a claim about that session that is not true. They are the
/// same rows in the same shape, read from
/// [`bs::SESSIONLESS_HELPERS_FILE`](h5i_core::browser_session::SESSIONLESS_HELPERS_FILE),
/// and every one is host-observed: h5i built the argv and ran the program, so
/// the row is an observation rather than the helper's account of itself.
fn sessionless_audit(root: &Path, json: bool) -> anyhow::Result<()> {
    let rows = bs::sessionless_helpers(root)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("  no helper has run outside a session on this machine.");
        return Ok(());
    }

    println!("  runs     : {} with no session", rows.len());
    println!(
        "  {}     h5i ran these itself, so every row is an observation.",
        style("note").dim()
    );
    println!(
        "           None of their fetches are in `h5i browser requests`: \
         they were not the engine's."
    );
    println!();
    for row in &rows {
        let outcome = match row.status {
            Some(0) | None => String::new(),
            Some(code) => format!("  (exit {code})"),
        };
        println!(
            "  {}  {}  helper {} {}{outcome}",
            style("host  ").green(),
            row.at,
            row.name,
            row.argv.join(" ")
        );
        if let Some(note) = &row.note {
            println!("          {}", style(bs::scrub_text(note)).dim());
        }
    }
    Ok(())
}

fn availability(a: bs::Availability) -> console::StyledObject<&'static str> {
    match a {
        bs::Availability::Read => style(a.as_str()).green(),
        bs::Availability::Empty => style(a.as_str()).dim(),
        // The one that must stand out: nothing can be concluded from the
        // silence of a log h5i could not read.
        bs::Availability::Unavailable => style(a.as_str()).red(),
        // Nor from the end of one that was cut short.
        bs::Availability::Partial => style(a.as_str()).yellow(),
    }
}

/// One audit row, as a line.
fn render_event(kind: &h5i_core::browser_events::EventKind) -> String {
    use h5i_core::browser_events::EventKind as K;
    match kind {
        K::Lifecycle { state, reason } => format!(
            "session {state}{}",
            reason
                .as_deref()
                .map(|r| format!("  ({r})"))
                .unwrap_or_default()
        ),
        K::Control { holder, note } => format!(
            "control -> {holder}{}",
            note.as_deref()
                .map(|n| format!("  ({n})"))
                .unwrap_or_default()
        ),
        K::AgentAction { action, forwarded } => {
            format!("{} {action}", if *forwarded { "verb  " } else { "verb !" })
        }
        K::Request {
            seq,
            method,
            url,
            allowed,
            denied_reason,
            ..
        } => {
            if *allowed {
                format!("#{seq} {method} {url}")
            } else {
                format!(
                    "#{seq} DENIED {method} {url}  ({})",
                    denied_reason.as_deref().unwrap_or("no reason recorded")
                )
            }
        }
        K::Response {
            seq,
            status,
            bytes,
            error,
            ..
        } => match (error, status) {
            (Some(error), _) => format!("#{seq} failed  ({error})"),
            (None, Some(status)) => format!(
                "#{seq} {status}{}",
                bytes.map(|b| format!("  {b} bytes")).unwrap_or_default()
            ),
            // A denied request never reaches the wire, so its outcome row has
            // no status at all. Saying so beats printing an empty line that
            // reads as a response nobody recorded.
            (None, None) => format!("#{seq} no response (refused before the wire)"),
        },
        K::Navigated { url } => format!("page {url}"),
        K::Console { level, text } => format!("console {} {text}", level.as_str()),
        K::PolicyVerdict { subject, reason } => format!("refused {subject}  ({reason})"),
        K::SessionReset { source } => format!("source restarted: {source}"),
        // The lane's boundary, drawn where a reader will see it. `helper` is
        // its own word rather than `verb` because the fetches it made are not
        // in the rows above or below it, and an auditor who reads this row as
        // one more engine action has been misled about the one thing this
        // timeline is for.
        K::Helper {
            name,
            argv,
            status,
            note,
        } => {
            let outcome = match status {
                Some(0) | None => String::new(),
                Some(code) => format!("  (exit {code})"),
            };
            format!(
                "helper {name} {}{outcome}{}",
                argv.join(" "),
                note.as_deref()
                    .map(|n| format!("  {n}"))
                    .unwrap_or_default()
            )
        }
    }
}

/// End the process behind a session, wherever it is.
///
/// The host path signals the engine directly. The boxed path goes through
/// `service_stop`, which is what ingests the engine's in-box log as a capture
/// and writes the stop into the box's event log, so closing a boxed session
/// leaves evidence in the box's own record, not only in the session's.
fn stop_engine(session: &bs::Session) -> anyhow::Result<()> {
    match &session.placement {
        bs::Placement::Host => {
            if let Some(pid) = session.control.pid {
                kill(pid);
            }
            Ok(())
        }
        bs::Placement::Box { name } => {
            let repo = super::discover_repo("h5i browser close")?;
            let h5i_root = h5i_core::storage::h5i_root_for_repo(&repo)?;
            let manifest = h5i_core::env::find(&h5i_root, name)?;
            match h5i_core::env::service_stop(&repo, &h5i_root, &manifest, BROWSER_SERVICE) {
                Ok(_) => Ok(()),
                // A service that is already gone is the state `close` wanted.
                Err(e) => {
                    eprintln!("  note     the box had no engine left to stop ({e})");
                    Ok(())
                }
            }
        }
    }
}

fn take(root: &Path, selector: Option<&str>) -> anyhow::Result<()> {
    let session = bs::resolve(root, selector).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dir = bs::dir(root, &session.id);
    let control = h5i_core::control::take(&dir)?;
    bs::journal_control(&dir, control.holder.as_str(), Some("taken by a human"));
    println!(
        "{} control taken by {} — the agent's automation is paused",
        SUCCESS,
        control.holder.as_str()
    );
    // Say which kind of pause this is, because the two are genuinely different
    // and only one of them is a boundary.
    match &session.placement {
        bs::Placement::Box { .. } => println!(
            "  {}  the session is in a box, so this is enforced: every verb is carried in \
             from here and none of them is now",
            style("enforced").green()
        ),
        bs::Placement::Host => println!(
            "  {} the session runs on this machine, so this pauses `h5i browser` and nothing \
             else: an agent that drives the engine binary directly is not stopped by it. \
             Place the session in a box (`--in`) to make the pause a boundary.",
            style("advisory").yellow()
        ),
    }
    Ok(())
}

fn release(root: &Path, selector: Option<&str>) -> anyhow::Result<()> {
    let session = bs::resolve(root, selector).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dir = bs::dir(root, &session.id);
    let control = h5i_core::control::release(&dir)?;
    bs::journal_control(
        &dir,
        control.holder.as_str(),
        Some("handed back; the agent must re-snapshot"),
    );
    println!(
        "{} control returned to {} — it must re-snapshot before acting",
        SUCCESS,
        control.holder.as_str()
    );
    Ok(())
}

/// Watch a session, in this terminal or in a browser.
///
/// Both viewers open from here, so which session, what it is called, and what
/// this machine can honestly say about what holds it are decided once.
fn view(
    root: &Path,
    selector: Option<&str>,
    web: bool,
    port: u16,
    fps: u32,
    assume_graphics: bool,
) -> anyhow::Result<()> {
    // `open_live` rather than `resolve`: a viewer on an ended session would sit
    // on a dead socket reporting nothing.
    let session = bs::open_live(root, &bs::resolve(root, selector).map_err(|e| anyhow::anyhow!("{e}"))?.id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let dir = bs::dir(root, &session.id);
    let stream_file = dir.join(bs::STREAM_FILE);

    // What this machine can say about what the page may reach, in the session
    // record's own words. A host session is engine-claimed however carefully its
    // engine is confined, and saying so beats a blank.
    let egress = match &session.placement {
        bs::Placement::Box { name } => format!("box:{name}"),
        bs::Placement::Host => session.lane.as_str().to_string(),
    };

    if !web {
        return h5i_core::termview::run(h5i_core::termview::Options {
            state_dir: dir,
            subject: session.name.clone().unwrap_or_else(|| session.id.clone()),
            policy_digest: session.policy_digest.clone(),
            attach: h5i_core::termview::Attach::Host { stream_file },
            command: "h5i browser view".into(),
            egress,
            // Named for the hint text a failure prints.
            engine: Some(session.engine.as_str().to_string()),
            max_fps: fps,
            assume_graphics,
        })
        .map_err(Into::into);
    }

    let stream_port = h5i_core::view::session_stream_port(&stream_file).ok_or_else(|| {
        anyhow::anyhow!(
            "this session is not serving a live view, so there is nothing to watch. \
             Only a resident session does: open one with `h5i browser open <url>` and try again."
        )
    })?;
    let forward = h5i_core::view::Forward::on(
        &dir,
        &session.id,
        &session.policy_digest,
        port,
        h5i_core::view::Route::Host { port: stream_port },
    )?;
    let holder = h5i_core::control::read(&dir).holder;
    println!("{} viewer for {}", SUCCESS, session.id);
    println!("   open     {}", forward.url()?);
    println!("   control  {holder:?} — `h5i browser take` to drive");
    println!("   stop     Ctrl-C");
    forward.serve()?;
    Ok(())
}

fn viewer_url(name: &str, port: u16) -> anyhow::Result<()> {
    let repo = super::discover_repo("h5i browser url")?;
    let h5i_root = h5i_core::storage::h5i_root_for_repo(&repo)?;
    let manifest = h5i_core::env::find(&h5i_root, name)?;
    let dir = h5i_core::env::env_dir(&h5i_root, &manifest.agent, &manifest.slug);
    let token = h5i_core::view::read_token(&dir).ok_or_else(|| {
        anyhow::anyhow!("this box has no viewer token — it predates the viewer. Create a new box.")
    })?;
    // Printed whether or not a forward is running: the URL is a property of the
    // box, and `h5i box view` is what makes it answer.
    println!("http://127.0.0.1:{port}/?token={token}");
    Ok(())
}

/// Render an engine answer for a person.
///
/// Known shapes get a plain rendering; anything else falls back to the JSON,
/// which is a shape an agent can still use. Nothing here interprets the values.
/// They came from a page, and [`bs::scrub`] has already run.
fn print_answer(answer: &Value) {
    let body = answer.get("data").unwrap_or(answer);
    for key in ["outline", "text", "markdown", "message"] {
        if let Some(text) = body.get(key).and_then(Value::as_str) {
            println!("{text}");
            return;
        }
    }
    if let Some(text) = body.as_str() {
        println!("{text}");
        return;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(body).unwrap_or_else(|_| body.to_string())
    );
}

/// A credential the session was told to carry and could not be given.
///
/// The broker's own sentence already names the grant and the variable, which is
/// the part someone needs; this adds what to do about it and drops the error
/// enum's prefix, because "Metadata error" is not what went wrong.
fn unresolved_credential(error: h5i_core::error::H5iError) -> anyhow::Error {
    let why = match &error {
        h5i_core::error::H5iError::Metadata(text) => text.clone(),
        other => other.to_string(),
    };
    anyhow::anyhow!(
        "{why}\n\n  \
         `--secret` names a credential h5i resolves from the environment you start the \
         session in, and a session is not started without one it was told to carry. Set the \
         variable there, or drop the flag."
    )
}

/// The `H5I_SECRET_*` variables `--secret` named, in the one spelling that works.
fn secret_variables(named: &[String]) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for raw in named {
        let name = raw.trim();
        let full = if name.starts_with(h5i_browser::secrets::PREFIX) {
            name.to_string()
        } else {
            format!("{}{name}", h5i_browser::secrets::PREFIX)
        };
        let suffix = &full[h5i_browser::secrets::PREFIX.len()..];
        if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            anyhow::bail!(
                "`--secret {raw}` is not the name of an environment variable. Name the \
                 credential as `ACME_PASS` or `H5I_SECRET_ACME_PASS`, and set \
                 `H5I_SECRET_ACME_PASS` in the environment you start the session from."
            );
        }
        if !out.contains(&full) {
            out.push(full);
        }
    }
    // Sorted, because the list is serialized into the profile and the profile
    // is digested: two callers naming the same two credentials in a different
    // order must not produce two policies.
    out.sort();
    Ok(out)
}

/// Where a file-injected secret would go, which is deliberately *not* under
/// the session directory.
///
/// `$WORK` is the one place the engine may write, so a credential written there
/// would be a credential the confined engine could read back and rewrite. The
/// path is unreachable today, `broker` refuses `inject = file` off the
/// workspace tier, and it is chosen as though it were not.
fn secret_dir(root: &Path) -> PathBuf {
    root.join("secrets")
}

/// Forward `H5I_BROWSER_NO_SPLIT` into a confined session, if it is set.
///
/// The engine runs as two processes (a broker that decides and records, and a
/// renderer that parses the page) and this is the switch that runs it as one.
/// Empty in the ordinary case, which is the case that matters: the sandbox
/// clears the environment, and everything that is not deliberately passed stays
/// out.
fn single_process_switch() -> Vec<(String, String)> {
    match std::env::var(h5i_browser::ipc::NO_SPLIT_VAR) {
        Ok(value) => vec![(h5i_browser::ipc::NO_SPLIT_VAR.to_string(), value)],
        Err(_) => Vec::new(),
    }
}

/// The engine is this binary. There is nothing to find.
fn engine_binary() -> anyhow::Result<PathBuf> {
    std::env::current_exe().map_err(|e| {
        anyhow::anyhow!("could not find this executable, so the browser engine cannot start: {e}")
    })
}

/// Put the child in its own session, so closing the terminal that started it
/// does not take the browser with it.
#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach(_command: &mut Command) {}

/// Has this child exited? `Some(reason)` when it has.
///
/// `waitpid(WNOHANG)` rather than `kill(pid, 0)`. The confined spawn hands back
/// a pid and drops the `Child`, so an engine that died is an unreaped zombie
/// and a signal probe calls it alive, which is what made a start wait its whole
/// timeout on an engine that exited immediately.
#[cfg(unix)]
fn reap(pid: u32) -> Option<String> {
    let mut status: libc::c_int = 0;
    // SAFETY: `pid` is a child of this process and `status` is a live local.
    match unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) } {
        0 => None,
        -1 => Some("lost track of the browser engine".to_string()),
        _ => Some(format!(
            "the browser engine exited (status {})",
            libc::WEXITSTATUS(status)
        )),
    }
}

#[cfg(not(unix))]
fn reap(_pid: u32) -> Option<String> {
    None
}

#[cfg(unix)]
fn kill(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn kill(_pid: u32) {}
