//! End-to-end tests for the box console (`h5i ui`).
#![cfg(feature = "web")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use tempfile::TempDir;

const H5I: &str = env!("CARGO_BIN_EXE_h5i");

// ─── a repository with boxes in it ───────────────────────────────────────────

struct Repo {
    dir: PathBuf,
    /// Where browser sessions live for this test. Sessions are the machine's,
    /// not the repository's, so a test that did not pin this would read (and
    /// show) the developer's own registry.
    browser_home: PathBuf,
    _root: TempDir,
}

impl Repo {
    fn new() -> Repo {
        let root = TempDir::new().expect("tempdir");
        let dir = root.path().join("repo");
        ok(Command::new("git").args(["init", "-b", "main"]).arg(&dir));
        git(&dir, &["config", "user.name", "Console Tester"]);
        git(&dir, &["config", "user.email", "console@h5i.test"]);
        std::fs::write(dir.join("README.md"), "seed\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "seed"]);
        let browser_home = root.path().join("browser-home");
        std::fs::create_dir_all(&browser_home).expect("browser home");
        Repo {
            dir,
            browser_home,
            _root: root,
        }
    }

    fn h5i_ok(&self, args: &[&str]) -> Output {
        let out = Command::new(H5I)
            .args(args)
            .envs(self.env())
            .current_dir(&self.dir)
            .output()
            .expect("failed to run h5i");
        assert!(
            out.status.success(),
            "h5i {} failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        out
    }

    /// Hermetic: a fixed agent identity, and the workspace tier pinned so box
    /// creation never probes the host. These tests are about the HTTP surface,
    /// not about confinement. The kernel tiers are `env_integration.rs`'s job.
    fn env(&self) -> [(&'static str, String); 3] {
        [
            ("H5I_AGENT", "tester".to_string()),
            ("H5I_DEFAULT_ISOLATION", "workspace".to_string()),
            (
                "H5I_BROWSER_HOME",
                self.browser_home.display().to_string(),
            ),
        ]
    }

    /// Write a session record straight into the registry.
    ///
    /// Rather than opening one: these tests are about the HTTP surface, and a
    /// real session would need a target to fetch and an engine to run.
    fn session(&self, id: &str, name: &str, live: bool) -> PathBuf {
        let dir = self.browser_home.join("sessions").join(id);
        std::fs::create_dir_all(&dir).expect("session dir");
        let dir = dir.clone();
        let record = serde_json::json!({
            "id": id,
            "name": name,
            "engine": "h5i-light",
            "lane": "engine-claimed",
            "placement": {"kind": "host"},
            "url": "http://127.0.0.1:1/",
            "started_at": "2026-09-07T10:00:00.000000Z",
            "expires_at": null,
            "storage": "ephemeral",
            "policy_digest": "sha256:test",
            "identity": "native",
            "identity_digest": "test",
            "restored_from": null,
            "state": if live { "live" } else { "closed" },
            "ended_at": if live { serde_json::Value::Null } else { "2026-09-07T10:05:00.000000Z".into() },
            "end_reason": if live { serde_json::Value::Null } else { "closed by the user".into() },
            // A tagged enum, as the record writes it. Getting this shape wrong
            // is silently invisible: `list` skips a record it cannot read.
            "confinement": {"kind": "process"},
            "enclosing_box": null,
            "control": {"channel": "port", "file": null, "witness": null, "pid": null},
            "logs": {
                "actions": null,
                "requests": dir.join("requests.jsonl").display().to_string(),
            },
            "permissive_cors": false,
        });
        std::fs::write(
            dir.join("session.json"),
            serde_json::to_vec(&record).expect("record"),
        )
        .expect("write record");
        dir
    }
}

fn git(dir: &Path, args: &[&str]) {
    ok(Command::new("git").args(args).current_dir(dir));
}

fn ok(cmd: &mut Command) {
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{:?} failed: {}",
        cmd,
        String::from_utf8_lossy(&out.stderr)
    );
}

// ─── the console under test ──────────────────────────────────────────────────

struct Console {
    child: Child,
    /// `127.0.0.1:<port>`: also the `Host` header and the self-origin.
    addr: String,
    token: String,
}

impl Console {
    /// Start `h5i ui` on an OS-assigned port and wait for its URL line.
    ///
    /// Reading that line is a sufficient handshake, by construction: the
    /// command binds the listener *before* it prints (that is why
    /// `Console::bind` and `Console::serve` are separate), so by the time the
    /// URL exists the socket is listening and a connection can only queue in
    /// the accept backlog. Never be refused.
    fn start(repo: &Repo) -> Console {
        let mut child = Command::new(H5I)
            .args(["ui", "--port", "0"])
            .envs(repo.env())
            .current_dir(&repo.dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn h5i ui");

        let stdout = child.stdout.take().expect("piped stdout");
        let mut lines = BufReader::new(stdout).lines();
        let url = loop {
            match lines.next() {
                Some(Ok(line)) if line.contains("http://127.0.0.1:") => break line,
                Some(Ok(_)) => continue,
                // The process died before printing a URL. Kill-and-report
                // rather than block forever on a pipe that will never fill.
                _ => {
                    let _ = child.kill();
                    let mut err = String::new();
                    if let Some(mut e) = child.stderr.take() {
                        let _ = e.read_to_string(&mut err);
                    }
                    panic!("h5i ui exited before printing a URL; stderr:\n{err}");
                }
            }
        };

        let url = url.trim();
        let start = url.find("http://").expect("a url on the line");
        let url = &url[start..];
        let rest = url.strip_prefix("http://").unwrap();
        let (addr, query) = rest.split_once("/?").expect("host and query");
        let token = query
            .strip_prefix("token=")
            .expect("a token in the query")
            .to_string();
        assert_eq!(token.len(), 32, "128 bits of hex: {token}");

        // Keep draining stdout for the life of the console. Dropping the read
        // end here would close the pipe, and the next line the command prints
        // (it prints two more after the URL) would kill it, which surfaces as
        // a connection reset three assertions later, only when the machine is
        // busy enough for the ordering to flip. Detached: the reader ends when
        // `Drop` kills the child and the pipe closes.
        std::thread::spawn(move || lines.for_each(drop));

        Console {
            child,
            addr: addr.to_string(),
            token,
        }
    }

    fn get(&self, path: &str) -> Reply {
        self.request("GET", path, &[])
    }

    fn get_with(&self, path: &str, headers: &[(&str, &str)]) -> Reply {
        self.request("GET", path, headers)
    }

    /// With the token in the query string, the way the printed URL carries it.
    fn get_authed(&self, path: &str) -> Reply {
        let sep = if path.contains('?') { '&' } else { '?' };
        self.request("GET", &format!("{path}{sep}token={}", self.token), &[])
    }

    /// With the cookie the page holds after its first load.
    fn get_with_cookie(&self, path: &str) -> Reply {
        self.request(
            "GET",
            path,
            &[("Cookie", &format!("h5i_console={}", self.token))],
        )
    }

    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> Reply {
        let mut sock = TcpStream::connect(&self.addr).expect("connect to the console");
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.addr
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        sock.write_all(head.as_bytes()).expect("write request");
        // `Connection: close` makes the server hang up after the response, so
        // read-to-end terminates without parsing Content-Length.
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).expect("read response");
        Reply::parse(&raw)
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Reply {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

impl Reply {
    fn parse(raw: &[u8]) -> Reply {
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a header/body boundary");
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let status = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .expect("a status code");
        Reply {
            status,
            headers: head.to_lowercase(),
            body: raw[split + 4..].to_vec(),
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("expected JSON, got {e}: {}", self.text()))
    }
}

// ─── the gate ────────────────────────────────────────────────────────────────

#[test]
fn the_console_is_reachable_only_with_the_token_it_printed() {
    let repo = Repo::new();
    let ui = Console::start(&repo);

    assert_eq!(ui.get("/api/boxes").status, 401, "no token at all");
    assert_eq!(
        ui.get("/api/boxes?token=deadbeefdeadbeefdeadbeefdeadbeef")
            .status,
        401,
        "a wrong token of the right length"
    );
    assert_eq!(
        ui.get_with("/api/boxes", &[("Cookie", "h5i_console=nope")])
            .status,
        401,
        "a wrong cookie"
    );
    // Another loopback service's cookie is not this console's.
    assert_eq!(
        ui.get_with(
            "/api/boxes",
            &[("Cookie", &format!("jupyter={}", ui.token))]
        )
        .status,
        401,
        "the right value under the wrong cookie name"
    );

    assert_eq!(ui.get_authed("/api/boxes").status, 200);
    assert_eq!(ui.get_with_cookie("/api/boxes").status, 200);
}

#[test]
fn a_page_on_another_origin_cannot_read_the_console_even_holding_the_token() {
    let repo = Repo::new();
    let ui = Console::start(&repo);

    let refused = ui.get_with(
        &format!("/api/boxes?token={}", ui.token),
        &[("Origin", "http://evil.test")],
    );
    assert_eq!(refused.status, 403, "{}", refused.text());

    // The page's own fetches send Origin too, and must still get through.
    let same = ui.get_with(
        "/api/boxes",
        &[
            ("Origin", &ui.origin()),
            ("Cookie", &format!("h5i_console={}", ui.token)),
        ],
    );
    assert_eq!(same.status, 200, "{}", same.text());
}

#[test]
fn loading_the_page_hands_over_the_cookie_the_rest_of_the_session_uses() {
    let repo = Repo::new();
    let ui = Console::start(&repo);

    let page = ui.get_authed("/");
    assert_eq!(page.status, 200);
    assert!(
        page.headers.contains(&format!(
            "set-cookie: h5i_console={}; path=/; httponly; samesite=strict",
            ui.token
        )),
        "the page must set a strict, script-invisible cookie; got:\n{}",
        page.headers
    );
}

#[test]
fn the_console_has_no_way_to_change_anything() {
    let repo = Repo::new();
    let ui = Console::start(&repo);

    // Read-only is the console's central claim, and it is only true as long as
    // no route answers a mutating method. 405 means the router knows the path
    // and refuses the verb, which is the assertion, not 404.
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let reply = ui.request(
            method,
            &format!("/api/boxes?token={}", ui.token),
            &[("Content-Length", "0")],
        );
        assert_eq!(
            reply.status, 405,
            "{method} /api/boxes should be method-not-allowed, got {}",
            reply.status
        );
    }
}

// ─── the payloads ────────────────────────────────────────────────────────────

#[test]
fn an_empty_repository_renders_an_empty_fleet_rather_than_failing() {
    let repo = Repo::new();
    let ui = Console::start(&repo);

    let boxes = ui.get_authed("/api/boxes");
    assert_eq!(boxes.status, 200);
    assert_eq!(boxes.json().as_array().expect("an array").len(), 0);

    // A host report is available before any box exists. It is the top strip,
    // and a fleet of zero is exactly when someone reads it.
    let probe = ui.get_authed("/api/probe");
    assert_eq!(probe.status, 200);
    let probe = probe.json();
    assert!(probe["os"].is_string(), "{probe}");
    assert!(
        probe["claims"].as_array().is_some_and(|c| !c.is_empty()),
        "the probe must enumerate the isolation claims: {probe}"
    );

    assert_eq!(ui.get_authed("/api/box/tester/nope").status, 404);
}

#[test]
fn a_box_and_its_run_reach_the_fleet_and_the_detail_pane() {
    let repo = Repo::new();
    repo.h5i_ok(&["box", "create", "consoled"]);
    repo.h5i_ok(&["box", "run", "consoled", "--", "sh", "-c", "echo hello"]);
    let ui = Console::start(&repo);

    let fleet = ui.get_authed("/api/boxes").json();
    let rows = fleet.as_array().expect("an array");
    assert_eq!(rows.len(), 1, "{fleet}");
    let row = &rows[0];
    // The manifest is flattened in, so a fleet row is a superset of
    // `h5i box list --json`. If that ever stops being true the console and the
    // scriptable CLI have started describing a box two different ways.
    assert_eq!(row["id"], "env/tester/consoled");
    assert_eq!(row["agent"], "tester");
    assert_eq!(row["isolation_claim"], "workspace");
    assert!(row["policy_digest"].as_str().is_some_and(|d| d.len() == 64));
    assert_eq!(row["drift"], "up-to-date");
    assert_eq!(row["has_workspace"], true);

    let signals = &row["signals"];
    assert_eq!(signals["runs"], 1, "the run should have left a receipt");
    assert_eq!(signals["failed"], 0);
    assert_eq!(signals["egress_denied"], 0);
    assert_eq!(signals["verdict"], "clean");
    // Workspace tier confines nothing, and the console has to keep saying so
    // rather than let a green "clean" imply containment.
    assert_eq!(signals["weak_isolation"], true);
    assert_eq!(signals["host_observed"], 1);
    assert_eq!(signals["box_claimed_only"], false);

    let detail = ui.get_authed("/api/box/tester/consoled");
    assert_eq!(detail.status, 200);
    let detail = detail.json();
    assert_eq!(detail["item"]["id"], "env/tester/consoled");
    assert_eq!(detail["policy"]["isolation"], "workspace");
    assert!(
        detail["policy"]["wall_secs"].as_u64().is_some(),
        "the enforced-policy panel needs the limits: {}",
        detail["policy"]
    );
    assert!(
        detail["events"]
            .as_array()
            .is_some_and(|e| e.iter().any(|ev| ev["event"] == "created")),
        "the event log should carry creation: {}",
        detail["events"]
    );

    let receipts = detail["receipts"].as_array().expect("receipts");
    assert_eq!(receipts.len(), 1, "{:?}", receipts);
    let receipt = &receipts[0];
    assert_eq!(receipt["exit_code"], 0);
    assert_eq!(
        receipt["source"], "host-env-run",
        "an `h5i box run` is observed from the host, not claimed by the box"
    );
    assert_eq!(detail["receipts_folded"], 0);

    // The rendered receipt is the same text `h5i box inspect` prints.
    let id = receipt["id"].as_str().expect("a receipt id");
    let render = ui.get_authed(&format!("/api/box/tester/consoled/receipts/{id}"));
    assert_eq!(render.status, 200);
    let text = render.json()["render"]
        .as_str()
        .expect("a render field")
        .to_string();
    assert!(text.contains("echo hello"), "{text}");
    assert!(text.contains("env/tester/consoled"), "{text}");
}

#[test]
fn one_boxs_receipt_cannot_be_read_through_another_box() {
    let repo = Repo::new();
    repo.h5i_ok(&["box", "create", "alpha"]);
    repo.h5i_ok(&["box", "create", "beta"]);
    repo.h5i_ok(&["box", "run", "alpha", "--", "sh", "-c", "echo secret"]);
    let ui = Console::start(&repo);

    let alpha = ui.get_authed("/api/box/tester/alpha").json();
    let id = alpha["receipts"][0]["id"]
        .as_str()
        .expect("alpha has a receipt")
        .to_string();

    assert_eq!(
        ui.get_authed(&format!("/api/box/tester/alpha/receipts/{id}"))
            .status,
        200,
        "its own box can read it"
    );
    // `env::inspect` enforces ownership; the route must not have a way around
    // it, or a capture id would be a read primitive over every box on the host.
    assert_eq!(
        ui.get_authed(&format!("/api/box/tester/beta/receipts/{id}"))
            .status,
        404,
        "a sibling box must not be able to read it"
    );
}

// ─── the bundle ──────────────────────────────────────────────────────────────

#[test]
fn the_binary_carries_a_real_console_and_not_the_build_scripts_stub() {
    let repo = Repo::new();
    let ui = Console::start(&repo);

    let page = ui.get_authed("/");
    assert_eq!(page.status, 200);
    let html = page.text();

    // The stub `build.rs` writes when H5I_SKIP_WEB_BUILD is set says so in as
    // many words. A binary serving it would look completely healthy (200, a
    // page, no error anywhere) which is exactly why it is asserted against.
    assert!(
        !html.contains("console bundle not built"),
        "this binary embeds the build-script stub, not the console. Build the \
         frontend (`cd web && npm ci && npm run build`) and rebuild without \
         H5I_SKIP_WEB_BUILD.\n{html}"
    );

    let asset = html
        .split_once("/assets/")
        .map(|(_, rest)| rest.split(['"', '\'']).next().unwrap_or("").to_string())
        .expect("the page should reference a bundled asset");
    assert!(asset.ends_with(".js"), "expected a script asset, got {asset}");

    let served = ui.get_authed(&format!("/assets/{asset}"));
    assert_eq!(served.status, 200, "the referenced asset must be embedded");
    assert!(
        served.headers.contains("content-type: text/javascript"),
        "assets need their real media type or the browser refuses the module:\n{}",
        served.headers
    );
    assert!(!served.body.is_empty());

    assert_eq!(
        ui.get_authed("/assets/does-not-exist.js").status,
        404,
        "a missing asset is a 404, not a panic"
    );
}

#[test]
fn the_console_ships_the_same_fence_the_engine_prints() {
    // The engine wraps page content before it reaches a *model*, because that
    // is the moment attacker-controlled text meets something deciding what to do
    // next. The console showed the same text (page URLs, console output, policy
    // subjects, the rendered frame) to a *person*, with no boundary at all,
    // which left the human reader with less framing than the model got.
    //
    // Asserted against the served bundle rather than the source, because a
    // component that exists and is never rendered would pass a source grep and
    // fail the reader.
    let repo = Repo::new();
    let ui = Console::start(&repo);

    let html = ui.get_authed("/").text();
    let mut scripts: Vec<String> = Vec::new();
    let mut rest = html.as_str();
    while let Some((_, after)) = rest.split_once("/assets/") {
        let name = after.split(['"', '\'']).next().unwrap_or("").to_string();
        if name.ends_with(".js") {
            scripts.push(name);
        }
        rest = after;
    }
    assert!(!scripts.is_empty(), "no script assets referenced:\n{html}");

    // The markers are byte-identical to the engine's on purpose: a reader who
    // has seen one should recognise the other.
    let begin = "--- BEGIN UNTRUSTED PAGE CONTENT ---";
    let end = "--- END UNTRUSTED PAGE CONTENT ---";
    let found = scripts.iter().any(|name| {
        let body = ui.get_authed(&format!("/assets/{name}")).body;
        let text = String::from_utf8_lossy(&body);
        text.contains(begin) && text.contains(end)
    });
    assert!(
        found,
        "the console bundle does not carry the fence markers, so the page-derived \
         panes are rendered to a person with no boundary around them"
    );
}

// ─── sessions ────────────────────────────────────────────────────────────────

#[test]
fn the_console_lists_browser_sessions_with_the_evidence_behind_each_state() {
    let repo = Repo::new();
    repo.session("br_live", "watching", true);
    repo.session("br_over", "finished", false);
    let ui = Console::start(&repo);

    let fleet = ui.get_authed("/api/sessions").json();
    assert_eq!(fleet["total"], 2);
    assert_eq!(fleet["live"], 1);

    let rows = fleet["sessions"].as_array().expect("sessions");
    let by_name = |name: &str| {
        rows.iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("no session named {name} in {rows:?}"))
            .clone()
    };

    // A record that says live with no engine behind it is the one case this
    // cannot classify, and the console says so rather than guessing.
    let live = by_name("watching");
    assert_eq!(live["attention"]["state"], "unknown");
    assert!(
        live["attention"]["why"]
            .as_str()
            .unwrap_or_default()
            .contains("control file"),
        "a state has to carry its evidence: {live:?}"
    );

    // An ending nobody has read is `done`; which client has read it is the
    // client's business, not the server's.
    let over = by_name("finished");
    assert_eq!(over["attention"]["state"], "done");
    assert_eq!(over["attention"]["why"], "closed by the user");
}

#[test]
fn a_session_detail_answers_with_its_own_files_and_nothing_else() {
    let repo = Repo::new();
    let dir = repo.session("br_one", "one", false);
    std::fs::write(
        dir.join("requests.jsonl"),
        "{\"seq\":0,\"at\":\"2026-09-07T10:00:00.000000Z\",\"phase\":\"request\",\
         \"initiator\":\"navigation\",\"method\":\"GET\",\
         \"url\":\"https://target.test/a\",\"allowed\":true}\n         {\"seq\":1,\"at\":\"2026-09-07T10:00:01.000000Z\",\"phase\":\"request\",\
         \"initiator\":\"subresource\",\"method\":\"GET\",\
         \"url\":\"https://elsewhere.test/x\",\"allowed\":false,\
         \"denied_reason\":\"not in the allowlist\"}\n",
    )
    .expect("write log");
    let ui = Console::start(&repo);

    let detail = ui.get_authed("/api/session/br_one").json();
    assert_eq!(detail["requests"], 2, "both fetches counted");
    assert_eq!(detail["denied"], 1, "a refusal is counted apart");
    assert_eq!(detail["captured"], serde_json::Value::Null, "capture was off");
    assert_eq!(
        detail["requests_log"].as_array().map(Vec::len),
        Some(2),
        "the log itself, for the request table"
    );
}

#[test]
fn a_session_id_that_is_not_one_component_reads_nothing() {
    let repo = Repo::new();
    repo.session("br_one", "one", true);
    let ui = Console::start(&repo);

    // The id becomes a path. A boxed session writes its own record, so this is
    // target-adjacent input, and the guard belongs on the route.
    for id in ["..", "%2e%2e%2fetc", "a/b"] {
        let reply = ui.get_authed(&format!("/api/session/{id}"));
        assert_ne!(
            reply.status, 200,
            "`{id}` should not resolve to a session, got {}",
            reply.status
        );
    }
}
