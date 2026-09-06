//! `h5i browser` end to end, against the real engine.
//!
//! These drive the binary rather than the library, because the properties they
//! pin are properties of the command an agent actually types: that a dead session
//! is refused with its own exit code, that an id is never handed back to a second
//! session, and that what a page composed does not reach a terminal with its
//! escape sequences intact.
//!
//! Skipped, loudly, when the binary under test has not been built.

use std::path::PathBuf;
use std::process::Command;

/// Exit status for a verb sent to a session that is not live. Copied rather
/// than imported so a change to the constant has to be made in two places, one
/// of which is a test that says why it matters.
const EXIT_SESSION_GONE: i32 = 69;

/// There is no separate engine to find any more: `h5i` execs itself. Kept as a
/// function so the skip below still reads as a precondition rather than as a
/// bare `h5i` existence check.
fn engine() -> Option<PathBuf> {
    let h5i = h5i();
    h5i.exists().then_some(h5i)
}

fn h5i() -> PathBuf {
    let mut here = std::env::current_exe().expect("test binary path");
    here.pop();
    here.pop();
    here.join("h5i")
}

/// A one-page HTTP server, so the tests exercise the path the product is for.
///
/// `file://` was the obvious shortcut and the wrong one: the engine loads a
/// local file as a *start target* and refuses to *fetch* one, so a second
/// `open` on a file URL is denied by policy. That is correct behaviour (a
/// page-initiated navigation to `file:` is an exfiltration path) and it makes
/// file URLs unable to test navigation at all.
struct Site {
    base: String,
}

impl Site {
    /// Serve until the process ends. One thread, one connection at a time,
    /// which is all any of these tests needs.
    fn start() -> Option<Site> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = Site::serve_one(stream);
            }
        });
        Some(Site {
            base: format!("http://127.0.0.1:{port}"),
        })
    }

    fn serve_one(mut stream: std::net::TcpStream) -> std::io::Result<()> {
        use std::io::{BufRead, BufReader, Write};
        let mut line = String::new();
        BufReader::new(stream.try_clone()?).read_line(&mut line)?;
        let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
        let body = match path.as_str() {
            "/two" => "<html><head><title>Two</title></head><body><h1>second</h1></body></html>"
                .to_string(),
            // A title carrying escape sequences: a page repainting the terminal
            // it is printed into.
            "/hostile" => "<html><body><h1>start\u{1b}[2K\u{1b}[1Aoverwritten</h1></body></html>"
                .to_string(),
            // A subresource on an origin the caller never named. Reachable only
            // if the allowlist means more than "what I asked for".
            "/third-party" => "<html><body><h1>third party</h1>\
                  <img src=\"https://cdn.example.invalid/x.png\"><p>body</p></body></html>"
                .to_string(),
            // Something to type a credential into.
            "/login" => "<html><body><form action=\"/in\" method=\"post\">\
                  <input name=\"pass\" type=\"password\"></form></body></html>"
                .to_string(),
            _ => "<html><head><title>t</title></head><body><h1>hello</h1>\
                  <p>a <a href=\"https://example.com/next\">link</a></p></body></html>"
                .to_string(),
        };
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )?;
        stream.flush()
    }
}

struct Fixture {
    home: tempfile::TempDir,
    site: Site,
}

impl Fixture {
    fn new() -> Option<Fixture> {
        engine()?;
        let home = tempfile::tempdir().ok()?;
        let site = Site::start()?;
        Some(Fixture { home, site })
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.run_with(args, &[])
    }

    /// The same, with variables set in the environment the command is started
    /// from. That environment is where `--secret` resolves a credential, so a
    /// test of the credential path has to control it rather than inherit it.
    fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
        let mut command = Command::new(h5i());
        command.args(args).env("H5I_BROWSER_HOME", self.home.path());
        for (name, value) in env {
            command.env(name, value);
        }
        command.output().expect("h5i runs")
    }

    /// The session's allowlist has to name the test server, because the engine
    /// reaches nothing it was not granted.
    fn open(&self, extra: &[&str]) -> String {
        let url = self.site.base.clone();
        let mut args = vec!["browser", "open", url.as_str(), "--allow", "127.0.0.1", "--json"];
        args.extend_from_slice(extra);
        let out = self.run(&args);
        assert!(
            out.status.success(),
            "open failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let record: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("open prints a record");
        record["id"].as_str().expect("an id").to_string()
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.home.path().join("sessions").join(id)
    }
}

fn skip(why: &str) {
    eprintln!("skipping: {why}");
}

/// The shape an agent actually types. No id anywhere.
#[test]
fn the_ordinary_case_names_no_session() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let url = fx.site.base.clone();
    assert!(fx.run(&["browser", "open", &url, "--allow", "127.0.0.1"]).status.success());

    let snapshot = fx.run(&["browser", "snapshot"]);
    assert!(
        snapshot.status.success(),
        "a bare verb must land on the session `open` just made: {}",
        String::from_utf8_lossy(&snapshot.stderr)
    );
    assert!(String::from_utf8_lossy(&snapshot.stdout).contains("hello"));

    assert!(fx.run(&["browser", "requests"]).status.success());
    assert!(fx.run(&["browser", "close"]).status.success());

    // And once it is closed, a bare verb says so rather than guessing.
    let after = fx.run(&["browser", "snapshot"]);
    assert_eq!(after.status.code(), Some(EXIT_SESSION_GONE));
    let why = String::from_utf8_lossy(&after.stderr);
    assert!(why.contains("h5i browser open"), "{why}");
}

/// A second `open` moves the session it finds. Forking silently would leave the
/// first one holding a page nothing points at.
#[test]
fn opening_again_navigates_rather_than_forking() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let url = fx.site.base.clone();
    fx.open(&[]);
    // No `--allow` on the second open: the policy is already fixed, and passing
    // it again is the thing `a_creation_flag_on_a_live_session_is_refused`
    // pins. This is the plain "go there" case.
    assert!(fx.run(&["browser", "open", &url]).status.success());

    let listed = fx.run(&["browser", "list", "--json"]);
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed.len(), 1, "a second open forked the session");

    // `--new` is how you say you meant a second one.
    assert!(fx.run(&["browser", "open", &url, "--allow", "127.0.0.1", "--new"]).status.success());
    let listed = fx.run(&["browser", "list", "--json"]);
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed.len(), 2);
}

/// Names are for running several at once, and the id still addresses one.
#[test]
fn a_name_addresses_a_session_and_so_does_its_id() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let auth = fx.open(&["--session", "auth"]);
    let public = fx.open(&["--session", "public", "--new"]);
    assert_ne!(auth, public);

    assert!(
        fx.run(&["browser", "status", "--session", "auth"])
            .status
            .success()
    );
    let by_id = fx.run(&["browser", "status", "--session", &auth, "--json"]);
    assert!(by_id.status.success());
    let record: serde_json::Value = serde_json::from_slice(&by_id.stdout).unwrap();
    assert_eq!(record["id"].as_str().unwrap(), auth);
    assert_eq!(record["name"].as_str().unwrap(), "auth");
}

/// A session's policy is fixed when its engine starts, so a flag that would
/// widen it is refused rather than accepted and ignored.
#[test]
fn a_creation_flag_on_a_live_session_is_refused() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    fx.open(&[]);
    let url = fx.site.base.clone();
    let out = fx.run(&["browser", "open", &url, "--allow", "example.com"]);
    assert!(!out.status.success());
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(why.contains("--allow"), "{why}");
    assert!(why.contains("--new"), "the refusal names the way forward: {why}");
}

/// A verb the session refused must not exit 0. A script that checks the status
/// code would otherwise read "denied by policy" as success.
#[test]
fn a_refused_verb_exits_non_zero() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    fx.open(&[]);
    assert!(fx.run(&["browser", "snapshot"]).status.success());
    // The page's only link leaves the session's allowlist.
    let out = fx.run(&["browser", "click", "@e1"]);
    assert!(!out.status.success(), "a policy denial exited 0");
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(why.contains("allowlist"), "{why}");
}

#[test]
fn a_session_starts_answers_and_closes() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let id = fx.open(&[]);

    let snapshot = fx.run(&["browser", "snapshot"]);
    assert!(snapshot.status.success());
    let text = String::from_utf8_lossy(&snapshot.stdout);
    assert!(text.contains("hello"), "{text}");

    // The engine fences page content for a model. h5i must not undo that.
    assert!(text.contains("UNTRUSTED"), "{text}");

    let closed = fx.run(&["browser", "close"]);
    assert!(closed.status.success());

    // The record outlives the session: that is what makes "how did it end"
    // answerable at all.
    assert!(fx.dir(&id).join("session.json").exists());
}

#[test]
fn a_verb_on_a_closed_session_is_refused_with_its_own_exit_code() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let _ = fx.open(&[]);
    assert!(fx.run(&["browser", "close"]).status.success());

    let out = fx.run(&["browser", "snapshot"]);
    assert_eq!(
        out.status.code(),
        Some(EXIT_SESSION_GONE),
        "a retry loop that cannot tell 'gone' from 'failed' silently starts a second browser"
    );
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(why.contains("was closed"), "{why}");
    assert!(why.contains("--restore"), "the refusal names the way forward");
}

#[test]
fn killing_the_engine_is_recorded_as_a_death_not_papered_over() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let id = fx.open(&[]);
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fx.dir(&id).join("session.json")).unwrap())
            .unwrap();
    let pid = record["control"]["pid"].as_u64().expect("a pid") as i32;

    unsafe { libc::kill(pid, libc::SIGKILL) };
    std::thread::sleep(std::time::Duration::from_millis(300));

    let out = fx.run(&["browser", "snapshot"]);
    assert_eq!(out.status.code(), Some(EXIT_SESSION_GONE));

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fx.dir(&id).join("session.json")).unwrap())
            .unwrap();
    assert_eq!(after["state"], "died");
    assert!(after["ended_at"].is_string(), "an ending needs a time");
}

#[test]
fn a_restore_is_a_new_session_with_the_inheritance_written_down() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let first = fx.open(&[]);
    assert!(fx.run(&["browser", "close"]).status.success());

    let url = fx.site.base.clone();
    let out = fx.run(&["browser", "open", &url, "--allow", "127.0.0.1", "--restore", &first, "--json"]);
    assert!(out.status.success());
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    assert_ne!(record["id"].as_str().unwrap(), first, "ids are not recycled");
    assert_eq!(record["restored_from"].as_str().unwrap(), first);
}

#[test]
fn a_host_session_says_which_lane_its_requests_are() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let _ = fx.open(&[]);
    let out = fx.run(&["browser", "status"]);
    let text = String::from_utf8_lossy(&out.stdout);
    // The default is honest about being the engine's own account. A page-only
    // claim rendered as host-observed would be the one lie this product cannot
    // afford, and the default sandbox must not tempt anyone into it, since a
    // process-tier sandbox corroborates no part of the request log.
    assert!(text.contains("engine-claimed"), "{text}");
    // And the placement line names what the sandbox does *not* contain, whether
    // or not this host could apply one.
    assert!(
        text.contains("not its network") || text.contains("unconfined"),
        "{text}"
    );
    let _ = fx.run(&["browser", "close"]);
}

#[test]
fn the_control_lock_pauses_a_mutating_verb_and_lets_a_read_through() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let _ = fx.open(&[]);
    assert!(fx.run(&["browser", "take"]).status.success());

    let click = fx.run(&["browser", "click", "@e1"]);
    assert!(!click.status.success());
    assert!(
        String::from_utf8_lossy(&click.stderr).contains("held by a human"),
        "a mutating verb waits"
    );
    // Watching never collides.
    assert!(fx.run(&["browser", "snapshot"]).status.success());

    assert!(fx.run(&["browser", "release"]).status.success());
    let stale = fx.run(&["browser", "click", "@e1"]);
    assert!(
        String::from_utf8_lossy(&stale.stderr).contains("stale"),
        "the page moved while the human drove"
    );
    let _ = fx.run(&["browser", "close"]);
}

#[test]
fn list_keeps_endings_and_hides_them_by_default() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let id = fx.open(&[]);
    assert!(fx.run(&["browser", "close"]).status.success());

    let live = fx.run(&["browser", "list", "--json"]);
    let live: Vec<serde_json::Value> = serde_json::from_slice(&live.stdout).unwrap();
    assert!(live.iter().all(|s| s["id"] != id.as_str()));

    let all = fx.run(&["browser", "list", "--all", "--json"]);
    let all: Vec<serde_json::Value> = serde_json::from_slice(&all.stdout).unwrap();
    assert!(all.iter().any(|s| s["id"] == id.as_str()));
}

#[test]
fn an_expired_session_is_an_ending_on_the_record() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    // One second, then a verb after it has passed: the sweep runs on the next
    // command rather than from a timer nothing is holding.
    let id = fx.open(&["--expires-in", "1"]);
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let out = fx.run(&["browser", "snapshot"]);
    assert_eq!(out.status.code(), Some(EXIT_SESSION_GONE));
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fx.dir(&id).join("session.json")).unwrap())
            .unwrap();
    assert_eq!(after["state"], "expired");
}

/// The engine writes its request log where the session directory says, and the
/// log is the session's own record, not something the caller assembles.
#[test]
fn the_request_log_lands_in_the_sessions_own_directory() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let _ = fx.open(&[]);
    let out = fx.run(&["browser", "requests", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let answer: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(answer.get("requests").is_some(), "{answer}");
    let _ = fx.run(&["browser", "close"]);
}

/// The whole record of a session, in one ordered timeline: what the agent
/// asked for, what the engine decided, who was driving, and how it ended.
#[test]
fn the_audit_carries_the_whole_session_in_one_timeline() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    fx.open(&[]);
    assert!(fx.run(&["browser", "snapshot"]).status.success());
    assert!(fx.run(&["browser", "take"]).status.success());
    assert!(fx.run(&["browser", "release"]).status.success());
    assert!(fx.run(&["browser", "snapshot"]).status.success());
    assert!(fx.run(&["browser", "close"]).status.success());

    let out = fx.run(&["browser", "audit", "--json"]);
    assert!(
        out.status.success(),
        "audit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let audit: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let events = audit["events"].as_array().expect("a timeline");

    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    for expected in ["lifecycle", "agent-action", "request", "control"] {
        assert!(kinds.contains(&expected), "no {expected} row: {kinds:?}");
    }

    // The handover sits between the two snapshots. That ordering is the whole
    // reason the audit exists: "was a human driving when that happened" cannot
    // be answered by a current-holder field.
    let positions: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e["kind"] == "agent-action" && e["action"].as_str().is_some_and(|a| a.starts_with("snapshot")))
        .map(|(i, _)| i)
        .collect();
    let control = events
        .iter()
        .position(|e| e["kind"] == "control")
        .expect("a handover");
    assert!(
        positions.len() >= 2 && positions[0] < control && control < positions[1],
        "the handover is not between the two snapshots: {kinds:?}"
    );

    // The lanes stay apart: a row h5i wrote from outside is never presented as
    // the engine reporting on itself.
    for event in events {
        let expected = match event["kind"].as_str() {
            Some("lifecycle") | Some("control") => "host-observed",
            _ => "box-claimed",
        };
        assert_eq!(event["lane"], expected, "wrong lane: {event}");
    }
}

/// An audit must say what it could not read. An empty timeline over a log h5i
/// cannot see looks exactly like a session that did nothing.
#[test]
fn the_audit_reports_a_log_it_could_not_read() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let id = fx.open(&[]);
    assert!(fx.run(&["browser", "close"]).status.success());

    // Take the engine's own logs away, the way an image-backed box would.
    let dir = fx.dir(&id);
    std::fs::remove_file(dir.join("actions.jsonl")).ok();
    std::fs::remove_file(dir.join("requests.jsonl")).ok();

    let out = fx.run(&["browser", "audit", "--json"]);
    let audit: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(audit["sources"]["actions"], "unavailable");
    assert_eq!(audit["sources"]["requests"], "unavailable");

    let text = String::from_utf8_lossy(&fx.run(&["browser", "audit"]).stdout).to_string();
    assert!(text.contains("unavailable"), "{text}");
}

/// The verb that reads the request log does not appear as the cause of it.
#[test]
fn reading_the_log_is_not_recorded_as_causing_it() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let id = fx.open(&[]);
    assert!(fx.run(&["browser", "requests"]).status.success());
    assert!(fx.run(&["browser", "close"]).status.success());

    let actions = std::fs::read_to_string(fx.dir(&id).join("actions.jsonl")).unwrap();
    let row = actions
        .lines()
        .find(|l| l.contains("\"verb\":\"requests\"") && l.contains("\"phase\":\"result\""))
        .expect("the verb was recorded");
    assert!(
        !row.contains("\"requests\":["),
        "the reader claimed to have caused what it read:\n{row}"
    );
}

/// The default sandbox is not a label. A session must be unable to read a file
/// the engine could read without it, and `--no-sandbox` must be able to.
///
/// `$HOME` is the boundary under test because it is the one that matters and the
/// one the profile's defaults do not grant: `/tmp` *is* granted, so a probe file
/// in the fixture's own tempdir would prove nothing.
#[test]
fn the_default_sandbox_denies_what_no_sandbox_allows() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return skip("no $HOME to place a probe file outside the granted paths");
    };
    let Ok(probe) = tempfile::Builder::new()
        .prefix(".h5i-sandbox-probe-")
        .suffix(".html")
        .tempfile_in(&home)
    else {
        return skip("cannot write a probe file into $HOME");
    };
    std::fs::write(probe.path(), "<html><body><h1>secret-probe-content</h1></body></html>").unwrap();
    let url = format!("file://{}", probe.path().display());

    // Whether this host can confine at all. A kernel without Landlock runs the
    // session unconfined and says so, and there is nothing here to test.
    let opened = fx.run(&["browser", "open", &url, "--json"]);
    if !opened.status.success() {
        let why = String::from_utf8_lossy(&opened.stderr);
        assert!(
            why.contains("Permission denied"),
            "the sandbox refused for some other reason: {why}"
        );
    } else {
        let record: serde_json::Value = serde_json::from_slice(&opened.stdout).unwrap();
        if record["confinement"]["kind"] != "process" {
            return skip("this host cannot confine a session");
        }
        panic!("the sandbox let the engine read a file under $HOME");
    }

    // And the escape hatch is a real escape hatch.
    let out = fx.run(&["browser", "open", &url, "--no-sandbox", "--json"]);
    assert!(
        out.status.success(),
        "--no-sandbox could not read it either: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["confinement"]["kind"], "none");
    let read = fx.run(&["browser", "markdown"]);
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("secret-probe-content"),
        "the unconfined session should have read the probe"
    );
    let _ = fx.run(&["browser", "close"]);
}

/// A sandboxed session still does the thing the product is for.
#[test]
fn the_sandbox_does_not_break_reading_or_recording() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let id = fx.open(&[]);
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fx.dir(&id).join("session.json")).unwrap())
            .unwrap();
    if record["confinement"]["kind"] != "process" {
        return skip("this host cannot confine a session");
    }

    let snapshot = fx.run(&["browser", "snapshot"]);
    assert!(snapshot.status.success(), "{}", String::from_utf8_lossy(&snapshot.stderr));
    assert!(String::from_utf8_lossy(&snapshot.stdout).contains("hello"));

    // The whole point: confined and still recording.
    let requests = fx.run(&["browser", "requests", "--json"]);
    let answer: serde_json::Value = serde_json::from_slice(&requests.stdout).unwrap();
    assert!(
        answer["total"].as_u64().unwrap_or(0) > 0,
        "a confined session recorded nothing: {answer}"
    );

    // And the lane is not upgraded by it. A process-tier sandbox corroborates
    // no part of the request log.
    assert_eq!(record["lane"], "engine-claimed");
    let _ = fx.run(&["browser", "close"]);
}

/// A read leaves nothing behind. That is the whole difference from a session,
/// and it is what lets a read have a tier a session cannot.
#[test]
fn a_read_leaves_no_session() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let url = fx.site.base.clone();
    // No `--allow`: naming the URL is what grants it.
    let out = fx.run(&["browser", "read", &url, "--text"]);
    assert!(
        out.status.success(),
        "read failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("hello"), "{text}");
    // It says what held it, before the page, because that is the part a reader
    // cannot recover from the output.
    assert!(text.contains("confined :"), "{text}");

    let listed = fx.run(&["browser", "list", "--all", "--json"]);
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout).unwrap();
    assert!(listed.is_empty(), "a read left a session behind: {listed:?}");
}

/// `open` grants the page it was told to open, and only that page.
#[test]
fn an_open_grants_the_page_it_was_opened_on() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let url = format!("{}/third-party", fx.site.base);
    // No `--allow`: naming the URL is what grants it, exactly as it is for
    // `read`.
    let out = fx.run(&["browser", "open", &url, "--no-loopback", "--json"]);
    assert!(
        out.status.success(),
        "a session was denied the page it was opened on: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let snapshot = fx.run(&["browser", "snapshot"]);
    assert!(
        String::from_utf8_lossy(&snapshot.stdout).contains("third party"),
        "the page did not load: {}",
        String::from_utf8_lossy(&snapshot.stdout)
    );

    // And the grant is the page, not "and whatever this page pulls in": the
    // off-origin image is still refused, and still says so in the log.
    let out = fx.run(&["browser", "requests", "--json"]);
    let answer: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let requests = answer["requests"].as_array().expect("a request log");
    assert!(
        requests.iter().any(|r| r["allowed"] == false),
        "an off-origin subresource was not refused: {answer}"
    );
    let _ = fx.run(&["browser", "close"]);
}

/// Two sessions allowed different things must not digest the same.
///
/// The policy digest is the only durable claim about what a host session could
/// reach, and the page a session was opened on is part of its allowlist now. A
/// digest taken over the flags alone would have called two sessions with
/// different reach identical, which is the one direction this field must not be
/// wrong in.
#[test]
fn the_policy_digest_follows_the_page_the_session_was_opened_on() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let one = fx.run(&["browser", "open", &fx.site.base, "--json"]);
    let one: serde_json::Value = serde_json::from_slice(&one.stdout).unwrap();
    let again = fx.run(&["browser", "open", &fx.site.base, "--new", "--json"]);
    let again: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
    // The same start, so the same grants, so the same digest. Two sessions
    // allowed the same things is exactly what an equal digest claims.
    assert_eq!(one["policy_digest"], again["policy_digest"], "{one} {again}");

    // Somewhere else entirely. It cannot serve a page, but the record a failed
    // start leaves behind still carries the policy it was started with, which
    // is the part under test.
    let elsewhere = fx.run(&["browser", "open", "http://127.0.0.2:9/", "--new", "--json"]);
    let listed = fx.run(&["browser", "list", "--all", "--json"]);
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout).unwrap();
    let other = listed
        .iter()
        .find(|s| s["url"].as_str() == Some("http://127.0.0.2:9/"))
        .unwrap_or_else(|| panic!("the failed start left no record: {:?}", elsewhere.status));
    assert_ne!(
        other["policy_digest"], one["policy_digest"],
        "two sessions granted different origins digested the same"
    );
    let _ = fx.run(&["browser", "close", "--all"]);
}

/// The helper lane runs with no session, when `--url` names the media.
#[cfg(unix)]
#[test]
fn a_helper_run_needs_no_session_when_a_url_names_the_media() {
    use std::os::unix::fs::PermissionsExt;

    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let bin = fx.home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("yt-dlp");
    // It expands `%(id)s` the way yt-dlp does and writes the two files h5i
    // reads back: the captions and the metadata beside them.
    std::fs::write(
        &script,
        r#"#!/bin/sh
out=""
while [ $# -gt 0 ]; do
  if [ "$1" = "-o" ]; then out="$2"; fi
  shift
done
out=$(printf '%s' "$out" | sed 's/%(id)s/vid/')
printf 'WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nhello from the helper\n' > "$out.en.vtt"
printf '{"title":"A talk","subtitles":{"en":[]}}' > "$out.info.json"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let out = fx.run_with(
        &[
            "browser",
            "transcript",
            "--via",
            "yt-dlp",
            "--url",
            "https://video.example/watch?v=1",
        ],
        &[("PATH", path.as_str())],
    );
    assert!(
        out.status.success(),
        "a helper run with no session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("hello from the helper"), "{text}");
    // And it says what held it, which was nothing.
    assert!(text.contains("no browser session open"), "{text}");

    // It invents no session on the way past.
    let listed = fx.run(&["browser", "list", "--all", "--json"]);
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout).unwrap();
    assert!(listed.is_empty(), "the helper lane left a session behind: {listed:?}");

    // The run is recorded, and it is findable: a lane whose whole value is
    // being written down cannot have a case that quietly is not.
    let audit = fx.run(&["browser", "audit", "--no-session", "--json"]);
    assert!(audit.status.success(), "{}", String::from_utf8_lossy(&audit.stderr));
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&audit.stdout).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["name"], "yt-dlp");
    let argv: Vec<&str> = rows[0]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert!(
        argv.contains(&"https://video.example/watch?v=1"),
        "the row does not name what ran: {argv:?}"
    );
    // What h5i built, not what the helper says it did: the flags that keep the
    // recorded argv a complete account travel with it.
    assert!(argv.contains(&"--ignore-config"), "{argv:?}");
}

/// A `--session` that names something gone is still an error, even with a URL.
///
/// The rule the sessionless case must not erode: running somewhere else would
/// move the lane to a boundary the caller did not choose, which is the same
/// reason a boxed run is never served from the host.
#[cfg(unix)]
#[test]
fn a_named_session_that_has_ended_is_refused_rather_than_run_here() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let id = fx.open(&[]);
    assert!(fx.run(&["browser", "close"]).status.success());

    let out = fx.run(&[
        "browser",
        "transcript",
        "--via",
        "yt-dlp",
        "--session",
        &id,
        "--url",
        "https://video.example/watch?v=1",
    ]);
    assert_eq!(out.status.code(), Some(EXIT_SESSION_GONE));
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(why.contains(&id), "{why}");
}

/// `--out` is a path on *this* machine, and h5i is what writes it.
///
/// The engine is confined to its own directory, so handing it the caller's path
/// made `--out ~/shot.png` fail with a bare `Permission denied` from a sandbox
/// the caller never asked about. It paints where it may write and h5i moves the
/// file, which is the rule the cookie jar already follows: h5i chooses the
/// path, the engine only chooses the bytes.
#[test]
fn a_screenshot_goes_where_the_caller_asked_and_not_where_the_engine_may_write() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let id = fx.open(&[]);

    // Deliberately outside everything the session's sandbox can write: a
    // directory made after the engine started, which no grant can name.
    let elsewhere = tempfile::tempdir().expect("a directory of our own");
    let out = elsewhere.path().join("deeper").join("shot.png");

    let taken = fx.run(&["browser", "screenshot", "--out", out.to_str().unwrap(), "--json"]);
    assert!(
        taken.status.success(),
        "screenshot failed: {}",
        String::from_utf8_lossy(&taken.stderr)
    );
    let answer: serde_json::Value = serde_json::from_slice(&taken.stdout).unwrap();
    // The reply names where the file is, not where it was painted.
    assert_eq!(answer["path"].as_str(), out.to_str(), "{answer}");
    let written = std::fs::metadata(&out).expect("the file the reply named");
    assert!(written.len() > 0, "an empty screenshot");
    assert_eq!(answer["bytes"].as_u64(), Some(written.len()), "{answer}");

    // And nothing was left behind in the session's own artifacts: the file
    // moved, it was not copied twice.
    let leftovers: Vec<_> = std::fs::read_dir(fx.dir(&id).join("artifacts"))
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "the shot was left in the session too: {leftovers:?}");

    let _ = fx.run(&["browser", "close"]);
}

/// A read grants the targets it was given, and nothing it was not asked about.
///
/// The first half is why `--allow` is not needed for the ordinary case: a URL
/// the caller typed is a URL the caller asked for, and making them say it twice
/// teaches nothing. The second half is why the grant is not wider than that:
/// the page's off-origin subresource is refused unless it too was named, and
/// says so in the log, which is the part that would have been given away by an
/// allowlist meaning "and whatever this page pulls in".
#[test]
fn a_read_grants_its_targets_and_nothing_else() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let url = format!("{}/third-party", fx.site.base);
    let out = fx.run(&["browser", "read", &url, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let answer: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let requests = answer["requests"].as_array().expect("a request log");

    assert!(
        requests
            .iter()
            .any(|r| r["allowed"] == true && r["url"].as_str().is_some_and(|u| u.starts_with(&url))),
        "the target was not reachable without `--allow`: {answer}"
    );
    assert!(
        requests.iter().any(|r| r["allowed"] == false),
        "an off-origin subresource was not refused: {answer}"
    );
}

/// And an origin the caller *does* name is granted, without a session.
///
/// The case this exists for is a page written in a library served from a CDN:
/// with the script refused, the page an agent reads is the one the library
/// never ran on. `open --allow` could always say it; a read is the shape most
/// scraping takes, and it could not.
#[test]
fn a_read_can_be_told_to_allow_a_third_party_origin() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let url = format!("{}/third-party", fx.site.base);
    let out = fx.run(&[
        "browser",
        "read",
        &url,
        "--allow",
        "https://cdn.example.invalid",
        "--json",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let answer: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let requests = answer["requests"].as_array().expect("a request log");

    // The subresource still does not load — `.invalid` resolves nowhere, which
    // is the point of the name — so what is asserted is the *reason*: the
    // policy is no longer the thing refusing it.
    assert!(
        !requests.iter().any(|r| r["denied_reason"]
            .as_str()
            .is_some_and(|why| why.contains("not in the allowlist"))),
        "a named origin was still refused by the allowlist: {answer}"
    );
}

/// The request log comes back with the page, machine-readable, which is what a
/// crawl needs and what no other headless browser can hand over completely.
#[test]
fn a_read_returns_its_request_log() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let url = fx.site.base.clone();
    let out = fx.run(&["browser", "read", &url, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let answer: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    // What held the engine travels with the log, not beside it: a request log
    // whose reader cannot tell whether anything was containing the requests is
    // half a receipt.
    assert!(answer["confinement"]["kind"].is_string(), "{answer}");
    let requests = answer["requests"].as_array().expect("a request log");
    assert!(
        requests.iter().any(|r| r["url"].as_str().is_some_and(|u| u.starts_with(&url))),
        "the page's own fetch is not in the log: {answer}"
    );
}

/// A page with a hostile heading cannot repaint the terminal it is printed
/// into. The engine carried the bytes; h5i is the last thing between them and a
/// person's screen.
#[test]
fn page_text_reaches_the_terminal_without_its_escape_sequences() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let url = format!("{}/hostile", fx.site.base);
    let opened = fx.run(&["browser", "open", &url, "--allow", "127.0.0.1", "--json"]);
    assert!(
        opened.status.success(),
        "open failed: {}",
        String::from_utf8_lossy(&opened.stderr)
    );
    let snapshot = fx.run(&["browser", "snapshot"]);
    let text = String::from_utf8_lossy(&snapshot.stdout);
    assert!(!text.contains('\u{1b}'), "an escape reached the terminal");
    let _ = fx.run(&["browser", "close"]);
}

/// A credential named with `--secret` reaches the session, and its value does
/// not reach anything the agent reads.
///
/// The whole point of the flag, and it was doing none of it: the profile named
/// the credential and nothing delivered it, so a confined session answered "no
/// credentials" for one it had been told to carry. Driven through the binary
/// because the failure lived in the seam between the CLI, the profile and the
/// spawn, which is exactly the part a library test does not cross.
#[test]
fn a_named_credential_reaches_a_confined_session_and_never_a_reply() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let url = format!("{}/login", fx.site.base);
    let opened = fx.run_with(
        &[
            "browser", "open", &url, "--allow", "127.0.0.1", "--secret", "ACME_PASS", "--json",
        ],
        &[("H5I_SECRET_ACME_PASS", "hunter2-from-the-test")],
    );
    assert!(
        opened.status.success(),
        "open failed: {}",
        String::from_utf8_lossy(&opened.stderr)
    );

    // The name, from the session's own answer. Never the value: there is no
    // verb in this engine that returns one. `--json` because the human form
    // prints the count and the advice, and the names are the field.
    let listed = fx.run(&["browser", "env", "--json"]);
    let text = String::from_utf8_lossy(&listed.stdout);
    assert!(text.contains("H5I_SECRET_ACME_PASS"), "{text}");
    assert!(!text.contains("hunter2-from-the-test"), "a value was printed: {text}");

    // And it substitutes on the way into the field, reported by placeholder.
    // The snapshot first, because a ref is something the session served rather
    // than something a caller may invent.
    assert!(fx.run(&["browser", "snapshot"]).status.success());
    let typed = fx.run(&["browser", "type", "@e1", "$H5I_SECRET_ACME_PASS"]);
    let reply = String::from_utf8_lossy(&typed.stdout);
    assert!(typed.status.success(), "{}", String::from_utf8_lossy(&typed.stderr));
    assert!(reply.contains("H5I_SECRET_ACME_PASS"), "{reply}");
    assert!(!reply.contains("hunter2-from-the-test"), "the value came back: {reply}");

    let _ = fx.run(&["browser", "close"]);
}

/// A credential that cannot be resolved stops the session before it exists.
///
/// Fail-closed, and the reason it matters here: the alternative is a session
/// that starts fine and refuses the credential at the moment an agent is
/// halfway through a login, which is the least recoverable place to learn it.
#[test]
fn a_credential_that_is_not_set_refuses_the_session_rather_than_starting_one() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let url = fx.site.base.clone();
    let out = fx.run(&[
        "browser", "open", &url, "--allow", "127.0.0.1", "--secret", "NOT_SET_ANYWHERE",
    ]);
    assert!(
        !out.status.success(),
        "a missing credential started a session\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(why.contains("H5I_SECRET_NOT_SET_ANYWHERE"), "{why}");

    // And nothing was left behind: `browser status` has no session to report.
    let after = fx.run(&["browser", "status"]);
    assert!(
        !after.status.success()
            || !String::from_utf8_lossy(&after.stdout).contains("NOT_SET_ANYWHERE"),
        "a failed start left a session"
    );
}

/// `--secret` and `--in` are two ways to say one thing, and a box already has
/// the better one. Refused rather than ignored: this flag was silently dropped
/// on the way to a box, so a session ran without the credential it was told to
/// carry and said nothing about it.
#[test]
fn a_secret_flag_on_a_boxed_session_is_refused_and_names_env_toml() {
    let Some(fx) = Fixture::new() else {
        return skip("no h5i binary to drive");
    };
    let url = fx.site.base.clone();
    let out = fx.run(&[
        "browser", "open", &url, "--in", "web", "--secret", "ACME_PASS",
    ]);
    assert!(!out.status.success());
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(why.contains("env.toml"), "the refusal names where to put it: {why}");
    assert!(why.contains("secrets"), "{why}");
}

// ── browser identities ───────────────────────────────────────────────────────
//
// Gated with the feature they exercise: without it there is no `--identity` to
// pass, and a test that asserted the flag was rejected would be pinning clap's
// error message rather than anything this crate decides.

#[cfg(feature = "identity")]
#[test]
fn an_identity_that_this_engine_cannot_back_refuses_the_session() {
    let Some(fixture) = Fixture::new() else {
        return skip("h5i is not built");
    };

    // Coherent as a description, and still refused: this engine sends no
    // Sec-CH-UA and has no WebGL, and a Chrome agent string in front of a
    // browser with neither is louder than an honest one. The refusal has to
    // reach the CLI rather than being a note in the engine's log, because the
    // session it would have started is the thing being prevented.
    let url = fixture.site.base.clone();
    let out = fixture.run(&[
        "browser",
        "open",
        url.as_str(),
        "--allow",
        "127.0.0.1",
        "--script",
        "--identity",
        "chrome-151-windows",
    ]);
    assert!(!out.status.success(), "a refused identity must not open a session");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("ua-client-hints"), "{said}");
    assert!(said.contains("webgl2"), "{said}");

    // And nothing was left behind: the check runs before anything is spawned.
    let list = fixture.run(&["browser", "list", "--json"]);
    let sessions: serde_json::Value =
        serde_json::from_slice(&list.stdout).unwrap_or(serde_json::Value::Array(vec![]));
    assert_eq!(
        sessions.as_array().map(Vec::len).unwrap_or(0),
        0,
        "a refused open left a session behind"
    );
}

#[cfg(feature = "identity")]
#[test]
fn the_identity_a_session_presented_is_in_its_record() {
    let Some(fixture) = Fixture::new() else {
        return skip("h5i is not built");
    };

    let id = fixture.open(&["--script", "--identity", "firefox-143-linux"]);
    let out = fixture.run(&["browser", "status", "--session", &id, "--json"]);
    let record: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("status prints a record");

    // The name is what someone typed; the digest is what was presented. A
    // hand-written identity file can be edited after the session opened, so the
    // digest is the half an audit can rely on.
    assert_eq!(record["identity"], "firefox-143-linux");
    let digest = record["identity_digest"].as_str().unwrap_or_default();
    assert_eq!(digest.len(), 16, "a digest belongs on the record: {record}");

    let _ = fixture.run(&["browser", "close", "--session", &id]);
}

#[cfg(feature = "identity")]
#[test]
fn identity_check_says_what_it_does_not_cover_as_plainly_as_what_it_does() {
    let Some(fixture) = Fixture::new() else {
        return skip("h5i is not built");
    };

    let out = fixture.run(&["browser", "identity", "check", "firefox-143-linux", "--script", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("check prints JSON");

    assert_eq!(report["admitted"], true);
    // The second list is the point. An identity that printed only what it
    // reaches would be promising invisibility, which is not what this is.
    let uncovered = report["does_not_cover"].to_string();
    assert!(uncovered.contains("ClientHello"), "{uncovered}");
    assert!(uncovered.contains("HTTP/2"), "{uncovered}");
}

/// h5i's default identity and the engine's are the same word.
///
/// This is the property the old `net_args` comment was reaching for when it
/// sent `--identity` on every invocation. It is a fact about two constants, and
/// paying for it on the wire broke every box running an older h5i, so it is
/// checked here instead, where it costs nothing and fails loudly.
#[cfg(feature = "identity")]
#[test]
fn the_two_defaults_are_one_word() {
    let Some(fixture) = Fixture::new() else {
        return skip("h5i is not built");
    };

    // What the engine falls back to when no `--identity` reaches it.
    let out = fixture.run(&["browser", "identity", "check", "native", "--json"]);
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("check prints JSON");
    assert_eq!(report["name"], "native");
    assert_eq!(report["mode"], "native");

    // And what h5i sends when nobody types the flag: nothing at all, so the
    // engine's own default is what decides. `identity` on the record is the
    // name h5i resolved, and the two have to agree or the record describes a
    // session that never ran.
    let id = fixture.open(&[]);
    let status = fixture.run(&["browser", "status", "--session", &id, "--json"]);
    let record: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status prints a record");
    assert_eq!(record["identity"], "native");
    let _ = fixture.run(&["browser", "close", "--session", &id]);
}

/// A default session sends the engine no `--identity` at all.
///
/// The regression this pins is specific and was shipped: `net_args` pushed the
/// flag unconditionally, so a boxed session whose in-box h5i predated it failed
/// at argument parsing: `h5i browser read --in <box>` stopped working for
/// callers who had never heard of identities. Every other flag here is
/// conditional; this one has to be too.
#[cfg(feature = "identity")]
#[test]
fn the_default_identity_adds_no_flag_an_older_engine_would_refuse() {
    let Some(fixture) = Fixture::new() else {
        return skip("h5i is not built");
    };

    // `--in` needs a box, so the check is made against the argv h5i builds for
    // the sessionless lane instead: with no `--identity` typed, the engine is
    // run with none, and the proof is that a build of the engine that has never
    // heard of the flag would have been given nothing to choke on.
    //
    // Asserted through behaviour rather than by reading argv: an engine that
    // received `--identity` and rejected it fails the read outright, and a
    // successful read is what says nothing unknown was passed.
    let url = fixture.site.base.clone();
    let out = fixture.run(&["browser", "read", url.as_str(), "--no-sandbox"]);
    assert!(
        out.status.success(),
        "a default read must not pass anything the engine could refuse: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // And when one *is* named, it does travel.
    let named = fixture.run(&[
        "browser", "read", url.as_str(), "--no-sandbox", "--script",
        "--identity", "chrome-151-windows",
    ]);
    assert!(!named.status.success(), "a refused identity must reach the engine");
    assert!(
        String::from_utf8_lossy(&named.stderr).contains("ua-client-hints"),
        "the refusal should come from the engine, not from clap"
    );
}

/// A file identity is refused in both lanes, and a mistyped name is not
/// mistaken for one.
///
/// Two bugs in one test because they are one mistake: the first fix checked
/// only whether the selector was a built-in, in only the `open` lane. So
/// `read --in` still sent a host path into a box, and a mistyped built-in was
/// told it had "named a file on this machine" that it never named.
#[cfg(feature = "identity")]
#[test]
fn a_file_identity_is_refused_in_a_box_and_a_typo_is_not_called_a_file() {
    let Some(fixture) = Fixture::new() else {
        return skip("h5i is not built");
    };

    let file = fixture.home.path().join("mine.toml");
    std::fs::write(&file, "name = \"x\"\n").expect("write an identity file");
    let path = file.display().to_string();
    let url = fixture.site.base.clone();

    // Both lanes name the boundary. `--in` names a box that does not exist, and
    // that is the point: this refusal comes *before* anything is spawned, so it
    // is reached whether or not the box is real.
    for args in [
        vec!["browser", "open", url.as_str(), "--in", "nobox", "--identity", &path],
        vec!["browser", "read", url.as_str(), "--in", "nobox", "--identity", &path],
    ] {
        let lane = args[1];
        let out = fixture.run(&args);
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{lane}: a host path must not go into a box");
        assert!(
            said.contains("names a file on this machine"),
            "{lane} did not name the boundary: {said}"
        );
    }

    // A mistyped built-in is a typo, not a file, and gets the answer that lists
    // what there is.
    let out = fixture.run(&[
        "browser", "read", url.as_str(), "--in", "nobox", "--identity", "firefox-143-linx",
    ]);
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        !said.contains("names a file on this machine"),
        "a typo was reported as a file: {said}"
    );

    // And on this machine a file identity is perfectly fine.
    let here = fixture.run(&["browser", "read", url.as_str(), "--no-sandbox", "--identity", &path]);
    assert!(
        !String::from_utf8_lossy(&here.stderr).contains("names a file on this machine"),
        "the boundary is a box's, not a file's"
    );
}
