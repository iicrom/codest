//! Context externalization ("indexing") — the storage contract only.
//!
//! Planned feature, not wired into any flow yet: the `/context` viewer will
//! be able to archive a context entry by writing its full text here and
//! replacing the in-context message with a short summary plus the archive
//! path, so a long document stops consuming context window but stays
//! reachable (the "索引地址" points at the local file).
//!
//! The wiring needs nothing new from the protocol: the replacement is an
//! ordinary [`crate::core::protocol::Op::EditMessage`], and the rollout's
//! `ContextEdited` marker replays it like any other edit. What this module
//! fixes today is the contract the future caller will program against —
//! where archives live, how they are named, and what the in-context stub
//! looks like — so the feature lands as a caller, not a format change.
//!
//! ```text
//! viewer: archive entry ──▶ write_entry(store, full text) ──▶ path
//!        ──▶ Op::EditMessage { content: stub_text(summary, path) }
//! ```

use std::path::Path;
use std::path::PathBuf;

use crate::core::rollout::codest_home;

/// Root of the archive: one directory per session, keyed by the rollout id
/// that anchors the session's JSONL file too.
pub fn store_root() -> PathBuf {
    codest_home().join("context-store")
}

/// Directory holding one session's archived entries.
pub fn session_dir(rollout_id: &str) -> PathBuf {
    store_root().join(rollout_id)
}

/// Write one archived entry and return the path that context will reference.
/// Entries are numbered rather than content-hashed: archive order is the
/// order a user reads them in, and the caller already holds the content.
pub fn write_entry(rollout_id: &str, index: usize, content: &str) -> std::io::Result<PathBuf> {
    write_entry_under(&store_root(), rollout_id, index, content)
}

/// [`write_entry`] against an explicit root — the seam tests (and any future
/// front-end) use instead of reaching for the process environment.
pub fn write_entry_under(
    root: &Path,
    rollout_id: &str,
    index: usize,
    content: &str,
) -> std::io::Result<PathBuf> {
    let dir = root.join(rollout_id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("entry-{index:04}.md"));
    std::fs::write(&path, content)?;
    Ok(path)
}

/// The stub that replaces an archived entry in the live context: a summary
/// line the model can still reason about, plus the local path to reload the
/// full text from.
pub fn stub_text(summary: &str, path: &Path) -> String {
    format!(
        "[archived] {summary}\n(full text stored locally at {}; load it with the fs tools when needed)",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codest-context-store-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn entries_write_under_the_session_directory_in_order() {
        let root = scratch("write");
        let first = write_entry_under(&root, "abc123", 0, "long document").expect("write");
        let second = write_entry_under(&root, "abc123", 1, "another").expect("write");
        assert!(first.starts_with(root.join("abc123")));
        assert_eq!(first.file_name().unwrap().to_str(), Some("entry-0000.md"));
        assert_eq!(second.file_name().unwrap().to_str(), Some("entry-0001.md"));
        assert_eq!(
            std::fs::read_to_string(&first).expect("read"),
            "long document"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_stub_names_the_summary_and_the_archive_path() {
        let stub = stub_text("quarterly report, 3 findings", Path::new("/h/e-0007.md"));
        assert!(
            stub.starts_with("[archived] quarterly report, 3 findings"),
            "{stub}"
        );
        assert!(stub.contains("/h/e-0007.md"), "{stub}");
    }

    #[test]
    fn the_store_lives_beside_the_sessions_directory() {
        // Contract shape only (no env writes): rooted at the codest home,
        // one subdirectory per rollout id.
        assert_eq!(
            session_dir("abc"),
            codest_home().join("context-store").join("abc")
        );
    }
}
