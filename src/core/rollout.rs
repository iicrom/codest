//! Session persistence (rollouts), distilled from codex's `rollout` crate.
//!
//! One session = one append-only JSONL file under
//! `~/.codest/sessions/<workspace-hash>/rollout-<id>.jsonl` (override the root
//! with `CODEST_HOME`, mirroring codex's `CODEX_HOME`). The first line is the
//! [`SessionMeta`]; every following line is a [`RolloutItem`] stamped with a
//! timestamp. Replaying the items rebuilds the exact conversation the worker
//! held, so resume is faithful by construction.
//!
//! Like codex, the recorder lives in the core: the worker appends a line
//! whenever the conversation actually changes (user message, finished
//! assistant message, clear, rename). Writes are best-effort — a persistence
//! failure must never take the TUI down.

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

use crate::core::protocol::Message;

const ROLLOUT_PREFIX: &str = "rollout-";
const ROLLOUT_SUFFIX: &str = ".jsonl";
const SESSIONS_SUBDIR: &str = "sessions";
const FORMAT_VERSION: u32 = 1;

/// Root directory for codest state: `$CODEST_HOME`, else `$HOME/.codest`,
/// else `$USERPROFILE/.codest` on Windows.
pub fn codest_home() -> PathBuf {
    if let Some(home) = std::env::var_os("CODEST_HOME") {
        return PathBuf::from(home);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".codest");
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(home).join(".codest");
    }
    PathBuf::from(".")
}

/// FNV-1a of the workspace path, rendered as 16 hex digits. Deterministic
/// across releases on purpose: it names the session directory.
pub fn workspace_hash(workspace: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in workspace.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// `~/.codest/sessions/<workspace-hash>/` for the given workspace.
pub fn workspace_sessions_dir(workspace: &Path) -> PathBuf {
    codest_home()
        .join(SESSIONS_SUBDIR)
        .join(workspace_hash(workspace))
}

/// UTC timestamp formatted as `YYYY-MM-DDTHH:MM:SSZ` from Unix seconds.
/// `std` has no date formatting, so this is the classic days-from-civil
/// inverse (Howard Hinnant's algorithm), unit-tested against known epochs.
pub fn format_timestamp(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rem = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// `YYYYMMDDTHHMMSS` variant used where compact display matters.
pub fn format_timestamp_compact(seconds: u64) -> String {
    format_timestamp(seconds)
        .replace(['-', ':'], "")
        .replace('Z', "")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Session-level data that does not belong to a specific turn (the rollout
/// file's first line), the codest counterpart of codex's `SessionMeta`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: u32,
    /// Unique id; also encoded in the rollout file name.
    pub id: String,
    pub created_at: String,
    /// Workspace the session was created in.
    pub cwd: String,
    pub system_prompt: String,
    /// Initial name, if the session was created with one. Later renames are
    /// appended as [`RolloutItem::Renamed`] items and win on replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The provider half of the model selection this session started with
    /// (`"provider/model"`; later switches are appended as
    /// [`RolloutItem::ModelSwitched`] items). Absent in files written before
    /// multi-provider support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model half of the initial selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The profile this session started with (`--profile`/`profiles.toml`);
    /// later switches are appended as [`RolloutItem::ProfileSwitched`] items.
    /// Absent in files written before profiles existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// One persisted record, distilled from codex's `RolloutItem` to what a chat
/// agent needs to rebuild its conversations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RolloutItem {
    SessionMeta(SessionMeta),
    UserMessage {
        message: String,
    },
    AssistantMessage {
        message: String,
    },
    /// The reasoning the model streamed before the following assistant message.
    ///
    /// Display-only: it never re-enters the conversation (a model must not see
    /// its own thinking replayed), but a resumed session needs it to show the
    /// thinking cards again. It is written right before the
    /// [`RolloutItem::AssistantMessage`] or [`RolloutItem::AssistantToolCalls`]
    /// of the same round, and replay attaches it to that message.
    AssistantReasoning {
        text: String,
        /// How long the reasoning stream ran, for the card's duration chip;
        /// absent in files written before it was recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// The model asked for tool calls; one item carries the whole round's
    /// calls and replays as one assistant message with `tool_calls`.
    AssistantToolCalls {
        calls: Vec<crate::core::tools::ToolCall>,
    },
    /// The result of one executed tool call, replayed as a `Role::Tool`
    /// message answering `tool_call_id`.
    ToolResult {
        tool_call_id: String,
        content: String,
    },
    Renamed {
        name: String,
    },
    /// The conversation history was dropped (system prompt retained).
    Cleared,
    /// The active model selection changed (`Op::SwitchModel`).
    ModelSwitched {
        provider: String,
        model: String,
    },
    /// The reasoning effort changed (`Op::SetReasoningEffort`, `/effort`);
    /// `None` clears the override. Metadata for attribution, like
    /// [`RolloutItem::ModelSwitched`] — it does not change the replayed
    /// conversation.
    ReasoningEffortSwitched {
        effort: Option<String>,
    },
    /// Token usage of one completed turn, with the selection that served it
    /// (`None` on a fixed backend). Metadata for attribution and context
    /// accounting, like [`RolloutItem::ModelSwitched`] — it does not change
    /// the replayed conversation.
    TurnUsage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        input: u64,
        output: u64,
    },
    /// The named profile became active (`Op::SwitchProfile`). Metadata for
    /// attribution — it tracks which preset produced the session's tail, not
    /// a conversation change.
    ProfileSwitched {
        name: String,
    },
    /// The session's enabled-tool set was replaced (`Op::SetToolsEnabled`,
    /// `/tools`). The last item wins on replay; a session without one had
    /// every declared tool enabled.
    ToolsSwitched {
        enabled: Vec<String>,
    },
    /// Transcript messages were deleted by the `/context` viewer
    /// (`Op::DeleteMessages`). `indices` are the transcript positions at
    /// deletion time, `removed` the messages that stood there (same length,
    /// ascending order) — the payload is the delete's own inverse, so replay
    /// can both apply it and rebuild the checkpoint stack an undo needs.
    ContextDeleted {
        indices: Vec<usize>,
        removed: Vec<Message>,
    },
    /// A transcript message's content was replaced by the `/context` viewer
    /// (`Op::EditMessage`). `previous` is the replaced text — again the
    /// inverse, for replay and undo.
    ContextEdited {
        index: usize,
        content: String,
        previous: String,
    },
    /// The most recent `ContextDeleted`/`ContextEdited` was undone
    /// (`Op::UndoContextEdit`). Replay pops the checkpoint stack the earlier
    /// markers rebuilt and applies the same inverse, so undoing survives a
    /// restart.
    ContextRestored,
}

/// One JSONL line: a timestamp plus the payload's fields flattened in.
#[derive(Debug, Serialize, Deserialize)]
pub struct RolloutLine {
    pub timestamp: String,
    #[serde(flatten)]
    pub item: RolloutItem,
}

/// One reasoning block restored from a rollout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingRecord {
    /// The index of the message the reasoning preceded: the transcript shows
    /// this thinking card before message `before` (and after the one at
    /// `before - 1`).
    pub before: usize,
    pub text: String,
    pub duration_ms: Option<u64>,
}

/// A session restored from disk.
#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub meta: SessionMeta,
    pub path: PathBuf,
    /// Replayed conversation messages, in order.
    pub messages: Vec<Message>,
    /// Replayed reasoning blocks, keyed to the message they preceded. Display
    /// only: [`Self::messages`] is what reaches the model.
    pub thinking: Vec<ThinkingRecord>,
    /// Name after replaying renames; `None` falls back to the first user
    /// message (the default naming rule).
    pub name: Option<String>,
    /// The selection this session last used (`"provider/model"`), replayed
    /// from the meta line, [`RolloutItem::ModelSwitched`] items, and per-turn
    /// usage records — the model a resumed session starts with.
    pub model: Option<String>,
    /// Usage of the last recorded turn; `None` when the rollout predates
    /// usage records or a `/clear` reset them.
    pub last_usage: Option<(u64, u64)>,
    /// Cumulative usage across the session's recorded turns (reset by
    /// [`RolloutItem::Cleared`]).
    pub total_input: u64,
    pub total_output: u64,
    /// The profile last active in this session, replayed from the meta line
    /// and [`RolloutItem::ProfileSwitched`] items.
    pub profile: Option<String>,
    /// The enabled-tool set the session last ran with (`/tools`), replayed
    /// from the last [`RolloutItem::ToolsSwitched`] item; `None` when the
    /// session never customized it (every declared tool enabled).
    pub tools: Option<Vec<String>>,
    /// The `/context` checkpoint stack rebuilt from the `ContextDeleted` /
    /// `ContextEdited` / `ContextRestored` markers, newest last — what lets
    /// an undo work after a restart.
    pub undo: Vec<crate::core::conversation::UndoEntry>,
}

/// Creates rollout files for new sessions.
pub struct RolloutWriter {
    dir: PathBuf,
    cwd: String,
}

impl RolloutWriter {
    pub fn new(dir: PathBuf, cwd: String) -> Self {
        Self { dir, cwd }
    }

    /// Create a new rollout file (meta line included) for a fresh session.
    /// The id comes from the current time; on the unlikely name collision the
    /// id is regenerated. `provider`/`model` record the initial selection,
    /// `profile` the profile the run started with.
    pub fn start_session(
        &self,
        system_prompt: &str,
        provider: Option<&str>,
        model: Option<&str>,
        profile: Option<&str>,
    ) -> io::Result<RolloutFile> {
        std::fs::create_dir_all(&self.dir)?;
        let mut last_error = None;
        for _ in 0..5 {
            let id = format!("{:x}", now_nanos());
            let path = self
                .dir
                .join(format!("{ROLLOUT_PREFIX}{id}{ROLLOUT_SUFFIX}"));
            let meta = SessionMeta {
                version: FORMAT_VERSION,
                id: id.clone(),
                created_at: format_timestamp(now_seconds()),
                cwd: self.cwd.clone(),
                system_prompt: system_prompt.to_string(),
                name: None,
                provider: provider.map(str::to_string),
                model: model.map(str::to_string),
                profile: profile.map(str::to_string),
            };
            match OpenOptions::new().create_new(true).append(true).open(&path) {
                Ok(file) => {
                    let mut rollout = RolloutFile { path, file };
                    rollout.append(&RolloutItem::SessionMeta(meta))?;
                    return Ok(rollout);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| io::Error::other("could not create rollout file")))
    }

    /// Reopen an existing rollout for appending (a resumed session keeps
    /// writing to its original file).
    pub fn open_existing(&self, path: &Path) -> io::Result<RolloutFile> {
        let file = OpenOptions::new().append(true).open(path)?;
        Ok(RolloutFile {
            path: path.to_path_buf(),
            file,
        })
    }
}

/// Append handle for one session's rollout file.
pub struct RolloutFile {
    pub path: PathBuf,
    file: File,
}

impl RolloutFile {
    /// Serialize one item as a JSONL line and flush immediately, so a crash
    /// loses at most the line being written.
    pub fn append(&mut self, item: &RolloutItem) -> io::Result<()> {
        let line = RolloutLine {
            timestamp: format_timestamp(now_seconds()),
            item: item.clone(),
        };
        let mut json = serde_json::to_string(&line).map_err(io::Error::other)?;
        json.push('\n');
        use std::io::Write as _;
        self.file.write_all(json.as_bytes())?;
        self.file.flush()
    }
}

/// Every rollout file of a workspace, oldest first. Files whose names do not
/// parse sort after the parseable ones, by name.
pub fn list_workspace_sessions(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files: Vec<(Option<u128>, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if !(name.starts_with(ROLLOUT_PREFIX) && name.ends_with(ROLLOUT_SUFFIX)) {
            continue;
        }
        let id = name
            .trim_start_matches(ROLLOUT_PREFIX)
            .trim_end_matches(ROLLOUT_SUFFIX)
            .parse::<u128>()
            .ok();
        files.push((id, path));
    }
    files.sort_by(|(left_id, left), (right_id, right)| {
        left_id.cmp(right_id).then_with(|| left.cmp(right))
    });
    Ok(files.into_iter().map(|(_, path)| path).collect())
}

/// Attach a replayed reasoning block to the message that follows it.
///
/// Reasoning and its assistant message are written back to back, so a file
/// truncated between the two lines leaves it dangling — it is dropped rather
/// than shown above an unrelated turn.
fn attach_reasoning(
    thinking: &mut Vec<ThinkingRecord>,
    pending: &mut Option<(String, Option<u64>)>,
    before: usize,
) {
    if let Some((text, duration_ms)) = pending.take() {
        thinking.push(ThinkingRecord {
            before,
            text,
            duration_ms,
        });
    }
}

/// Load and replay one rollout file. Unknown item types are skipped so newer
/// codest versions can extend the format without breaking older readers.
pub fn load_session(path: &Path) -> io::Result<LoadedSession> {
    let content = std::fs::read_to_string(path)?;
    let mut meta = None;
    let mut messages = Vec::new();
    let mut thinking: Vec<ThinkingRecord> = Vec::new();
    // A reasoning item waits for the assistant message it preceded; the two
    // are written back to back, so the gap only spans a truncated file.
    let mut pending_reasoning: Option<(String, Option<u64>)> = None;
    let mut undo = Vec::new();
    let mut name = None;
    let mut model = None;
    let mut profile = None;
    let mut tools = None;
    let mut last_usage = None;
    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // Unparsable lines (corrupt or from a newer format) are skipped.
        let Ok(parsed) = serde_json::from_str::<RolloutLine>(line) else {
            continue;
        };
        match parsed.item {
            RolloutItem::SessionMeta(session_meta) => {
                // The initial selection, until a later switch or usage record
                // supersedes it.
                model = match (
                    session_meta.provider.as_deref(),
                    session_meta.model.as_deref(),
                ) {
                    (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
                    _ => None,
                };
                profile = session_meta.profile.clone();
                meta = Some(session_meta);
            }
            RolloutItem::UserMessage { message } => messages.push(Message::user(message)),
            RolloutItem::AssistantMessage { message } => {
                attach_reasoning(&mut thinking, &mut pending_reasoning, messages.len());
                messages.push(Message::assistant(message));
            }
            RolloutItem::AssistantToolCalls { calls } => {
                attach_reasoning(&mut thinking, &mut pending_reasoning, messages.len());
                messages.push(Message::assistant_tool_calls(calls))
            }
            RolloutItem::AssistantReasoning { text, duration_ms } => {
                pending_reasoning = Some((text, duration_ms));
            }
            RolloutItem::ToolResult {
                tool_call_id,
                content,
            } => messages.push(Message::tool_result(tool_call_id, content)),
            RolloutItem::Renamed { name: renamed } => name = Some(renamed),
            RolloutItem::Cleared => {
                messages.clear();
                // Reasoning is display metadata for the messages above it, so
                // a clear drops it too.
                thinking.clear();
                pending_reasoning = None;
                // The dropped turns' usage no longer describes the context,
                // and the live `/clear` drops the checkpoints with it.
                undo.clear();
                last_usage = None;
                total_input = 0;
                total_output = 0;
            }
            // The active model selection and the reasoning effort are
            // metadata for attribution; they do not change the replayed
            // conversation. The selection still tracks the last used model.
            RolloutItem::ModelSwitched { provider, model: switched } => {
                model = Some(format!("{provider}/{switched}"));
            }
            RolloutItem::ReasoningEffortSwitched { .. } => {}
            RolloutItem::TurnUsage { model: served, input, output } => {
                // A fixed backend reports no model; that cannot clear the
                // tracked selection.
                if served.is_some() {
                    model = served;
                }
                last_usage = Some((input, output));
                total_input = total_input.saturating_add(input);
                total_output = total_output.saturating_add(output);
            }
            RolloutItem::ProfileSwitched { name } => {
                profile = Some(name);
            }
            RolloutItem::ToolsSwitched { enabled } => {
                // The session's own tool configuration, like the model above:
                // the last switch is the one a resumed session restarts with.
                tools = Some(enabled);
            }
            // `/context` edits replay as the mutations they were, each also
            // rebuilding its checkpoint. A corrupt marker (index out of
            // range, mismatched delete payload) is skipped whole: applying
            // half a mutation would desync both the transcript and the stack.
            RolloutItem::ContextDeleted { indices, removed } => {
                if indices.len() != removed.len() {
                    continue;
                }
                let mut pairs: Vec<(usize, Message)> =
                    indices.iter().copied().zip(removed).collect();
                pairs.sort_by_key(|(index, _)| *index);
                if pairs.iter().any(|(index, _)| *index >= messages.len()) {
                    continue;
                }
                for (index, _) in pairs.iter().rev() {
                    messages.remove(*index);
                }
                // A thinking card sits before a message index; every earlier
                // deletion shifts it down with the message it precedes.
                for (index, _) in pairs.iter().rev() {
                    for record in thinking.iter_mut() {
                        if record.before > *index {
                            record.before -= 1;
                        }
                    }
                }
                undo.push(crate::core::conversation::UndoEntry::Deleted { removed: pairs });
            }
            RolloutItem::ContextEdited {
                index,
                content,
                previous,
            } => {
                let Some(message) = messages.get_mut(index) else {
                    continue;
                };
                undo.push(crate::core::conversation::UndoEntry::Edited { index, previous });
                message.content = content;
            }
            RolloutItem::ContextRestored => {
                match undo.pop() {
                    Some(crate::core::conversation::UndoEntry::Deleted { removed }) => {
                        for (index, message) in removed {
                            messages.insert(index, message);
                            for record in thinking.iter_mut() {
                                if record.before >= index {
                                    record.before += 1;
                                }
                            }
                        }
                    }
                    Some(crate::core::conversation::UndoEntry::Edited { index, previous }) => {
                        if let Some(message) = messages.get_mut(index) {
                            message.content = previous;
                        }
                    }
                    None => {}
                }
            }
        }
    }

    let meta = meta.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} has no session meta line", path.display()),
        )
    })?;
    Ok(LoadedSession {
        meta,
        path: path.to_path_buf(),
        messages,
        thinking,
        undo,
        name,
        model,
        last_usage,
        total_input,
        total_output,
        profile,
        tools,
    })
}

/// Load every session of a workspace, oldest first. Files that fail to load
/// are reported on stderr and skipped; the rest still restore.
pub fn load_workspace_sessions(dir: &Path) -> io::Result<Vec<LoadedSession>> {
    let mut loaded = Vec::new();
    for path in list_workspace_sessions(dir)? {
        match load_session(&path) {
            Ok(session) => loaded.push(session),
            Err(error) => eprintln!("codest: skipping rollout: {error}"),
        }
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::conversation::Conversation;
    use crate::core::protocol::Role;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codest-rollout-test-{name}-{}-{}",
            std::process::id(),
            now_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    #[test]
    fn workspace_hash_is_stable_hex() {
        let first = workspace_hash(Path::new("/tmp/demo"));
        let second = workspace_hash(Path::new("/tmp/demo"));
        assert_eq!(first, second);
        assert_eq!(first.len(), 16);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(workspace_hash(Path::new("/tmp/other")), first);
    }

    #[test]
    fn timestamps_match_known_epochs() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00Z");
        // 2001-09-09T01:46:40Z is the classic 1e9 epoch.
        assert_eq!(format_timestamp(1_000_000_000), "2001-09-09T01:46:40Z");
        // Leap day.
        assert_eq!(format_timestamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_timestamp_compact(1_000_000_000), "20010909T014640");
    }

    #[test]
    fn start_session_writes_meta_and_reloads() {
        let dir = tempdir("start");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer.start_session("be brief", None, None, None).expect("start");

        assert!(rollout.path.starts_with(&dir));
        rollout
            .append(&RolloutItem::UserMessage {
                message: "hello".to_string(),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::Renamed {
                name: "my chat".to_string(),
            })
            .expect("append");

        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.meta.system_prompt, "be brief");
        assert_eq!(loaded.meta.cwd, "/tmp/demo");
        assert_eq!(loaded.meta.version, FORMAT_VERSION);
        assert_eq!(loaded.name.as_deref(), Some("my chat"));
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].role, Role::User);
        assert_eq!(loaded.messages[0].content, "hello");
    }

    #[test]
    fn replay_applies_clears_and_renames_in_order() {
        let dir = tempdir("replay");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer.start_session("be brief", None, None, None).expect("start");

        rollout
            .append(&RolloutItem::UserMessage {
                message: "first".to_string(),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::AssistantMessage {
                message: "hi".to_string(),
            })
            .expect("append");
        rollout.append(&RolloutItem::Cleared).expect("append");
        rollout
            .append(&RolloutItem::UserMessage {
                message: "second".to_string(),
            })
            .expect("append");

        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content, "second");
        assert_eq!(loaded.name, None);
    }

    /// The `/context` markers replay as the mutations they recorded and
    /// rebuild the checkpoint stack: the loaded session carries both the
    /// edited transcript and an undo that still works.
    #[test]
    fn context_edits_replay_and_rebuild_the_checkpoint_stack() {
        use crate::core::conversation::UndoEntry;

        let dir = tempdir("context-edit");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer.start_session("be brief", None, None, None).expect("start");
        for text in ["one", "two", "three"] {
            rollout
                .append(&RolloutItem::UserMessage {
                    message: text.to_string(),
                })
                .expect("append");
        }
        rollout
            .append(&RolloutItem::ContextDeleted {
                indices: vec![0, 2],
                removed: vec![
                    Message::user("one"),
                    Message::user("three"),
                ],
            })
            .expect("append");
        rollout
            .append(&RolloutItem::ContextEdited {
                index: 0,
                content: "two (edited)".to_string(),
                previous: "two".to_string(),
            })
            .expect("append");

        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(
            loaded
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            vec!["two (edited)"]
        );
        assert_eq!(loaded.undo.len(), 2);

        // The stack pops in LIFO order, exactly like the live conversation's.
        let mut conversation = Conversation::replayed(
            loaded.meta.system_prompt.clone(),
            loaded.messages.clone(),
            loaded.undo.clone(),
        );
        assert!(conversation.undo_last_edit());
        assert_eq!(conversation.messages()[0].content, "two");
        assert!(conversation.undo_last_edit());
        assert_eq!(
            conversation
                .messages()
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );
        assert_eq!(
            loaded.undo.first(),
            Some(&UndoEntry::Deleted {
                removed: vec![(0, Message::user("one")), (2, Message::user("three"))]
            })
        );
    }

    /// `ContextRestored` replays the same inverse the live undo applied, and
    /// a `Cleared` drops the checkpoints being rebuilt just like the live
    /// `/clear` drops them.
    #[test]
    fn context_restored_replays_and_clear_drops_the_stack() {
        let dir = tempdir("context-undo");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer.start_session("be brief", None, None, None).expect("start");
        rollout
            .append(&RolloutItem::UserMessage {
                message: "hello".to_string(),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::ContextDeleted {
                indices: vec![0],
                removed: vec![Message::user("hello")],
            })
            .expect("append");
        rollout.append(&RolloutItem::ContextRestored).expect("append");

        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content, "hello");
        assert!(loaded.undo.is_empty(), "the restore consumed its checkpoint");

        rollout.append(&RolloutItem::Cleared).expect("append");
        rollout
            .append(&RolloutItem::ContextEdited {
                index: 9,
                content: "orphan".to_string(),
                previous: "gone".to_string(),
            })
            .expect("append");
        let loaded = load_session(&rollout.path).expect("load");
        assert!(loaded.messages.is_empty());
        assert!(loaded.undo.is_empty(), "a clear drops the rebuilt stack");
    }

    /// A corrupt delete marker (index past the transcript) is skipped whole.
    #[test]
    fn corrupt_context_delete_markers_are_skipped() {
        let dir = tempdir("context-corrupt");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer.start_session("be brief", None, None, None).expect("start");
        rollout
            .append(&RolloutItem::UserMessage {
                message: "hello".to_string(),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::ContextDeleted {
                indices: vec![5],
                removed: vec![Message::user("ghost")],
            })
            .expect("append");
        rollout
            .append(&RolloutItem::ContextDeleted {
                indices: vec![0],
                removed: vec![],
            })
            .expect("append");

        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.messages.len(), 1);
        assert!(loaded.undo.is_empty());
    }

    /// Reasoning is display metadata: replay hands it back attached to the
    /// message it preceded, without ever becoming a conversation message of
    /// its own.
    #[test]
    fn reasoning_replays_ahead_of_the_message_it_preceded() {
        let dir = tempdir("thinking");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer.start_session("be brief", None, None, None).expect("start");

        rollout
            .append(&RolloutItem::UserMessage {
                message: "q".to_string(),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::AssistantReasoning {
                text: "thinking about it".to_string(),
                duration_ms: Some(1_500),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::AssistantMessage {
                message: "a".to_string(),
            })
            .expect("append");
        // A pure tool-call round carries no text but still had reasoning; the
        // block belongs to the calls message that follows it.
        rollout
            .append(&RolloutItem::AssistantReasoning {
                text: "should call a tool".to_string(),
                duration_ms: None,
            })
            .expect("append");
        rollout
            .append(&RolloutItem::AssistantToolCalls {
                calls: vec![crate::core::tools::ToolCall {
                    id: "call_1".to_string(),
                    name: "ping".to_string(),
                    arguments: "{}".to_string(),
                }],
            })
            .expect("append");
        // Reasoning with no message after it (a file truncated mid-write) is
        // dropped rather than shown above an unrelated turn.
        rollout
            .append(&RolloutItem::AssistantReasoning {
                text: "orphan".to_string(),
                duration_ms: None,
            })
            .expect("append");

        let loaded = load_session(&rollout.path).expect("load");
        // The conversation is unchanged: user, answer, tool calls.
        assert_eq!(
            loaded.messages.iter().map(|m| m.role).collect::<Vec<_>>(),
            vec![Role::User, Role::Assistant, Role::Assistant]
        );
        assert_eq!(
            loaded.thinking,
            vec![
                ThinkingRecord {
                    before: 1,
                    text: "thinking about it".to_string(),
                    duration_ms: Some(1_500),
                },
                ThinkingRecord {
                    before: 2,
                    text: "should call a tool".to_string(),
                    duration_ms: None,
                },
            ]
        );
    }

    /// A reasoning block is anchored to a message index, so `/context`
    /// deletions and their undo shift it with the message it sits above.
    #[test]
    fn context_deletions_shift_thinking_with_their_message() {
        let dir = tempdir("thinking-shift");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer.start_session("be brief", None, None, None).expect("start");

        for (text, reasoning) in [("one", "about one"), ("two", "about two")] {
            rollout
                .append(&RolloutItem::UserMessage {
                    message: text.to_string(),
                })
                .expect("append");
            rollout
                .append(&RolloutItem::AssistantReasoning {
                    text: reasoning.to_string(),
                    duration_ms: None,
                })
                .expect("append");
            rollout
                .append(&RolloutItem::AssistantMessage {
                    message: format!("re {text}"),
                })
                .expect("append");
        }
        // Drop the first user message: every later message (and the thinking
        // above it) moves down one slot.
        rollout
            .append(&RolloutItem::ContextDeleted {
                indices: vec![0],
                removed: vec![Message::user("one")],
            })
            .expect("append");

        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(
            loaded.thinking,
            vec![
                // "about one" preceded the deleted answer, which is gone; it
                // now sits above the following message.
                ThinkingRecord {
                    before: 0,
                    text: "about one".to_string(),
                    duration_ms: None,
                },
                ThinkingRecord {
                    before: 2,
                    text: "about two".to_string(),
                    duration_ms: None,
                },
            ]
        );
    }

    #[test]
    fn listing_sorts_oldest_first() {
        let dir = tempdir("list");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let first = writer.start_session("one", None, None, None).expect("start");
        // Ensure distinct ids even with a coarse clock.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = writer.start_session("two", None, None, None).expect("start");

        let listed = list_workspace_sessions(&dir).expect("list");
        assert_eq!(listed, vec![first.path, second.path]);
    }

    #[test]
    fn load_skips_unknown_item_types() {
        let dir = tempdir("unknown");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer.start_session("be brief", None, None, None).expect("start");
        rollout
            .append(&RolloutItem::UserMessage {
                message: "hello".to_string(),
            })
            .expect("append");
        // Hand-write a line with an item type a newer codest might add.
        {
            use std::io::Write as _;
            let mut file = &rollout.file;
            writeln!(
                file,
                r#"{{"timestamp":"x","type":"future_thing","data":1}}"#
            )
            .expect("write");
        }

        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.messages.len(), 1);
    }

    #[test]
    fn reasoning_effort_switched_round_trips_and_replay_ignores_it() {
        let dir = tempdir("effort");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer.start_session("be brief", None, None, None).expect("start");
        rollout
            .append(&RolloutItem::ReasoningEffortSwitched {
                effort: Some("high".to_string()),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::UserMessage {
                message: "hello".to_string(),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::ReasoningEffortSwitched { effort: None })
            .expect("append");

        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content, "hello");
    }

    #[test]
    fn profile_round_trips_and_replays_the_last_active_one() {
        let dir = tempdir("profile");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer
            .start_session("be brief", None, None, Some("code"))
            .expect("start");
        // The meta line seeds the startup profile…
        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.profile.as_deref(), Some("code"));

        // …and switches supersede it in file order.
        rollout
            .append(&RolloutItem::ProfileSwitched {
                name: "fast".to_string(),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::UserMessage {
                message: "hello".to_string(),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::ProfileSwitched {
                name: "code".to_string(),
            })
            .expect("append");
        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.profile.as_deref(), Some("code"));
        assert_eq!(loaded.messages.len(), 1, "metadata, not conversation");
    }

    #[test]
    fn tools_switched_round_trips_and_replays_the_last_set() {
        let dir = tempdir("tools");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer.start_session("be brief", None, None, None).expect("start");
        // No switch yet: the session never customized its tools.
        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.tools, None);

        // The last switch in file order is the one a resume restores.
        rollout
            .append(&RolloutItem::ToolsSwitched {
                enabled: vec!["read".to_string(), "edit".to_string()],
            })
            .expect("append");
        rollout
            .append(&RolloutItem::UserMessage {
                message: "hello".to_string(),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::ToolsSwitched {
                enabled: vec!["read".to_string()],
            })
            .expect("append");
        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(
            loaded.tools,
            Some(vec!["read".to_string()]),
            "last switch wins"
        );
        assert_eq!(loaded.messages.len(), 1, "metadata, not conversation");
        let raw = std::fs::read_to_string(&rollout.path).expect("read");
        assert!(raw.contains(r#""type":"tools_switched""#), "{raw}");
    }

    #[test]
    fn replay_tracks_the_last_used_model() {
        let dir = tempdir("model");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer
            .start_session("be brief", Some("openai"), Some("gpt-4o-mini"), None)
            .expect("start");
        // The meta line seeds the initial selection…
        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.model.as_deref(), Some("openai/gpt-4o-mini"));

        // …switches and served turns update it in file order; a usage record
        // without a model (fixed backend) leaves it alone.
        rollout
            .append(&RolloutItem::ModelSwitched {
                provider: "openai".to_string(),
                model: "gpt-4o".to_string(),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::TurnUsage {
                model: Some("openai/gpt-4o".to_string()),
                input: 10,
                output: 2,
            })
            .expect("append");
        rollout
            .append(&RolloutItem::TurnUsage {
                model: None,
                input: 10,
                output: 2,
            })
            .expect("append");
        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.model.as_deref(), Some("openai/gpt-4o"));

        rollout
            .append(&RolloutItem::ModelSwitched {
                provider: "glm".to_string(),
                model: "glm-4.7".to_string(),
            })
            .expect("append");
        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.model.as_deref(), Some("glm/glm-4.7"));
    }

    #[test]
    fn turn_usage_round_trips_and_clear_resets_it() {
        let dir = tempdir("usage");
        let writer = RolloutWriter::new(dir.clone(), "/tmp/demo".to_string());
        let mut rollout = writer.start_session("be brief", None, None, None).expect("start");
        rollout
            .append(&RolloutItem::UserMessage {
                message: "first".to_string(),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::TurnUsage {
                model: Some("openai/gpt-4o-mini".to_string()),
                input: 1_000,
                output: 200,
            })
            .expect("append");
        rollout
            .append(&RolloutItem::AssistantMessage {
                message: "hi".to_string(),
            })
            .expect("append");
        rollout
            .append(&RolloutItem::TurnUsage {
                model: None,
                input: 1_300,
                output: 300,
            })
            .expect("append");

        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.last_usage, Some((1_300, 300)));
        assert_eq!(loaded.total_input, 2_300);
        assert_eq!(loaded.total_output, 500);
        // Usage records are metadata: the conversation still replays to two
        // messages, and the raw line carries the tagged variant name.
        assert_eq!(loaded.messages.len(), 2);
        let raw = std::fs::read_to_string(&rollout.path).expect("read");
        assert!(raw.contains(r#""type":"turn_usage""#), "{raw}");
        assert!(raw.contains(r#""model":"openai/gpt-4o-mini""#), "{raw}");

        // A clear drops the conversation and the usage accounting with it.
        rollout.append(&RolloutItem::Cleared).expect("append");
        rollout
            .append(&RolloutItem::UserMessage {
                message: "second".to_string(),
            })
            .expect("append");
        let loaded = load_session(&rollout.path).expect("load");
        assert_eq!(loaded.last_usage, None);
        assert_eq!(loaded.total_input, 0);
        assert_eq!(loaded.total_output, 0);
        assert_eq!(loaded.messages.len(), 1);
    }
}
