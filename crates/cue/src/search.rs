//! Searching the workspace's files for text, with ripgrep's engine.
//!
//! A search runs on background threads over the same files as the file
//! picker, skipping `.git`, ignored files, binary files, and files that
//! aren't UTF-8 (which cue can't open). Each file's matches come back with
//! the lines around them, for showing excerpts. Open files with unsaved
//! changes are searched as they are in the editor, not as saved.
//!
//! Results arrive a file at a time, in no particular order. Each file's
//! syntax colors come after it, once the file is parsed, so highlighting
//! never holds up results. Dropping the [`Search`] stops it.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkState;

use crate::file_index;
use crate::keymap::Command;
use crate::language::{self, Language};
use crate::syntax::{ExcerptHighlighter, LineColors, MAX_EXCERPT_BYTES};
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
    /// Syntax colors, as byte ranges in `text`, in order. Empty until they
    /// come in, or if the file's language isn't highlighted.
    pub syntax: LineColors,
    /// Where the whole line starts in the file, in bytes.
    offset: usize,
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

impl FileMatches {
    /// Colors the lines, with a list of colors for each.
    pub fn set_colors(&mut self, colors: Vec<LineColors>) {
        for (line, colors) in self.lines.iter_mut().zip(colors) {
            line.syntax = colors;
        }
    }
}

/// What a search found.
pub enum Found {
    /// A file with matches, not colored yet.
    File(FileMatches),
    /// The syntax colors of a file found before, for
    /// [`FileMatches::set_colors`].
    Colors {
        path: PathBuf,
        /// As in [`FileMatches::display`].
        display: String,
        colors: Vec<LineColors>,
    },
}

enum Message {
    Found(Found),
    Done,
}

/// A search in progress, or done.
pub struct Search {
    receiver: Receiver<Message>,
    /// Stops the search, and coloring what it found.
    dropped: Arc<AtomicBool>,
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
        let dropped = Arc::new(AtomicBool::new(false));
        let found = Arc::new(AtomicUsize::new(0));
        let search = Search {
            receiver,
            dropped: Arc::clone(&dropped),
            found: Arc::clone(&found),
            done: false,
        };
        let workspace = workspace.clone();
        std::thread::spawn(move || {
            run(&workspace, matcher, open, &dropped, &found, &sender);
            let _ = sender.send(Message::Done);
        });
        Ok(search)
    }

    /// What was found since the last call, in order: a file's colors come
    /// after the file.
    pub fn poll(&mut self) -> Vec<Found> {
        let mut found = Vec::new();
        loop {
            match self.receiver.try_recv() {
                Ok(Message::Found(f)) => found.push(f),
                Ok(Message::Done) | Err(TryRecvError::Disconnected) => {
                    self.done = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        found
    }

    /// Whether the search finished, and every file it found was polled.
    pub fn done(&self) -> bool {
        self.done
    }

    /// Whether the search stopped at [`MAX_MATCHES`].
    pub fn truncated(&self) -> bool {
        self.found.load(Ordering::Relaxed) >= MAX_MATCHES
    }

    /// Waits for the search to finish, for tests: the files, colored.
    #[cfg(test)]
    pub fn wait(&mut self) -> Vec<FileMatches> {
        let mut files: Vec<FileMatches> = Vec::new();
        while !self.done {
            for found in self.poll() {
                match found {
                    Found::File(file) => files.push(file),
                    Found::Colors { path, colors, .. } => {
                        let file = files.iter_mut().find(|file| file.path == path);
                        file.unwrap().set_colors(colors);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        files
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Relaxed);
    }
}

/// Searches every file in the workspace, sending those with matches, and
/// then their colors: files are colored on threads of their own, so
/// searching goes on at full speed.
fn run(
    workspace: &Workspace,
    matcher: RegexMatcher,
    open: HashMap<PathBuf, String>,
    dropped: &AtomicBool,
    found: &AtomicUsize,
    sender: &Sender<Message>,
) {
    let Some(walker) = file_index::walker(workspace) else {
        return;
    };
    // Set once there are enough matches, which stops searching but not
    // coloring the files found.
    let full = AtomicBool::new(false);
    let full = &full;
    let open = &open;
    let (jobs, queue) = mpsc::channel::<Job>();
    let queue = Mutex::new(queue);
    std::thread::scope(|scope| {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        for _ in 0..threads {
            let queue = &queue;
            let sender = sender.clone();
            scope.spawn(move || {
                let mut highlighter = ExcerptHighlighter::new();
                loop {
                    let job = queue.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok(job) = job else {
                        return;
                    };
                    let Some(colors) = job.color(&mut highlighter, dropped) else {
                        continue;
                    };
                    if sender.send(Message::Found(colors)).is_err() {
                        return;
                    }
                }
            });
        }
        walker.build_parallel().run(|| {
            let matcher = matcher.clone();
            let sender = sender.clone();
            let jobs = jobs.clone();
            let mut searcher = SearcherBuilder::new()
                .line_number(true)
                .before_context(CONTEXT_LINES)
                .after_context(CONTEXT_LINES)
                .binary_detection(BinaryDetection::quit(0))
                .build();
            Box::new(move |entry| {
                if dropped.load(Ordering::Relaxed) || full.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };
                if !file_index::is_file(&entry) {
                    return WalkState::Continue;
                }
                let path = entry.path();
                let unsaved = open.get(path);
                let Some(lines) = search_file(&mut searcher, &matcher, path, unsaved) else {
                    return WalkState::Continue;
                };
                let excerpts = excerpts(path, unsaved, &lines);
                let match_count = lines.iter().map(|line| line.matches.len()).sum();
                if found.fetch_add(match_count, Ordering::Relaxed) + match_count >= MAX_MATCHES {
                    full.store(true, Ordering::Relaxed);
                }
                let file = FileMatches {
                    path: path.to_path_buf(),
                    display: workspace.display_path(path),
                    lines,
                    match_count,
                };
                // Queued after the file is sent, so its colors come after it.
                let job = excerpts.map(|excerpts| Job {
                    path: file.path.clone(),
                    display: file.display.clone(),
                    excerpts,
                });
                if sender.send(Message::Found(Found::File(file))).is_err() {
                    return WalkState::Quit;
                }
                if let Some(job) = job {
                    let _ = jobs.send(job);
                }
                WalkState::Continue
            })
        });
        // Searching's done: the coloring threads finish what's queued.
        drop(jobs);
    });
}

/// A file to color.
struct Job<'t> {
    path: PathBuf,
    display: String,
    excerpts: Excerpts<'t>,
}

impl Job<'_> {
    fn color(self, highlighter: &mut ExcerptHighlighter, dropped: &AtomicBool) -> Option<Found> {
        let Excerpts {
            language,
            text,
            lines,
        } = self.excerpts;
        let colors = highlighter.highlight(&self.path, language, &text, &lines, dropped)?;
        Some(Found::Colors {
            path: self.path,
            display: self.display,
            colors,
        })
    }
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

/// What coloring a file's lines takes.
struct Excerpts<'t> {
    language: &'static Language,
    text: Cow<'t, str>,
    /// The lines' byte ranges in `text`.
    lines: Vec<Range<usize>>,
}

/// What coloring `lines` of the file at `path`, or of `unsaved` if given,
/// takes. `None` if cue doesn't highlight its language, or it's too big, or
/// changed since it was searched.
fn excerpts<'t>(path: &Path, unsaved: Option<&'t String>, lines: &[Line]) -> Option<Excerpts<'t>> {
    let read = || -> Option<Cow<'t, str>> {
        if let Some(text) = unsaved {
            return Some(Cow::Borrowed(text));
        }
        let size = std::fs::metadata(path).ok()?.len();
        if size > MAX_EXCERPT_BYTES as u64 {
            return None;
        }
        std::fs::read_to_string(path).ok().map(Cow::Owned)
    };
    // Only read to find a `#!` line if the name doesn't tell.
    let mut text = None;
    let language = language::detect(Some(path), || {
        text = read();
        let first = text.as_deref().and_then(|text| text.lines().next());
        first.unwrap_or_default().to_string()
    })?;
    language.syntax.as_ref()?;
    let text = text.or_else(read)?;
    let ranges: Vec<Range<usize>> = lines
        .iter()
        .map(|line| {
            let start = line.offset + line.start;
            start..start + line.text.len()
        })
        .collect();
    let unchanged = lines
        .iter()
        .zip(&ranges)
        .all(|(line, range)| text.get(range.clone()) == Some(line.text.as_str()));
    unchanged.then_some(Excerpts {
        language,
        text,
        lines: ranges,
    })
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
    fn push(&mut self, number: Option<u64>, offset: u64, bytes: &[u8], is_match: bool) -> bool {
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
            syntax: Vec::new(),
            offset: offset as usize,
        });
        true
    }
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        let offset = mat.absolute_byte_offset();
        Ok(self.push(mat.line_number(), offset, mat.bytes(), true))
    }

    fn context(&mut self, _: &Searcher, context: &SinkContext<'_>) -> Result<bool, std::io::Error> {
        let offset = context.absolute_byte_offset();
        Ok(self.push(context.line_number(), offset, context.bytes(), false))
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
    use crate::theme::SyntaxColor;
    use std::fs;

    /// A fresh workspace folder with `files` (name, contents).
    fn fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-search-{}", std::process::id()))
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

    /// The syntax color of the first `needle` in `line`.
    fn color_of(line: &Line, needle: &str) -> Option<SyntaxColor> {
        let at = line.text.find(needle).unwrap();
        let span = line.syntax.iter().find(|(bytes, _)| bytes.contains(&at));
        span.map(|&(_, color)| color)
    }

    #[test]
    fn excerpts_are_syntax_highlighted() {
        let rust = "/* a block\ncomment */\nfn main() {\n    let s = \"hit\";\n}\n";
        let root = fixture(
            "syntax",
            &[
                ("a.rs", rust),
                ("b.txt", "fn hit\n"),
                ("script", "#!/bin/sh\necho \"hit\"\n"),
                ("unsaved.rs", "// hit\n"),
            ],
        );
        let open = HashMap::from([(root.join("unsaved.rs"), "fn hit() {}\n".to_string())]);
        let files = search(&root, &query("hit"), open);
        let [a, b, script, unsaved] = &files[..] else {
            panic!("{files:?}");
        };
        let comment = SyntaxColor::of("comment");
        let keyword = SyntaxColor::of("keyword");
        let string = SyntaxColor::of("string");
        // The whole file is parsed: line 1 is inside a comment.
        assert_eq!(a.lines[0].number, 1);
        assert_eq!(color_of(&a.lines[0], "comment"), comment);
        assert_eq!(color_of(&a.lines[1], "fn"), keyword);
        assert_eq!(color_of(&a.lines[2], "hit"), string);
        assert!(b.lines[0].syntax.is_empty(), "no language");
        assert_eq!(color_of(&script.lines[1], "hit"), string, "by #! line");
        assert_eq!(color_of(&unsaved.lines[0], "fn"), keyword, "as edited");
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
