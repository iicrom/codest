//! Copying selected text to the system clipboard.
//!
//! The chain is the same as reference TUIs use, most reliable route first:
//!
//! 1. **A native utility** — `pbcopy` on macOS, `wl-copy`/`xclip`/`xsel` on
//!    Linux — is spawned with the text on stdin. Skipped over SSH, where the
//!    machine's clipboard is not the one the user is sitting at.
//! 2. **tmux** — `tmux load-buffer -w`, whose `-w` propagates to the outer
//!    terminal's clipboard (dropped under iTerm2, which crashes on it).
//! 3. **OSC 52** — the escape sequence that asks the terminal itself to set
//!    the clipboard. Works over SSH and in any supporting terminal; written
//!    tmux-passthrough-wrapped when inside tmux, since tmux otherwise eats
//!    the sequence.
//!
//! [`copy`] reports which route took the text so the footer can say so
//! honestly — `copied 128 chars` versus `copied 128 chars (osc52)`.

use std::io::Write;
use std::process::Command;
use std::process::Stdio;
use std::sync::OnceLock;

/// Which route the text took to reach the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyPath {
    /// A native utility (pbcopy, wl-copy, xclip, xsel) took the text.
    Native,
    /// `tmux load-buffer -w` took the text, propagating outward.
    Tmux,
    /// Nothing local answered; the OSC 52 sequence went to stdout for the
    /// terminal to interpret.
    Osc52,
}

impl CopyPath {
    /// The toast suffix naming the route when it is not the obvious one.
    pub fn suffix(self) -> &'static str {
        match self {
            CopyPath::Native => "",
            CopyPath::Tmux => " (tmux)",
            CopyPath::Osc52 => " (osc52)",
        }
    }
}

/// Copy `text`, most reliable route first, and report the route taken.
pub fn copy(text: &str) -> CopyPath {
    if native_copy(text) {
        return CopyPath::Native;
    }
    if tmux_copy(text) {
        return CopyPath::Tmux;
    }
    let sequence = osc52_sequence(text, inside_tmux());
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(sequence.as_bytes());
    let _ = stdout.flush();
    CopyPath::Osc52
}

fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|value| !value.is_empty())
}

/// The native route: one clipboard utility, probed once, fed the text on
/// stdin. Absent on platforms none of the utilities support.
fn native_copy(text: &str) -> bool {
    // A remote session's clipboard belongs to the remote machine; copying
    // there would silently lose the text for the user sitting elsewhere.
    if std::env::var_os("SSH_CONNECTION").is_some_and(|value| !value.is_empty()) {
        return false;
    }
    static UTILITY: OnceLock<Option<(&'static str, &'static [&'static str])>> = OnceLock::new();
    let utility = *UTILITY.get_or_init(|| {
        if cfg!(target_os = "macos") {
            return Some(("pbcopy", &[] as &'static [&'static str]));
        }
        if cfg!(target_os = "linux") {
            return [
                ("wl-copy", &[] as &[&str]),
                ("xclip", &["-selection", "clipboard"]),
                ("xsel", &["--clipboard", "--input"]),
            ]
            .into_iter()
            .find(|(program, _)| in_path(program))
            .map(|(program, args)| (program, args as &'static [&'static str]));
        }
        None
    });
    let Some((program, args)) = utility else {
        return false;
    };
    pipe_to(program, args, text)
}

/// The tmux route: the buffer lands in tmux, and `-w` asks it to forward to
/// the outer terminal's clipboard.
fn tmux_copy(text: &str) -> bool {
    if !inside_tmux() {
        return false;
    }
    // iTerm2 crashes on `load-buffer -w`; the plain load still copies into
    // tmux, which is the best that can be done from here.
    let iterm = std::env::var("TERM_PROGRAM").is_ok_and(|value| value == "iTerm.app");
    let args: &[&str] = if iterm {
        &["load-buffer", "-"]
    } else {
        &["load-buffer", "-w", "-"]
    };
    pipe_to("tmux", args, text)
}

/// Spawn `program`, pipe `text` into its stdin, and wait for it to finish.
fn pipe_to(program: &str, args: &[&str], text: &str) -> bool {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let written = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(text.as_bytes()).is_ok())
        .unwrap_or(false);
    // Dropping stdin closes the pipe; a copy utility exits once its input
    // ends, so the wait here is short.
    let finished = child.wait().map(|status| status.success()).unwrap_or(false);
    written && finished
}

fn in_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .any(|dir| dir.join(program).is_file())
        })
        .unwrap_or(false)
}

/// The OSC 52 sequence (`ESC]52;c;<base64>BEL`), tmux-passthrough-wrapped
/// when `wrapped`: tmux swallows the raw sequence unless it arrives inside a
/// DCS pass-through, with every escape doubled.
pub fn osc52_sequence(text: &str, wrapped: bool) -> String {
    let sequence = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    if wrapped {
        format!(
            "\x1bPtmux;{}\x1b\\",
            sequence.replace('\x1b', "\x1b\x1b")
        )
    } else {
        sequence
    }
}

/// Standard base64 with padding — enough for a clipboard payload, without a
/// new dependency.
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("你好".as_bytes()), "5L2g5aW9");
    }

    #[test]
    fn the_osc52_sequence_carries_base64_and_terminates() {
        let sequence = osc52_sequence("foo", false);
        assert_eq!(sequence, "\x1b]52;c;Zm9v\x07");
        // A payload is always a single sequence; a long one still is.
        let long = osc52_sequence(&"x".repeat(10_000), false);
        assert!(long.starts_with("\x1b]52;c;"));
        assert!(long.ends_with('\x07'));
    }

    #[test]
    fn tmux_wrapping_doubles_escapes() {
        let sequence = osc52_sequence("foo", true);
        assert_eq!(sequence, "\x1bPtmux;\x1b\x1b]52;c;Zm9v\x07\x1b\\");
    }

    #[test]
    fn the_copy_path_suffix_names_the_route() {
        assert_eq!(CopyPath::Native.suffix(), "");
        assert_eq!(CopyPath::Tmux.suffix(), " (tmux)");
        assert_eq!(CopyPath::Osc52.suffix(), " (osc52)");
    }
}
