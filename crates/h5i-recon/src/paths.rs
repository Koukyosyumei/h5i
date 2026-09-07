//! Paths an application never disclosed, from a list the operator brings
//! (design-recon.md N10).
//!
//! h5i ships no wordlist. Generation is mechanical: extensions, backup forms,
//! the directory form, and the words this session has already seen.

use std::collections::BTreeSet;
use std::path::Path;

use h5i_error::H5iError;

/// The most words one list contributes, after cleaning.
pub const MAX_WORDS: usize = 100_000;

/// The most bytes read from a list. A wordlist is text; this is a file the
/// operator named, not a target's bytes, but a runaway is still a runaway.
pub const MAX_LIST_BYTES: u64 = 32 * 1024 * 1024;

/// Backup names an editor, an admin or a deploy leaves behind.
pub const BACKUP_FORMS: &[&str] = &[".bak", ".old", ".orig", ".save", "~", ".swp", ".copy"];

/// How a word becomes the paths worth asking for.
#[derive(Debug, Clone, Default)]
pub struct Shapes {
    /// Extensions to append: `php`, `json`, `bak`.
    pub extensions: Vec<String>,
    /// Also ask for backup forms of each word.
    pub backups: bool,
}

/// Read a wordlist: one entry per line, `#` comments and blanks dropped.
pub fn read_wordlist(path: &Path) -> Result<Vec<String>, H5iError> {
    let size = std::fs::metadata(path)
        .map_err(|e| match e.kind() {
            // A resume names the list the first run was given, so a list that
            // has since moved is the ordinary way this fails.
            std::io::ErrorKind::NotFound => H5iError::Metadata(format!(
                "no wordlist at {}. h5i ships none: name a list you have, and keep it                  where a resume can find it again",
                path.display()
            )),
            _ => H5iError::with_path(e, path),
        })?
        .len();
    if size > MAX_LIST_BYTES {
        return Err(H5iError::Metadata(format!(
            "{} is {size} bytes, larger than the {MAX_LIST_BYTES} this reads",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(path).map_err(|e| H5iError::with_path(e, path))?;
    Ok(clean(text.lines()))
}

/// Words worth trying that this session has already seen.
///
/// Path segments the crawl or an extractor recorded. A target's own vocabulary
/// beats a generic list, and it costs nothing to collect.
pub fn words_from_paths<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for path in paths {
        for segment in path.split('/') {
            let word = segment.split('.').next().unwrap_or_default();
            if is_wordlike(word) {
                seen.insert(word.to_string());
            }
        }
    }
    seen.into_iter().collect()
}

/// The request targets one word becomes, under one directory.
pub fn expand(directory: &str, word: &str, shapes: &Shapes) -> Vec<String> {
    let base = directory.trim_end_matches('/');
    // The word, and the word as a directory. A server that answers `/admin/`
    // and 404s `/admin` is ordinary, and asking only one of the two is how a
    // run misses the page it was looking for.
    let mut out: Vec<String> = vec![format!("{base}/{word}"), format!("{base}/{word}/")];
    for extension in &shapes.extensions {
        let extension = extension.trim().trim_start_matches('.');
        let candidate = format!("{base}/{word}.{extension}");
        if !extension.is_empty() && !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    if shapes.backups {
        // Backups of the word and of each extended form, which is where
        // `config.php.bak` lives. A directory has no backup.
        for stem in out.clone().into_iter().filter(|s| !s.ends_with('/')) {
            for form in BACKUP_FORMS {
                let candidate = format!("{stem}{form}");
                if !out.contains(&candidate) {
                    out.push(candidate);
                }
            }
        }
    }
    out
}

/// Entries worth asking for: trimmed, deduplicated, and free of what is not a
/// path segment.
fn clean<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for line in lines {
        let word = line.trim().trim_start_matches('/');
        if word.is_empty() || word.starts_with('#') || word.len() > 255 {
            continue;
        }
        // A list of URLs is a list for `recon import`, not for this.
        if word.contains("://") || word.contains(char::is_whitespace) {
            continue;
        }
        if seen.insert(word.to_string()) {
            out.push(word.to_string());
        }
        if out.len() >= MAX_WORDS {
            break;
        }
    }
    out
}

fn is_wordlike(word: &str) -> bool {
    word.len() >= 3
        && word.len() <= 32
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        && word.chars().any(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_becomes_the_forms_worth_asking_for() {
        let shapes = Shapes {
            extensions: vec!["php".to_string(), ".json".to_string()],
            backups: true,
        };
        let targets = expand("/admin/", "config", &shapes);
        assert!(targets.contains(&"/admin/config".to_string()));
        assert!(
            targets.contains(&"/admin/config/".to_string()),
            "a server that answers `/admin/config/` and 404s `/admin/config` is ordinary"
        );
        assert!(
            !targets.contains(&"/admin/config/.bak".to_string()),
            "a directory has no backup: {targets:?}"
        );
        assert!(targets.contains(&"/admin/config.php".to_string()));
        assert!(targets.contains(&"/admin/config.json".to_string()));
        assert!(
            targets.contains(&"/admin/config.php.bak".to_string()),
            "the backup of an extended form is where the interesting one lives: {targets:?}"
        );
        assert!(targets.contains(&"/admin/config~".to_string()));
    }

    #[test]
    fn nothing_is_asked_for_twice() {
        let shapes = Shapes {
            extensions: vec!["php".to_string(), "php".to_string()],
            ..Shapes::default()
        };
        let targets = expand("/", "index", &shapes);
        assert_eq!(
            targets,
            vec![
                "/index".to_string(),
                "/index/".to_string(),
                "/index.php".to_string()
            ]
        );
    }

    #[test]
    fn a_list_is_cleaned_and_a_url_list_is_not_a_wordlist() {
        let words = clean(
            "# comment\n\n  admin \n/backup\nadmin\nhttps://elsewhere.test/x\ntwo words\n".lines(),
        );
        assert_eq!(words, vec!["admin".to_string(), "backup".to_string()]);
    }

    #[test]
    fn the_targets_own_vocabulary_is_a_wordlist() {
        let words = words_from_paths(
            ["/admin/users", "/admin/users/42", "/static/app.js", "/a"].into_iter(),
        );
        assert_eq!(
            words,
            vec![
                "admin".to_string(),
                "app".to_string(),
                "static".to_string(),
                "users".to_string()
            ],
            "an id is not a word, and neither is a two-letter segment"
        );
    }
}
