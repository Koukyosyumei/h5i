//! The agent skill, carried by the binary.

use std::path::{Path, PathBuf};

use crate::error::H5iError;

/// One page of the skill: its path relative to the skill root, and its text.
pub struct Page {
    pub path: &'static str,
    pub text: &'static str,
}

/// Every page, in the order `install` writes them.
pub const PAGES: &[Page] = &[
    Page {
        path: "SKILL.md",
        text: include_str!("../../../skills/h5i/SKILL.md"),
    },
    Page {
        path: "references/boxes.md",
        text: include_str!("../../../skills/h5i/references/boxes.md"),
    },
    Page {
        path: "references/browser.md",
        text: include_str!("../../../skills/h5i/references/browser.md"),
    },
    Page {
        path: "references/policy.md",
        text: include_str!("../../../skills/h5i/references/policy.md"),
    },
    Page {
        path: "references/export.md",
        text: include_str!("../../../skills/h5i/references/export.md"),
    },
    Page {
        path: "references/share.md",
        text: include_str!("../../../skills/h5i/references/share.md"),
    },
    Page {
        path: "references/troubleshooting.md",
        text: include_str!("../../../skills/h5i/references/troubleshooting.md"),
    },
    Page {
        path: "references/websec.md",
        text: include_str!("../../../skills/h5i/references/websec.md"),
    },
    Page {
        path: "references/recon.md",
        text: include_str!("../../../skills/h5i/references/recon.md"),
    },
];

/// The name the skill installs under.
pub const NAME: &str = "h5i";

/// Where an install writes by default: the per-user skill directory for the
/// runtime we are running under. `$H5I_SKILL_DIR` overrides it, which is how
/// box bootstrap points it at the in-box location.
pub fn default_target() -> Result<PathBuf, H5iError> {
    if let Ok(dir) = std::env::var("H5I_SKILL_DIR")
        && !dir.trim().is_empty()
    {
        return Ok(PathBuf::from(dir).join(NAME));
    }
    let home = std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| {
            H5iError::Metadata("cannot locate HOME — pass --target <dir>".to_string())
        })?;
    let runtime = match std::env::var("H5I_AGENT").as_deref() {
        Ok("codex") => "codex",
        _ => "claude",
    };
    Ok(PathBuf::from(home)
        .join(format!(".{runtime}"))
        .join("skills")
        .join(NAME))
}

/// Write every page under `target`, creating directories as needed. Returns the
/// paths written, so the caller can report exactly what landed where.
pub fn install(target: &Path) -> Result<Vec<PathBuf>, H5iError> {
    let mut written = Vec::new();
    for page in PAGES {
        let path = target.join(page.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| H5iError::with_path(e, parent))?;
        }
        std::fs::write(&path, page.text.as_bytes()).map_err(|e| H5iError::with_path(e, &path))?;
        written.push(path);
    }
    Ok(written)
}

/// One page's text by its relative path. `None` for the main page.
pub fn page(name: Option<&str>) -> Result<&'static str, H5iError> {
    let wanted = match name {
        None => "SKILL.md",
        Some(n) => n,
    };
    // Accept `policy`, `references/policy.md` and `policy.md` alike: the caller
    // is usually an agent typing from memory.
    let candidates = [
        wanted.to_string(),
        format!("{wanted}.md"),
        format!("references/{wanted}"),
        format!("references/{wanted}.md"),
    ];
    PAGES
        .iter()
        .find(|p| candidates.iter().any(|c| c == p.path))
        .map(|p| p.text)
        .ok_or_else(|| {
            let known: Vec<&str> = PAGES.iter().map(|p| p.path).collect();
            H5iError::Metadata(format!(
                "no skill page '{wanted}' — known pages: {}",
                known.join(", ")
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn every_page_has_content() {
        for p in PAGES {
            assert!(!p.text.trim().is_empty(), "{} is empty", p.path);
        }
    }

    #[test]
    fn skill_md_carries_frontmatter() {
        let text = page(None).unwrap();
        assert!(text.starts_with("---\nname: h5i\n"), "{}", &text[..40]);
        assert!(text.contains("description:"));
    }

    #[test]
    fn install_writes_every_page() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("h5i");
        let written = install(&target).unwrap();
        assert_eq!(written.len(), PAGES.len());
        for p in PAGES {
            assert!(target.join(p.path).is_file(), "{} missing", p.path);
        }
    }

    #[test]
    fn install_is_idempotent() {
        let td = TempDir::new().unwrap();
        let target = td.path().join("h5i");
        install(&target).unwrap();
        install(&target).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("SKILL.md")).unwrap(),
            page(None).unwrap()
        );
    }

    #[test]
    fn pages_resolve_by_short_name() {
        assert!(page(Some("policy")).is_ok());
        assert!(page(Some("policy.md")).is_ok());
        assert!(page(Some("references/policy.md")).is_ok());
        assert!(page(Some("websec")).is_ok());
        assert!(page(Some("websec.md")).is_ok());
        assert!(page(Some("recon")).is_ok());
        assert!(page(Some("references/recon.md")).is_ok());
        assert!(page(Some("nope")).is_err());
    }
}
