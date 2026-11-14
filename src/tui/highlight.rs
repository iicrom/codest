//! Syntax highlighting for fenced code blocks.
//!
//! The renderer only ever sees [`TokenKind`]s, never a highlighting engine's
//! types. [`Highlighter`] is the one seam that knows about syntect: swapping in
//! another engine means writing another `Highlighter` and leaving
//! [`markdown`](crate::tui::markdown) alone.
//!
//! Highlighting is per fenced block, not per line, because a multi-line string
//! or comment only parses correctly when the parser state carries across lines.
//!
//! [`block`] caches results: the transcript re-renders every history cell on
//! every frame, so a settled block would otherwise be re-tokenized on each
//! tick. The cache stores [`TokenKind`]s rather than styles, so a palette
//! change does not invalidate it.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::OnceLock;

use ratatui::style::Style;
use syntect::parsing::ParseState;
use syntect::parsing::Scope;
use syntect::parsing::ScopeStack;
use syntect::parsing::SyntaxSet;

use crate::tui::theme;

/// What a run of code text is, in palette-independent terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Plain,
    Keyword,
    Type,
    Function,
    String,
    Number,
    Comment,
    Constant,
    Attribute,
}

/// A styled run of one source line.
#[derive(Clone, Copy, Debug)]
pub struct Token<'a> {
    pub text: &'a str,
    pub kind: TokenKind,
}

/// A byte range of a source line and the kind of text it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
}

/// One entry per source line; an empty entry means "render this line plain".
pub type Block = Vec<Vec<Segment>>;

/// Skip highlighting for blocks larger than this. A huge paste stays readable
/// as plain text instead of stalling the frame that renders it.
const MAX_BLOCK_BYTES: usize = 128 * 1024;
/// syntect notes that very long lines are slow: their regexes scan the whole
/// line for every rule. Such a block is rendered plain.
const MAX_LINE_BYTES: usize = 8 * 1024;
/// Bounds the cache. Streaming inserts a new variant of the live block on
/// every delta, so the entry cap is what keeps memory flat while a message
/// arrives; the byte cap catches a few very large blocks. Sized for toggle
/// cycles: one Ctrl+O pass inserts an expanded head + tail entry per card,
/// the fold-back another folded entry per card — a smaller cache evicted the
/// expanded blocks between cycles and every second expand re-tokenized them.
const MAX_CACHE_ENTRIES: usize = 256;
const MAX_CACHE_BYTES: usize = 24 * 1024 * 1024;

/// Tokenize one fenced block of `lines`, returning one segment list per line.
///
/// Returns plain (empty) entries for unknown languages, oversized blocks, and
/// any line that cannot be tokenized, so the caller has exactly one fallback
/// path: a missing entry renders the raw text.
pub fn block(lang: &str, lines: &[&str]) -> Arc<Block> {
    let bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
    if lines.is_empty() {
        return Arc::new(Vec::new());
    }
    if bytes > MAX_BLOCK_BYTES || lines.iter().any(|line| line.len() > MAX_LINE_BYTES) {
        return Arc::new(plain_block(lines));
    }

    let key = Key::new(lang, lines, bytes);
    if let Some(hit) = CACHE.with(|cache| cache.borrow_mut().get(&key)) {
        return hit;
    }

    let tokens = match Highlighter::new(lang) {
        Some(mut highlighter) => {
            let mut out = Vec::with_capacity(lines.len());
            for line in lines {
                let mut offset = 0usize;
                let mut segments = Vec::new();
                for token in highlighter.line(line) {
                    let start = offset;
                    offset += token.text.len();
                    segments.push(Segment {
                        start,
                        end: offset,
                        kind: token.kind,
                    });
                }
                out.push(segments);
            }
            out
        }
        // Unknown language: cache the plain result so the syntax lookup (a
        // linear scan over every definition) does not repeat every frame.
        None => plain_block(lines),
    };

    let tokens = Arc::new(tokens);
    // Cache every block, however small: a five-line fence is ~200 bytes but
    // still costs milliseconds to tokenize, and the transcript renders every
    // history cell on every frame.
    CACHE.with(|cache| cache.borrow_mut().insert(key, tokens.clone(), bytes));
    tokens
}

/// The palette mapping. The only place a [`TokenKind`] meets a color.
pub fn style(kind: TokenKind) -> Style {
    match kind {
        TokenKind::Plain => Style::default(),
        TokenKind::Keyword => Style::default().fg(theme::CODE_KEYWORD),
        TokenKind::Type => Style::default().fg(theme::CODE_TYPE),
        TokenKind::Function => Style::default().fg(theme::CODE_FUNCTION),
        TokenKind::String => Style::default().fg(theme::CODE_STRING),
        TokenKind::Number => Style::default().fg(theme::CODE_NUMBER),
        TokenKind::Comment => Style::default().fg(theme::CODE_COMMENT),
        TokenKind::Constant => Style::default().fg(theme::CODE_CONSTANT),
        TokenKind::Attribute => Style::default().fg(theme::CODE_ATTRIBUTE),
    }
}

/// Line-oriented tokenizer. Owns the parser state between lines, which is what
/// makes multi-line strings and comments come out right.
pub struct Highlighter {
    parse: ParseState,
    stack: ScopeStack,
}

impl Highlighter {
    /// `None` when `lang` names no known syntax, or names one that is not
    /// worth its cost.
    pub fn new(lang: &str) -> Option<Self> {
        let lang = lang.trim();
        if lang.is_empty() {
            return None;
        }
        // The Markdown sublime grammar embeds dozens of sub-grammars and
        // stays pathologically slow even on oniguruma — ~150 ms for a
        // 200-line read window, which alone stalls a Ctrl+O frame. Prose
        // gains almost nothing from token colors, so documents render plain.
        if matches!(lang.to_ascii_lowercase().as_str(), "md" | "markdown" | "txt" | "text") {
            return None;
        }
        let syntaxes = syntax_set();
        let syntax = syntaxes
            .find_syntax_by_token(lang)
            .or_else(|| syntaxes.find_syntax_by_extension(lang))?;
        if syntax.hidden {
            return None;
        }
        Some(Self {
            parse: ParseState::new(syntax),
            stack: ScopeStack::new(),
        })
    }

    /// Tokenize one line without its terminator.
    ///
    /// The `*_newlines` syntax set expects each line to end with a newline so
    /// rules anchored at `$` match; the padded copy is used for parsing only
    /// and every returned slice points into `line`.
    pub fn line<'a>(&mut self, line: &'a str) -> Vec<Token<'a>> {
        let syntaxes = syntax_set();
        let mut padded = String::with_capacity(line.len() + 1);
        padded.push_str(line);
        padded.push('\n');
        let Ok(ops) = self.parse.parse_line(&padded, syntaxes) else {
            // A parse failure leaves the state intact enough for the next line;
            // this line just renders unstyled.
            return vec![Token {
                text: line,
                kind: TokenKind::Plain,
            }];
        };

        let limit = line.len();
        let mut tokens: Vec<Token<'a>> = Vec::new();
        let mut pos = 0usize;
        for (end, op) in ops {
            // Offsets come from regex matches on `padded`, so they are char
            // boundaries inside `line`; anything else is clamped rather than
            // sliced, since slicing mid-codepoint would panic.
            let end = if end > limit || !line.is_char_boundary(end) {
                limit
            } else {
                end
            };
            if end > pos {
                push_token(&mut tokens, &line[pos..end], kind_for(&self.stack));
            }
            if self.stack.apply(&op).is_err() {
                self.stack = ScopeStack::new();
            }
            pos = end;
        }
        if pos < limit {
            push_token(&mut tokens, &line[pos..], kind_for(&self.stack));
        }
        tokens
    }
}

/// Append `text` as its own run. Adjacent runs of the same kind are left
/// separate here: the wrap step re-merges equal styles when it builds spans.
fn push_token<'a>(tokens: &mut Vec<Token<'a>>, text: &'a str, kind: TokenKind) {
    if !text.is_empty() {
        tokens.push(Token { text, kind });
    }
}

/// The kind of the innermost scope that matches a known prefix.
fn kind_for(stack: &ScopeStack) -> TokenKind {
    for scope in stack.as_slice().iter().rev() {
        for (prefix, kind) in scope_table() {
            if prefix.is_prefix_of(*scope) {
                return *kind;
            }
        }
    }
    TokenKind::Plain
}

/// Syntax definitions, loaded once. `two-face` supplies bat's dump, which adds
/// the languages syntect's own set lacks (TypeScript, TOML, Dockerfile, …).
fn syntax_set() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// Scope prefixes to [`TokenKind`]s, most specific first. The first match on
/// the innermost scope wins, so `keyword.operator` beats a later `keyword`.
///
/// `storage.type` maps to [`TokenKind::Keyword`] rather than `Type`: grammars
/// use it for declaration keywords (`let`, `struct`, `var`, `int`), while the
/// names being declared carry `entity.name.*` and land in `Type`.
fn scope_table() -> &'static [(Scope, TokenKind)] {
    static TABLE: OnceLock<Vec<(Scope, TokenKind)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        const RULES: &[(&str, TokenKind)] = &[
            ("comment", TokenKind::Comment),
            ("string", TokenKind::String),
            ("constant.numeric", TokenKind::Number),
            ("constant.language", TokenKind::Constant),
            ("constant", TokenKind::Constant),
            // Operators are punctuation, not vocabulary: coloring them makes
            // the code shout.
            ("keyword.operator", TokenKind::Plain),
            ("storage.type.function", TokenKind::Keyword),
            ("storage.type", TokenKind::Keyword),
            ("storage", TokenKind::Keyword),
            ("keyword", TokenKind::Keyword),
            ("entity.name.function", TokenKind::Function),
            ("support.function", TokenKind::Function),
            ("variable.function", TokenKind::Function),
            ("meta.function-call", TokenKind::Function),
            ("support.macro", TokenKind::Function),
            ("entity.name.type", TokenKind::Type),
            ("entity.name.class", TokenKind::Type),
            ("entity.name.struct", TokenKind::Type),
            ("entity.name.enum", TokenKind::Type),
            ("entity.name.namespace", TokenKind::Type),
            ("support.type", TokenKind::Type),
            ("support.class", TokenKind::Type),
            ("entity.other.attribute-name", TokenKind::Attribute),
            ("entity.name.tag", TokenKind::Attribute),
        ];
        RULES
            .iter()
            .filter_map(|(scope, kind)| Some((Scope::new(scope).ok()?, *kind)))
            .collect()
    })
}

fn plain_block(lines: &[&str]) -> Block {
    vec![Vec::new(); lines.len()]
}

#[derive(Clone, PartialEq, Eq)]
struct Key {
    lang: String,
    len: usize,
    hash: u64,
}

impl Key {
    fn new(lang: &str, lines: &[&str], len: usize) -> Self {
        let mut hasher = DefaultHasher::new();
        for line in lines {
            line.hash(&mut hasher);
            0u8.hash(&mut hasher); // separator, so ["ab","c"] != ["a","bc"]
        }
        Self {
            lang: lang.to_ascii_lowercase(),
            len,
            hash: hasher.finish(),
        }
    }
}

struct Entry {
    key: Key,
    tokens: Arc<Block>,
    bytes: usize,
}

#[derive(Default)]
struct Cache {
    entries: Vec<Entry>,
    bytes: usize,
}

impl Cache {
    fn get(&mut self, key: &Key) -> Option<Arc<Block>> {
        let index = self.entries.iter().position(|entry| &entry.key == key)?;
        // Move to the back on use: an entry that keeps being read is the one
        // worth keeping, which is what eviction below relies on.
        let entry = self.entries.remove(index);
        let tokens = entry.tokens.clone();
        self.entries.push(entry);
        Some(tokens)
    }

    fn insert(&mut self, key: Key, tokens: Arc<Block>, bytes: usize) {
        self.bytes += bytes;
        self.entries.push(Entry { key, tokens, bytes });
        // Least recently used first. Streaming inserts a new variant of the
        // live block on every delta, so a plain FIFO would evict the settled
        // history blocks between frames and re-tokenize them every frame.
        while self.entries.len() > MAX_CACHE_ENTRIES || self.bytes > MAX_CACHE_BYTES {
            let entry = self.entries.remove(0);
            self.bytes -= entry.bytes;
        }
    }
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tokenize `line` as Rust, joining adjacent runs of the same kind back
    /// together so assertions can talk about whole words.
    fn kinds(line: &str) -> Vec<(String, TokenKind)> {
        let mut highlighter = Highlighter::new("rust").expect("rust syntax");
        let mut out: Vec<(String, TokenKind)> = Vec::new();
        for token in highlighter.line(line) {
            match out.last_mut() {
                Some((text, kind)) if *kind == token.kind => text.push_str(token.text),
                _ => out.push((token.text.to_string(), token.kind)),
            }
        }
        out
    }

    /// The kind covering the whole of `text`, if it appears as its own run.
    fn kind_of(tokens: &[(String, TokenKind)], text: &str) -> Option<TokenKind> {
        tokens
            .iter()
            .find(|(token, _)| token == text)
            .map(|(_, kind)| *kind)
    }

    #[test]
    fn rust_line_splits_into_styled_runs() {
        let tokens = kinds("let x = 42; // note");
        assert!(tokens.len() > 1, "expected several runs: {tokens:?}");
        assert_eq!(kind_of(&tokens, "42"), Some(TokenKind::Number));
        assert_eq!(kind_of(&tokens, "// note"), Some(TokenKind::Comment));
        assert_eq!(kind_of(&tokens, "let"), Some(TokenKind::Keyword));
    }

    #[test]
    fn strings_and_keywords_are_not_plain() {
        let tokens = kinds("fn main() { let s = \"hi\"; }");
        assert_eq!(kind_of(&tokens, "\"hi\""), Some(TokenKind::String));
        assert_eq!(kind_of(&tokens, "fn"), Some(TokenKind::Keyword));
        assert_eq!(kind_of(&tokens, "main"), Some(TokenKind::Function));
    }

    #[test]
    fn declaration_keywords_beat_the_type_scope() {
        // Rust scopes `let` and type names both as `storage.type`; the keyword
        // reading is the useful one.
        let tokens = kinds("let x: i32 = 0;");
        assert_eq!(kind_of(&tokens, "let"), Some(TokenKind::Keyword));
        assert_eq!(kind_of(&tokens, "i32"), Some(TokenKind::Keyword));
    }

    #[test]
    fn operators_are_left_uncolored() {
        // `a + b` has no vocabulary words, and the operator must not pick up
        // the keyword color just because its scope starts with `keyword.`.
        let tokens = kinds("a + b");
        assert!(
            tokens.iter().all(|(_, kind)| *kind == TokenKind::Plain),
            "operator picked up a color: {tokens:?}"
        );
    }

    #[test]
    fn parser_state_carries_across_lines() {
        let mut highlighter = Highlighter::new("rust").expect("rust syntax");
        highlighter.line("let s = \"\"\"");
        let tokens: Vec<_> = highlighter.line("still inside the string");
        assert_eq!(
            tokens.first().map(|token| token.kind),
            Some(TokenKind::String)
        );
    }

    #[test]
    fn unknown_language_yields_a_plain_block() {
        let lines = ["x = 1", "y = 2"];
        let tokens = block("nosuchlang", &lines);
        assert_eq!(*tokens, vec![Vec::new(), Vec::new()]);

        assert!(Highlighter::new("nosuchlang").is_none());
        assert!(Highlighter::new("").is_none());
    }

    #[test]
    fn block_reports_byte_ranges_that_cover_each_line() {
        let lines = ["fn main() {", "    let x = 1;", "}"];
        let tokens = block("rust", &lines);
        assert_eq!(tokens.len(), lines.len());
        for (index, line) in lines.iter().enumerate() {
            let segments = &tokens[index];
            assert!(!segments.is_empty(), "line {index} was not tokenized");
            assert_eq!(segments.first().unwrap().start, 0);
            assert_eq!(segments.last().unwrap().end, line.len());
        }
    }

    #[test]
    fn repeated_blocks_hit_the_cache() {
        // Large enough to be cached.
        let line = "fn main() { let value = compute(alpha, beta); } // padding padding";
        let lines = [line; 12];
        let first = block("rust", &lines);
        let second = block("rust", &lines);
        assert!(Arc::ptr_eq(&first, &second), "expected a cache hit");

        // A changed body is a different key.
        let mut changed = lines;
        changed[0] = "fn other() {}";
        assert!(!Arc::ptr_eq(&first, &block("rust", &changed)));
    }

    #[test]
    fn a_settled_block_survives_streaming_churn() {
        // Streaming inserts a fresh variant of the live block on every delta.
        // Both blocks are read each frame, so both must stay cached; evicting
        // in plain insertion order would drop the settled one repeatedly and
        // re-tokenize it on every frame.
        let settled: Vec<String> = (0..6)
            .map(|i| format!("fn f{i}() {{ let value = compute(alpha, beta); }}"))
            .collect();
        let settled_refs: Vec<&str> = settled.iter().map(String::as_str).collect();
        let settled_first = block("rust", &settled_refs);

        let mut stable_hits = 0;
        let rounds = MAX_CACHE_ENTRIES * 2;
        for extra in 0..rounds {
            // A new live block each frame, then a read of the settled one.
            let live: Vec<String> = (0..6)
                .map(|i| format!("fn live{extra}_{i}() {{ let v = compute(g, d); }}"))
                .collect();
            let live_refs: Vec<&str> = live.iter().map(String::as_str).collect();
            let _ = block("rust", &live_refs);
            if Arc::ptr_eq(&settled_first, &block("rust", &settled_refs)) {
                stable_hits += 1;
            }
        }
        assert_eq!(
            stable_hits, rounds,
            "the settled block was evicted by streaming churn"
        );
    }

    #[test]
    fn oversized_blocks_stay_plain() {
        let huge = "x".repeat(MAX_BLOCK_BYTES + 1);
        let tokens = block("rust", &[huge.as_str()]);
        assert_eq!(*tokens, vec![Vec::new()]);
    }

    #[test]
    fn every_kind_has_a_distinct_palette_entry() {
        // Plain is the unstyled fallback; every other kind must paint something.
        for kind in [
            TokenKind::Keyword,
            TokenKind::Type,
            TokenKind::Function,
            TokenKind::String,
            TokenKind::Number,
            TokenKind::Comment,
            TokenKind::Constant,
            TokenKind::Attribute,
        ] {
            assert!(style(kind).fg.is_some(), "{kind:?} has no color");
        }
        assert!(style(TokenKind::Plain).fg.is_none());
    }
}
