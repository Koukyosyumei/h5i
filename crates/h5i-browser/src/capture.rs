//! Opt-in storage for message data omitted from receipts.
//!
//! Receipts remain safe to export; this bounded, owner-only store may contain
//! bodies and credentials. Writes are best-effort and failures are counted, so
//! capture never blocks a fetch. Missing and truncated bodies retain an explicit
//! state instead of appearing empty.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use h5i_error::H5iError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// The stored shapes are data, and their readers (the websec plugin, recon's
// ledger) have no business linking an engine to name them. They live in
// `h5i-wire`; the writer below stays here, because writing them is the
// engine's job and nobody else's.
pub use h5i_wire::message::{
    Body, Health, MAX_BODY_BYTES, MAX_STORE_BYTES, Skip, StoredRequest, StoredResponse, body_file,
};
use h5i_wire::record::now_rfc3339;

/// Skip large media unlikely to aid inspection. Images remain capturable for
/// upload analysis.
fn is_skipped_type(content_type: Option<&str>) -> bool {
    let Some(kind) = content_type else {
        return false;
    };
    let kind = kind.trim().to_ascii_lowercase();
    kind.starts_with("font/")
        || kind.starts_with("audio/")
        || kind.starts_with("video/")
        || kind.starts_with("application/font")
}





/// Response data offered to the store.
#[derive(Debug, Clone)]
pub struct Response<'a> {
    pub seq: u64,
    /// The URL this hop was sent to.
    pub url: &'a str,
    pub status: Option<u16>,
    pub headers: Vec<(String, String)>,
    /// How the body was encoded on the wire, when it was.
    pub content_encoding: Option<String>,
    /// What crossed the wire, when that is a different number.
    pub wire_bytes: Option<u64>,
    pub body: Received<'a>,
    /// What the connection carried after this response. See
    /// [`StoredResponse::trailing`].
    pub trailing: &'a [u8],
}

/// What the engine has to offer the store for a response body.
#[derive(Debug, Clone, Copy)]
pub enum Received<'a> {
    /// The decoded body.
    Bytes(&'a [u8]),
    /// There was a response, and its body was never read. See [`Skip::NotRead`].
    NotRead,
}



/// A session's stored messages.
pub struct Capture {
    dir: PathBuf,
    bodies: PathBuf,
    /// Bytes of *body* held: what a reader means by "how much evidence is in
    /// here", and half of what [`MAX_STORE_BYTES`] bounds.
    used: AtomicU64,
    /// Bytes of message file beside them, the other half of the allowance.
    ///
    /// These were outside it entirely, and they are not small: the raw sender
    /// accepts a response head as large as the whole response cap.
    overhead: AtomicU64,
    /// Hashes already stored, used for deduplication and byte accounting.
    seen: Mutex<HashSet<String>>,
    /// How many messages this store failed to write. Reported rather than
    /// raised; see the module documentation.
    errors: AtomicU64,
    /// How many it wrote, so a reader can tell an empty store from a broken one.
    messages: AtomicU64,
}

impl Capture {
    /// Open or create a store at `dir`.
    pub fn open(dir: &Path) -> Result<Self, H5iError> {
        let bodies = dir.join("bodies");
        std::fs::create_dir_all(&bodies).map_err(|e| H5iError::with_path(e, &bodies))?;
        owner_only_dir(dir);
        owner_only_dir(&bodies);
        // What is already here counts against the restored session's quota.
        let mut used = 0u64;
        let mut seen = HashSet::new();
        if let Ok(entries) = std::fs::read_dir(&bodies) {
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                used = used.saturating_add(meta.len());
                if let Some(name) = entry.file_name().to_str() {
                    seen.insert(name.to_string());
                }
            }
        }
        let mut overhead = 0u64;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata()
                    && meta.is_file()
                {
                    overhead = overhead.saturating_add(meta.len());
                }
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            bodies,
            used: AtomicU64::new(used),
            overhead: AtomicU64::new(overhead),
            seen: Mutex::new(seen),
            errors: AtomicU64::new(0),
            messages: AtomicU64::new(0),
        })
    }

    /// Where this store lives.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// How many messages could not be written.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// How many bytes of body this store holds.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Everything this store has put on disk: bodies and message files. What
    /// [`MAX_STORE_BYTES`] is a bound on.
    fn held(&self) -> u64 {
        self.used().saturating_add(self.overhead.load(Ordering::Relaxed))
    }

    /// What this store has done, for whoever is asking whether to trust it.
    pub fn health(&self) -> Health {
        Health {
            messages: self.messages.load(Ordering::Relaxed),
            bytes: self.used(),
            errors: self.errors(),
        }
    }

    /// Store a request, as built, just before it goes to the wire.
    pub fn request(
        &self,
        seq: u64,
        method: &str,
        url: &str,
        headers: Vec<(String, String)>,
        body: &[u8],
        content_type: Option<&str>,
    ) {
        let stored = StoredRequest {
            seq,
            at: now_rfc3339(),
            method: method.to_string(),
            url: url.to_string(),
            headers,
            body: self.store_body(Received::Bytes(body), content_type),
        };
        self.write(seq, "request", &stored);
    }

    /// Store a response, as received.
    pub fn response(&self, response: Response<'_>) {
        let content_type = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone());
        let stored = StoredResponse {
            seq: response.seq,
            at: now_rfc3339(),
            url: response.url.to_string(),
            status: response.status,
            headers: response.headers,
            content_encoding: response.content_encoding,
            wire_bytes: response.wire_bytes,
            body: self.store_body(response.body, content_type.as_deref()),
            trailing: (!response.trailing.is_empty())
                .then(|| self.store_body(Received::Bytes(response.trailing), None)),
        };
        self.write(stored.seq, "response", &stored);
    }

    /// Read one stored request back.
    pub fn read_request(&self, seq: u64) -> Result<StoredRequest, H5iError> {
        self.read(seq, "request")
    }

    /// Read one stored response back.
    pub fn read_response(&self, seq: u64) -> Result<StoredResponse, H5iError> {
        self.read(seq, "response")
    }

    /// Read a body out of the store, by the hash a [`Body::Stored`] names.
    pub fn read_body(&self, sha256: &str) -> Result<Vec<u8>, H5iError> {
        let path = self.body_path(sha256)?;
        std::fs::read(&path).map_err(|e| H5iError::with_path(e, &path))
    }

    /// The file a hash names, refusing anything that is not one.
    fn body_path(&self, sha256: &str) -> Result<PathBuf, H5iError> {
        body_file(&self.dir, sha256).ok_or_else(|| {
            H5iError::Internal(format!(
                "{sha256:?} is not a body hash, so there is nothing in the store to read"
            ))
        })
    }

    /// Put a body in the store, and say what became of it.
    fn store_body(&self, body: Received<'_>, content_type: Option<&str>) -> Body {
        let bytes = match body {
            Received::NotRead => {
                return Body::Skipped {
                    reason: Skip::NotRead,
                    bytes: None,
                };
            }
            Received::Bytes(bytes) => bytes,
        };
        if bytes.is_empty() {
            return Body::Empty;
        }
        let full = bytes.len() as u64;
        if is_skipped_type(content_type) {
            return Body::Skipped {
                reason: Skip::Media,
                bytes: Some(full),
            };
        }
        let truncated = bytes.len() > MAX_BODY_BYTES;
        let kept = if truncated {
            &bytes[..MAX_BODY_BYTES]
        } else {
            bytes
        };

        let sha256 = hex(&Sha256::digest(kept));
        let path = self.bodies.join(&sha256);

        // Already stored, by this session or by the one that left this
        // directory behind. Content addressing makes that check free, and it is
        // the common case: a loop that replays one request a hundred times
        // usually gets one of two answers back.
        {
            let mut seen = match self.seen.lock() {
                Ok(seen) => seen,
                Err(poisoned) => poisoned.into_inner(),
            };
            if seen.contains(&sha256) {
                return Body::Stored {
                    sha256,
                    bytes: kept.len() as u64,
                    of_bytes: truncated.then_some(full),
                    truncated,
                };
            }
            // The allowance is checked under the same lock that claims the
            // hash, so two threads storing two different bodies at once cannot
            // both decide there is room for the last of it.
            let used = self.held();
            if used.saturating_add(kept.len() as u64) > MAX_STORE_BYTES {
                // Refused rather than evicted. Eviction wants a pin, so that a
                // body a finding rests on is not the one thrown away to make
                // room for a font; until `websec pin` exists there is nothing to
                // consult, and silently dropping the oldest evidence is worse
                // than refusing the newest and saying so.
                return Body::Skipped {
                    reason: Skip::StoreFull,
                    bytes: Some(full),
                };
            }
            if let Err(e) = write_owner_only(&path, kept) {
                self.errors.fetch_add(1, Ordering::Relaxed);
                let _ = e;
                return Body::Skipped {
                    reason: Skip::Failed,
                    bytes: Some(full),
                };
            }
            self.used.fetch_add(kept.len() as u64, Ordering::Relaxed);
            seen.insert(sha256.clone());
        }

        Body::Stored {
            sha256,
            bytes: kept.len() as u64,
            of_bytes: truncated.then_some(full),
            truncated,
        }
    }

    /// Write one message file. Best-effort, and counted when it fails.
    ///
    /// Against the same allowance a body is: these hold every header, and
    /// leaving them outside the quota left the directory unbounded.
    fn write<T: Serialize>(&self, seq: u64, phase: &str, message: &T) {
        let path = self.dir.join(format!("{seq}.{phase}.json"));
        let wrote = serde_json::to_vec(message)
            .map_err(H5iError::from)
            .and_then(|bytes| {
                let used = self.held();
                if used.saturating_add(bytes.len() as u64) > MAX_STORE_BYTES {
                    // Refused rather than evicted, like a body, and counted.
                    return Err(H5iError::Metadata(format!(
                        "the message store is full ({used} of {MAX_STORE_BYTES} bytes)"
                    )));
                }
                write_owner_only(&path, &bytes)?;
                self.overhead.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Ok(())
            });
        match wrote {
            Ok(()) => self.messages.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.errors.fetch_add(1, Ordering::Relaxed),
        };
    }

    fn read<T: for<'de> Deserialize<'de>>(&self, seq: u64, phase: &str) -> Result<T, H5iError> {
        let path = self.dir.join(format!("{seq}.{phase}.json"));
        let bytes = std::fs::read(&path).map_err(|e| H5iError::with_path(e, &path))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// Lowercase hex, for a hash that is also a file name.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Write a file readable only by its owner.
///
/// This holds session cookies and `Authorization` in full, in a directory that
/// can sit under a shared `/tmp`. `OpenOptions::mode` applies only on create,
/// so a pre-made 0666 file got the credentials written into it and a symlink
/// got them written wherever it pointed.
fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), H5iError> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(|e| H5iError::with_path(e, path))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // On the handle, not the path.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(H5iError::Io)?;
    }
    file.write_all(bytes).map_err(H5iError::Io)?;
    file.flush().map_err(H5iError::Io)?;
    Ok(())
}

/// Narrow a directory to its owner. Best-effort: a filesystem without modes is
/// not a reason to refuse to capture.
fn owner_only_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Capture) {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = Capture::open(&dir.path().join("messages")).expect("store opens");
        (dir, capture)
    }

    /// `mode` applies only to a file it creates, so one already there kept
    /// whatever mode it had — with `Cookie` and `Authorization` written into it.
    #[cfg(unix)]
    #[test]
    fn a_file_that_was_already_there_is_narrowed_rather_than_written_wide() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("waiting.json");
        std::fs::write(&path, b"{}").expect("a file to be there first");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
            .expect("wide open");

        write_owner_only(&path, b"{\"secret\":true}").expect("writes");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    /// And a symlink is somebody else naming the file this is about to truncate.
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_refused_rather_than_followed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, b"do not touch").expect("a target");
        let path = dir.path().join("42.response.json");
        std::os::unix::fs::symlink(&elsewhere, &path).expect("a symlink in the way");

        assert!(write_owner_only(&path, b"{}").is_err(), "a symlink is refused");
        assert_eq!(
            std::fs::read(&elsewhere).expect("still there"),
            b"do not touch",
            "and what it pointed at is untouched"
        );
    }

    /// Message files were written outside the quota entirely, so `used` read
    /// as empty while the directory grew.
    #[test]
    fn a_message_file_is_counted_against_the_store_the_way_a_body_is() {
        let (_dir, capture) = store();
        assert_eq!(capture.used(), 0);

        capture.request(
            1,
            "GET",
            "https://app.test/a",
            vec![("x-big".to_string(), "v".repeat(50_000))],
            b"",
            None,
        );

        assert_eq!(capture.used(), 0, "a GET has no body, so no body is held");
        assert!(
            capture.held() > 50_000,
            "but its headers are on disk and inside the allowance: {}",
            capture.held()
        );
        assert_eq!(capture.errors(), 0);
    }

    /// And the allowance is over both.
    #[test]
    fn message_files_fill_the_same_allowance_a_body_does() {
        let (_dir, capture) = store();
        capture.overhead.store(MAX_STORE_BYTES, Ordering::Relaxed);
        let body = capture.store_body(Received::Bytes(b"evidence"), Some("text/plain"));
        assert!(
            matches!(body, Body::Skipped { reason: Skip::StoreFull, .. }),
            "{body:?}"
        );
    }

    #[test]
    fn a_request_and_its_response_round_trip() {
        let (_dir, capture) = store();
        capture.request(
            7,
            "POST",
            "https://example.test/login",
            vec![("cookie".to_string(), "session=abc".to_string())],
            b"user=admin",
            Some("application/x-www-form-urlencoded"),
        );
        capture.response(Response {
            seq: 7,
            url: "https://example.test/login",
            status: Some(302),
            headers: vec![("location".to_string(), "/home".to_string())],
            content_encoding: None,
            wire_bytes: None,
            body: Received::Bytes(b"see other"),
            trailing: &[],
        });

        let request = capture.read_request(7).expect("request reads back");
        assert_eq!(request.method, "POST");
        // The store holds what a receipt refuses to: the credential itself.
        assert_eq!(request.headers[0].1, "session=abc");
        let Body::Stored { sha256, bytes, .. } = &request.body else {
            panic!("a body was stored: {:?}", request.body);
        };
        assert_eq!(*bytes, 10);
        assert_eq!(capture.read_body(sha256).expect("body reads"), b"user=admin");

        let response = capture.read_response(7).expect("response reads back");
        assert_eq!(response.status, Some(302));
        assert_eq!(capture.errors(), 0);
    }

    #[test]
    fn an_empty_body_is_empty_and_not_a_stored_nothing() {
        let (_dir, capture) = store();
        capture.request(1, "GET", "https://example.test/", Vec::new(), b"", None);
        assert_eq!(capture.read_request(1).expect("reads").body, Body::Empty);
    }

    #[test]
    fn a_body_never_read_says_so() {
        let (_dir, capture) = store();
        capture.response(Response {
            seq: 2,
            url: "https://example.test/",
            status: Some(301),
            headers: Vec::new(),
            content_encoding: None,
            wire_bytes: None,
            body: Received::NotRead,
            trailing: &[],
        });
        assert_eq!(
            capture.read_response(2).expect("reads").body,
            Body::Skipped {
                reason: Skip::NotRead,
                bytes: None,
            }
        );
    }

    #[test]
    fn media_is_named_rather_than_kept() {
        let (_dir, capture) = store();
        capture.response(Response {
            seq: 3,
            url: "https://example.test/f.woff2",
            status: Some(200),
            headers: vec![("content-type".to_string(), "font/woff2".to_string())],
            content_encoding: None,
            wire_bytes: None,
            body: Received::Bytes(b"not really a font"),
            trailing: &[],
        });
        assert_eq!(
            capture.read_response(3).expect("reads").body,
            Body::Skipped {
                reason: Skip::Media,
                bytes: Some(17),
            }
        );
        assert_eq!(capture.used(), 0);
    }

    #[test]
    fn one_body_stored_twice_occupies_one_file() {
        let (_dir, capture) = store();
        for seq in 0..5 {
            capture.response(Response {
                seq,
                url: "https://example.test/",
                status: Some(200),
                headers: Vec::new(),
                content_encoding: None,
                wire_bytes: None,
                body: Received::Bytes(b"the same answer"),
                trailing: &[],
            });
        }
        assert_eq!(capture.used(), 15);
        let first = capture.read_response(0).expect("reads").body;
        let last = capture.read_response(4).expect("reads").body;
        assert_eq!(first, last);
    }

    #[test]
    fn a_reopened_store_inherits_what_it_already_holds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("messages");
        let capture = Capture::open(&path).expect("opens");
        capture.response(Response {
            seq: 1,
            url: "https://example.test/",
            status: Some(200),
            headers: Vec::new(),
            content_encoding: None,
            wire_bytes: None,
            body: Received::Bytes(b"body"),
            trailing: &[],
        });
        drop(capture);

        let reopened = Capture::open(&path).expect("reopens");
        assert_eq!(reopened.used(), 4, "the earlier session's bytes still count");
    }


    #[test]
    fn a_hash_that_is_not_one_names_nothing() {
        let (_dir, capture) = store();
        let escaped = capture.read_body("../../../etc/passwd");
        assert!(escaped.is_err(), "a body name cannot leave the store");
    }

    #[test]
    fn an_oversized_body_keeps_its_head_and_says_so() {
        let (_dir, capture) = store();
        let big = vec![b'a'; MAX_BODY_BYTES + 100];
        capture.response(Response {
            seq: 9,
            url: "https://example.test/big",
            status: Some(200),
            headers: Vec::new(),
            content_encoding: None,
            wire_bytes: None,
            body: Received::Bytes(&big),
            trailing: &[],
        });
        let Body::Stored {
            bytes,
            of_bytes,
            truncated,
            ..
        } = capture.read_response(9).expect("reads").body
        else {
            panic!("the head was kept");
        };
        assert!(truncated);
        assert_eq!(bytes, MAX_BODY_BYTES as u64);
        assert_eq!(of_bytes, Some(MAX_BODY_BYTES as u64 + 100));
    }
}
