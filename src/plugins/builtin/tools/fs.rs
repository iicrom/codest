//! The `fs` tools: `read`, `write`, and `edit`.
//!
//! * `read` — line-numbered window over a text file (dsh's caps: 2000
//!   chars/line, 50 KB total) with an `offset` continuation hint from pi.
//! * `write` — create or overwrite a file, parent directories included.
//! * `edit` — exact-string replacement (dsh's `old_string`/`new_string`/
//!   `replace_all` semantics: exactly one match unless `replace_all`), with
//!   pi's BOM strip/restore and CRLF preservation so the untouched lines
//!   keep their bytes.
//!
//! All paths resolve against the working directory when relative. Reads are
//! `read_only`; writes and edits are not — that flag drives `/tools` markers
//! and the future read-only profiles.

use std::path::PathBuf;

use super::truncate;
use super::workdir;

/// Lines `read` returns when the model does not ask for a window.
const DEFAULT_LIMIT: usize = truncate::MAX_LINES;

/// Resolve `path` against the working directory when relative, and refuse
/// directories early with a clear message.
fn resolve_path(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("`path` is empty".to_string());
    }
    let path = PathBuf::from(trimmed);
    let resolved = if path.is_absolute() {
        path
    } else {
        workdir()?.join(path)
    };
    if resolved.is_dir() {
        return Err(format!("`{}` is a directory", resolved.display()));
    }
    Ok(resolved)
}

/// Pull a string argument out of a parsed JSON object.
fn arg_string<'a>(
    parsed: &'a serde_json::Value,
    key: &str,
) -> Result<&'a str, String> {
    parsed
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("missing required argument `{key}` (a string)"))
}

/// `read`: a line-numbered window over a text file.
pub(crate) fn run_read(arguments: &str) -> Result<String, String> {
    let parsed: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|error| format!("invalid arguments: {error}"))?;
    let path = resolve_path(arg_string(&parsed, "path")?)?;
    let offset = match parsed.get("offset") {
        None | Some(serde_json::Value::Null) => 1usize,
        Some(value) => {
            let raw = value
                .as_u64()
                .ok_or("`offset` must be a positive integer line number")?;
            if raw == 0 {
                return Err("`offset` is 1-based: the first line is offset=1".to_string());
            }
            raw as usize
        }
    };
    let limit = match parsed.get("limit") {
        None | Some(serde_json::Value::Null) => DEFAULT_LIMIT,
        Some(value) => {
            let raw = value
                .as_u64()
                .ok_or("`limit` must be a positive integer number of lines")?;
            if raw == 0 {
                return Err("`limit` must be at least 1".to_string());
            }
            (raw as usize).min(DEFAULT_LIMIT)
        }
    };

    let bytes = std::fs::read(&path).map_err(|error| read_error(&path, error))?;
    let head = &bytes[..bytes.len().min(4096)];
    if head.contains(&0u8) {
        return Err(format!(
            "`{}` looks like a binary file; use `bash` with a targeted command instead",
            path.display()
        ));
    }
    let content = String::from_utf8_lossy(&bytes);
    // A BOM is not content; hide it from the model and the match offsets.
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    if content.is_empty() {
        return Ok(format!("{} is empty", path.display()));
    }

    let all_lines: Vec<&str> = content.lines().collect();
    let total = all_lines.len();
    if offset > total {
        return Err(format!(
            "offset {offset} is past the end of `{}` ({total} line{})",
            path.display(),
            if total == 1 { "" } else { "s" }
        ));
    }
    let window: Vec<String> = all_lines[offset - 1..(offset - 1 + limit).min(total)]
        .iter()
        .enumerate()
        .map(|(index, line)| {
            format!(
                "{:>6}\t{}",
                offset + index,
                truncate::clip_line(line)
            )
        })
        .collect();
    let (mut kept, dropped) = truncate::truncate_head(&window, offset);
    if dropped == 0 {
        kept.push(format!(
            "[{total} line{} total, showing {start}-{end}]",
            if total == 1 { "" } else { "s" },
            start = offset,
            end = offset + window.len() - 1,
        ));
    }
    Ok(kept.join("\n"))
}

fn read_error(path: &std::path::Path, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!("file not found: {}", path.display())
    } else {
        format!("cannot read `{}`: {error}", path.display())
    }
}

/// `write`: create or overwrite a file with the given content, creating
/// parent directories as needed.
pub(crate) fn run_write(arguments: &str) -> Result<String, String> {
    let parsed: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|error| format!("invalid arguments: {error}"))?;
    let path = resolve_path(arg_string(&parsed, "path")?)?;
    let content = arg_string(&parsed, "content")?;

    let existed = path.exists();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    std::fs::write(&path, content.as_bytes())
        .map_err(|error| format!("cannot write `{}`: {error}", path.display()))?;
    Ok(format!(
        "{} {} ({} bytes)",
        if existed { "overwrote" } else { "created" },
        path.display(),
        content.len()
    ))
}

/// `edit`: replace exact occurrences of `old_string` with `new_string`.
pub(crate) fn run_edit(arguments: &str) -> Result<String, String> {
    let parsed: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|error| format!("invalid arguments: {error}"))?;
    let path = resolve_path(arg_string(&parsed, "path")?)?;
    let old_string = arg_string(&parsed, "old_string")?;
    let new_string = match parsed.get("new_string") {
        Some(value) if value.is_string() => value.as_str().unwrap_or_default(),
        _ => {
            return Err("missing required argument `new_string` (use \"\" to delete text)"
                .to_string())
        }
    };
    let replace_all = parsed
        .get("replace_all")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if old_string.is_empty() {
        return Err("`old_string` is empty; exact non-empty text is required".to_string());
    }

    let bytes = std::fs::read(&path).map_err(|error| read_error(&path, error))?;
    let raw = String::from_utf8(bytes)
        .map_err(|_| format!("`{}` is not valid UTF-8; edit it with `bash` instead", path.display()))?;

    // Edit on a normalized form (BOM gone, CRLF folded to LF) and restore
    // both afterwards, so untouched lines keep their exact bytes (pi's
    // normalize-and-restore).
    let had_bom = raw.starts_with('\u{feff}');
    let had_crlf = raw.contains("\r\n");
    let content = raw.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let old_string = old_string.replace("\r\n", "\n");
    let new_string = new_string.replace("\r\n", "\n");

    let matches = content.matches(&old_string).count();
    if matches == 0 {
        return Err(format!(
            "`old_string` not found in `{}`; read the file first and match its exact text",
            path.display()
        ));
    }
    if matches > 1 && !replace_all {
        return Err(format!(
            "`old_string` matches {matches} locations in `{}`; add surrounding context to make it unique, or pass replace_all=true",
            path.display()
        ));
    }
    let replaced = if replace_all {
        matches
    } else {
        1
    };
    let edited = if replace_all {
        content.replace(&old_string, &new_string)
    } else {
        content.replacen(&old_string, &new_string, 1)
    };
    let mut final_text = edited;
    if had_crlf {
        final_text = final_text.replace('\n', "\r\n");
    }
    if had_bom {
        final_text.insert(0, '\u{feff}');
    }
    std::fs::write(&path, final_text.as_bytes())
        .map_err(|error| format!("cannot write `{}`: {error}", path.display()))?;
    Ok(format!(
        "replaced {replaced} occurrence{} in {}",
        if replaced == 1 { "" } else { "s" },
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codest-fs-test-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    fn write_setup(name: &str, content: &str) -> (PathBuf, String) {
        let dir = tempdir(name);
        let path = dir.join("sample.txt");
        std::fs::write(&path, content).expect("setup");
        (path.clone(), path.display().to_string())
    }

    #[test]
    fn read_formats_lines_with_numbers() {
        let (path, path_str) = write_setup("read-numbering", "alpha\nbeta\ngamma\n");
        let out = run_read(&format!(r#"{{"path": "{path_str}"}}"#)).expect("read");
        assert!(out.contains("     1\talpha"), "{out}");
        assert!(out.contains("     3\tgamma"), "{out}");
        assert!(out.contains("3 lines total"), "{out}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_windows_with_offset_and_limit() {
        let (path, path_str) = write_setup("read-window", "l1\nl2\nl3\nl4\nl5\n");
        let out = run_read(&format!(
            r#"{{"path": "{path_str}", "offset": 2, "limit": 2}}"#
        ))
        .expect("read");
        assert!(out.contains("     2\tl2"), "{out}");
        assert!(out.contains("     3\tl3"), "{out}");
        assert!(!out.contains("l4"), "{out}");
        assert!(out.contains("showing 2-3"), "{out}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_reports_line_breaks_and_binary_files() {
        let (_, path_str) = write_setup("read-missing", "x\n");
        let missing = std::path::Path::new(&path_str)
            .with_file_name("does-not-exist.txt")
            .display()
            .to_string();
        assert!(run_read(&format!(r#"{{"path": "{missing}"}}"#)).is_err());
        assert!(run_read(&format!(r#"{{"path": "{missing}", "offset": 0}}"#)).is_err());
        assert!(run_read(&format!(r#"{{"path": "{missing}", "offset": 99}}"#)).is_err());

        let dir = tempdir("read-binary");
        let bin = dir.join("blob.bin");
        std::fs::write(&bin, [0x89u8, b'P', 0, b'N', 0, b'G']).expect("setup");
        let out = run_read(&format!(r#"{{"path": "{}"}}"#, bin.display()));
        assert!(
            matches!(&out, Err(message) if message.contains("binary")),
            "{out:?}"
        );
        let _ = std::fs::remove_file(bin);
    }

    #[test]
    fn write_creates_files_and_parents() {
        let dir = tempdir("write-create");
        let path = dir.join("a/b/new.txt").display().to_string();
        let out = run_write(&format!(
            r#"{{"path": "{path}", "content": "hello"}}"#
        ))
        .expect("write");
        assert!(out.contains("created"), "{out}");
        assert!(out.contains("5 bytes"), "{out}");
        let second =
            run_write(&format!(r#"{{"path": "{path}", "content": "hi"}}"#)).expect("rewrite");
        assert!(second.contains("overwrote"), "{second}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn write_rejects_missing_arguments() {
        assert!(run_write(r#"{"path": "/tmp/x"}"#).is_err());
        assert!(run_write(r#"{"content": "x"}"#).is_err());
    }

    #[test]
    fn edit_replaces_one_exact_match() {
        let (path, path_str) = write_setup("edit-basic", "fn main() {\n    todo!()\n}\n");
        let out = run_edit(&format!(
            r#"{{"path": "{path_str}", "old_string": "todo!()", "new_string": "println!(\"hi\");"}}"#
        ))
        .expect("edit");
        assert!(out.contains("replaced 1 occurrence"), "{out}");
        let content = std::fs::read_to_string(&path).expect("read back");
        assert!(content.contains("println!(\"hi\");"), "{content}");
        assert!(content.ends_with("}\n"), "untouched bytes kept: {content}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn edit_requires_uniqueness_unless_replace_all() {
        let (path, path_str) = write_setup("edit-unique", "a b a\n");
        let multi = run_edit(&format!(
            r#"{{"path": "{path_str}", "old_string": "a", "new_string": "c"}}"#
        ));
        assert!(
            matches!(&multi, Err(message) if message.contains("matches 2 locations")),
            "{multi:?}"
        );
        let all = run_edit(&format!(
            r#"{{"path": "{path_str}", "old_string": "a", "new_string": "c", "replace_all": true}}"#
        ))
        .expect("edit all");
        assert!(all.contains("replaced 2 occurrences"), "{all}");
        let content = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(content, "c b c\n");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn edit_preserves_crlf_and_bom() {
        let (path, path_str) = write_setup("edit-crlf", "\u{feff}one\r\ntwo\r\n");
        run_edit(&format!(
            r#"{{"path": "{path_str}", "old_string": "two", "new_string": "TWO"}}"#
        ))
        .expect("edit");
        let bytes = std::fs::read(&path).expect("read back");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.starts_with('\u{feff}'), "BOM kept: {text:?}");
        assert!(text.contains("one\r\nTWO\r\n"), "CRLF kept: {text:?}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn edit_reports_missing_text_and_invalid_utf8() {
        let (path, path_str) = write_setup("edit-missing", "hello\n");
        let miss = run_edit(&format!(
            r#"{{"path": "{path_str}", "old_string": "absent", "new_string": "x"}}"#
        ));
        assert!(
            matches!(&miss, Err(message) if message.contains("not found")),
            "{miss:?}"
        );
        let _ = std::fs::remove_file(path);

        let dir = tempdir("edit-binary");
        let bin = dir.join("blob.bin");
        std::fs::write(&bin, [0xff, 0xfe, 0x00]).expect("setup");
        let bad = run_edit(&format!(
            r#"{{"path": "{}", "old_string": "a", "new_string": "b"}}"#,
            bin.display()
        ));
        assert!(
            matches!(&bad, Err(message) if message.contains("UTF-8")),
            "{bad:?}"
        );
        let _ = std::fs::remove_file(bin);
    }
}
