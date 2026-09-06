//! Loading a page and getting something back out of it.
//!
//! One shot, by design: fetch, parse, resolve, then answer questions about the
//! result (a snapshot, a screenshot, the text). There is no event loop and no
//! session here, because Tier 1 has no script to run and nothing that changes
//! the document after load. When Tier 2 adds a live view and Tier 3 adds
//! script, the loop belongs around this, not inside it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use anyrender::ImageRenderer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::node::SpecialElementData;
use blitz_dom::{BaseDocument, DocumentConfig, local_name};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use h5i_error::H5iError;
use url::Url;

use crate::fonts::FontSetup;
use crate::broker::Broker;
use crate::net::BrokerNet;
use crate::receipt::Initiator;
use crate::snapshot::Snapshot;

/// Viewport and budget for a page.
#[derive(Debug, Clone)]
pub struct PageOptions {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub max_snapshot_lines: usize,
    /// Run the page's own scripts.
    ///
    /// Off by default and opt-in at every layer above, because turning it on is
    /// a change to what an untrusted page can do inside the box rather than a
    /// rendering preference (roadmap-history.md §12.5).
    pub script: bool,
    /// How long one navigation may take, first byte to last.
    pub navigation_budget: std::time::Duration,

    /// How long the script realm's job queue may run before it is cancelled.
    pub script_budget: Option<std::time::Duration>,

    /// Install the WebIDL member decoration in the script realm.
    ///
    /// *For instruments*, and off by default: see
    /// [`crate::script::RealmOptions::webidl_conformance`] for what it costs
    /// and who observes it.
    pub webidl_conformance: bool,
}

impl Default for PageOptions {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            scale: 1.0,
            max_snapshot_lines: 500,
            script: false,
            script_budget: None,
            webidl_conformance: false,
            // Above what a slow real page takes and far below what a stuck one
            // would, which is the shape every ceiling in this engine has.
            navigation_budget: std::time::Duration::from_secs(45),
        }
    }
}

/// How many nodes an inline union walks before giving up. Far above any real
/// anchor's subtree, far below anything that would make an overlay feel slow.
const MAX_INLINE_NODES: usize = 4096;

/// What [`Page::select_option`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectOutcome {
    /// Set. Carries the value the form will submit, which is what a recording
    /// should hold. The text is what the agent read, and the two differ on
    /// most real forms.
    Chosen(String),
    /// It is a `<select>`, and nothing in it matched by value or by text.
    NoSuchOption,
    /// It was never a `<select>`.
    NotASelect,
}

/// Where an element is on screen, in viewport pixels.
///
/// Two sources, because a document has two layout systems. Blocks and
/// inline-blocks get a taffy box; a *non-replaced inline* element has zero taffy
/// size, its text laid out by parley into the containing block. That is the
/// ordinary shape of a link, so the fallback unions its inline runs, as
/// `getClientRects` does. `None` when there is nothing to point at.
fn hint_rect(doc: &BaseDocument, node_id: usize) -> Option<(f64, f64, f64, f64)> {
    if let Some(rect) = doc.get_client_bounding_rect(node_id)
        && rect.width > 0.0
        && rect.height > 0.0
    {
        return Some((rect.x, rect.y, rect.width, rect.height));
    }
    inline_rect(doc, node_id)
}

/// The union of an inline element's line boxes, in viewport pixels.
fn inline_rect(doc: &BaseDocument, node_id: usize) -> Option<(f64, f64, f64, f64)> {
    let node = doc.get_node(node_id)?;
    let root = node.inline_root_ancestor()?;
    let layout = &root.element_data()?.inline_layout_data.as_ref()?.layout;

    // Which nodes count as "this element's text". Parley labels a glyph run with
    // the node whose style it took, which for `<a><b>bold</b></a>` is the `<b>`,
    // so matching the anchor alone would find nothing.
    let mut owned = std::collections::HashSet::new();
    let mut stack = vec![node_id];
    // Bounded: this runs on a keystroke over a tree the page controls.
    let mut budget = MAX_INLINE_NODES;
    while let Some(id) = stack.pop() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        if !owned.insert(id) {
            continue;
        }
        if let Some(child) = doc.get_node(id) {
            stack.extend(child.children.iter().copied());
        }
    }

    let (mut x0, mut y0) = (f64::MAX, f64::MAX);
    let (mut x1, mut y1) = (f64::MIN, f64::MIN);
    let mut found = false;
    for line in layout.lines() {
        for item in line.items() {
            let (a, b, c, d) = match item {
                parley::layout::PositionedLayoutItem::GlyphRun(run) => {
                    if !owned.contains(&run.style().brush.id) {
                        continue;
                    }
                    let metrics = run.run().metrics();
                    let baseline = run.baseline() as f64;
                    (
                        run.offset() as f64,
                        baseline - metrics.ascent as f64,
                        run.advance() as f64,
                        (metrics.ascent + metrics.descent) as f64,
                    )
                }
                parley::layout::PositionedLayoutItem::InlineBox(ibox) => {
                    if !owned.contains(&(ibox.id as usize)) {
                        continue;
                    }
                    (
                        ibox.x as f64,
                        ibox.y as f64,
                        ibox.width as f64,
                        ibox.height as f64,
                    )
                }
            };
            found = true;
            x0 = x0.min(a);
            y0 = y0.min(b);
            x1 = x1.max(a + c);
            y1 = y1.max(b + d);
        }
    }
    if !found || x1 <= x0 || y1 <= y0 {
        return None;
    }

    // Inline coordinates are relative to the inline root's content box, so its
    // padding and border are added back — the correction `hit_inner` makes on
    // the way in, applied on the way out.
    let origin = root.absolute_position(0.0, 0.0);
    let inset_x = (root.final_layout.padding.left + root.final_layout.border.left) as f64;
    let inset_y = (root.final_layout.padding.top + root.final_layout.border.top) as f64;
    let scroll = doc.viewport_scroll();

    Some((
        origin.x as f64 + inset_x + x0 - scroll.x,
        origin.y as f64 + inset_y + y0 - scroll.y,
        x1 - x0,
        y1 - y0,
    ))
}

/// One actionable element, with the geometry an overlay needs to label it.
///
/// The ref is carried whole, not reduced to its id: a viewer has to show what
/// the target is as well as address it.
#[derive(Debug, Clone, PartialEq)]
pub struct HintTarget {
    pub entry: crate::snapshot::RefEntry,
    /// Viewport pixels: the left edge, past the scroll offset.
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A request a form asked for, caught on its way to the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub url: Url,
    /// The document the form was in.
    ///
    /// A form's `action` is chosen by the page, not by the agent. The agent
    /// asks for a button to be pressed, and the page decides where that goes.
    /// So the submission is policed as a request *from* this origin, which is
    /// what stops a page on the open web POSTing to the box's dev server the
    /// moment somebody clicks its submit button. Filled in by
    /// [`Page::submit_form`], which is the only thing that knows it.
    pub document: Url,
    /// `GET` or `POST`. Anything else never reaches here: Blitz's own
    /// submission algorithm declines to produce it.
    pub method: String,
    /// The encoded body, for `POST`. Empty for `GET`, whose fields are already
    /// in the URL's query by the time it arrives.
    pub body: Vec<u8>,
    pub content_type: Option<String>,
}

/// A form's entry list as a query string: what a `GET` form puts in the URL.
pub fn encode_form_query(entries: &[(String, String)]) -> String {
    let mut out = String::new();
    url::form_urlencoded::Serializer::new(&mut out).extend_pairs(entries);
    out
}

/// A form's entry list as a request body, and the content type describing it.
///
/// Three encodings, because a server that reads one does not read the others.
/// Uploads are absent: filling one in would mean reading the box's filesystem.
pub fn encode_form_body(enctype: &str, entries: &[(String, String)]) -> (Vec<u8>, String) {
    match enctype.trim() {
        "text/plain" => {
            // The one encoding that escapes nothing.
            let mut out = String::new();
            for (name, value) in entries {
                out.push_str(name);
                out.push('=');
                out.push_str(value);
                out.push_str("\r\n");
            }
            (out.into_bytes(), "text/plain;charset=UTF-8".to_string())
        }
        "multipart/form-data" => {
            let boundary = crate::multipart::fresh_boundary();
            let parts: Vec<crate::multipart::Part> = entries
                .iter()
                .map(|(name, value)| crate::multipart::Part {
                    name: name.clone(),
                    data: value.as_bytes().to_vec(),
                    ..Default::default()
                })
                .collect();
            (
                crate::multipart::serialize(&parts, &boundary),
                format!("multipart/form-data; boundary={boundary}"),
            )
        }
        _ => (
            encode_form_query(entries).into_bytes(),
            "application/x-www-form-urlencoded".to_string(),
        ),
    }
}

/// Where a submission waits to be picked up.
///
/// One slot for both fillers, Blitz's algorithm and the page's own
/// `form.submit()`, so a handler cannot leave a second request behind for the
/// next verb to send by surprise. Last one wins, as in a browser.
pub type NavigationSlot = Arc<std::sync::Mutex<Option<Submission>>>;

/// A [`NavigationProvider`] that catches the request instead of following it.
///
/// Blitz calls this from inside `submit_form`, so the request arrives on the
/// same thread and is picked up immediately afterwards. The `Mutex` is here to
/// satisfy the trait's `Send + Sync` bound rather than to guard a race. The
/// page has exactly one owner (see `stream`'s module docs).
#[derive(Clone)]
struct CapturedNavigation {
    slot: NavigationSlot,
    /// The document the form lives in, so the captured request carries the
    /// origin it was made from. Filled here rather than left for the caller
    /// because a `Submission` with no origin is one the policy trusts, and a
    /// field that defaults to trusted is a field somebody forgets to set.
    document: Url,
}

impl blitz_traits::navigation::NavigationProvider for CapturedNavigation {
    fn navigate_to(&self, options: blitz_traits::navigation::NavigationOptions) {
        let (body, content_type) = match &options.document_resource {
            blitz_traits::net::Body::Form(form) => {
                let mut encoded = String::new();
                url::form_urlencoded::Serializer::new(&mut encoded).extend_pairs(
                    form.iter().filter_map(|entry| match &entry.value {
                        blitz_traits::net::EntryValue::String(value) => {
                            Some((entry.name.clone(), value.clone()))
                        }
                        // A file upload has no bytes this engine ever had: it
                        // would have to read the box's filesystem to fill one
                        // in, which is a capability a browser should not
                        // quietly acquire. Dropped, and the field is absent
                        // rather than empty so a server can tell.
                        _ => None,
                    }),
                );
                (
                    encoded.into_bytes(),
                    Some("application/x-www-form-urlencoded".to_string()),
                )
            }
            _ => (Vec::new(), options.content_type.clone()),
        };

        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(Submission {
                url: options.url.clone(),
                document: self.document.clone(),
                method: format!("{:?}", options.method).to_uppercase(),
                body,
                content_type,
            });
        }
    }
}

/// The one real DOM, shared with the script realm.
pub type Dom = Rc<RefCell<BaseDocument>>;

/// A loaded, resolved document.
pub struct Page {
    doc: Dom,
    url: Url,
    /// What this document is written in.
    ///
    /// Carried on the page rather than worked out where it is needed, because
    /// two very different things depend on it (decoding the bytes, and
    /// encoding a URL's query) and they must not be able to disagree.
    encoding: &'static encoding_rs::Encoding,
    options: PageOptions,
    /// Where [`CapturedNavigation`] leaves whatever the last form asked for.
    pending_navigation: NavigationSlot,
    /// So the page can hear `load` and `error`. See [`crate::net::ResourceLog`].
    resources: crate::net::ResourceLog,
    /// The script realm, when this page has one. `None` when script is off,
    /// which is still the default: `capabilities.javascript` is the gate, and
    /// flipping it is a threat-model decision rather than a feature flag
    /// (roadmap-history.md §12.5).
    script: Option<crate::script::Script>,
    /// What this page's load cost, against what it was allowed.
    ///
    /// Recorded on the page rather than read from the broker at snapshot time,
    /// because the broker's counters belong to whatever page is loading *now*
    /// and a snapshot of an earlier page would read the wrong ones.
    budget_spent: Option<(crate::budget::Spent, crate::budget::Limits)>,
    /// How long this navigation has left.
    ///
    /// Armed when the page began loading, not when the script phase starts, so
    /// the time already spent on the network counts against it. That is the
    /// point: the per-phase budgets each bound their own step, and a page that
    /// is inside every one of them can still take the better part of a minute.
    deadline: crate::budget::Deadline,
    /// Whether `run_scripts` was called, regardless of whether it built a realm.
    ///
    /// A page with no script elements never gets one, so `script.is_some()`
    /// alone cannot tell "script is off" from "there was nothing to run".
    ran_scripts: bool,
    /// Set when the layout engine panicked while reading this page.
    ///
    /// The outline that follows was produced from whatever state layout reached,
    /// which is worth saying out loud: a short page and a half-laid-out one look
    /// the same to a reader who is not told.
    layout_failure: Option<String>,
    /// What the last settle did, for the snapshot to report.
    settled: Option<crate::script::Settled>,
    /// Engine-level facts the next snapshot should carry.
    notes: Vec<String>,
}

/// How many submissions a page may chain on its own before this stops following.
const MAX_SELF_SUBMISSIONS: usize = 3;

/// Enough for an image that loads an image. A page that keeps going is looping.
const RESOURCE_EVENT_PASSES: usize = 3;

/// The event-handler content attributes, as a selector.
///
/// Enumerated rather than discovered because it has to be a selector: CSS can
/// match an attribute's *value* by prefix but not its *name*, so "any element
/// with an `on*` attribute" is not something a selector can ask for. The list
/// mirrors the one `prelude.js` compiles from, and the two are meant to agree.
/// This one decides whether the realm is built, that one decides what the
/// realm compiles once it is.
const INLINE_HANDLER_SELECTOR: &str = "[onload],[onclick],[onerror],[onchange],[oninput],\
    [onsubmit],[onfocus],[onblur],[onkeydown],[onkeyup],[onkeypress],[onmousedown],\
    [onmouseup],[onmouseover],[onmouseout],[onmousemove],[ondblclick],[onscroll],\
    [onselect],[onreset],[oncontextmenu],[onwheel],[ontoggle],[onpageshow],\
    [onpagehide],[onhashchange],[onpopstate],[onresize],[onmessage],[onunload],\
    [onbeforeunload],[onanimationend],[ontransitionend],[onpointerdown],[onpointerup]";

/// What the snapshot says when a navigation left an origin behind.
///
/// One string rather than two, because it is written at two points now, when
/// the document is built, which is before its subresources and frames are
/// fetched, and again when the page is finished, for an origin change that
/// happened after that.
const SESSION_DROPPED_NOTE: &str =
    "cookies from the previous origin were dropped on navigation: this engine holds a \
     session only for the origin currently loaded";

/// How many frame documents one page may pull in, including nested ones.
///
/// A bound, and a *said* bound (§B16.10): ad-stuffed pages carry dozens of
/// frames, each of which may carry more, and every fetch spends the page's
/// own network budget. Eight is far above what a page an agent is driving
/// legitimately embeds and far below what an ad cascade produces.
const MAX_FRAMES: usize = 8;

/// Load each frame's document and graft it under the frame element (§B21).
fn load_frames(page: &mut Page, broker: &Arc<dyn Broker>) {
    // Worklist rather than one pass, because a grafted document may itself
    // hold frames. The cap bounds the whole tree, and being over it is noted
    // rather than silent.
    let mut loaded: usize = 0;
    let mut stripped_scripts: usize = 0;
    let mut failures: Vec<String> = Vec::new();
    let mut seen: Vec<usize> = Vec::new();
    let mut capped = false;
    // Where each grafted document came from, for the note. See the note itself
    // for why counting them was not enough.
    let mut origins: Vec<String> = Vec::new();

    loop {
        // One frame per iteration: the graft invalidates any longer list of
        // ids collected up front.
        let next = {
            let doc = page.doc.borrow();
            doc.query_selector_all("iframe, frame")
                .map(|ids| ids.into_iter().collect::<Vec<usize>>())
                .unwrap_or_default()
                .into_iter()
                .find(|id| !seen.contains(id))
        };
        let Some(frame_id) = next else { break };
        seen.push(frame_id);
        if loaded >= MAX_FRAMES {
            capped = true;
            continue;
        }

        let (srcdoc, src) = {
            let doc = page.doc.borrow();
            let attr = |name: &str| {
                doc.get_node(frame_id).and_then(|node| {
                    node.attrs().and_then(|attrs| {
                        attrs
                            .iter()
                            .find(|a| a.name.local.as_ref() == name)
                            .map(|a| a.value.to_string())
                    })
                })
            };
            (attr("srcdoc"), attr("src"))
        };

        // `srcdoc` wins over `src`, per spec, and needs no fetch: it is inline
        // content the parser already carried, like a `data:` URL, and so has
        // this page's own origin, which is why it contributes no name.
        let html = if let Some(inline) = srcdoc {
            inline
        } else {
            let Some(raw) = src else { continue };
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with("about:") {
                continue;
            }
            if trimmed.starts_with("javascript:") {
                // A javascript: frame is script by another road, and script in
                // frames is the boundary. Skipped by name.
                failures.push("a frame with a javascript: source was not run".to_string());
                continue;
            }
            let Ok(resolved) = page.url.join(trimmed) else {
                failures.push(format!("`{trimmed}` is not a resolvable frame URL"));
                continue;
            };
            let outcome = broker.send_from(
                &resolved,
                Initiator::Frame,
                "GET",
                &[],
                None,
                Some(&page.url),
            );
            if let Some(error) = &outcome.error {
                // A policy refusal is the engine working; it is reported in
                // the note rather than silently rendering as an empty frame.
                failures.push(format!("{resolved}: {error}"));
                continue;
            }
            let status = outcome.status.unwrap_or(0);
            if !(200..300).contains(&status) {
                failures.push(format!("{resolved}: the server answered {status}"));
                continue;
            }
            let content_type = declared_content_type(&outcome);
            if let Some(kind) = &content_type
                && !kind.contains("html")
            {
                // An image or a PDF in a frame is content this engine cannot
                // flatten into an outline; saying so beats grafting bytes.
                failures.push(format!("{resolved}: `{kind}` is not a document"));
                continue;
            }
            match crate::cors::Origin::of(&outcome.final_url) {
                Some(origin) => origins.push(origin.header()),
                None => origins.push(outcome.final_url.to_string()),
            }
            let encoding = crate::encoding::sniff(&outcome.body, content_type.as_deref());
            crate::encoding::decode(&outcome.body, encoding)
        };

        // Graft, then strip. Stripping *after* the graft rather than editing the
        // string, because removing markup with a regex is how a `<script>` inside a
        // comment ends up half-removed.
        //
        // The graft goes into a `<div>` appended under the frame, not into the frame
        // element itself, and the difference is the HTML parser's: fragment parsing
        // in an `<iframe>` context treats the input as *raw text*, so setting the
        // frame's own innerHTML produced one text node holding escaped markup.
        {
            let mut doc = page.doc.borrow_mut();
            let mut mutator = doc.mutate();
            let container = mutator.create_element(
                blitz_dom::QualName::new(
                    None,
                    blitz_dom::ns!(html),
                    blitz_dom::LocalName::from("div"),
                ),
                Vec::new(),
            );
            mutator.set_inner_html(container, &html);
            mutator.append_children(frame_id, &[container]);
        }
        {
            let mut doomed: Vec<usize> = Vec::new();
            // Script that is not in a `<script>` element: the event-handler
            // content attributes, and the `javascript:` URLs. See
            // `defuse_attribute` for why removing the elements alone was only
            // half the boundary.
            let mut defused: Vec<(usize, blitz_dom::QualName)> = Vec::new();
            {
                let doc = page.doc.borrow();
                let mut stack = vec![frame_id];
                while let Some(at) = stack.pop() {
                    let Some(node) = doc.get_node(at) else { continue };
                    if at != frame_id
                        && let Some(el) = node.element_data()
                    {
                        let name = el.name.local.as_ref();
                        if name == "script" || name == "style" || name == "link" {
                            if name == "script" {
                                stripped_scripts += 1;
                            }
                            doomed.push(at);
                            continue;
                        }
                        for attribute in el.attrs() {
                            if defuse_attribute(attribute.name.local.as_ref(), &attribute.value) {
                                stripped_scripts += 1;
                                defused.push((at, attribute.name.clone()));
                            }
                        }
                    }
                    for child in node.children.clone() {
                        stack.push(child);
                    }
                }
            }
            if !doomed.is_empty() || !defused.is_empty() {
                let mut doc = page.doc.borrow_mut();
                let mut mutator = doc.mutate();
                for (id, name) in defused {
                    mutator.clear_attribute(id, name);
                }
                for id in doomed {
                    mutator.remove_node(id);
                }
            }
        }
        loaded += 1;
    }

    if loaded > 0 {
        // Named, not just counted. A flattened frame is somebody else's
        // document appearing inline in the agent's reading of this one, with
        // nothing in the outline to say which lines came from where. For an
        // engine whose claim is that a reader can tell where bytes came from,
        // "three frames were loaded" is not that: a third party writing into
        // the agent's reading of a page is a prompt-injection channel, and the
        // one thing that makes it answerable is naming the origin.
        let mut named = origins;
        named.sort();
        named.dedup();
        let from = if named.is_empty() {
            "written inline by this page".to_string()
        } else {
            format!("served by {}", named.join(", "))
        };
        page.note(&format!(
            "{loaded} frame(s) were loaded as content, {from}: each document was fetched \
             through the policy (initiator `frame` in the request log) and appears in the \
             outline below, flattened, so some of what follows is another origin's page \
             rather than this one's. Their scripts do not run ({stripped_scripts} stripped) \
             and their styles do not apply — a frame here is content, not a second page — \
             and `contentDocument` answers null."
        ));
    }
    if capped {
        page.note(&format!(
            "this page has more than {MAX_FRAMES} frames; the rest were not loaded. The \
             bound exists because ad cascades nest frames without limit, and it is said \
             here rather than silently applied."
        ));
    }
    if !failures.is_empty() {
        page.note(&format!(
            "{} frame(s) could not be loaded: {}",
            failures.len(),
            failures.join("; ")
        ));
    }
}

/// Whether this attribute of a flattened frame's content is script.
fn defuse_attribute(name: &str, value: &str) -> bool {
    // Lowercased rather than compared as it arrives. The HTML parser hands
    // back lowercase local names, but foreign content (SVG, MathML) keeps the
    // case the document wrote, and a boundary that a page can step around by
    // capitalising an attribute is not one.
    let name = name.to_ascii_lowercase();
    if name.len() > 2 && name.starts_with("on") {
        return true;
    }
    matches!(
        name.as_str(),
        "href" | "src" | "action" | "formaction" | "data" | "xlink:href"
    ) && is_javascript_url(value)
}

/// Whether a URL attribute names the `javascript:` scheme.
///
/// The comparison is on the value with ASCII whitespace and control characters
/// removed, because the HTML parser strips those from a URL before resolving
/// it: `java\tscript:alert(1)` is a `javascript:` URL, and a plain
/// `starts_with` on the raw value is the classic way to miss one.
fn is_javascript_url(value: &str) -> bool {
    let cleaned: String = value
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && !c.is_control())
        .collect();
    cleaned.len() >= "javascript:".len()
        && cleaned[.."javascript:".len()].eq_ignore_ascii_case("javascript:")
}

/// Whether to compile the browser prelude while this navigation is in flight.
fn worth_warming(url: &Url, options: &PageOptions) -> bool {
    if !options.script {
        return false;
    }
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    !crate::policy::is_loopback(host.strip_suffix('.').unwrap_or(host))
}

impl Page {
    /// Fetch a URL and load it.
    ///
    /// The navigation itself is policy-checked like any other request: asking
    /// to open a page is not a way around the allowlist, it is the first entry
    /// in the receipt.
    pub fn open(
        url: &Url,
        broker: Arc<dyn Broker>,
        fonts: FontSetup,
        options: PageOptions,
    ) -> Result<Self, H5iError> {
        let mut target = url.clone();
        // Where a `<meta refresh>` chain has already been, so a page that
        // refreshes to itself, which news and dashboard pages do on purpose,
        // is loaded once rather than forever.
        let mut visited: Vec<Url> = vec![url.clone()];
        let mut followed: Vec<String> = Vec::new();

        for _ in 0..=MAX_META_REFRESH_HOPS {
            let outcome = if worth_warming(&target, &options) {
                broker.fetch_while(&target, Initiator::Navigation, &mut || {
                    crate::script::warm_prelude();
                })
            } else {
                broker.fetch(&target, Initiator::Navigation)
            };
            if let Some(error) = outcome.error {
                return Err(H5iError::Metadata(format!("could not open {target}: {error}")));
            }

            // Decoded as the document says it is written, not as UTF-8. A
            // `euc-jp` page read as UTF-8 is mojibake, and every answer that
            // follows (the outline, the snapshot, a link's query) is then
            // wrong with nothing to say so. Still lossy within that encoding: a
            // page with one bad byte should render.
            let content_type = declared_content_type(&outcome);
            let encoding = crate::encoding::sniff(&outcome.body, content_type.as_deref());
            let html = crate::encoding::decode(&outcome.body, encoding);
            let final_url = outcome.final_url.clone();
            let status = outcome.status.unwrap_or(0);

            // Decided before the page is built, because the marker lives in the
            // markup we already hold and building is the expensive half.
            let refresh = meta_refresh(&html, &final_url);
            if let Some((delay, next)) = &refresh
                && *delay <= META_REFRESH_MAX_DELAY_SECONDS && !visited.contains(next)
            {
                followed.push(final_url.to_string());
                visited.push(next.clone());
                target = next.clone();
                continue;
            }

            let mut page = Self::from_html(&html, &final_url, broker.clone(), fonts, options);
            page.encoding = encoding;

            // An HTTP error still has a body, and rendering it silently is how
            // an agent ends up reading a 404 page as though it were the page it
            // asked for. Found by the corpus: crates.io answered 404, the
            // outline came back empty, and nothing anywhere said why.
            if !(200..300).contains(&status) {
                page.note(&format!(
                    "the server answered {status} for this URL; what follows is whatever it \
                     returned with that status, not the page that was asked for"
                ));
            }

            // Frames are loaded as *content*: each frame's document is
            // fetched through the broker, policy-checked and receipted under
            // its own initiator, and grafted under the frame element, with
            // its scripts and styles stripped. See `load_frames` for the
            // boundary this deliberately does not cross (§B21).
            load_frames(&mut page, &broker);

            // A challenge is not the page, and an outline of one reads as a
            // page that is simply empty. Naming it is the difference between an
            // agent concluding "there is nothing here" and "I was blocked".
            if let Some(marker) = challenge_marker(&html) {
                page.note(&format!(
                    "this looks like a bot challenge rather than the page that was asked for \
                     (it says \"{marker}\"). The content below is the challenge. This engine \
                     runs script but solves no proof-of-work and has no browser fingerprint to \
                     offer, so this site is not readable from here."
                ));
            }

            for from in &followed {
                page.note(&format!(
                    "{from} asked for a <meta refresh> and this engine followed it; what \
                     follows is the page it named"
                ));
            }

            // Present, but not followed. Saying which is the point: a page that
            // refreshes in ten minutes is a page that intends to update itself,
            // not one that redirected.
            if let Some((delay, next)) = refresh {
                if delay > META_REFRESH_MAX_DELAY_SECONDS {
                    page.note(&format!(
                        "this page asks to reload itself as {next} after {delay}s; that is a \
                         page updating itself rather than a redirect, so it was not followed"
                    ));
                } else if visited.iter().filter(|v| **v == next).count() > 0 && followed.is_empty()
                {
                    page.note(&format!(
                        "this page's <meta refresh> points at {next}, which is where we already \
                         are; it was not followed"
                    ));
                }
            }

            return Ok(page);
        }

        Err(H5iError::Metadata(format!(
            "could not open {url}: it redirected through more than {MAX_META_REFRESH_HOPS} \
             <meta refresh> hops without arriving anywhere"
        )))
    }

    /// Load HTML that is already in hand (a local file, or a test fixture).
    /// Subresources still go through the broker, so a local file cannot pull a
    /// remote tracker without a policy decision and a receipt line.
    ///
    /// Build a page from the bytes a server sent, rather than from a string
    /// somebody already decoded. *How* those bytes become a string is a property of
    /// the document: a `euc-jp` page decoded as UTF-8 is mojibake, and every
    /// downstream answer (the outline, the snapshot, a link's query) is then wrong
    /// in a way nothing reports.
    pub fn from_bytes(
        bytes: &[u8],
        content_type: Option<&str>,
        base_url: &Url,
        broker: Arc<dyn Broker>,
        fonts: FontSetup,
        options: PageOptions,
    ) -> Self {
        let encoding = crate::encoding::sniff(bytes, content_type);
        let html = crate::encoding::decode(bytes, encoding);
        let mut page = Self::from_html(&html, base_url, broker, fonts, options);
        page.encoding = encoding;
        page
    }

    /// Parse markup into a document.
    const CANVAS_UA_CSS: &'static str = "canvas { display: inline-block; }";

    fn parse(
        html: &str,
        base_url: &Url,
        broker: Arc<dyn Broker>,
        fonts: &FontSetup,
        viewport: Viewport,
        captured: CapturedNavigation,
        resources: crate::net::ResourceLog,
    ) -> BaseDocument {
        HtmlDocument::from_html(
            html,
            DocumentConfig {
                viewport: Some(viewport),
                base_url: Some(base_url.to_string()),
                net_provider: Some(Arc::new(BrokerNet::with_log(
                    broker,
                    Some(base_url.clone()),
                    resources,
                ))),
                font_ctx: Some(fonts.context.clone()),
                // Without this Blitz uses `DummyHtmlParserProvider` and
                // `set_inner_html` silently does nothing: the old children are
                // dropped and no new ones are parsed, so `el.innerHTML = x`
                // empties the element. Supplying the real parser is what makes
                // innerHTML, insertAdjacentHTML and template content work.
                html_parser_provider: Some(Arc::new(blitz_html::HtmlProvider)),
                // Forms dispatch through this. Without it Blitz's default
                // provider does nothing at all, and a submit would look like a
                // page that simply ignored the button.
                navigation_provider: Some(Arc::new(captured)),
                ..Default::default()
            },
        )
        .into_inner()
    }

    /// The one rule that lets an *open* popover be seen.
    const POPOVER_UA_CSS: &'static str = "
        [popover][popover][popover][popover].__h5i_popover_open__ { display: block; }
    ";

    /// Apply this engine's own additions to the user-agent stylesheet.
    ///
    /// One rule today ([`Page::CANVAS_UA_CSS`]). Kept as a step of its own so
    /// the next one has an obvious home, and so it runs on every path that
    /// builds a document rather than on whichever one somebody remembered.
    fn apply_ua_stylesheet(doc: &mut BaseDocument) {
        doc.add_user_agent_stylesheet(Self::CANVAS_UA_CSS);
        doc.add_user_agent_stylesheet(Self::POPOVER_UA_CSS);
    }

    pub fn from_html(
        html: &str,
        base_url: &Url,
        broker: Arc<dyn Broker>,
        fonts: FontSetup,
        options: PageOptions,
    ) -> Self {
        // Before the document exists, and therefore before its subresources and frames are
        // fetched.
        let dropped_session = broker.keep_only_origin(base_url);

        let viewport = Viewport::new(
            options.width,
            options.height,
            options.scale,
            ColorScheme::Light,
        );

        let captured = CapturedNavigation {
            slot: Arc::default(),
            document: base_url.clone(),
        };
        let pending_navigation = captured.slot.clone();
        let resources = crate::net::ResourceLog::default();

        // Parsing can abort the process, so it is guarded and retried.
        let attempt = |markup: &str| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Self::parse(
                    markup,
                    base_url,
                    broker.clone(),
                    &fonts,
                    viewport.clone(),
                    captured.clone(),
                    resources.clone(),
                )
            }))
        };
        let (mut doc, unloadable_images) = match attempt(html) {
            Ok(doc) => (doc, false),
            Err(_) => match attempt(&strip_image_sources(html)) {
                Ok(doc) => (doc, true),
                // Nothing left to try. An empty document that says so beats a
                // process that is not there to say anything.
                Err(_) => (
                    attempt("<html><body></body></html>")
                        .unwrap_or_else(|_| unreachable!("empty markup always parses")),
                    true,
                ),
            },
        };

        Self::apply_ua_stylesheet(&mut doc);
        seed_checkbox_state(&mut doc);
        // Before layout, because layout is what a deep tree kills. See
        // `prune_deep_nesting`.
        let over_nested = prune_deep_nesting(&mut doc);

        // Twice, deliberately. The broker is synchronous, so subresources have
        // already completed by the time parsing returns, but their results
        // arrive as messages that `resolve` drains at its *start*. The first
        // pass applies the stylesheets; the second lays out with them.
        // A panic in either pass is caught and becomes a note on the reading.
        let mut layout_failure = guard_layout(|| {
            doc.resolve(0.0);
            doc.resolve(0.0);
        })
        .err();

        // Say what was lost. A page rebuilt without its images is a different
        // page from the one the server sent, and a reading that does not
        // mention it is a reading that quietly changed the subject.
        if unloadable_images && layout_failure.is_none() {
            layout_failure = Some(
                "this page's images could not be loaded, so it was read without them"
                    .to_string(),
            );
        }

        let mut notes: Vec<String> = Vec::new();
        if over_nested > 0 {
            notes.push(format!(
                "this page nests elements more than {MAX_ELEMENT_DEPTH} deep; {over_nested} \
                 subtree(s) below that were dropped before layout. The bound exists because \
                 the layout pass recurses and a deep enough page ends the process rather than \
                 the page, and it is said here rather than silently applied."
            ));
        }
        if dropped_session {
            notes.push(SESSION_DROPPED_NOTE.to_string());
        }

        Self {
            // Assumed until `from_bytes` says otherwise: a string handed
            // straight to `from_html` has already been decoded by someone.
            encoding: encoding_rs::UTF_8,
            doc: Rc::new(RefCell::new(doc)),
            url: base_url.clone(),
            // Armed here rather than at the script phase, so the fetching and
            // parsing already done count against it.
            deadline: crate::budget::Deadline::new(options.navigation_budget),
            budget_spent: None,
            options,
            pending_navigation,
            resources,
            script: None,
            ran_scripts: false,
            layout_failure,
            settled: None,
            notes,
        }
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Run the page's own scripts, then settle.
    pub fn run_scripts(&mut self, broker: Arc<dyn Broker>) -> Result<(), H5iError> {
        // In document order, inline and external together, because execution
        // order is semantics: a bundle that defines a global in one script and
        // uses it in the next breaks if they are reordered.
        enum Source {
            Inline(String),
            External(String),
            /// `type="module"`, inline or external. Deferred by definition and
            /// therefore kept apart: modules evaluate after every classic
            /// script, in their own document order.
            ModuleInline(String),
            ModuleExternal(String),
            /// `type="importmap"`. Never executed: it is read once, before the
            /// first script, and tells the loader where bare specifiers go.
            ImportMap(String),
        }

        /// A script and the element it came from, so `document.currentScript`
        /// can name that element while the code runs.
        type Pending = (usize, Source);

        let sources: Vec<Pending> = {
            let doc = self.doc.borrow();
            doc.query_selector_all("script")
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| {
                            let node = doc.get_node(*id)?;
                            let attr = |name: &str| {
                                node.attrs().and_then(|attrs| {
                                    attrs
                                        .iter()
                                        .find(|a| a.name.local.as_ref() == name)
                                        .map(|a| a.value.to_string())
                                })
                            };
                            // What the `type` attribute means, which is not "run it
                            // anyway". A page embeds data in script elements
                            // (`application/json` for state, `text/template` for markup)
                            // and those are data blocks the spec says never execute.
                            // Running them parses JSON as JavaScript and fills the console
                            // with syntax errors that blame the page. Found by pointing
                            // this at github.com.
                            let kind = attr("type").unwrap_or_default();
                            let kind = kind.trim().to_ascii_lowercase();
                            // Not script, and not data either: it is a
                            // declaration the module loader reads. Collected in
                            // the same walk so document order decides which one
                            // wins, and returned as its own `Source` so the
                            // partition below cannot mistake it for code.
                            if kind == "importmap" {
                                let text = node.text_content();
                                if text.trim().is_empty() {
                                    return None;
                                }
                                return Some((*id, Source::ImportMap(text)));
                            }
                            let is_module = kind == "module";
                            let is_classic = kind.is_empty()
                                || matches!(
                                    kind.as_str(),
                                    "text/javascript"
                                        | "application/javascript"
                                        | "text/ecmascript"
                                        | "application/ecmascript"
                                        | "module"
                                );
                            if !is_classic {
                                return None;
                            }

                            let source = match (attr("src"), is_module) {
                                (Some(src), true) => Source::ModuleExternal(src),
                                (Some(src), false) => Source::External(src),
                                (None, is_module) => {
                                    let text = node.text_content();
                                    if text.trim().is_empty() {
                                        return None;
                                    } else if is_module {
                                        Source::ModuleInline(text)
                                    } else {
                                        Source::Inline(text)
                                    }
                                }
                            };
                            Some((*id, source))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        // A handler attribute is script, and the shortcut below could not see it.
        let has_inline_handler = || {
            let doc = self.doc.borrow();
            doc.query_selector_all(INLINE_HANDLER_SELECTOR)
                .map(|ids| !ids.is_empty())
                .unwrap_or(false)
        };

        // Nothing to run means nothing to build. Starting the realm costs about 15
        // ms (273 KiB of JavaScript evaluated from scratch, the prelude's compile
        // no longer part of it, §B8.9) and a page with no script elements was
        // paying all of it for a realm never asked a question.
        //
        // A page whose only `<script>` is an import map has no code to run: the map
        // is a declaration for imports that never happen. Filtered here rather than
        // in the walk, so the walk stays one pass.
        let has_code = sources
            .iter()
            .any(|(_, source)| !matches!(source, Source::ImportMap(_)));
        if !has_code && !has_inline_handler() {
            self.ran_scripts = true;
            // Trivially settled, and said so rather than left null: a page with
            // no script has finished by definition, and "we do not know" is a
            // different answer that an agent would have to act on differently.
            self.settled = Some(crate::script::Settled {
                elapsed_ms: 0,
                timers_run: 0,
                cut_off: false,
                pending_timers: 0,
                periodic_timers: 0,
            });
            return Ok(());
        }

        let mut script = crate::script::Script::with_options(
            self.dom(),
            broker.clone(),
            &self.url,
            crate::script::RealmOptions {
                webidl_conformance: self.options.webidl_conformance,
            },
        )
        .map_err(H5iError::Metadata)?;
        script.set_encoding(self.encoding);
        // Shared, not copied: the document keeps filling this in as script adds
        // images and frames.
        script.set_resource_log(self.resources.clone());
        // The slot Blitz's algorithm fills too, so the page and the `submit`
        // verb produce one request between them rather than two.
        script.set_navigation_slot(self.pending_navigation.clone());

        // The map, before anything can import. First one wins and the rest are
        // ignored, which is what the specification says to do with a second:
        // an import map that took effect after resolution had begun would
        // change what an already-resolved import had meant.
        let mut maps = sources.iter().filter_map(|(_, source)| match source {
            Source::ImportMap(text) => Some(text),
            _ => None,
        });
        if let Some(text) = maps.next() {
            script.set_import_map(text);
            let extra = maps.count();
            if extra > 0 {
                // Said, not swallowed. A page with two maps is a page whose
                // author believes both are in effect, and the second one
                // silently doing nothing is a long debugging session.
                script.note_error(&format!(
                    "{extra} further import map(s) ignored: only the first in document order \
                     is used"
                ));
            }
        }

        // `<div id="x">` makes `x` a global, which is legacy and is also how a
        // great deal of test and older page script finds its subject. Installed
        // before the first script rather than after, because the first script is
        // usually the one that reaches for it, and a ReferenceError on line one
        // ends a file before it can report anything at all.
        if let Err(error) = script.eval("__h5iInstallNamedAccess()") {
            script.note_error(&format!("named access could not be installed: {error}"));
        }

        // Classic scripts first, in document order, then modules in document
        // order. That is the deferred semantics `type="module"` carries: a
        // module never runs before a classic script that follows it in the
        // markup, and a page that relies on that ordering breaks if we run them
        // as they appear.
        let (classic, modules): (Vec<Pending>, Vec<Pending>) = sources
            .into_iter()
            // The map has been read; it is not code and must reach neither list.
            .filter(|(_, source)| !matches!(source, Source::ImportMap(_)))
            .partition(|(_, source)| {
                matches!(source, Source::Inline(_) | Source::External(_))
            });

        let phase_started = std::time::Instant::now();
        // Whichever runs out first. The script phase has its own ceiling, and
        // the navigation has one over the whole load; a page that spent thirty
        // seconds fetching does not then get a fresh twenty to run in.
        // The ceiling the script phase actually runs under. An instrument may
        // raise it (`--script-seconds`); nothing else does, and the navigation
        // deadline still bounds the whole load either way.
        let phase_budget = self
            .options
            .script_budget
            .unwrap_or(SCRIPT_PHASE_BUDGET)
            .min(self.deadline.remaining());
        let mut skipped = 0usize;
        // The origin every `src` below is fetched on behalf of. Cloned once so
        // the loops do not have to hold a borrow of `self` across the calls
        // that mutate the script realm.
        let document = self.url.clone();
        // A `<script src>` is fetched here rather than by the net provider, so
        // without this it is the one subresource the page never hears about.
        let resources = self.resources.clone();

        for (index, (node, source)) in classic.into_iter().enumerate() {
            if phase_started.elapsed() >= phase_budget {
                skipped += 1;
                continue;
            }
            // Which script this was. Boa 0.19 reports neither a line number nor
            // a stack, so the element is the only locus available, and a bare
            // "TypeError: cannot convert null" with no locus at all is the
            // hardest kind of error for an agent to act on.
            let where_from = match &source {
                Source::External(src) => src.clone(),
                _ => format!("inline script #{}", index + 1),
            };
            let code = match source {
                Source::Inline(text) => text,
                Source::External(src) => {
                    // Fetched through the broker like every other subresource,
                    // so a script file is policy-checked and receipted before it
                    // is ever executed. A refusal is reported and the page runs
                    // without it, which is what the agent needs to know.
                    let Ok(url) = self.url.join(&src) else {
                        script.note_error(&format!("script src `{src}` is not a URL"));
                        continue;
                    };
                    // With the document's origin: a `src` is chosen by the page,
                    // and the response is *executed* in it. Without one the
                    // policy read it as the agent naming a URL, so a page from
                    // the open web could point a `<script src>` at the box's
                    // dev server and run whatever came back.
                    let outcome = broker.fetch_from(
                        &url,
                        crate::receipt::Initiator::Subresource,
                        Some(&document),
                    );
                    if let Ok(mut log) = resources.lock() {
                        log.record(&url, &outcome);
                    }
                    if let Some(error) = outcome.error {
                        script.note_refused_script(url.as_str());
                        script.note_error(&format!("could not load {url}: {error}"));
                        continue;
                    }
                    // Same rule as modules: an error page is not a script, and
                    // running one produces a syntax error that blames the page.
                    let status = outcome.status.unwrap_or(0);
                    if !(200..300).contains(&status) {
                        script.note_refused_script(url.as_str());
                        script.note_error(&format!(
                            "could not load {url}: the server answered {status}"
                        ));
                        continue;
                    }
                    String::from_utf8_lossy(&outcome.body).into_owned()
                }
                _ => unreachable!("partitioned above"),
            };

            script.set_current_script(Some(node));
            if let Err(error) = script.eval_named(&code, &where_from) {
                // Reported, not fatal: a page with one broken script is still a
                // page, and the agent needs to know which half it is reading.
                //
                // Recorded as not-run too: a bundle that threw halfway leaves
                // its globals undefined exactly as a refused one does, and the
                // ReferenceError that follows should blame this, not the engine.
                script.note_refused_script(&where_from);
                script.note_error(&format!("{where_from}: {error}"));
            }
        }
        // Null again once the classic scripts are done, because that is what a
        // module and a later callback are supposed to see.
        script.set_current_script(None);

        // One budget for the whole phase, not one per stage. The settle used to
        // arm a fresh deadline of its own, so a page that spent the script
        // budget and then the job budget cost the sum of the two. Lit.dev took
        // 46 seconds against a 20-second intent. What is left of the phase is
        // what settling gets.
        let left = phase_budget.saturating_sub(phase_started.elapsed());
        script.set_job_budget(left.max(std::time::Duration::from_secs(1)));

        for (_, source) in modules {
            if phase_started.elapsed() >= phase_budget {
                skipped += 1;
                continue;
            }
            let (code, path) = match source {
                Source::ModuleInline(text) => (text, self.url.to_string()),
                Source::ModuleExternal(src) => {
                    // Fetched here rather than by the loader because this is the
                    // entry point rather than an import, but through the same
                    // broker and with the same origin, so it is receipted and
                    // policed identically.
                    let Ok(url) = self.url.join(&src) else {
                        script.note_error(&format!("module src `{src}` is not a URL"));
                        continue;
                    };
                    // Document-scoped for the same reason the classic `src` above
                    // is: the URL is the page's choice and the body is executed.
                    // And CORS-checked, which the classic one is not. That
                    // difference is the spec's and is the whole of why JSONP
                    // exists: a classic script may be loaded cross-origin without
                    // asking, a module script may not. This fetched one with no
                    // CORS context, so a cross-origin module was parsed and
                    // evaluated in this page's realm without the server ever being
                    // asked. See `crate::script::modules`.
                    let outcome = broker.send_script(
                        &url,
                        "GET",
                        &[],
                        None,
                        &document,
                        &[],
                        crate::cors::Mode::Cors,
                        crate::cors::Credentials::SameOrigin,
                    );
                    if let Some(error) = outcome.error {
                        script.note_error(&format!("could not load {url}: {error}"));
                        continue;
                    }
                    let status = outcome.status.unwrap_or(0);
                    if !(200..300).contains(&status) {
                        script.note_error(&format!(
                            "could not load {url}: the server answered {status}"
                        ));
                        continue;
                    }
                    let text = String::from_utf8_lossy(&outcome.body).into_owned();
                    (text, outcome.final_url.to_string())
                }
                _ => unreachable!("partitioned above"),
            };

            if let Err(error) = script.eval_module(&code, &path) {
                script.note_error(&error);
            }
        }

        // The document is now as loaded as it is going to get, so say so.
        if let Err(error) = script.eval("__h5iFireLifecycle()") {
            script.note_error(&format!("the load lifecycle could not be fired: {error}"));
        }

        let settled = script.settle();
        if script.take_dirty() {
            self.note_layout_failure(lay_out(&self.doc));
        }
        // Drained and discarded: these are the page *loading*. Its module
        // graph and any fetch its startup made. Leaving them queued would
        // attribute them to whatever the agent did first, and "this click
        // caused these requests" is the one claim here that has to be exact.
        if skipped > 0 {
            script.note_error(&format!(
                "this page's scripts took longer than {}s, so {skipped} of them were not run. \
                 What follows was rendered by the ones that finished.",
                SCRIPT_PHASE_BUDGET.as_secs()
            ));
        }

        let _ = script.take_requests();
        self.settled = Some(settled);
        self.script = Some(script);
        self.ran_scripts = true;
        // A canvas drawn during the script phase reaches the page here. The
        // realm has to be installed on `self` first, because the surfaces live
        // on its host, so this cannot move above the line before it.
        if self.composite_canvases() {
            self.note_layout_failure(lay_out(&self.doc));
        }
        // An image a startup script appended was only fetched by the layout
        // pass above. Its handlers' requests belong to the load, so they drain
        // with it.
        self.deliver_resource_events();
        if let Some(script) = self.script.as_mut() {
            let _ = script.take_requests();
        }
        Ok(())
    }

    /// What this document is written in, as the canonical label.
    pub fn encoding(&self) -> &'static str {
        self.encoding.name()
    }

    /// Fire a real event at a node and let the page respond.
    pub fn dispatch_event(
        &mut self,
        node_id: usize,
        kind: &str,
    ) -> Option<Vec<crate::script::host::RequestLink>> {
        let script = self.script.as_mut()?;
        let _ = script.dispatch(node_id, kind);
        let settled = script.settle();
        self.after_script(settled);
        // Before the requests are taken, so an `onerror` handler's fetch is
        // attributed to this action rather than to the agent's next one.
        self.deliver_resource_events();
        let requests = self.script.as_mut()?.take_requests();
        Some(requests)
    }

    /// Wait until something is on the page, or until nothing can put it there.
    ///
    /// Three answers, and the third is the one worth having. A page that runs
    /// no script cannot grow the thing being waited for, so the honest reply is
    /// *immediately* "not there, and nothing here will change that" rather than
    /// a budget spent proving it. The same holds for a scripted page that has
    /// gone quiet, which is where the settle loop's `Quiescent` end comes from.
    pub fn wait_for(&mut self, target: &WaitTarget) -> crate::script::Waited {
        let dom = self.doc.clone();
        let max_lines = self.options.max_snapshot_lines;
        let url = self.url.to_string();
        let scripted = self.script.is_some();
        let mut ready = move || {
            let doc = dom.borrow();
            match target {
                WaitTarget::Selector(selector) => {
                    matches!(doc.query_selector_all(selector), Ok(found) if !found.is_empty())
                }
                // Read through the snapshot walker rather than the raw tree, so
                // "the text is on the page" means the same thing here as it
                // does in the outline the agent is reading. A match in a
                // `<script>` body is not a match a reader would see.
                WaitTarget::Text(needle) => {
                    crate::snapshot::Snapshot::capture(&doc, &url, max_lines, scripted)
                        .lines
                        .iter()
                        .any(|line| line.text.contains(needle.as_str()))
                }
            }
        };

        let Some(script) = self.script.as_mut() else {
            // No realm: it either matches now or it never will.
            let met = ready();
            return crate::script::Waited {
                met,
                // No realm ran, so nothing can have changed.
                changed: false,
                settled: crate::script::Settled {
                    elapsed_ms: 0,
                    timers_run: 0,
                    cut_off: false,
                    pending_timers: 0,
                    periodic_timers: 0,
                },
                end: if met {
                    crate::script::WaitEnd::Met
                } else {
                    crate::script::WaitEnd::Quiescent
                },
            };
        };

        let mut waited = script.settle_until(&mut ready);
        waited.changed = self.after_script(waited.settled.clone());
        waited
    }

    /// Wait until a page expression is true.
    ///
    /// `None` when this session has no realm, which the caller reports as a
    /// routing answer rather than as a condition that failed.
    pub fn wait_for_script(&mut self, expr: &str) -> Option<crate::script::Waited> {
        let script = self.script.as_mut()?;
        let mut waited = script.settle_until_expr(expr);
        waited.changed = self.after_script(waited.settled.clone());
        Some(waited)
    }

    /// Settle bookkeeping shared by everything that re-enters the realm.
    ///
    /// Factored out of `dispatch_event`, which had it inline: a wait can run a
    /// page's own code, so it owes the same layout re-resolve and the same
    /// `settled` record, and forgetting either would leave the next snapshot
    /// describing a document the engine had not laid out.
    fn after_script(&mut self, settled: crate::script::Settled) -> bool {
        let dirty = self.script.as_mut().map(|s| s.take_dirty()).unwrap_or(false);
        self.settled = Some(settled);
        // Canvas surfaces reach the page here, before layout rather than after:
        // attaching the pixels sets the element's intrinsic size, which the
        // layout pass then has to see.
        let painted = self.composite_canvases();
        if dirty || painted {
            self.note_layout_failure(lay_out(&self.doc));
        }
        dirty || painted
    }

    /// Let the page hear about the subresources that have resolved.
    ///
    /// After layout, not before: Blitz starts a fetch when it resolves the
    /// tree, so an `<img>` script appended has no outcome until then. A handler
    /// can add another, so this repeats, bounded.
    fn deliver_resource_events(&mut self) {
        for _ in 0..RESOURCE_EVENT_PASSES {
            let Some(script) = self.script.as_mut() else {
                return;
            };
            if !script.fire_resource_events() {
                return;
            }
            let settled = script.settle();
            self.after_script(settled);
        }
    }

    /// Hand every drawn canvas surface to the document as raster image data.
    fn composite_canvases(&mut self) -> bool {
        let Some(script) = self.script.as_ref() else {
            return false;
        };
        let host = script.host();
        if host.canvases.borrow().is_empty() {
            return false;
        }

        let mut canvases = host.canvases.borrow_mut();
        let dirty = canvases.dirty();
        if dirty.is_empty() {
            return false;
        }

        let doc = self.doc.clone();
        let mut doc = doc.borrow_mut();
        let mut painted = false;
        for node_id in dirty {
            let Some(canvas) = canvases.get_mut(node_id) else {
                continue;
            };
            let (width, height) = (canvas.width(), canvas.height());
            // The paint path expects straight-alpha RGBA; the surface is
            // premultiplied, which is how the rasteriser writes it. Getting
            // this backwards darkens every semi-transparent pixel. The kind
            // of wrong that looks plausible until it is beside a browser.
            let mut straight = Vec::with_capacity(canvas.pixels().len());
            for pixel in canvas.pixels().as_chunks::<4>().0 {
                let alpha = pixel[3];
                if alpha == 0 {
                    straight.extend_from_slice(&[0, 0, 0, 0]);
                    continue;
                }
                let un = |channel: u8| -> u8 {
                    ((channel as u32 * 255 + alpha as u32 / 2) / alpha as u32).min(255) as u8
                };
                straight.extend_from_slice(&[un(pixel[0]), un(pixel[1]), un(pixel[2]), alpha]);
            }

            let Some(node) = doc.get_node_mut(node_id) else {
                continue;
            };
            let Some(element) = node.element_data_mut() else {
                continue;
            };
            element.special_data = blitz_dom::node::SpecialElementData::Image(Box::new(
                blitz_dom::node::ImageData::Raster(blitz_dom::node::RasterImageData::new(
                    width,
                    height,
                    std::sync::Arc::new(straight),
                )),
            ));
            canvas.mark_composited();
            painted = true;
        }
        painted
    }

    /// Fire one key event at a node, if this page has a realm to fire it into.
    ///
    /// Silent when there is none, which is the same shape `dispatch_event`
    /// takes: a page with no script cannot be listening, so there is nothing
    /// the absence could be hiding.
    fn dispatch_key(&mut self, node_id: usize, kind: &str, key: &str) {
        let Some(script) = self.script.as_mut() else {
            return;
        };
        let _ = script.dispatch_key(node_id, kind, key);
        let settled = script.settle();
        self.after_script(settled);
        self.deliver_resource_events();
        if let Some(script) = self.script.as_mut() {
            let _ = script.take_requests();
        }
    }

    /// How many sockets this page holds open.
    ///
    /// Surfaced because it is the one thing in this engine that makes a session
    /// non-deterministic. Everything else runs on a virtual clock, so two reads of
    /// one page agree; a live socket delivers on wall-clock time, so the page can
    /// differ between two reads without the agent having done anything. That is a
    /// real capability and a real caveat, and the determinism claim is one this
    /// engine makes loudly elsewhere.
    pub fn open_sockets(&mut self) -> usize {
        self.script
            .as_mut()
            .map(|script| script.open_sockets())
            .unwrap_or(0)
    }

    /// Record an engine-level fact for the next snapshot to carry.
    pub fn note(&mut self, text: &str) {
        self.notes.push(text.to_string());
    }

    /// What the last settle did, if script ran.
    pub fn settled(&self) -> Option<&crate::script::Settled> {
        self.settled.as_ref()
    }

    /// Web APIs the page asked for and this engine does not have.
    pub fn unsupported(&self) -> Vec<(String, usize)> {
        self.script
            .as_ref()
            .map(|s| s.unsupported())
            .unwrap_or_default()
    }

    /// What the page logged, for the console pane and the receipt.
    pub fn console(&self) -> Vec<crate::script::host::ConsoleLine> {
        self.script.as_ref().map(|s| s.console()).unwrap_or_default()
    }

    pub fn has_script(&self) -> bool {
        // Whether this page ran script, not whether a realm exists: a page with
        // no script elements never builds one, and reporting that as "script is
        // off" would describe the session wrongly.
        self.ran_scripts
    }

    /// A handle to the document, for the script realm.
    ///
    /// Handing out the `Rc` rather than a reference is the point: the script
    /// realm outlives any single call into it, and both sides must see one tree.
    pub fn dom(&self) -> Dom {
        self.doc.clone()
    }

    /// Remember that layout broke, keeping the first failure.
    ///
    /// The first, because a later pass that happens to survive does not undo
    /// the fact that the document was laid out incompletely, and an outline
    /// read from it is short for a reason the agent should be told.
    fn note_layout_failure(&mut self, outcome: Result<(), String>) {
        if let Err(detail) = outcome
            && self.layout_failure.is_none()
        {
            self.layout_failure = Some(detail);
        }
    }

    /// Re-resolve style and layout after script changed the tree.
    ///
    /// Called once after a settle rather than after each mutation: a script that
    /// appends fifty rows should lay out once, not fifty times.
    pub fn refresh(&mut self) {
        self.note_layout_failure(lay_out(&self.doc));
    }

    /// The outline an agent reads.
    ///
    /// Carries the engine's own notes alongside it: whether the page had
    /// finished settling, and which Web APIs it asked for that this engine does
    /// not have. Both are outside the fence because both are facts about the
    /// reading rather than about the page, and both exist so an agent can tell
    /// "this page is empty" from "this page needed something I lack".
    pub fn snapshot(&self) -> Snapshot {
        let mut snapshot = Snapshot::capture(
            &self.doc.borrow(),
            self.url.as_str(),
            self.options.max_snapshot_lines,
            self.ran_scripts,
        );

        snapshot.notes.extend(self.notes.iter().cloned());

        // Silence is the one answer an agent cannot act on. A page with nothing
        // in it is either genuinely empty, blocked, or built by script this
        // engine could not run, and which of those it is belongs in the
        // outline rather than in the agent's imagination.
        if snapshot.lines.is_empty() {
            let scripts = self.doc.borrow().query_selector_all("script").map(|s| s.len()).unwrap_or(0);
            snapshot.notes.push(format!(
                "this page produced no readable content. It has {scripts} script element(s) \
                 and this engine {}. If it needs JavaScript beyond what is listed above, the \
                 chromium engine has more of it.",
                match (self.script.is_some(), self.ran_scripts, scripts) {
                    (true, _, _) => "ran them",
                    // No realm because there was nothing to run, which is a
                    // different fact from script being switched off.
                    (false, true, 0) => "had none to run",
                    (false, true, _) => "ran them",
                    (false, false, _) => "did not run them (script is off)",
                }
            ));
        }

        if let Some(detail) = &self.layout_failure {
            snapshot.notes.push(format!(
                "this engine's layout stage failed on this page ({detail}). What follows was \
                 read from a partly laid-out document and may be incomplete."
            ));
        }

        // What this page spent, when it spent enough to matter.
        //
        // Said rather than left in the request log, because a page that ran out
        // of allowance is a page whose reading is *incomplete*. The same class
        // of fact as "this page had not finished". An agent that is not told
        // reads a half-loaded page as the whole one.
        if let Some((spent, limits)) = &self.budget_spent {
            if spent.requests > limits.max_requests {
                snapshot.notes.push(format!(
                    "this page asked for more than {} requests in one navigation and the \
                     rest were refused. What follows was read from what it managed to load; \
                     the request log names which were denied.",
                    limits.max_requests
                ));
            } else if spent.wire_bytes > limits.max_wire_bytes
                || spent.decoded_bytes > limits.max_decoded_bytes
            {
                snapshot.notes.push(format!(
                    "this page pulled {} bytes ({} decoded) and passed its budget for one \
                     navigation, so later requests were refused.",
                    spent.wire_bytes, spent.decoded_bytes
                ));
            } else if spent.network_time > limits.max_network_time {
                snapshot.notes.push(format!(
                    "this page spent {}ms waiting on the network, past its budget for one \
                     navigation, so later requests were refused.",
                    spent.network_time.as_millis()
                ));
            }
        }

        // Two different facts, one line. `cut_off` says the reading stopped
        // before the page did. `periodic_timers` says the page finished what it
        // owed and is still running a loop that re-arms itself, which makes two
        // reads of it disagree without the agent having acted. The same caveat
        // `open_sockets` carries, and it belongs in the outline for the same
        // reason. Silence here would leave an agent unable to tell a page that
        // is animating from one that is done.
        if let Some(settled) = &self.settled
            && (settled.cut_off || settled.periodic_timers > 0)
        {
            snapshot.notes.push(settled.render());
        }

        let unsupported = self.unsupported();
        if !unsupported.is_empty() {
            let listed = unsupported
                .iter()
                .take(6)
                .map(|(name, count)| format!("{name} x{count}"))
                .collect::<Vec<_>>()
                .join(", ");
            snapshot.notes.push(format!(
                "this page used Web APIs this engine does not have ({listed}). \
                 What depends on them did not run; the chromium engine has them."
            ));
        }

        snapshot
    }

    /// Rasterise the viewport and encode it as a PNG.
    pub fn screenshot_png(&mut self) -> Result<Vec<u8>, H5iError> {
        let width = self.options.width;
        let height = self.options.height;
        let scale = self.options.scale as f64;

        let mut renderer = VelloCpuImageRenderer::new(width, height);
        let mut rgba: Vec<u8> = Vec::new();
        let mut doc = self.doc.borrow_mut();
        renderer.render_to_vec(
            |scene| paint_scene(scene, &mut doc, scale, width, height, 0, 0),
            &mut rgba,
        );
        drop(doc);

        encode_png(&rgba, width, height)
    }

    /// Rasterise the viewport and encode it as a JPEG.
    ///
    /// The live view wants JPEG rather than PNG: the viewers expect it, and a
    /// photographic-quality frame every scroll costs far less on the wire than
    /// a lossless one nobody is diffing.
    pub fn screenshot_jpeg(&mut self, quality: u8) -> Result<Vec<u8>, H5iError> {
        let width = self.options.width;
        let height = self.options.height;
        let scale = self.options.scale as f64;

        let mut renderer = VelloCpuImageRenderer::new(width, height);
        let mut rgba: Vec<u8> = Vec::new();
        let mut doc = self.doc.borrow_mut();
        renderer.render_to_vec(
            |scene| paint_scene(scene, &mut doc, scale, width, height, 0, 0),
            &mut rgba,
        );
        drop(doc);

        encode_jpeg(&rgba, width, height, quality)
    }

    /// Put `text` into a text field, replacing whatever was there.
    pub fn set_checked(&mut self, node_id: usize, checked: bool) -> Option<bool> {
        let was = {
            let doc = self.doc.borrow();
            let element = doc.get_node(node_id)?.element_data()?;
            if element.name.local != local_name!("input") {
                return None;
            }
            let kind = element.attr(local_name!("type")).unwrap_or("text");
            if !(kind.eq_ignore_ascii_case("checkbox") || kind.eq_ignore_ascii_case("radio")) {
                return None;
            }
            matches!(element.special_data, SpecialElementData::CheckboxInput(true))
        };

        if was == checked {
            // Already there. Reported as a no-op rather than dispatched, or a
            // replay would fire a `change` the original run never fired.
            return Some(false);
        }

        {
            let mut doc = self.doc.borrow_mut();
            // A radio turns its group off, which is what makes the group a
            // group. Done here rather than left to the page, because nothing
            // else in this engine implements the exclusivity and a form
            // submitted with two of a group checked is a wrong answer.
            if checked {
                let name = doc
                    .get_node(node_id)
                    .and_then(|node| node.element_data())
                    .filter(|el| {
                        el.attr(local_name!("type"))
                            .is_some_and(|k| k.eq_ignore_ascii_case("radio"))
                    })
                    .and_then(|el| el.attr(local_name!("name")))
                    .map(str::to_string);
                if let Some(name) = name {
                    let siblings: Vec<usize> = doc
                        .tree()
                        .iter()
                        .filter_map(|(id, node)| {
                            if id == node_id {
                                return None;
                            }
                            let el = node.data.downcast_element()?;
                            let is_radio = el
                                .attr(local_name!("type"))
                                .is_some_and(|k| k.eq_ignore_ascii_case("radio"));
                            let same_group =
                                el.attr(local_name!("name")).is_some_and(|n| n == name);
                            (is_radio && same_group).then_some(id)
                        })
                        .collect();
                    for id in siblings {
                        if let Some(el) = doc
                            .get_node_mut(id)
                            .and_then(|node| node.data.downcast_element_mut())
                        {
                            el.special_data = SpecialElementData::CheckboxInput(false);
                        }
                    }
                }
            }

            let element = doc
                .get_node_mut(node_id)
                .and_then(|node| node.data.downcast_element_mut())?;
            element.special_data = SpecialElementData::CheckboxInput(checked);
            // `:checked` is a real selector and the cascade has to see it.
            let _ = lay_out_doc(&mut doc);
        }

        // The pair a *user* edit fires, in the order a page expects. A page
        // that enables its submit button on `change` needs this or the button
        // stays disabled through a replay that looks like it worked.
        self.dispatch_event(node_id, "input");
        self.dispatch_event(node_id, "change");
        Some(true)
    }

    /// Choose an option in a `<select>`, by its value or its visible text.
    pub fn select_option(&mut self, node_id: usize, wanted: &str) -> SelectOutcome {
        let options: Vec<(usize, String, String)> = {
            let doc = self.doc.borrow();
            let element = doc.get_node(node_id).and_then(|node| node.element_data());
            let Some(element) = element else {
                return SelectOutcome::NotASelect;
            };
            if element.name.local != local_name!("select") {
                return SelectOutcome::NotASelect;
            }
            let mut found = Vec::new();
            let mut stack: Vec<usize> = doc.get_node(node_id).map(|n| n.children.clone()).unwrap_or_default();
            stack.reverse();
            while let Some(id) = stack.pop() {
                let Some(child) = doc.get_node(id) else { continue };
                if child.data.is_element_with_tag_name(&local_name!("option")) {
                    let text = crate::snapshot::collapse(&child.text_content());
                    let value = child
                        .element_data()
                        .and_then(|el| el.attr(local_name!("value")))
                        .map(str::to_string)
                        // An option with no `value` submits its text, which is
                        // the rule a form actually follows.
                        .unwrap_or_else(|| text.clone());
                    found.push((id, value, text));
                }
                let mut kids = child.children.clone();
                kids.reverse();
                stack.extend(kids);
            }
            found
        };

        let chosen = options
            .iter()
            .find(|(_, value, _)| value == wanted)
            .or_else(|| options.iter().find(|(_, _, text)| text == wanted))
            .cloned();
        let Some((chosen_id, value, _)) = chosen else {
            return SelectOutcome::NoSuchOption;
        };

        {
            let mut doc = self.doc.borrow_mut();
            // Exactly one selected, which is what a single `<select>` means
            // and what `snapshot` reads back to name the control.
            for (id, _, _) in &options {
                if let Some(el) = doc
                    .get_node_mut(*id)
                    .and_then(|node| node.data.downcast_element_mut())
                {
                    if *id == chosen_id {
                        el.attrs.push(blitz_dom::node::Attribute {
                            name: blitz_dom::QualName::new(
                                None,
                                blitz_dom::ns!(),
                                local_name!("selected"),
                            ),
                            value: "selected".into(),
                        });
                    } else {
                        el.attrs.retain(|a| a.name.local != local_name!("selected"));
                    }
                }
            }
            let _ = lay_out_doc(&mut doc);
        }

        self.dispatch_event(node_id, "input");
        self.dispatch_event(node_id, "change");
        SelectOutcome::Chosen(value)
    }

    /// Send a key to whatever has focus, or to a named element.
    ///
    /// Not typing: this is `Enter` to submit, `Escape` to dismiss, `Tab` to
    /// move on. The keys that *do* something rather than the ones that enter
    /// text. `type` covers the second and this covers the first, and merging
    /// them would mean a verb whose meaning depended on its argument.
    pub fn press(&mut self, node_id: usize, key: &str) -> bool {
        {
            let mut doc = self.doc.borrow_mut();
            if doc.get_node(node_id).is_none() {
                return false;
            }
            doc.set_focus_to(node_id);
        }
        // A real key is three events, and a page may be listening for any of
        // them: `keydown` is where `preventDefault` belongs, `keypress` is
        // where legacy code lives, `keyup` is where a debounce ends.
        for kind in ["keydown", "keypress", "keyup"] {
            self.dispatch_key(node_id, kind, key);
        }
        true
    }

    pub fn type_into(&mut self, node_id: usize, text: &str) -> bool {
        let mut doc = self.doc.borrow_mut();
        let takes_text = doc
            .get_node(node_id)
            .and_then(|node| node.element_data())
            .and_then(|el| el.text_input_data())
            .is_some();
        if !takes_text {
            return false;
        }

        // Focus first: the caret is drawn from it, so a viewer watching sees
        // the field an agent is typing into rather than text appearing in a
        // box nothing is pointing at.
        doc.set_focus_to(node_id);
        doc.with_text_input(node_id, |mut driver| {
            driver.select_all();
            driver.insert_or_replace_selection(text);
        });
        // Typing changes layout, a longer value can reflow the form, and
        // nothing else in this file re-resolves on the agent's behalf.
        let _ = lay_out_doc(&mut doc);
        drop(doc);

        // A *user* edit fires input then change, in that order. Script setting
        // `.value` does not, and must not, or a framework that re-renders on
        // its own write would loop. This is the user path, so it fires.
        if let Some(script) = self.script.as_mut() {
            let _ = script.dispatch(node_id, "input");
            let _ = script.dispatch(node_id, "change");
            let settled = script.settle();
            let dirty = script.take_dirty();
            self.settled = Some(settled);
            if dirty {
                self.note_layout_failure(lay_out(&self.doc));
            }
        }
        true
    }

    /// Put the caret in a field without disturbing what it holds.
    ///
    /// The counterpart of [`Page::type_into`], which sets a value — right for an
    /// agent that knows what the field should say. This is right for a person who
    /// came to append to it or correct a character.
    ///
    /// Returns whether there is a field here, so aiming at a button gets an
    /// answer rather than a caret nowhere.
    pub fn focus(&mut self, node_id: usize) -> bool {
        let mut doc = self.doc.borrow_mut();
        let takes_text = doc
            .get_node(node_id)
            .and_then(|node| node.element_data())
            .and_then(|el| el.text_input_data())
            .is_some();
        if !takes_text {
            return false;
        }
        doc.set_focus_to(node_id);
        // At the end: a caret at zero puts everything typed before what is there.
        doc.with_text_input(node_id, |mut driver| driver.move_to_text_end());
        true
    }

    /// Whether anything on the page has keyboard focus, so a key a focused field
    /// declined is not then handed to the scroller.
    pub fn has_focus(&self) -> bool {
        self.doc.borrow().get_focussed_node_id().is_some()
    }

    /// What a key does to the focused element.
    ///
    /// The key-to-edit mapping is a pure decision and lives in [`crate::keys`];
    /// this is the half that needs a document.
    ///
    /// Returns whether anything changed, so a key that does nothing costs no
    /// frame. The events are dispatched *around* the edit — `keydown` before,
    /// `input` after — which is the order an autocomplete or a controlled input
    /// is written against, and the reason this exists beside `type_into`.
    pub fn key_to_focused(&mut self, key: &crate::keys::Key) -> bool {
        use crate::keys::Edit;

        let focused = self.doc.borrow().get_focussed_node_id();
        let Some(node_id) = focused else {
            return false;
        };
        let edit = crate::keys::edit_for(key);

        // Tab moves between controls rather than into one, so it is answered
        // before the field is consulted at all.
        if edit == Edit::FocusNext || edit == Edit::FocusPrevious {
            self.dispatch_key(node_id, "keydown", &key.name);
            let moved = {
                let mut doc = self.doc.borrow_mut();
                // Blitz offers forward only. Backwards is left unhandled rather
                // than faked by cycling round the whole form.
                match edit {
                    Edit::FocusNext => doc.focus_next_node().is_some(),
                    _ => false,
                }
            };
            self.dispatch_key(node_id, "keyup", &key.name);
            return moved;
        }

        let takes_text = {
            let doc = self.doc.borrow();
            doc.get_node(node_id)
                .and_then(|node| node.element_data())
                .and_then(|el| el.text_input_data())
                .is_some()
        };
        if !takes_text {
            // Not a field, but the key is still delivered: a page may be
            // listening for it on a button or on the document.
            for kind in ["keydown", "keypress", "keyup"] {
                self.dispatch_key(node_id, kind, &key.name);
            }
            return false;
        }

        let before = self.field_value(node_id);
        self.dispatch_key(node_id, "keydown", &key.name);
        if edit != Edit::Ignore {
            let mut doc = self.doc.borrow_mut();
            doc.with_text_input(node_id, |mut driver| match edit {
                Edit::Insert(text) => driver.insert_or_replace_selection(text),
                Edit::Backspace => driver.backdelete(),
                Edit::DeleteForward => driver.delete(),
                Edit::BackspaceWord => driver.backdelete_word(),
                Edit::DeleteWord => driver.delete_word(),
                Edit::Left => driver.move_left(),
                Edit::Right => driver.move_right(),
                Edit::WordLeft => driver.move_word_left(),
                Edit::WordRight => driver.move_word_right(),
                Edit::LineStart => driver.move_to_line_start(),
                Edit::LineEnd => driver.move_to_line_end(),
                Edit::TextStart => driver.move_to_text_start(),
                Edit::TextEnd => driver.move_to_text_end(),
                Edit::Up => driver.move_up(),
                Edit::Down => driver.move_down(),
                Edit::SelectLeft => driver.select_left(),
                Edit::SelectRight => driver.select_right(),
                Edit::SelectAll => driver.select_all(),
                Edit::SelectToLineStart => driver.select_to_line_start(),
                Edit::SelectToLineEnd => driver.select_to_line_end(),
                Edit::FocusNext | Edit::FocusPrevious | Edit::Ignore => {}
            });
            // Typing reflows a form.
            let _ = lay_out_doc(&mut doc);
        }

        let after = self.field_value(node_id);
        let changed = before != after;
        if edit.types() {
            self.dispatch_key(node_id, "keypress", &key.name);
        }
        self.dispatch_key(node_id, "keyup", &key.name);

        // A user edit fires `input`; script setting `.value` must not, or a
        // framework that re-renders on its own write would loop.
        if changed
            && let Some(script) = self.script.as_mut()
        {
            let _ = script.dispatch(node_id, "input");
            let settled = script.settle();
            let dirty = script.take_dirty();
            self.settled = Some(settled);
            if dirty {
                self.note_layout_failure(lay_out(&self.doc));
            }
        }

        // The caret is drawn, so a motion is still a new picture.
        changed || edit.moves_the_caret()
    }

    /// What a text field currently holds.
    ///
    /// Read from the editor rather than the `value` attribute, because typing
    /// updates the former and leaves the latter at whatever the HTML said. A
    /// snapshot built from the attribute would show an agent the value it was
    /// served rather than the one it just typed.
    pub fn field_value(&self, node_id: usize) -> Option<String> {
        let doc = self.doc.borrow();
        let node = doc.get_node(node_id)?;
        let input = node.element_data()?.text_input_data()?;
        Some(input.editor.text().to_string())
    }

    /// Whatever the page asked to navigate to on its own, if it asked.
    ///
    /// A form the page submitted leaves its request here rather than sending
    /// it, so the session sends it like every other: through the broker,
    /// receipted, with the agent told the page moved. See [`NavigationSlot`].
    pub fn take_pending_submission(&mut self) -> Option<Submission> {
        self.pending_navigation.lock().ok()?.take()
    }

    /// Submit the form that owns `node_id`, and return the request it produced.
    pub fn submit_form(&mut self, node_id: usize) -> Result<Submission, H5iError> {
        // Blitz keeps a control-to-form map but does not expose it, so the
        // owner is found by walking up. That misses the `form=` attribute's
        // remote-owner case, which is rare enough to be a stated limit rather
        // than a reimplementation of the association algorithm.
        let form_id = self
            .enclosing_form(node_id)
            .ok_or_else(|| {
                H5iError::Metadata("that control is not inside a form this page defines".into())
            })?;

        self.doc.borrow_mut().submit_form(form_id, node_id);

        self.pending_navigation
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .ok_or_else(|| {
                H5iError::Metadata(
                    "the form produced no request — its method or scheme is not one this \
                     engine submits (http and https, GET and POST)"
                        .into(),
                )
            })
    }

    /// Walk up for a `<form>`, for controls Blitz's owner map does not cover.
    /// Activate a control the way a browser does, and say what is left to do.
    ///
    /// A click on a submit button is three steps, not one: fire `click`, and if
    /// nothing prevented it fire `submit` at the form, and if nothing prevented
    /// *that* send the request. Pages rely on every join in that chain. This one
    /// listens for `submit` on the form and calls `preventDefault` so it can
    /// `fetch` instead, which is the commonest shape a modern form has; firing
    /// only `click` left its handler unreached and the page did nothing at all.
    ///
    /// Returns the requests the handlers caused, and whether the form should
    /// still be submitted the ordinary way.
    pub fn activate(&mut self, node_id: usize) -> Option<(Vec<crate::script::host::RequestLink>, bool)> {
        let script = self.script.as_mut()?;
        let mut proceed = script.dispatch_reporting(node_id, "click").unwrap_or(true);
        if proceed
            && self.is_submit_control(node_id)
            && let Some(form) = self.enclosing_form(node_id)
            && let Some(script) = self.script.as_mut()
        {
            proceed = script.dispatch_reporting(form, "submit").unwrap_or(true);
        }
        let script = self.script.as_mut()?;
        let settled = script.settle();
        self.after_script(settled);
        self.deliver_resource_events();
        let requests = self.script.as_mut()?.take_requests();
        Some((requests, proceed))
    }

    /// Is this control one that submits the form it sits in?
    ///
    /// `<button>` with no type, `<button type=submit>`, `<input type=submit>`
    /// and `<input type=image>`. Not `type=button` and not `type=reset`, which
    /// are exactly the two that do nothing without script.
    ///
    /// This is what a browser does with a click on such a control whether or not
    /// script is running, and it is why a form works with JavaScript switched
    /// off. An engine that refused it would be refusing the ordinary way to log
    /// in to an ordinary application.
    pub fn is_submit_control(&self, node_id: usize) -> bool {
        let doc = self.doc.borrow();
        let Some(node) = doc.get_node(node_id) else {
            return false;
        };
        let Some(element) = node.element_data() else {
            return false;
        };
        let kind = element.attr(local_name!("type")).map(str::to_ascii_lowercase);
        match element.name.local.as_ref() {
            "button" => matches!(kind.as_deref(), None | Some("submit")),
            "input" => matches!(kind.as_deref(), Some("submit") | Some("image")),
            _ => false,
        }
    }

    /// The form this control belongs to, when it belongs to one.
    pub fn form_of(&self, node_id: usize) -> Option<usize> {
        self.enclosing_form(node_id)
    }

    fn enclosing_form(&self, node_id: usize) -> Option<usize> {
        let doc = self.doc.borrow();
        let mut current = doc.get_node(node_id)?;
        for _ in 0..64 {
            if current
                .element_data()
                .is_some_and(|el| el.name.local.as_ref() == "form")
            {
                return Some(current.id);
            }
            current = doc.get_node(current.parent?)?;
        }
        None
    }

    /// How far down the document the viewport currently sits.
    pub fn scroll_offset(&self) -> (f64, f64) {
        let scroll = self.doc.borrow().viewport_scroll();
        (scroll.x, scroll.y)
    }

    /// The height of the document including whatever overflows the root box.
    ///
    /// Not `size.height`, and the difference is the whole page on a real site. A
    /// stylesheet that says `html, body { height: 100% }`, which is most of the web
    /// including Wikipedia, sizes the root box to the viewport and lets the article
    /// overflow it. Reading `size.height` there reports a 40-screen article as one
    /// screen tall, so `scroll_by` clamped every scroll to zero and the engine
    /// could only scroll unstyled pages, which is what the local test pages were.
    pub fn content_height(&self) -> f64 {
        let doc = self.doc.borrow();
        let layout = &doc.root_element().final_layout;
        layout.size.height.max(layout.content_size.height) as f64
    }

    /// How far this document can scroll: everything below the fold.
    ///
    /// Deliberately not taffy's `Layout::scroll_height`, which was tried and is the
    /// wrong question. That measures overflow *within* an element's own box, so it
    /// reads zero for an unstyled page whose root box simply grew to 4000px: there
    /// is no overflow inside the root, the overflow is past the viewport. The
    /// scrollable range of a document is its height minus the window.
    fn max_scroll_y(&self) -> f64 {
        (self.content_height() - self.options.height as f64).max(0.0)
    }

    /// Scroll, clamped to the document.
    ///
    /// Returns whether anything moved, which is what lets the live view stay
    /// at zero frames per second: a scroll at the bottom of the page is not a
    /// reason to encode and send an identical frame.
    pub fn scroll_by(&mut self, dx: f64, dy: f64) -> bool {
        let (x, y) = self.scroll_offset();
        let max_y = self.max_scroll_y();
        let next_x = (x + dx).max(0.0);
        let next_y = (y + dy).clamp(0.0, max_y);

        if (next_x - x).abs() < f64::EPSILON && (next_y - y).abs() < f64::EPSILON {
            return false;
        }
        self.doc.borrow_mut().set_viewport_scroll(blitz_dom::Point {
            x: next_x,
            y: next_y,
        });
        true
    }

    /// Fire the `scroll` event a lazy-loader is listening for.
    ///
    /// Window and document listeners both land on the root element in this
    /// engine, so one dispatch reaches every shape of `scroll` handler, and the
    /// settle that follows re-runs the intersection observers against the
    /// offset the page is now at. Without this a scroll moved the viewport and
    /// told nobody, which is the "scrolling loads nothing here" the scraping
    /// course found.
    pub fn scrolled(&mut self) -> Option<Vec<crate::script::host::RequestLink>> {
        let root = self.doc.borrow().root_element().id;
        self.dispatch_event(root, "scroll")
    }

    /// The link at a viewport coordinate, resolved against the page's base.
    ///
    /// Hit-testing takes the scroll offset into account because the viewer
    /// reports where the human clicked on screen, not where that is in the
    /// document.
    pub fn link_at(&self, x: f32, y: f32) -> Option<Url> {
        let (scroll_x, scroll_y) = self.scroll_offset();
        let doc = self.doc.borrow();
        let hit = doc.hit(x + scroll_x as f32, y + scroll_y as f32)?;

        // The hit lands on whatever box is topmost, often a text run inside
        // the anchor rather than the anchor itself, so walk up for the href.
        let mut node_id = hit.node_id;
        for _ in 0..16 {
            let node = doc.get_node(node_id)?;
            if let Some(href) = node
                .attrs()
                .and_then(|attrs| {
                    attrs
                        .iter()
                        .find(|attr| attr.name.local.as_ref() == "href")
                })
                .map(|attr| attr.value.as_str())
            {
                return self.url.join(href).ok();
            }
            node_id = node.parent?;
        }
        None
    }


    /// Everything on screen a human could act on, and where it is.
    ///
    /// The snapshot's own refs rather than a second opinion about what is
    /// clickable, so the overlay cannot offer a target the verb layer refuses.
    /// What this adds is the geometry the outline leaves out.
    ///
    /// Viewport coordinates, past the scroll offset. Offscreen elements are
    /// dropped rather than clamped, so a label never points at something
    /// invisible.
    pub fn hint_targets(&self) -> Vec<HintTarget> {
        let snapshot = self.snapshot();
        let doc = self.doc.borrow();
        let (width, height) = (self.options.width as f64, self.options.height as f64);

        snapshot
            .refs
            .iter()
            .filter_map(|entry| {
                let (x, y, w, h) = hint_rect(&doc, entry.node_id)?;
                // Intersection, not containment: a link half off the right edge
                // is still one a human can see and click.
                let visible = x < width && y < height && x + w > 0.0 && y + h > 0.0;
                if !visible {
                    return None;
                }
                Some(HintTarget {
                    entry: entry.clone(),
                    x,
                    y,
                    width: w,
                    height: h,
                })
            })
            .collect()
    }

    /// The document's visible text, for the case where the caller wants prose
    /// rather than structure.
    pub fn text(&self) -> String {
        // Read through the IR rather than through a snapshot. This path wants
        // the page's words and nothing else, and a `Snapshot` is a vector of
        // owned strings plus a ref list plus the engine notes, every one of
        // which was built here and thrown away. The words come out the same;
        // `plain_text_reads_what_the_outline_reads` is what says so.
        crate::read_ir::ReadTree::capture(
            &self.doc.borrow(),
            self.url.as_str(),
            self.options.max_snapshot_lines,
            self.ran_scripts,
        )
        .plain_text()
    }
}

/// What a `wait_for` is waiting for.
#[derive(Debug, Clone)]
pub enum WaitTarget {
    /// A CSS selector that must match at least one element.
    Selector(String),
    /// Text that must appear in the outline a reader would see.
    Text(String),
}

/// Builds pages, so a session can navigate.
///
/// It exists because a `Page` consumes its `FontContext`, and parley's is not
/// cloneable, so following a link needs the ingredients kept aside rather
/// than the previous page's leftovers. Rebuilding registers the same font
/// files again, which costs a few milliseconds per navigation and buys a much
/// simpler ownership story than sharing a collection across documents.
pub struct PageFactory {
    broker: Arc<dyn Broker>,
    font_sources: Vec<std::path::PathBuf>,
    options: PageOptions,
}

impl PageFactory {
    pub fn new(
        broker: Arc<dyn Broker>,
        font_sources: Vec<std::path::PathBuf>,
        options: PageOptions,
    ) -> Self {
        Self {
            broker,
            font_sources,
            options,
        }
    }

    pub fn options(&self) -> &PageOptions {
        &self.options
    }

    pub fn broker(&self) -> &Arc<dyn Broker> {
        &self.broker
    }

    fn fonts(&self) -> FontSetup {
        crate::fonts::load(&self.font_sources, &[], Some(self.font_sources.len()))
    }

    /// Load whatever a form asked for, then whatever *that* page submits on its
    /// own. A refusal is an error the agent reads, not a blank page.
    pub fn open_submission(&self, submission: &Submission) -> Result<Page, H5iError> {
        Ok(self.follow_self_submissions(self.load_submission(submission)?))
    }

    /// Send the submissions a page made on its own, and end on the last answer.
    ///
    /// A `form.submit()` at load is a login interstitial or a POST CSRF proof.
    /// The request is receipted like every other and the page is *told* it
    /// moved. Bounded: a form that submits itself on load is a redirect loop.
    fn follow_self_submissions(&self, mut page: Page) -> Page {
        for _ in 0..MAX_SELF_SUBMISSIONS {
            let Some(submission) = page.take_pending_submission() else {
                return page;
            };
            let from = page.url().clone();
            match self.load_submission(&submission) {
                Ok(next) => {
                    page = next;
                    page.note(&format!(
                        "{from} submitted a form to {} by itself, without anything being \
                         clicked, and this is the answer to that submission",
                        submission.url
                    ));
                }
                Err(error) => {
                    page.note(&format!("this page tried to submit a form by itself: {error}"));
                    return page;
                }
            }
        }
        page.note(&format!(
            "this page submitted a form by itself more than {MAX_SELF_SUBMISSIONS} times \
             running, so the engine stopped following it"
        ));
        page
    }

    /// One submission, without following whatever the answer submits next.
    fn load_submission(&self, submission: &Submission) -> Result<Page, H5iError> {
        let _navigating = self.begin_navigation();
        let outcome = self.broker.send_from(
            &submission.url,
            Initiator::Navigation,
            &submission.method,
            &submission.body,
            submission.content_type.as_deref(),
            // The form's own document. A submission is a navigation the *page*
            // chose the destination of, so it is policed as a request from that
            // origin. A page on the open web does not get to POST to the box's
            // dev server because somebody pressed its button.
            Some(&submission.document),
        );
        if let Some(error) = outcome.error {
            return Err(H5iError::Metadata(format!(
                "could not submit to {}: {error}",
                submission.url
            )));
        }
        // A form's response is a document like any other, so it gets the same
        // encoding treatment: a legacy site that answers a POST in shift_jis
        // must not come back as replacement characters, and the same `finish`,
        // which is where the cookie-origin drop and the script run live. This
        // returned the page directly, so a submission was the one navigation
        // that both kept the previous origin's session and never ran its own
        // scripts.
        self.finish(Page::from_bytes(
            &outcome.body,
            declared_content_type(&outcome).as_deref(),
            &outcome.final_url,
            self.broker.clone(),
            self.fonts(),
            self.options.clone(),
        ))
    }

    /// Load a page and, when the options ask for it, run its scripts.
    fn finish(&self, mut page: Page) -> Result<Page, H5iError> {
        self.finish_page(&mut page)?;
        Ok(page)
    }

    /// The rule itself, on a borrow, so the two infallible constructors can run
    /// it too rather than keeping their own copy of half of it.
    fn finish_page(&self, page: &mut Page) -> Result<(), H5iError> {
        // The backstop. `Page::from_html` drops against the origin actually
        // served, *before* the page's subresources and frames go out; this
        // catches an origin change that happened after that. A script
        // navigation, a submission that built its page another way.
        if self.broker.keep_only_origin(page.url()) {
            page.note(SESSION_DROPPED_NOTE);
        }
        if self.options.script {
            page.run_scripts(self.broker.clone())?;
        }
        // After everything, so subresources and script fetches are counted.
        let allowance = self.broker.budget();
        page.budget_spent = Some((allowance.spent, allowance.limits));
        Ok(())
    }

    /// [`Self::finish`] for the constructors that cannot fail.
    ///
    /// They ran scripts themselves and did not drop the previous origin's
    /// cookies. A third and fourth copy of half the rule, which is how
    /// `open_submission` came to be missing it entirely.
    fn finish_reporting(&self, mut page: Page) -> Page {
        if let Err(error) = self.finish_page(&mut page) {
            eprintln!("h5i-browser: the script realm failed to start: {error}");
        }
        page
    }

    /// Whether this factory runs page script, for `capabilities` and for the
    /// engine line the viewers show.
    pub fn runs_script(&self) -> bool {
        self.options.script
    }

    /// A navigation is starting.
    fn begin_navigation(&self) -> crate::budget::HardStop {
        self.broker.reset_budget();
        // Held by the caller until the page is built, which is the span every
        // deadline in this engine is supposed to cover and the one where none of
        // them reach layout. See `HardStop`.
        crate::budget::HardStop::arm(self.options.navigation_budget)
    }

    pub fn open(&self, url: &Url) -> Result<Page, H5iError> {
        let _navigating = self.begin_navigation();
        // Leaving an origin drops its cookies. In `finish`, against the origin
        // actually loaded rather than the one asked for. See
        // `cookies::Jar::retain_origin` for why that bound exists and what it
        // costs, and `finish` for why it moved.
        let page = Page::open(url, self.broker.clone(), self.fonts(), self.options.clone())?;
        Ok(self.follow_self_submissions(self.finish(page)?))
    }

    /// Load HTML already in hand, running its scripts if the options ask.
    ///
    /// Infallible in the loading, because HTML always parses into something. A
    /// script that failed to *run* is reported through the page's console
    /// rather than here: one broken script does not make a page unreadable, and
    /// the agent needs the half that worked.
    /// The same as [`PageFactory::from_html`], but from bytes whose encoding is
    /// not yet known, so the document gets to say what it is written in.
    pub fn from_bytes(&self, bytes: &[u8], content_type: Option<&str>, base_url: &Url) -> Page {
        let _navigating = self.begin_navigation();
        self.follow_self_submissions(self.finish_reporting(Page::from_bytes(
            bytes,
            content_type,
            base_url,
            self.broker.clone(),
            self.fonts(),
            self.options.clone(),
        )))
    }

    pub fn from_html(&self, html: &str, base_url: &Url) -> Page {
        let _navigating = self.begin_navigation();
        self.follow_self_submissions(self.finish_reporting(Page::from_html(
            html,
            base_url,
            self.broker.clone(),
            self.fonts(),
            self.options.clone(),
        )))
    }
}

/// Resolve style and layout, surviving a panic in the layout engine.
pub(crate) fn lay_out(doc: &Rc<RefCell<BaseDocument>>) -> Result<(), String> {
    lay_out_doc(&mut doc.borrow_mut())
}

/// The same, for a caller already holding the borrow.
fn lay_out_doc(doc: &mut BaseDocument) -> Result<(), String> {
    prune_deep_nesting(doc);
    guard_layout(|| doc.resolve(0.0))
}

fn guard_layout(body: impl FnOnce()) -> Result<(), String> {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));

    outcome.map_err(|payload| {
        
        payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "the layout engine panicked".to_string())
    })
}

/// How deeply one page may nest elements.
pub(crate) const MAX_ELEMENT_DEPTH: usize = 512;

/// Cut every subtree deeper than [`MAX_ELEMENT_DEPTH`], and say how many were cut.
fn prune_deep_nesting(doc: &mut BaseDocument) -> usize {
    let mut doomed: Vec<usize> = Vec::new();
    {
        let root = doc.root_node().id;
        let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some((id, depth)) = stack.pop() {
            let Some(node) = doc.get_node(id) else { continue };
            if depth >= MAX_ELEMENT_DEPTH {
                doomed.push(id);
                continue;
            }
            for child in &node.children {
                stack.push((*child, depth + 1));
            }
        }
    }
    if doomed.is_empty() {
        return 0;
    }
    let cut = doomed.len();
    let mut mutator = doc.mutate();
    for id in doomed {
        mutator.remove_node(id);
    }
    cut
}

/// Remove `src` from every `<img>`, for a second attempt at markup that killed
/// the parser.
///
/// Deliberately blunt: this runs only after a panic has already proved the
/// markup is not survivable as written, and picking out *which* URL blitz
/// choked on would mean re-implementing its resolver to find out.
fn strip_image_sources(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let lowered = html.to_ascii_lowercase();
    let mut at = 0;
    while let Some(found) = lowered[at..].find("<img") {
        let start = at + found;
        let end = lowered[start..].find('>').map(|e| start + e + 1).unwrap_or(html.len());
        out.push_str(&html[at..start]);
        // Keep the tag and everything except its source attributes, so `alt`
        // survives, which is the part an agent was going to read anyway.
        for piece in html[start..end].split_ascii_whitespace() {
            let name = piece.split('=').next().unwrap_or("").to_ascii_lowercase();
            if matches!(name.as_str(), "src" | "srcset" | "data-src") {
                continue;
            }
            out.push_str(piece);
            out.push(' ');
        }
        out.push('>');
        at = end;
    }
    out.push_str(&html[at..]);
    out
}

/// The `Content-Type` a response declared, if it declared one.
fn declared_content_type(outcome: &crate::net::FetchOutcome) -> Option<String> {
    outcome
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone())
}

/// How many `<meta refresh>` hops to follow before giving up.
///
/// Low on purpose. A refresh chain longer than this is not a site pointing at
/// its real address, it is a loop or a tracker bounce.
const MAX_META_REFRESH_HOPS: usize = 3;

/// How long the whole script phase may take before this engine stops starting more of it.
const SCRIPT_PHASE_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

/// The line between "this page redirected" and "this page updates itself".
///
/// json.org serves a one-line document whose only content is a `<meta refresh>`
/// to `json-en.html`; the corpus recorded one line and no error, which reads as
/// a site with nothing on it. A refresh further out than this is a dashboard or
/// a scoreboard intending to reload later, and following it would be wrong.
const META_REFRESH_MAX_DELAY_SECONDS: u64 = 15;

/// Phrases that mean "you are being challenged", not "here is the page".
///
/// Matched against the raw markup because a challenge page renders to almost
/// nothing. The whole problem is that its *outline* is indistinguishable from
/// an empty page. Deliberately specific: a false positive would tell an agent
/// it was blocked by a site that simply had little to say.
const CHALLENGE_MARKERS: [&str; 10] = [
    "enable javascript and cookies to continue",
    "checking your browser before accessing",
    "verifying you are human",
    "cf-browser-verification",
    "please enable cookies",
    "ddos protection by",
    "attention required! | cloudflare",
    // pypi.org's search results, which say the page did not load rather than
    // that you were challenged, but mean the same thing to a reader: what
    // follows is not the page that was asked for.
    "javascript is disabled in your browser",
    "please enable javascript to proceed",
    "a required part of this site couldn't load",
];

fn challenge_marker(html: &str) -> Option<&'static str> {
    // Typographic apostrophes normalised to ASCII: pypi writes "couldn’t" with
    // U+2019, and a matcher that only knows `'` would miss it while looking
    // like it had checked.
    let lowered = html.to_ascii_lowercase().replace(['\u{2019}', '\u{02BC}'], "'");
    CHALLENGE_MARKERS
        .into_iter()
        .find(|marker| lowered.contains(marker))
}

/// The `<meta http-equiv="refresh">` target, if the document names one.
///
/// Parsed from the markup rather than the tree because this decides whether the
/// tree is worth building at all. The content attribute is `delay` optionally
/// followed by `; url=...`, with the quoting and spacing of twenty-five years of
/// hand-written HTML, so it is parsed leniently on purpose.
fn meta_refresh(html: &str, base: &Url) -> Option<(u64, Url)> {
    let lowered = html.to_ascii_lowercase();
    let mut from = 0usize;

    while let Some(found) = lowered[from..].find("<meta") {
        let start = from + found;
        let end = lowered[start..].find('>').map(|e| start + e)?;
        let tag = &html[start..end];
        from = end;

        let lowered_tag = tag.to_ascii_lowercase();
        if !lowered_tag.contains("http-equiv") || !lowered_tag.contains("refresh") {
            continue;
        }
        let Some(content) = attribute_value(tag, "content") else {
            continue;
        };

        let mut parts = content.splitn(2, ';');
        let delay = parts
            .next()
            .map(|d| d.trim().parse::<f64>().unwrap_or(0.0).max(0.0) as u64)
            .unwrap_or(0);
        let Some(rest) = parts.next() else { continue };
        let target = rest
            .trim()
            .trim_start_matches(|c: char| c.is_ascii_alphabetic())
            .trim_start()
            .trim_start_matches('=')
            .trim()
            .trim_matches(|c| c == '\'' || c == '"');
        if target.is_empty() {
            continue;
        }
        if let Ok(url) = base.join(target) {
            return Some((delay, url));
        }
    }
    None
}

/// One attribute out of a raw tag, single or double quoted or bare.
fn attribute_value(tag: &str, name: &str) -> Option<String> {
    let lowered = tag.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(found) = lowered[from..].find(name) {
        let at = from + found;
        from = at + name.len();
        // Must be a whole attribute name, not the tail of another one.
        if at > 0 && !lowered.as_bytes()[at - 1].is_ascii_whitespace() {
            continue;
        }
        let after = tag[from..].trim_start();
        let Some(after) = after.strip_prefix('=') else {
            continue;
        };
        let after = after.trim_start();
        let value = match after.chars().next() {
            Some(quote @ ('"' | '\'')) => after[1..].split(quote).next().unwrap_or(""),
            // Unquoted: ends at whitespace or at the end of the tag. Splitting
            // on whitespace alone kept the `>` and turned `content=ab>` into
            // the value "ab>".
            _ => after
                .split(|c: char| c.is_ascii_whitespace() || c == '>')
                .next()
                .unwrap_or(""),
        };
        return Some(value.to_string());
    }
    None
}

/// Flatten the renderer's premultiplied RGBA onto an opaque white canvas.
fn flatten_onto_white(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, H5iError> {
    let expected = width as usize * height as usize * 4;
    if rgba.len() < expected {
        return Err(H5iError::Internal(format!(
            "renderer produced {} bytes for a {width}x{height} frame, expected {expected}",
            rgba.len()
        )));
    }

    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    // `as_chunks` rather than `chunks_exact`: the chunk size is a constant, so
    // each pixel arrives as a `[u8; 4]` whose four indexes the compiler can
    // bounds-check once instead of four times per pixel.
    for pixel in rgba[..expected].as_chunks::<4>().0 {
        let backdrop = 255 - pixel[3];
        rgb.extend_from_slice(&[
            pixel[0].saturating_add(backdrop),
            pixel[1].saturating_add(backdrop),
            pixel[2].saturating_add(backdrop),
        ]);
    }
    Ok(rgb)
}

fn encode_jpeg(
    rgba: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<Vec<u8>, H5iError> {
    use image::codecs::jpeg::JpegEncoder;

    let rgb = flatten_onto_white(rgba, width, height)?;

    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, quality.clamp(1, 100))
        .encode(&rgb, width, height, image::ExtendedColorType::Rgb8)
        .map_err(|e| H5iError::Metadata(format!("failed to encode the frame: {e}")))?;
    Ok(jpeg)
}

/// Encode the screenshot, opaque.
///
/// Opaque rather than alpha-preserving on purpose: a screenshot of a page that
/// declared no background is not a transparency the caller asked for, it is a
/// canvas nobody painted, and handing it over as a hole means the image reads
/// differently against a light and a dark viewer. This is also what Chromium's
/// `captureScreenshot` does unless transparency is requested explicitly.
fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, H5iError> {
    use image::codecs::png::PngEncoder;
    use image::ImageEncoder;

    let rgb = flatten_onto_white(rgba, width, height)?;

    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
        .map_err(|e| H5iError::Metadata(format!("failed to encode the screenshot: {e}")))?;
    Ok(png)
}

/// Give every checkbox and radio its checked state *before* the first style pass, so `:checked`
/// can match during the cascade.
fn seed_checkbox_state(doc: &mut BaseDocument) {
    let inputs: Vec<(usize, bool)> = doc
        .tree()
        .iter()
        .filter_map(|(id, node)| {
            let element = node.data.downcast_element()?;
            if element.name.local != local_name!("input") {
                return None;
            }
            // Only the two types blitz builds checkbox state for. A `type` it
            // does not know is a text input there, and would be one here too.
            if !matches!(
                element.attr(local_name!("type")),
                Some(t) if t.eq_ignore_ascii_case("checkbox") || t.eq_ignore_ascii_case("radio")
            ) {
                return None;
            }
            // Never overwrite state that already exists: on a re-parse the
            // element may carry a value a click or a script put there.
            if !matches!(element.special_data, SpecialElementData::None) {
                return None;
            }
            Some((id, element.has_attr(local_name!("checked"))))
        })
        .collect();

    for (id, checked) in inputs {
        if let Some(element) = doc
            .get_node_mut(id)
            .and_then(|node| node.data.downcast_element_mut())
        {
            element.special_data = SpecialElementData::CheckboxInput(checked);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;
    use crate::receipt::MemorySink;

    #[test]
    fn the_prelude_is_warmed_only_where_the_bet_cannot_lose() {
        let scripted = PageOptions {
            script: true,
            ..Default::default()
        };
        let at = |s: &str| Url::parse(s).expect("url");

        // A remote page that may run script: the case the measurement covers,
        // where 92% of pages use the realm and the rest are slow enough to hide
        // the compile in anyway.
        assert!(worth_warming(&at("https://docs.example/guide"), &scripted));
        assert!(worth_warming(&at("http://docs.example/guide"), &scripted));

        // Scripting off means no realm is ever built, so this would be waste on
        // every page rather than on some.
        assert!(!worth_warming(&at("https://docs.example/"), &PageOptions::default()));

        // Nothing local. These answer in about a millisecond, so a scriptless
        // one would pay the whole compile as added latency, and local pages are
        // exactly what the corpus measurement does not cover.
        for local in [
            "http://localhost:3000/",
            "http://localhost./",
            "http://dev.localhost/",
            "http://127.0.0.1:8080/app",
            "http://127.13.9.4/",
            "http://[::1]:5173/",
            "file:///tmp/page.html",
            "data:text/html,<p>hi",
        ] {
            assert!(
                !worth_warming(&at(local), &scripted),
                "{local} has no wait to hide a 67ms compile in"
            );
        }

        // A name that merely looks like loopback is not loopback, and must not
        // lose the optimisation by string-matching.
        assert!(worth_warming(&at("http://127.0.0.1.evil.test/"), &scripted));
        assert!(worth_warming(&at("https://localhost.evil.test/"), &scripted));
    }

    fn page_from(html: &str, policy: Policy, sink: Arc<MemorySink>) -> Page {
        let broker = crate::net::LocalBroker::new(policy, sink, None).expect("broker");
        let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
        Page::from_html(
            html,
            &Url::parse("https://example.com/").unwrap(),
            broker,
            fonts,
            PageOptions {
                width: 400,
                height: 200,
                ..Default::default()
            },
        )
    }

    /// An element the page made clickable is one an agent can reach (#609).
    ///
    /// A `<div onclick=…>` had no role, so no `@ref`, so no verb could fire the
    /// handler. Its own role word: it is not a button.
    #[test]
    fn an_element_made_clickable_by_a_handler_attribute_takes_a_ref() {
        let sink = Arc::new(MemorySink::new());
        let page = page_from(
            r#"<!doctype html><html><body>
                <div id="d" onclick="run()">Delete everything</div>
                <span onmouseover="hint()">just a hint</span>
                <button onclick="run()">Run</button>
            </body></html>"#,
            Policy::new(),
            sink,
        );

        let snapshot = page.snapshot();
        let rendered = snapshot.render();
        assert!(
            rendered.contains("clickable \"Delete everything\""),
            "a div with an inline click handler must be addressable:\n{rendered}"
        );
        // Pointer activation only. `click` does not apply to a hover handler,
        // and a ref would be offering a verb that does nothing.
        assert!(!rendered.contains("just a hint\" [ref"), "{rendered}");
        // And nothing that has a role of its own is relabelled by this.
        assert!(rendered.contains("button \"Run\""), "{rendered}");
        let roles: Vec<&str> = snapshot.refs.iter().map(|r| r.role.as_str()).collect();
        assert_eq!(roles, vec!["clickable", "button"]);
    }

    /// ...and it does not swallow the structure it wraps: a clickable card read
    /// as one leaf line loses the heading and link the reader came for.
    #[test]
    fn a_clickable_wrapper_still_lets_its_contents_speak() {
        let sink = Arc::new(MemorySink::new());
        let page = page_from(
            r#"<!doctype html><html><body>
                <div onclick="open()"><h2>Invoice 41</h2><p>Due Friday</p></div>
            </body></html>"#,
            Policy::new(),
            sink,
        );

        let rendered = page.snapshot().render();
        assert!(rendered.contains("heading2 \"Invoice 41\""), "{rendered}");
        assert!(rendered.contains("paragraph \"Due Friday\""), "{rendered}");
    }

    #[test]
    fn a_document_becomes_an_outline_with_refs_on_the_actionable_parts() {
        let sink = Arc::new(MemorySink::new());
        let page = page_from(
            r#"<!doctype html><html><head><title>Docs</title></head><body>
                <h1>Getting started</h1>
                <p>Install it first.</p>
                <div><span><a href="/guide">Read the guide</a></span></div>
                <button>Run</button>
            </body></html>"#,
            Policy::new(),
            sink,
        );

        let snapshot = page.snapshot();
        let rendered = snapshot.render();

        assert_eq!(snapshot.title, "Docs");
        assert!(rendered.contains("heading1 \"Getting started\""), "{rendered}");
        assert!(rendered.contains("paragraph \"Install it first.\""), "{rendered}");
        // The link is wrapped in div>span, but the outline should not make an
        // agent walk through anonymous containers to find it.
        assert!(rendered.contains("link \"Read the guide\""), "{rendered}");
        assert_eq!(snapshot.refs.len(), 2, "link and button take refs");
        assert_eq!(snapshot.refs[0].role, "link");
        assert_eq!(snapshot.refs[1].role, "button");
    }

    #[test]
    fn actionable_elements_inside_prose_still_get_refs() {
        // The bug this pins: treating `p` and `label` as leaves and not
        // recursing lost the link inside the sentence and the input inside
        // the label. The two things an agent is most likely to want.
        let sink = Arc::new(MemorySink::new());
        let page = page_from(
            r#"<!doctype html><body>
                 <p>See the <a href="/guide">guide</a> for more.</p>
                 <label>Email <input type="email" placeholder="you@example.com"></label>
                 <label><input type="checkbox"> Subscribe</label>
                 <input type="hidden" name="csrf" value="secret-token">
               </body>"#,
            Policy::new(),
            sink,
        );

        let snapshot = page.snapshot();
        let roles: Vec<&str> = snapshot.refs.iter().map(|r| r.role.as_str()).collect();
        assert_eq!(roles, vec!["link", "textbox", "checkbox"], "{roles:?}");

        let rendered = snapshot.render();
        // The paragraph keeps its whole sentence...
        assert!(rendered.contains("paragraph \"See the guide for more.\""), "{rendered}");
        // ...and the link is addressable underneath it rather than lost in it.
        assert!(rendered.contains("link \"guide\" [ref=e1]"), "{rendered}");
        // Named by its `<label>`, which beats the placeholder: that is the
        // order the accessible-name computation specifies, and it is the
        // better handle. "Email" is what a person sees the field called, and
        // the placeholder is example text that a redesign will change.
        assert!(rendered.contains("textbox \"Email\""), "{rendered}");
        // The placeholder is still the fallback where there is no label.
        let bare = page_from(
            r#"<!doctype html><body><input type="email" placeholder="you@example.com"></body>"#,
            Policy::new(),
            Arc::new(MemorySink::new()),
        );
        assert!(
            bare.snapshot().render().contains("textbox \"you@example.com\""),
            "{}",
            bare.snapshot().render()
        );
        // The hidden CSRF field is not something to act on, and its value is
        // not something to put in front of a model.
        assert!(!rendered.contains("secret-token"), "{rendered}");
    }

    #[test]
    fn an_image_is_named_by_its_alt_text() {
        let sink = Arc::new(MemorySink::new());
        let page = page_from(
            r#"<!doctype html><body><img src="/d.png" alt="Architecture diagram"></body>"#,
            Policy::new().allow("example.com"),
            sink,
        );
        let snapshot = page.snapshot();
        assert_eq!(snapshot.refs.len(), 1);
        assert_eq!(snapshot.refs[0].name, "Architecture diagram");
    }

    #[test]
    fn script_and_style_never_reach_the_snapshot() {
        let sink = Arc::new(MemorySink::new());
        let page = page_from(
            r#"<!doctype html><html><head>
                 <style>.a { color: red }</style>
               </head><body>
                 <script>var secret = "do not exfiltrate";</script>
                 <p>Visible</p>
               </body></html>"#,
            Policy::new(),
            sink,
        );

        let rendered = page.snapshot().render();
        assert!(rendered.contains("Visible"));
        assert!(!rendered.contains("do not exfiltrate"), "{rendered}");
        assert!(!rendered.contains("color: red"), "{rendered}");
    }

    #[test]
    fn a_third_party_subresource_is_denied_and_the_page_still_renders() {
        // This is the whole product in one test: the tracker never loads, the
        // decision is recorded, and the page is not collateral damage.
        let sink = Arc::new(MemorySink::new());
        let mut page = page_from(
            r#"<!doctype html><html><body>
                 <link rel="stylesheet" href="https://cdn.tracker.test/s.css">
                 <img src="https://cdn.tracker.test/pixel.gif">
                 <h1>Still here</h1>
               </body></html>"#,
            Policy::new().allow("example.com"),
            sink.clone(),
        );

        let denied = sink.denied_urls();
        assert!(
            denied.iter().any(|u| u.contains("s.css")),
            "the stylesheet should be refused: {denied:?}"
        );
        assert!(
            denied.iter().any(|u| u.contains("pixel.gif")),
            "the tracking pixel should be refused: {denied:?}"
        );
        assert!(sink.fetched_urls().is_empty(), "nothing should reach the wire");

        assert!(page.snapshot().render().contains("Still here"));

        // And the screenshot is a real frame, not the blank one you get when a
        // denied resource is left pending forever (see `net`'s module docs).
        let png = page.screenshot_png().expect("screenshot encodes");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "png magic");
        assert!(png.len() > 100, "a blank-refusal render would be tiny");
    }

    #[test]
    fn screenshot_dimensions_follow_the_viewport() {
        let sink = Arc::new(MemorySink::new());
        let mut page = page_from("<!doctype html><p>hi</p>", Policy::new(), sink);
        let png = page.screenshot_png().expect("screenshot");

        // PNG IHDR carries width/height big-endian at a fixed offset.
        let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        assert_eq!((width, height), (400, 200));
    }

    /// Decode an encoded frame so a test can talk about pixels.
    fn decoded(encoded: &[u8]) -> image::RgbImage {
        image::load_from_memory(encoded)
            .expect("the frame decodes")
            .to_rgb8()
    }

    /// The bottom-right corner: past the end of any content these tests lay
    /// out, so it is the canvas and nothing else. Sampling there is what keeps
    /// these assertions independent of whether the host has fonts.
    const CANVAS: (u32, u32) = (399, 199);

    #[test]
    fn a_page_that_declares_no_background_renders_white_not_black() {
        let sink = Arc::new(MemorySink::new());
        let mut page = page_from("<!doctype html><body><p>hi</p></body>", Policy::new(), sink);

        let png = decoded(&page.screenshot_png().expect("screenshot"));
        assert_eq!(
            png.get_pixel(CANVAS.0, CANVAS.1).0,
            [255, 255, 255],
            "an undeclared background is the canvas, and the canvas is white"
        );

        // The JPEG is the one that was broken: it has no alpha channel to hide
        // an unpainted pixel in, so `(0,0,0,0)` became black and the default
        // black text became invisible on it.
        let jpeg = decoded(&page.screenshot_jpeg(85).expect("frame"));
        let corner = jpeg.get_pixel(CANVAS.0, CANVAS.1).0;
        assert!(
            corner.iter().all(|&c| c > 250),
            "the live view's frame should be white here, got {corner:?}"
        );
    }

    #[test]
    fn a_checked_input_styles_its_siblings() {
        // The script-free tab pattern: a radio, and a panel that only becomes
        // visible because a `:checked ~` rule says so. blitz decides an input's
        // checked state during *layout construction*, which runs after selector
        // matching, so before `seed_checkbox_state` every `:checked` rule lost
        // and this page painted a white square where the panel should be.
        //
        // Asserted in pixels rather than in text, because text extraction does
        // not honour `display: none` and would report both panels either way.
        let sink = Arc::new(MemorySink::new());
        let mut page = page_from(
            "<!doctype html><style>html,body{margin:0}\
             .panel{display:none;width:400px;height:200px;background:#000}\
             #on:checked ~ .panel{display:block}</style>\
             <input type=\"radio\" name=\"t\" id=\"on\" checked><div class=\"panel\"></div>",
            Policy::new(),
            sink,
        );

        let painted = decoded(&page.screenshot_png().expect("screenshot"));
        assert_eq!(
            painted.get_pixel(CANVAS.0, CANVAS.1).0,
            [0, 0, 0],
            "`:checked ~ .panel` should have made the black panel visible"
        );
    }

    #[test]
    fn an_unchecked_input_does_not_style_its_siblings() {
        // The other half, so the test above cannot pass by making everything
        // match: the same page with the attribute removed must stay blank.
        let sink = Arc::new(MemorySink::new());
        let mut page = page_from(
            "<!doctype html><style>html,body{margin:0}\
             .panel{display:none;width:400px;height:200px;background:#000}\
             #on:checked ~ .panel{display:block}</style>\
             <input type=\"radio\" name=\"t\" id=\"on\"><div class=\"panel\"></div>",
            Policy::new(),
            sink,
        );

        let painted = decoded(&page.screenshot_png().expect("screenshot"));
        assert_eq!(
            painted.get_pixel(CANVAS.0, CANVAS.1).0,
            [255, 255, 255],
            "nothing is checked, so the panel should still be display:none"
        );
    }

    #[test]
    fn a_checkbox_keeps_its_checked_state_through_layout() {
        // `seed_checkbox_state` writes the state blitz would have written
        // later. If the two ever disagreed, the cascade and the layout would be
        // describing different pages; this pins that they agree for a checkbox
        // as well as a radio.
        let sink = Arc::new(MemorySink::new());
        let mut page = page_from(
            "<!doctype html><style>html,body{margin:0}\
             .panel{display:none;width:400px;height:200px;background:#000}\
             input:checked ~ .panel{display:block}</style>\
             <input type=\"checkbox\" checked><div class=\"panel\"></div>",
            Policy::new(),
            sink,
        );

        let painted = decoded(&page.screenshot_png().expect("screenshot"));
        assert_eq!(
            painted.get_pixel(CANVAS.0, CANVAS.1).0,
            [0, 0, 0],
            "a checked checkbox should match `:checked` during the cascade too"
        );
    }

    #[test]
    fn black_content_stays_visible_against_the_canvas() {
        // Black-on-black, stated without needing a glyph: the default text
        // colour painted as a box has to survive the flatten as black while
        // the canvas around it stays white.
        let sink = Arc::new(MemorySink::new());
        let mut page = page_from(
            "<!doctype html><style>html,body{margin:0}\
             div{width:100px;height:100px;background:#000}</style><div></div>",
            Policy::new(),
            sink,
        );

        let img = decoded(&page.screenshot_jpeg(90).expect("frame"));
        let inside = img.get_pixel(50, 50).0;
        let outside = img.get_pixel(CANVAS.0, CANVAS.1).0;
        assert!(inside.iter().all(|&c| c < 5), "the black box: {inside:?}");
        assert!(outside.iter().all(|&c| c > 250), "the canvas: {outside:?}");
    }

    #[test]
    fn a_translucent_fill_composites_onto_white_rather_than_darkening() {
        // The renderer's buffer is premultiplied, so a 50%-red fill arrives as
        // (128,0,0,128). Written out as straight alpha that reads as a dark
        // red; composited onto white it is the colour the page actually shows.
        let sink = Arc::new(MemorySink::new());
        let mut page = page_from(
            "<!doctype html><style>html,body{margin:0}\
             div{width:200px;height:100px;background:rgba(255,0,0,0.5)}</style><div></div>",
            Policy::new(),
            sink,
        );

        let img = decoded(&page.screenshot_png().expect("screenshot"));
        let [r, g, b] = img.get_pixel(100, 50).0;
        assert!(r > 250, "the red channel stays full, got {r}");
        assert!(
            (120..=135).contains(&g) && (120..=135).contains(&b),
            "green and blue should be lifted halfway to white, got {g} and {b}"
        );
    }

    #[test]
    fn text_extraction_drops_the_structure_and_keeps_the_prose() {
        let sink = Arc::new(MemorySink::new());
        let page = page_from(
            "<!doctype html><body><h1>Title</h1><div><p>Body copy.</p></div></body>",
            Policy::new(),
            sink,
        );
        let text = page.text();
        assert!(text.contains("Title"));
        assert!(text.contains("Body copy."));
    }
}

#[cfg(test)]
#[cfg(test)]
mod frame_tests {
    use super::*;
    use crate::policy::Policy;
    use crate::receipt::MemorySink;

    fn page_with(html: &str, policy: Policy) -> (Page, Arc<crate::net::LocalBroker>) {
        let broker =
            crate::net::LocalBroker::new(policy, Arc::new(MemorySink::new()), None).expect("broker");
        let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
        let factory = PageFactory::new(broker.clone(), fonts.sources.clone(), PageOptions::default());
        let mut page = Page::from_bytes(
            html.as_bytes(),
            Some("text/html"),
            &Url::parse("https://host.example/").unwrap(),
            broker.clone(),
            factory.fonts(),
            PageOptions::default(),
        );
        let dyn_broker: Arc<dyn Broker> = broker.clone();
        load_frames(&mut page, &dyn_broker);
        (factory.finish(page).expect("finish"), broker)
    }

    /// One line per request, so a hung test names the request it hung on.
    fn one_shot_server(body: &'static str) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..4 {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                        break;
                    }
                }
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        port
    }

    #[test]
    fn a_srcdoc_frame_is_flattened_and_its_script_never_runs() {
        // §B21: the frame's *content* is readable and actionable; its script
        // is the boundary and is stripped, not executed.
        let (page, _broker) = page_with(
            r#"<html><body><p>host</p>
               <iframe srcdoc="<form><input name=card><button>Pay</button></form><script>document.title='ran'</script>"></iframe>
               </body></html>"#,
            Policy::new(),
        );
        let rendered = page.snapshot().render();
        assert!(rendered.contains("Pay"), "frame content missing:\n{rendered}");
        assert!(
            !rendered.contains("document.title"),
            "script text leaked into the outline:\n{rendered}"
        );
        assert!(
            page.notes.iter().any(|n| n.contains("loaded as content")),
            "the flattening is stated, not silent: {:?}",
            page.notes
        );
    }

    /// §B21 says a flattened frame's scripts never run.
    #[test]
    fn a_deeply_nested_page_does_not_take_the_process_down() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                let depth = 5_000;
                let mut html = String::with_capacity(depth * 12);
                html.push_str("<html><body><p>shallow</p>");
                for _ in 0..depth {
                    html.push_str("<div>");
                }
                html.push_str("deep");
                for _ in 0..depth {
                    html.push_str("</div>");
                }
                html.push_str("</body></html>");

                let (page, _broker) = page_with(&html, Policy::new());
                let rendered = page.snapshot().render();
                assert!(!rendered.is_empty());
                assert!(
                    page.notes.iter().any(|n| n.contains("nests elements more than")),
                    "the bound is said, not silently applied: {:?}",
                    page.notes
                );
                // The content above the bound is still there.
                assert!(rendered.contains("shallow"), "{rendered}");
            })
            .expect("a thread")
            .join()
            .expect("the engine survives a page it cannot lay out in full");
    }

    /// The parse-time bound is not the only door into a deep tree: script can
    /// build one after it, and the next layout walks whatever is there.
    #[test]
    fn a_script_built_deep_tree_does_not_take_the_process_down() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                let broker = crate::net::LocalBroker::new(
                    Policy::new(),
                    Arc::new(MemorySink::new()),
                    None,
                )
                .expect("broker");
                let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
                let factory = PageFactory::new(
                    broker,
                    fonts.sources.clone(),
                    PageOptions {
                        script: true,
                        ..PageOptions::default()
                    },
                );
                let page = factory.from_html(
                    "<html><body><div id='host'></div><script>\
                     document.getElementById('host').innerHTML = '<div>'.repeat(20000);\
                     </script></body></html>",
                    &Url::parse("https://host.example/").unwrap(),
                );
                let rendered = page.snapshot().render();
                assert!(!rendered.is_empty());
            })
            .expect("a thread")
            .join()
            .expect("the engine survives a tree its own script built");
    }

    /// And the serialiser is a third door: `innerHTML` walks the tree by
    /// recursion too, and script can read it back in the same turn it built the
    /// tree in, before any layout, and so before the layout-time bound.
    #[test]
    fn reading_back_a_deep_tree_does_not_take_the_process_down() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                let broker = crate::net::LocalBroker::new(
                    Policy::new(),
                    Arc::new(MemorySink::new()),
                    None,
                )
                .expect("broker");
                let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
                let factory = PageFactory::new(
                    broker,
                    fonts.sources.clone(),
                    PageOptions {
                        script: true,
                        ..PageOptions::default()
                    },
                );
                let page = factory.from_html(
                    "<html><body><div id='host'></div><p id='out'>none</p><script>\
                     const h = document.getElementById('host');\
                     h.innerHTML = '<div>'.repeat(20000);\
                     document.getElementById('out').textContent = 'len=' + h.innerHTML.length;\
                     </script></body></html>",
                    &Url::parse("https://host.example/").unwrap(),
                );
                let rendered = page.snapshot().render();
                assert!(rendered.contains("len="), "the script finished:\n{rendered}");
            })
            .expect("a thread")
            .join()
            .expect("the engine survives reading back a tree its own script built");
    }

    /// What the bound costs a page that is nowhere near it. Printed rather
    /// than asserted: a threshold here would fail on a loaded machine and say
    /// nothing about the engine.
    #[test]
    #[ignore = "a measurement, not an assertion"]
    fn the_depth_walk_costs_little_against_a_layout() {
        let path = std::env::var("H5I_PERF_PAGE").expect("set H5I_PERF_PAGE");
        let html = std::fs::read_to_string(path).expect("read");
        let broker =
            crate::net::LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None)
                .expect("broker");
        let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
        let factory =
            PageFactory::new(broker.clone(), fonts.sources.clone(), PageOptions::default());
        let page = factory.from_html(&html, &Url::parse("https://host.example/").unwrap());

        let mut nodes = 0usize;
        {
            let doc = page.doc.borrow();
            let mut stack = vec![doc.root_node().id];
            while let Some(id) = stack.pop() {
                let Some(node) = doc.get_node(id) else { continue };
                nodes += 1;
                stack.extend(node.children.iter().copied());
            }
        }

        let walk = {
            let started = std::time::Instant::now();
            for _ in 0..100 {
                let mut doc = page.doc.borrow_mut();
                prune_deep_nesting(&mut doc);
            }
            started.elapsed() / 100
        };
        let layout = {
            let started = std::time::Instant::now();
            for _ in 0..10 {
                let _ = guard_layout(|| page.doc.borrow_mut().resolve(0.0));
            }
            started.elapsed() / 10
        };
        eprintln!("nodes={nodes} prune={walk:?} layout={layout:?}");
    }

    /// The fourth door: `getComputedStyle` resolves style on demand, so a
    /// style read reaches layout on a tree script has just built, before the
    /// settle loop does, and so before the bound the settle loop applies.
    #[test]
    fn a_style_read_on_a_deep_tree_does_not_take_the_process_down() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                let broker = crate::net::LocalBroker::new(
                    Policy::new(),
                    Arc::new(MemorySink::new()),
                    None,
                )
                .expect("broker");
                let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
                let factory = PageFactory::new(
                    broker,
                    fonts.sources.clone(),
                    PageOptions {
                        script: true,
                        ..PageOptions::default()
                    },
                );
                let page = factory.from_html(
                    "<html><body><div id='host'></div><p id='out'>none</p><script>\
                     const h = document.getElementById('host');\
                     h.innerHTML = '<div>'.repeat(20000);\
                     document.getElementById('out').textContent =\
                       'w=' + getComputedStyle(h).width;\
                     </script></body></html>",
                    &Url::parse("https://host.example/").unwrap(),
                );
                let rendered = page.snapshot().render();
                assert!(rendered.contains("w="), "the script finished:\n{rendered}");
            })
            .expect("a thread")
            .join()
            .expect("the engine survives a style read on a tree its own script built");
    }

    /// A fifth candidate, found by sweeping for the class rather than by
    /// hitting it: `collect_text_content` recurses with no depth of its own,
    /// and `innerText` reaches it for an element that is not rendered.
    #[test]
    fn reading_inner_text_of_a_deep_hidden_tree_does_not_take_the_process_down() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                let broker = crate::net::LocalBroker::new(
                    Policy::new(),
                    Arc::new(MemorySink::new()),
                    None,
                )
                .expect("broker");
                let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
                let factory = PageFactory::new(
                    broker,
                    fonts.sources.clone(),
                    PageOptions {
                        script: true,
                        ..PageOptions::default()
                    },
                );
                let page = factory.from_html(
                    "<html><body><div id='host' style='display:none'></div>\
                     <p id='out'>none</p><script>\
                     const h = document.getElementById('host');\
                     h.innerHTML = '<div>'.repeat(20000) + 'deep';\
                     document.getElementById('out').textContent = 'len=' + h.innerText.length;\
                     </script></body></html>",
                    &Url::parse("https://host.example/").unwrap(),
                );
                let rendered = page.snapshot().render();
                assert!(rendered.contains("len="), "the script finished:\n{rendered}");
            })
            .expect("a thread")
            .join()
            .expect("the engine survives an innerText read of a deep hidden tree");
    }

    /// An ordinary page is nowhere near the bound, so it is untouched and says
    /// nothing about depth. The deepest page in this project's corpus is under
    /// 40 levels.
    #[test]
    fn an_ordinarily_nested_page_is_not_pruned_or_annotated() {
        let mut html = String::from("<html><body>");
        for _ in 0..64 {
            html.push_str("<div>");
        }
        html.push_str("<p>content</p>");
        for _ in 0..64 {
            html.push_str("</div>");
        }
        html.push_str("</body></html>");

        let (page, _broker) = page_with(&html, Policy::new());
        let rendered = page.snapshot().render();
        assert!(rendered.contains("content"), "{rendered}");
        assert!(
            !page.notes.iter().any(|n| n.contains("nests elements")),
            "{:?}",
            page.notes
        );
    }

    #[test]
    fn a_frames_inline_handlers_are_script_and_do_not_run_either() {
        let broker = crate::net::LocalBroker::new(
            Policy::new(),
            Arc::new(MemorySink::new()),
            None,
        )
        .expect("broker");
        let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
        let options = PageOptions {
            script: true,
            ..PageOptions::default()
        };
        let factory = PageFactory::new(broker.clone(), fonts.sources.clone(), options.clone());
        let html = r#"<html><body><p id="mark">host</p>
               <iframe srcdoc="<span id='trap' onclick='document.getElementById(&quot;mark&quot;).textContent = &quot;pwned&quot;'>inner</span>"></iframe>
               </body></html>"#;
        let mut page = Page::from_bytes(
            html.as_bytes(),
            Some("text/html"),
            &Url::parse("https://host.example/").unwrap(),
            broker.clone(),
            factory.fonts(),
            options,
        );
        let dyn_broker: Arc<dyn Broker> = broker.clone();
        load_frames(&mut page, &dyn_broker);
        let page = factory.finish(page).expect("finish");
        let mut page = page;
        let trap = page
            .dom()
            .borrow()
            .query_selector_all("#trap")
            .ok()
            .and_then(|ids| ids.into_iter().next())
            .expect("the frame content is in the tree");
        page.dispatch_event(trap, "click");
        let rendered = page.snapshot().render();
        assert!(rendered.contains("inner"), "frame content missing:\n{rendered}");
        assert!(
            !rendered.contains("pwned"),
            "a frame's inline handler ran in the host realm:\n{rendered}"
        );
    }

    /// The same boundary, on the other road into it. A `javascript:` frame
    /// *source* was already refused by name; a `javascript:` link inside the
    /// flattened content was not, and clicking one is the same script in the
    /// same realm.
    #[test]
    fn a_javascript_url_inside_a_frame_is_defused_however_it_is_spelled() {
        assert!(is_javascript_url("javascript:alert(1)"));
        assert!(is_javascript_url("JaVaScRiPt:alert(1)"));
        // The parser strips ASCII whitespace and control characters from a URL
        // before resolving it, so these are `javascript:` URLs too and a plain
        // `starts_with` is how one gets missed.
        assert!(is_javascript_url("  javascript:alert(1)"));
        assert!(is_javascript_url("java\tscript:alert(1)"));
        assert!(is_javascript_url("java\nscript:alert(1)"));
        assert!(is_javascript_url("java\u{0}script:alert(1)"));
        assert!(!is_javascript_url("https://example.com/javascript:x"));
        assert!(!is_javascript_url("/not-javascript"));

        assert!(defuse_attribute("onclick", "x()"));
        assert!(defuse_attribute("ONLOAD", "x()"));
        assert!(defuse_attribute("href", " javascript:x()"));
        assert!(!defuse_attribute("href", "https://example.com/"));
        // Not every short name beginning with `on` is a handler, and `on` is
        // an attribute of its own on some elements.
        assert!(!defuse_attribute("on", "x"));
    }

    /// The jar is bounded to the origin currently loaded, but the drop happened
    /// in `finish`, at the *end* of the navigation, and a page's frames and
    /// subresources are fetched before that. So arriving at `evil.example` with
    /// `bank.example`'s session still in the jar, and being told to fetch
    /// `bank.example` in a frame, carried the credential, and §B21 then
    /// flattened the authenticated answer into the outline the agent reads.
    #[test]
    fn a_page_cannot_frame_the_previous_origin_with_its_session_still_in_the_jar() {
        let body = "<p>the account page</p>";
        let bank = one_shot_server(body);
        let broker = crate::net::LocalBroker::new(
            Policy::new(),
            Arc::new(MemorySink::new()),
            None,
        )
        .expect("broker");
        // The previous navigation's session, held for the origin it belongs to.
        broker.jar().store(
            &Url::parse(&format!("http://127.0.0.1:{bank}/")).unwrap(),
            ["sid=secret"],
        );
        assert_eq!(broker.jar().len(), 1);

        let html = format!(
            r#"<html><body><iframe src="http://127.0.0.1:{bank}/account"></iframe></body></html>"#
        );
        let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
        let factory =
            PageFactory::new(broker.clone(), fonts.sources.clone(), PageOptions::default());
        // A *different* origin: a different loopback port is a different
        // origin, and this one is the page being read.
        let evil = Url::parse("http://127.0.0.2:9/").unwrap();
        let mut page = Page::from_bytes(
            html.as_bytes(),
            Some("text/html"),
            &evil,
            broker.clone(),
            factory.fonts(),
            PageOptions::default(),
        );
        let dyn_broker: Arc<dyn Broker> = broker.clone();
        load_frames(&mut page, &dyn_broker);
        let _ = factory.finish(page);

        let carried: usize = broker
            .records()
            .iter()
            .filter(|r| r.url.contains("/account"))
            .filter_map(|r| r.cookies_sent)
            .sum();
        assert_eq!(
            carried, 0,
            "the previous origin's session rode along on a frame fetch: {:?}",
            broker.records()
        );
    }

    /// A flattened frame is somebody else's document appearing inline in the
    /// agent's reading of this one. Counting them was not enough: for an engine
    /// whose claim is that a reader can tell where bytes came from, "three
    /// frames were loaded" leaves a third party writing into the agent's
    /// reading with no way to see whose words they are.
    #[test]
    fn the_note_names_the_origin_a_frames_content_came_from() {
        let body = "<p>inner page</p>";
        let port = one_shot_server(body);
        let html = format!(
            r#"<html><body><iframe src="http://127.0.0.1:{port}/inner"></iframe></body></html>"#
        );
        let broker =
            crate::net::LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None)
                .expect("broker");
        let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
        let factory =
            PageFactory::new(broker.clone(), fonts.sources.clone(), PageOptions::default());
        let mut page = Page::from_bytes(
            html.as_bytes(),
            Some("text/html"),
            &Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(),
            broker.clone(),
            factory.fonts(),
            PageOptions::default(),
        );
        let dyn_broker: Arc<dyn Broker> = broker.clone();
        load_frames(&mut page, &dyn_broker);
        let page = factory.finish(page).expect("finish");

        let note = page
            .notes
            .iter()
            .find(|n| n.contains("loaded as content"))
            .expect("the flattening is stated");
        assert!(
            note.contains(&format!("http://127.0.0.1:{port}")),
            "the origin is named: {note}"
        );
        assert!(
            note.contains("another origin's page"),
            "and what that means is said: {note}"
        );
    }

    #[test]
    fn a_frame_fetch_is_receipted_under_its_own_initiator() {
        let body = "<p>inner page</p>";
        let port = one_shot_server(body);
        // The host page is itself on loopback, because the document-origin
        // rule is part of what is under test: a *web* page's frame may not
        // reach the dev server (the sibling test proves that side), while the
        // dev server's own page framing itself is the everyday case.
        let html = format!(
            r#"<html><body><iframe src="http://127.0.0.1:{port}/inner"></iframe></body></html>"#
        );
        let broker = crate::net::LocalBroker::new(
            Policy::new(),
            Arc::new(MemorySink::new()),
            None,
        )
        .expect("broker");
        let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
        let factory =
            PageFactory::new(broker.clone(), fonts.sources.clone(), PageOptions::default());
        let mut page = Page::from_bytes(
            html.as_bytes(),
            Some("text/html"),
            &Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(),
            broker.clone(),
            factory.fonts(),
            PageOptions::default(),
        );
        let dyn_broker: Arc<dyn Broker> = broker.clone();
        load_frames(&mut page, &dyn_broker);
        let page = factory.finish(page).expect("finish");
        let rendered = page.snapshot().render();
        assert!(rendered.contains("inner page"), "{rendered}");
        let records = broker.records();
        let frame_record = records
            .iter()
            .find(|r| r.url.contains("/inner"))
            .expect("the frame fetch is in the log");
        assert_eq!(
            serde_json::to_value(frame_record.initiator).unwrap(),
            serde_json::json!("frame"),
            "an auditor asking \"did this page pull in another document\" gets the answer by name"
        );
    }

    #[test]
    fn a_cross_origin_frame_is_refused_by_the_allowlist_and_says_so() {
        let (page, broker) = page_with(
            r#"<html><body><p>host</p>
               <iframe src="https://tracker.example/pixel.html"></iframe></body></html>"#,
            Policy::new(),
        );
        // The note *names* the refused URL, that is the point, so the check
        // for leaked content has to look inside the fence, not at the render
        // as a whole, which carries the note.
        let rendered = page.snapshot().render();
        let fenced = rendered
            .split("BEGIN UNTRUSTED PAGE CONTENT")
            .nth(1)
            .unwrap_or(&rendered);
        assert!(!fenced.contains("pixel"), "refused content leaked:\n{rendered}");
        assert!(
            page.notes
                .iter()
                .any(|n| n.contains("could not be loaded") && n.contains("tracker.example")),
            "the refusal is a note the agent reads, not an empty frame: {:?}",
            page.notes
        );
        // And it is a *recorded* refusal: the deny is in the log.
        assert!(
            broker.records().iter().any(|r| r.url.contains("tracker.example") && !r.allowed),
            "the refusal must be receipted"
        );
    }

    #[test]
    fn a_web_pages_frame_may_not_reach_the_dev_server() {
        // Found by this test suite's own first draft, which put a loopback
        // frame under a web-origin host page and watched the document-origin
        // rule refuse it. That is §B3.1 doing its job on a new road: a page
        // from the open web embedding `<iframe src=http://127.0.0.1:3000>`
        // would otherwise read the box's dev server through the graft.
        let (page, _broker) = page_with(
            r#"<html><body><iframe src="http://127.0.0.1:3000/source"></iframe></body></html>"#,
            Policy::new(),
        );
        assert!(
            page.notes
                .iter()
                .any(|n| n.contains("could not be loaded") && n.contains("loopback")),
            "{:?}",
            page.notes
        );
    }

    #[test]
    fn a_javascript_frame_is_refused_by_name() {
        // Script by another road, and the boundary applies to the road too.
        let (page, _broker) = page_with(
            r#"<html><body><iframe src="javascript:document.title='owned'"></iframe></body></html>"#,
            Policy::new(),
        );
        assert!(
            page.notes.iter().any(|n| n.contains("javascript:")),
            "{:?}",
            page.notes
        );
    }

    #[test]
    fn hidden_content_inside_a_frame_stays_hidden() {
        // The injection defence must survive the styling gap: Blitz styles
        // nothing inside a frame, so "no styles" cannot mean "hidden" there,
        // but the vectors a page controls (`hidden`, inline display:none,
        // aria-hidden) keep their teeth.
        let (page, _broker) = page_with(
            r#"<html><body>
               <iframe srcdoc="<p>shown</p><p hidden>h-attr</p><p style='display: none'>h-style</p><div aria-hidden=true>h-aria</div>"></iframe>
               </body></html>"#,
            Policy::new(),
        );
        let rendered = page.snapshot().render();
        assert!(rendered.contains("shown"), "{rendered}");
        for leaked in ["h-attr", "h-style", "h-aria"] {
            assert!(!rendered.contains(leaked), "`{leaked}` leaked:\n{rendered}");
        }
    }
}

#[cfg(test)]
mod navigation_origin_tests {
    use super::*;
    use crate::policy::Policy;
    use crate::receipt::MemorySink;

    /// The jar bound `Jar::retain_origin` documents, "at any moment the jar
    /// holds only cookies for the origin currently loaded", rested on a drop
    /// `open` performed and `open_submission` did not.
    ///
    /// The jar is host-scoped, so arriving at another origin with the previous
    /// one's cookies still in it means the new page's script can `fetch` the
    /// previous origin *with its credentials*: the cross-origin credentialed
    /// read the bound exists to make impossible, reached by pressing submit.
    #[test]
    fn a_form_submission_drops_the_previous_origins_cookies() {
        let broker = crate::net::LocalBroker::new(
                Policy::new().allow("bank.example").allow("evil.example"),
                Arc::new(MemorySink::new()),
                None,
            )
            .expect("broker");
        // A live session on one origin.
        broker.jar().store(
            &Url::parse("https://bank.example/").unwrap(),
            ["sid=secret; HttpOnly"],
        );
        assert_eq!(broker.jar().len(), 1);

        let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
        let factory = PageFactory::new(broker.clone(), fonts.sources.clone(), PageOptions::default());

        // Landing on another origin, however we got there, must drop it.
        let page = Page::from_bytes(
            b"<html><body>hi</body></html>",
            Some("text/html"),
            &Url::parse("https://evil.example/collect").unwrap(),
            broker.clone(),
            factory.fonts(),
            PageOptions::default(),
        );
        let page = factory.finish(page).expect("finish");

        assert!(
            broker.jar().is_empty(),
            "the previous origin's session survived the navigation"
        );
        assert!(
            broker
                .jar()
                .header_for(&Url::parse("https://bank.example/").unwrap())
                .is_none(),
            "and it must not be sendable"
        );
        assert!(
            page.notes.iter().any(|n| n.contains("dropped on navigation")),
            "the drop is stated, not silent: {:?}",
            page.notes
        );
    }

    /// Same-origin navigation keeps the session, which is the whole point of
    /// having one.
    #[test]
    fn staying_on_an_origin_keeps_the_session_through_finish() {
        let broker = crate::net::LocalBroker::new(
                Policy::new().allow("bank.example"),
                Arc::new(MemorySink::new()),
                None,
            )
            .expect("broker");
        broker.jar().store(
            &Url::parse("https://bank.example/").unwrap(),
            ["sid=secret; HttpOnly"],
        );
        let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
        let factory = PageFactory::new(broker.clone(), fonts.sources.clone(), PageOptions::default());

        let page = Page::from_bytes(
            b"<html><body>ok</body></html>",
            Some("text/html"),
            &Url::parse("https://bank.example/account").unwrap(),
            broker.clone(),
            factory.fonts(),
            PageOptions::default(),
        );
        let page = factory.finish(page).expect("finish");
        assert_eq!(broker.jar().len(), 1);
        assert!(!page.notes.iter().any(|n| n.contains("dropped on navigation")));
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;
    use crate::policy::Policy;
    use crate::receipt::MemorySink;

    fn page_with(html: &str) -> Page {
        let broker = crate::net::LocalBroker::new(Policy::new(), Arc::new(MemorySink::new()), None).expect("broker");
        let fonts = crate::fonts::load(&[], &crate::fonts::default_font_dirs(), Some(4));
        let factory = PageFactory::new(broker, fonts.sources.clone(), PageOptions::default());
        factory.from_html(html, &Url::parse("https://site.example/page").unwrap())
    }

    /// A wrapper that has swallowed a block of structure used to report one
    /// run-on line (`TitleBody textRead more`, three pieces of the page with
    /// no separator) and then suppress those pieces as prose it claimed to
    /// have already said. It had said them all at once, unreadably, and the
    /// structure an outline exists to show was gone.
    #[test]
    fn a_list_item_does_not_swallow_the_block_inside_it() {
        let page = page_with(
            "<html><body><ul><li>\
               <h3>Widget</h3>\
               <p>A thing that widgets.</p>\
               <a href=\"/buy\">Buy now</a>\
             </li></ul></body></html>",
        );
        let rendered = page.snapshot().render();

        assert!(
            !rendered.contains("WidgetA thing"),
            "the wrapper ran the block together:\n{rendered}"
        );
        for piece in ["Widget", "A thing that widgets.", "Buy now"] {
            assert!(
                rendered.contains(piece),
                "{piece:?} was suppressed as prose the wrapper never said:\n{rendered}"
            );
        }
        // And each piece appears once, not once inside the wrapper's line and
        // again on its own.
        assert_eq!(
            rendered.matches("A thing that widgets.").count(),
            1,
            "duplicated:\n{rendered}"
        );
    }

    /// The other side of the same rule, and the reason it is scoped to *block*
    /// descendants. Prose with a link in it is read well by the existing rule,
    /// and a heading wrapping a single link is a shape where the wrapper's name
    /// is the only thing carrying the heading level. Neither may change.
    #[test]
    fn prose_with_a_link_and_a_heading_around_one_are_unchanged() {
        let page = page_with(
            "<html><body>\
               <p>Please <a href=\"/here\">read this</a> first.</p>\
               <h2><a href=\"/sec\">Section two</a></h2>\
             </body></html>",
        );
        let rendered = page.snapshot().render();
        assert!(
            rendered.contains("Please read this first."),
            "the paragraph lost its own sentence:\n{rendered}"
        );
        assert!(
            rendered.contains("Section two"),
            "the heading lost its name:\n{rendered}"
        );
        assert!(
            rendered.contains("heading2"),
            "the heading level went missing:\n{rendered}"
        );
    }

    /// A table cell is the same shape as a list item and was wrong the same way.
    #[test]
    fn a_table_cell_does_not_swallow_the_block_inside_it() {
        let page = page_with(
            "<html><body><table><tr><td>\
               <p>First</p><p>Second</p>\
             </td></tr></table></body></html>",
        );
        let rendered = page.snapshot().render();
        assert!(
            !rendered.contains("FirstSecond"),
            "the cell ran its paragraphs together:\n{rendered}"
        );
    }

    fn ref_node(page: &Page, name: &str) -> usize {
        let snapshot = page.snapshot();
        snapshot
            .refs
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("no ref named {name} in {:?}", snapshot.refs))
            .node_id
    }

    const LOGIN: &str = "<html><body><form method='post' action='/session'>\
        <input type='text' name='user' placeholder='username'>\
        <input type='password' name='password' placeholder='password'>\
        <input type='submit' value='Go'></form></body></html>";

    #[test]
    fn typing_replaces_the_field_rather_than_appending_to_it() {
        // Append semantics would turn a retry after a failed submit into
        // `alicealice`, which is the kind of bug an agent cannot see.
        let mut page = page_with(LOGIN);
        let user = ref_node(&page, "username");

        assert!(page.type_into(user, "alice"));
        assert_eq!(page.field_value(user).as_deref(), Some("alice"));
        assert!(page.type_into(user, "bob"));
        assert_eq!(page.field_value(user).as_deref(), Some("bob"));
    }

    #[test]
    fn the_snapshot_shows_what_was_typed_not_what_was_served() {
        // Read from the editor, not the `value` attribute: an outline built
        // from the attribute would make `type` look like it silently failed.
        let mut page = page_with(LOGIN);
        let user = ref_node(&page, "username");
        page.type_into(user, "alice");

        let rendered = page.snapshot().render();
        assert!(rendered.contains("\"alice\""), "{rendered}");
    }

    #[test]
    fn typing_into_something_that_is_not_a_field_is_refused() {
        let mut page = page_with("<html><body><a href='/x'>a link</a></body></html>");
        let link = ref_node(&page, "a link");
        assert!(!page.type_into(link, "nope"));
    }

    #[test]
    fn a_post_form_becomes_a_post_with_the_typed_values_in_its_body() {
        let mut page = page_with(LOGIN);
        let user = ref_node(&page, "username");
        let password = ref_node(&page, "password");
        page.type_into(user, "alice");
        page.type_into(password, "hunter2");

        let submission = page.submit_form(ref_node(&page, "Go")).expect("submits");
        assert_eq!(submission.method, "POST");
        assert_eq!(submission.url.as_str(), "https://site.example/session");
        let body = String::from_utf8(submission.body).unwrap();
        assert!(body.contains("user=alice"), "{body}");
        assert!(body.contains("password=hunter2"), "{body}");
        assert_eq!(
            submission.content_type.as_deref(),
            Some("application/x-www-form-urlencoded")
        );
    }

    #[test]
    fn a_get_form_puts_its_fields_in_the_query_and_carries_no_body() {
        let mut page = page_with(
            "<html><body><form method='get' action='/search'>\
             <input type='text' name='q' placeholder='query'>\
             <input type='submit' value='Find'></form></body></html>",
        );
        let q = ref_node(&page, "query");
        page.type_into(q, "kelp forests");

        let submission = page.submit_form(ref_node(&page, "Find")).expect("submits");
        assert_eq!(submission.method, "GET");
        assert!(submission.body.is_empty(), "a GET carries no body");
        assert!(
            submission.url.query().unwrap_or_default().contains("q=kelp+forests"),
            "{}",
            submission.url
        );
    }

    #[test]
    fn a_control_outside_any_form_says_so_rather_than_submitting_nothing() {
        let mut page = page_with("<html><body><input type='text' placeholder='loose'></body></html>");
        let loose = ref_node(&page, "loose");
        let error = page.submit_form(loose).expect_err("nothing to submit");
        assert!(format!("{error}").contains("not inside a form"), "{error}");
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    #[test]
    fn a_meta_refresh_is_parsed_the_way_hand_written_html_writes_it() {
        let base = Url::parse("https://site.example/dir/page.html").unwrap();

        // Twenty-five years of hand-written variants, all of which appear in
        // the wild and all of which mean the same thing.
        for markup in [
            r#"<meta http-equiv="refresh" content="0; url=next.html">"#,
            r#"<meta http-equiv="Refresh" content="0;URL=next.html">"#,
            r#"<meta http-equiv=refresh content='0; url=next.html'>"#,
            r#"<meta content="0; url=next.html" http-equiv="refresh">"#,
            r#"<META HTTP-EQUIV="REFRESH" CONTENT="0; URL='next.html'">"#,
        ] {
            let found = meta_refresh(markup, &base);
            assert_eq!(
                found.map(|(delay, url)| (delay, url.to_string())),
                Some((0, "https://site.example/dir/next.html".to_string())),
                "failed on {markup}"
            );
        }

        // The delay is read, not assumed.
        assert_eq!(
            meta_refresh(
                r#"<meta http-equiv="refresh" content="600; url=/live">"#,
                &base
            )
            .map(|(d, _)| d),
            Some(600)
        );

        // A refresh with no URL reloads this page; that is not a redirect.
        assert!(meta_refresh(r#"<meta http-equiv="refresh" content="30">"#, &base).is_none());
        // And an unrelated meta is not one.
        assert!(meta_refresh(r#"<meta name="description" content="0; url=x">"#, &base).is_none());
        // `http-equiv="refresh-policy"` must not match on a substring.
        assert!(meta_refresh("<p>content=\"0; url=x\"</p>", &base).is_none());
    }

    #[test]
    fn a_challenge_page_is_recognised_and_an_ordinary_short_page_is_not() {
        assert_eq!(
            challenge_marker("<html><body>Enable JavaScript and cookies to continue</body></html>"),
            Some("enable javascript and cookies to continue")
        );
        assert_eq!(
            challenge_marker("<title>Attention Required! | Cloudflare</title>"),
            Some("attention required! | cloudflare")
        );
        // A short page is not a challenge. Saying otherwise would tell an agent
        // it was blocked by a site that simply had little to say.
        assert_eq!(challenge_marker("<html><body><p>Not found.</p></body></html>"), None);
        assert_eq!(
            challenge_marker("<p>This article explains how to enable JavaScript.</p>"),
            None
        );
    }

    #[test]
    fn an_attribute_is_read_whichever_way_it_was_quoted() {
        assert_eq!(attribute_value(r#"<meta content="a b">"#, "content"), Some("a b".into()));
        assert_eq!(attribute_value(r#"<meta content='a b'>"#, "content"), Some("a b".into()));
        assert_eq!(attribute_value(r#"<meta content=ab>"#, "content"), Some("ab".into()));
        // Not the tail of another attribute: `data-content` is not `content`.
        assert_eq!(attribute_value(r#"<meta data-content="x">"#, "content"), None);
        assert_eq!(attribute_value(r#"<meta charset="utf-8">"#, "content"), None);
    }
}
