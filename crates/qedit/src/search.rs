//! Searching the workspace's files for text, with ripgrep's engine.
//!
//! A search runs on background threads over the same files as the file
//! picker, skipping `.git`, ignored files, binary files, and files that
//! aren't UTF-8 (which qedit can't open). Each file's matches come back with
//! the lines around them, for showing excerpts. Open files with unsaved
//! changes are searched as they are in the editor, not as saved.
//!
//! Results arrive a file at a time, in no particular order. Dropping the
//! [`Search`] stops it.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkState;

use crate::file_index;
use crate::keymap::Command;
use crate::workspace::Workspace;

/// Lines shown before and after each matching line.
pub const CONTEXT_LINES: usize = 2;
/// The search stops after about this many matches.
pub const MAX_MATCHES: usize = 10_000;
/// Longer lines keep only this many bytes, around their first match.
const MAX_LINE_BYTES: usize = 400;
/// How much of a long line to keep before its first match.
const LEAD_BYTES: usize = 40;

/// What to search for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// An option of a [`Query`], shown as a toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    Case,
    Word,
    Regex,
}

impl Toggle {
    pub const ALL: [Toggle; 3] = [Toggle::Case, Toggle::Word, Toggle::Regex];

    pub fn label(self) -> &'static str {
        match self {
            Toggle::Case => "Aa",
            Toggle::Word => "ab",
            Toggle::Regex => ".*",
        }
    }

    pub fn command(self) -> Command {
        match self {
            Toggle::Case => Command::SearchToggleCase,
            Toggle::Word => Command::SearchToggleWord,
            Toggle::Regex => Command::SearchToggleRegex,
        }
    }

    /// The toggle `command` flips, if any.
    pub fn for_command(command: Command) -> Option<Toggle> {
        Toggle::ALL.into_iter().find(|t| t.command() == command)
    }

    /// What the shortcut hint calls it.
    pub fn name(self) -> &'static str {
        match self {
            Toggle::Case => "case",
            Toggle::Word => "word",
            Toggle::Regex => "regex",
        }
    }

    pub fn is_on(self, query: &Query) -> bool {
        match self {
            Toggle::Case => query.case_sensitive,
            Toggle::Word => query.whole_word,
            Toggle::Regex => query.regex,
        }
    }

    pub fn flip(self, query: &mut Query) {
        let flag = match self {
            Toggle::Case => &mut query.case_sensitive,
            Toggle::Word => &mut query.whole_word,
            Toggle::Regex => &mut query.regex,
        };
        *flag = !*flag;
    }
}

impl Query {
    /// A matcher for the query, or why it isn't a valid regex. A match never
    /// spans lines.
    pub fn matcher(&self) -> Result<RegexMatcher, String> {
        RegexMatcherBuilder::new()
            .case_insensitive(!self.case_sensitive)
            .word(self.whole_word)
            .fixed_strings(!self.regex)
            // `$` matches before `\r\n` too; a match never spans lines.
            .crlf(true)
            .line_terminator(Some(b'\n'))
            .build(&self.text)
            .map_err(|e| {
                // The regex crate's messages span several lines, pointing at
                // the error; the last line says what's wrong.
                let message = e.to_string();
                let last = message.lines().rev().find(|line| !line.trim().is_empty());
                last.unwrap_or(&message).trim().to_string()
            })
    }
}

/// One file's matches, with the lines around them.
#[derive(Debug, Clone)]
pub struct FileMatches {
    pub path: PathBuf,
    /// The path as shown, relative to the workspace.
    pub display: String,
    /// Matching lines and their context, in order. Lines that follow each
    /// other form one excerpt.
    pub lines: Vec<Line>,
    /// The matches in all lines.
    pub match_count: usize,
}

/// A line of a file, matching or around a match.
#[derive(Debug, Clone)]
pub struct Line {
    /// 0-based.
    pub number: u32,
    /// The line without its line break, or part of it if it's long.
    pub text: String,
    /// Where `text` starts in the whole line, in bytes.
    pub start: usize,
    /// Whether the line goes on past `text`.
    pub cut: bool,
    /// The matches, as byte ranges in the whole line. Empty for context.
    pub matches: Vec<Range<usize>>,
}

impl Line {
    /// The matches as byte ranges in `text`, clipped to it.
    pub fn matches_in_text(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        let end = self.start + self.text.len();
        self.matches.iter().map(move |m| {
            let clip = |at: usize| at.clamp(self.start, end) - self.start;
            clip(m.start)..clip(m.end)
        })
    }
}

enum Message {
    File(FileMatches),
    Done,
}

/// A search in progress, or done.
pub struct Search {
    receiver: Receiver<Message>,
    stop: Arc<AtomicBool>,
    /// Matches found so far, across all threads.
    found: Arc<AtomicUsize>,
    done: bool,
}

impl Search {
    /// Starts searching `workspace` for `query`. `open` has the text of
    /// files open with unsaved changes, by path, to search instead of what's
    /// on disk. Fails with a message if the query isn't a valid regex.
    pub fn start(
        workspace: &Workspace,
        query: &Query,
        open: HashMap<PathBuf, String>,
    ) -> Result<Search, String> {
        let matcher = query.matcher()?;
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let found = Arc::new(AtomicUsize::new(0));
        let search = Search {
            receiver,
            stop: Arc::clone(&stop),
            found: Arc::clone(&found),
            done: false,
        };
        let workspace = workspace.clone();
        std::thread::spawn(move || {
            run(&workspace, matcher, open, &stop, &found, &sender);
            let _ = sender.send(Message::Done);
        });
        Ok(search)
    }

    /// The files with matches found since the last call.
    pub fn poll(&mut self) -> Vec<FileMatches> {
        let mut files = Vec::new();
        loop {
            match self.receiver.try_recv() {
                Ok(Message::File(file)) => files.push(file),
                Ok(Message::Done) | Err(TryRecvError::Disconnected) => {
                    self.done = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        files
    }

    /// Whether the search finished, and every file it found was polled.
    pub fn done(&self) -> bool {
        self.done
    }

    /// Whether the search stopped at [`MAX_MATCHES`].
    pub fn truncated(&self) -> bool {
        self.found.load(Ordering::Relaxed) >= MAX_MATCHES
    }

    /// Waits for the search to finish, for tests.
    #[cfg(test)]
    pub fn wait(&mut self) -> Vec<FileMatches> {
        let mut files = Vec::new();
        while !self.done {
            files.extend(self.poll());
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        files
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Searches every file in the workspace, sending those with matches.
fn run(
    workspace: &Workspace,
    matcher: RegexMatcher,
    open: HashMap<PathBuf, String>,
    stop: &AtomicBool,
    found: &AtomicUsize,
    sender: &Sender<Message>,
) {
    let Some(walker) = file_index::walker(workspace) else {
        return;
    };
    walker.build_parallel().run(|| {
        let matcher = matcher.clone();
        let open = &open;
        let sender = sender.clone();
        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .before_context(CONTEXT_LINES)
            .after_context(CONTEXT_LINES)
            .binary_detection(BinaryDetection::quit(0))
            .build();
        Box::new(move |entry| {
            if stop.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !file_index::is_file(&entry) {
                return WalkState::Continue;
            }
            let path = entry.path();
            let Some(lines) = search_file(&mut searcher, &matcher, path, open.get(path)) else {
                return WalkState::Continue;
            };
            let match_count = lines.iter().map(|line| line.matches.len()).sum();
            if found.fetch_add(match_count, Ordering::Relaxed) + match_count >= MAX_MATCHES {
                stop.store(true, Ordering::Relaxed);
            }
            let file = FileMatches {
                path: path.to_path_buf(),
                display: workspace.display_path(path),
                lines,
                match_count,
            };
            if sender.send(Message::File(file)).is_err() {
                return WalkState::Quit;
            }
            WalkState::Continue
        })
    });
}

/// The matching lines in the file at `path`, or in `text` if given, with
/// their context. `None` if nothing matches, or the file is binary or isn't
/// UTF-8.
fn search_file(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    path: &Path,
    text: Option<&String>,
) -> Option<Vec<Line>> {
    let mut sink = Collect {
        matcher,
        lines: Vec::new(),
        skip: false,
    };
    let result = match text {
        Some(text) => searcher.search_slice(matcher, text.as_bytes(), &mut sink),
        None => searcher.search_path(matcher, path, &mut sink),
    };
    let has_match = sink.lines.iter().any(|line| !line.matches.is_empty());
    (result.is_ok() && !sink.skip && has_match).then_some(sink.lines)
}

/// Collects a file's lines from the searcher.
struct Collect<'m> {
    matcher: &'m RegexMatcher,
    lines: Vec<Line>,
    /// The file turned out to be binary or not UTF-8.
    skip: bool,
}

impl Collect<'_> {
    /// Adds a line, returning whether to go on.
    fn push(&mut self, number: Option<u64>, bytes: &[u8], is_match: bool) -> bool {
        let bytes = strip_line_break(bytes);
        let Ok(text) = std::str::from_utf8(bytes) else {
            self.skip = true;
            return false;
        };
        let mut matches = Vec::new();
        if is_match {
            let _ = self.matcher.find_iter(bytes, |m| {
                matches.push(m.start()..m.end());
                true
            });
            // A match that finds only empty ranges (`^`) still marks the line.
            if matches.is_empty() {
                matches.push(0..0);
            }
        }
        let (start, end) = if text.len() <= MAX_LINE_BYTES {
            (0, text.len())
        } else {
            let lead = matches
                .first()
                .map_or(0, |m| m.start.saturating_sub(LEAD_BYTES));
            let start = floor_char_boundary(text, lead);
            let end = floor_char_boundary(text, start + MAX_LINE_BYTES);
            (start, end)
        };
        self.lines.push(Line {
            number: number.unwrap_or(1).saturating_sub(1) as u32,
            text: text[start..end].to_string(),
            start,
            cut: end < text.len(),
            matches,
        });
        true
    }
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        Ok(self.push(mat.line_number(), mat.bytes(), true))
    }

    fn context(&mut self, _: &Searcher, context: &SinkContext<'_>) -> Result<bool, std::io::Error> {
        Ok(self.push(context.line_number(), context.bytes(), false))
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, std::io::Error> {
        self.skip = true;
        Ok(false)
    }
}

fn strip_line_break(bytes: &[u8]) -> &[u8] {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

/// The largest char boundary in `text` at or before `at`.
fn floor_char_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh workspace folder with `files` (name, contents).
    fn fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("qedit-search-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (file, contents) in files {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        dir.canonicalize().unwrap()
    }

    fn query(text: &str) -> Query {
        Query {
            text: text.to_string(),
            ..Query::default()
        }
    }

    /// The files matching `query`, sorted by path.
    fn search(root: &Path, query: &Query, open: HashMap<PathBuf, String>) -> Vec<FileMatches> {
        let workspace = Workspace::new([root.to_path_buf()]).unwrap();
        let mut files = Search::start(&workspace, query, open).unwrap().wait();
        files.sort_by(|a, b| a.display.cmp(&b.display));
        files
    }

    /// Each line as `number:text`, with matches in brackets.
    fn lines(file: &FileMatches) -> Vec<String> {
        file.lines
            .iter()
            .map(|line| {
                let mut text = line.text.clone();
                for m in line.matches_in_text().collect::<Vec<_>>().iter().rev() {
                    text.insert(m.end, ']');
                    text.insert(m.start, '[');
                }
                format!("{}:{text}", line.number)
            })
            .collect()
    }

    #[test]
    fn finds_matches_with_context_skipping_ignored_and_binary_files() {
        let text = "a\nb\nc\nneedle one\nd\ne\nf\ng\nh\nNeedle two needle\ni\n";
        let root = fixture(
            "basic",
            &[
                ("src/a.txt", text),
                ("ignored/b.txt", "needle"),
                (".gitignore", "ignored/\n"),
                ("bin.dat", "needle\0"),
                ("none.txt", "nothing here"),
            ],
        );
        let files = search(&root, &query("needle"), HashMap::new());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].display, "src/a.txt");
        assert_eq!(files[0].match_count, 3);
        assert_eq!(
            lines(&files[0]),
            [
                "1:b",
                "2:c",
                "3:[needle] one",
                "4:d",
                "5:e",
                "7:g",
                "8:h",
                "9:[Needle] two [needle]",
                "10:i"
            ]
        );
    }

    #[test]
    fn case_whole_word_and_regex_options() {
        let root = fixture("options", &[("a.txt", "Foo foo foobar\n")]);
        let count = |query: Query| {
            search(&root, &query, HashMap::new())
                .first()
                .map_or(0, |f| f.match_count)
        };
        assert_eq!(count(query("foo")), 3);
        assert_eq!(
            count(Query {
                case_sensitive: true,
                ..query("Foo")
            }),
            1
        );
        assert_eq!(
            count(Query {
                whole_word: true,
                ..query("foo")
            }),
            2
        );
        assert_eq!(count(query("f.o")), 0, "literal by default");
        assert_eq!(
            count(Query {
                regex: true,
                ..query("f.o")
            }),
            3
        );
        let workspace = Workspace::new([root.clone()]).unwrap();
        let bad = Query {
            regex: true,
            ..query("(")
        };
        let error = Search::start(&workspace, &bad, HashMap::new())
            .err()
            .unwrap();
        assert!(!error.contains('\n'), "{error}");
    }

    #[test]
    fn unsaved_text_is_searched_instead_of_the_file() {
        let root = fixture("open", &[("a.txt", "old\n"), ("b.txt", "old\n")]);
        let open = HashMap::from([(root.join("a.txt"), "new\nold\n".to_string())]);
        let files = search(&root, &query("old"), open);
        assert_eq!(lines(&files[0]), ["0:new", "1:[old]"]);
        assert_eq!(lines(&files[1]), ["0:[old]"]);
    }

    #[test]
    fn long_lines_keep_the_part_around_the_first_match() {
        let long = format!("{}needle{}", "é".repeat(300), "x".repeat(1000));
        let root = fixture("long", &[("a.txt", &format!("{long}\r\nshort\r\n"))]);
        let files = search(&root, &query("needle"), HashMap::new());
        let line = &files[0].lines[0];
        assert!(line.cut);
        assert!(line.start > 0 && line.text.len() <= MAX_LINE_BYTES);
        let m = line.matches_in_text().next().unwrap();
        assert_eq!(&line.text[m], "needle");
        assert_eq!(line.matches[0].start, 600, "bytes in the whole line");
        assert_eq!(files[0].lines[1].text, "short", "no \\r");
    }

    #[test]
    fn stops_after_the_most_matches() {
        let text = "x\n".repeat(MAX_MATCHES / 20);
        let files: Vec<(String, &str)> = (0..40)
            .map(|i| (format!("{i}.txt"), text.as_str()))
            .collect();
        let files: Vec<(&str, &str)> = files.iter().map(|(n, t)| (n.as_str(), *t)).collect();
        let root = fixture("many", &files);
        let workspace = Workspace::new([root]).unwrap();
        let mut search = Search::start(&workspace, &query("x"), HashMap::new()).unwrap();
        let found: usize = search.wait().iter().map(|f| f.match_count).sum();
        assert!(search.truncated());
        assert!((MAX_MATCHES..MAX_MATCHES * 2).contains(&found), "{found}");
    }
}
