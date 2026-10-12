//! What a file defines, such as functions, types, and headings, for Go to
//! Symbol, from its language's tree-sitter tags query: the query code
//! navigation tools use, which captures each definition (`@definition.kind`)
//! and its name (`@name`).
//!
//! [`outline`] lists a file's symbols in order, each with the names of the
//! symbols around it, as a method's class. A [`SymbolIndex`] lists every
//! symbol in the workspace, indexed on the host (see [`crate::host`]),
//! which keeps them for the session: indexing again reads only the files
//! that changed since.

use std::collections::HashMap;
use std::fs;
use std::ops::{ControlFlow, Range};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tree_sitter::{ParseOptions, ParseState, Parser, Query, QueryCursor, StreamingIterator};

use crate::host::{Call, Link, Reply, Request, Responder};
use crate::language::{self, Language};
use crate::picker::Item;
use crate::theme::SyntaxColor;
use crate::workspace::Workspace;
use serde::{Deserialize, Serialize};

/// Larger files aren't outlined, or indexed for the workspace.
const MAX_BYTES: usize = 4 << 20;
const MAX_INDEXED_BYTES: u64 = 1 << 20;
/// Parsing that takes longer gives up.
const PARSE_BUDGET: Duration = Duration::from_millis(500);
/// The workspace index stops at this many symbols, to bound its memory.
const MAX_SYMBOLS: usize = 500_000;
/// How often the first indexing reports what it found so far.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Something a file defines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Symbol {
    pub name: String,
    /// What it is: `function`, `struct`, `heading`, and so on.
    pub kind: &'static str,
    /// The line of its name, 0-based.
    pub line: u32,
    /// Its name's bytes in the line.
    pub bytes: Range<usize>,
    /// The names of the symbols it's in, outermost first, as `Foo.bar`.
    pub container: String,
}

/// Kinds of definition named by the syntax node's own name, where that says
/// more than the tags query does: a Rust `struct_item` is a `struct`, not a
/// `class`.
const NODE_KINDS: &[&str] = &[
    "class",
    "const",
    "enum",
    "function",
    "impl",
    "interface",
    "macro",
    "method",
    "module",
    "namespace",
    "protocol",
    "record",
    "struct",
    "test",
    "trait",
    "type",
    "union",
];

/// A [`Symbol`] as sent, its kind one of [`kind_name`]'s or
/// [`NODE_KINDS`].
#[derive(Deserialize)]
struct Sent {
    name: String,
    kind: String,
    line: u32,
    bytes: Range<usize>,
    container: String,
}

impl<'de> Deserialize<'de> for Symbol {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Symbol, D::Error> {
        let sent = Sent::deserialize(de)?;
        let known = NODE_KINDS.iter().find(|&&known| known == sent.kind);
        Ok(Symbol {
            name: sent.name,
            kind: known.copied().unwrap_or_else(|| kind_name(&sent.kind)),
            line: sent.line,
            bytes: sent.bytes,
            container: sent.container,
        })
    }
}

/// A language's compiled tags query.
struct Tags {
    language: tree_sitter::Language,
    query: Query,
    name: u32,
    /// The kind of definition each capture marks, by capture index.
    kinds: Vec<Option<&'static str>>,
}

/// Tags queries by language name; `None` for a language without one, or
/// whose query doesn't compile.
type TagsByLanguage = HashMap<&'static str, Option<Arc<Tags>>>;

/// Compiled on first use, and shared by every thread.
static TAGS: LazyLock<Mutex<TagsByLanguage>> = LazyLock::new(Default::default);

fn tags_of(language: &'static Language) -> Option<Arc<Tags>> {
    let syntax = language.syntax.as_ref()?;
    if syntax.tags.is_empty() {
        return None;
    }
    let mut tags = TAGS.lock().unwrap_or_else(|e| e.into_inner());
    tags.entry(language.name)
        .or_insert_with(|| {
            let grammar = (syntax.grammar)();
            let query = Query::new(&grammar, &syntax.tags.join("\n")).ok()?;
            let kinds = query
                .capture_names()
                .iter()
                .map(|name| name.strip_prefix("definition.").map(kind_name))
                .collect();
            Some(Arc::new(Tags {
                name: query.capture_index_for_name("name")?,
                language: grammar,
                query,
                kinds,
            }))
        })
        .clone()
}

/// A tags query's kind of definition, as shown.
fn kind_name(kind: &str) -> &'static str {
    const KINDS: &[&str] = &[
        "class",
        "constant",
        "enum",
        "field",
        "function",
        "heading",
        "impl",
        "interface",
        "macro",
        "method",
        "module",
        "property",
        "struct",
        "test",
        "trait",
        "type",
        "union",
    ];
    KINDS
        .iter()
        .find(|&&known| known == kind)
        .copied()
        .unwrap_or("symbol")
}

/// What `text`, written in `language`, defines, in order.
pub fn outline(language: &'static Language, text: &str) -> Vec<Symbol> {
    let Some(tags) = tags_of(language) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    outline_with(&tags, &mut parser, text)
}

fn outline_with(tags: &Tags, parser: &mut Parser, text: &str) -> Vec<Symbol> {
    if text.len() > MAX_BYTES || parser.set_language(&tags.language).is_err() {
        return Vec::new();
    }
    let deadline = Instant::now() + PARSE_BUDGET;
    let mut progress = |_: &ParseState| match Instant::now() < deadline {
        true => ControlFlow::Continue(()),
        false => ControlFlow::Break(()),
    };
    let bytes = text.as_bytes();
    let Some(tree) = parser.parse_with_options(
        &mut |at, _| &bytes[at.min(bytes.len())..],
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    ) else {
        return Vec::new();
    };

    // Each definition once, as the first pattern that matched it says:
    // Rust's tags match a method as a function too.
    struct Found<'tree> {
        node: tree_sitter::Node<'tree>,
        name: tree_sitter::Node<'tree>,
        kind: &'static str,
        pattern: usize,
    }
    let mut found: HashMap<usize, Found> = HashMap::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&tags.query, tree.root_node(), bytes);
    while let Some(m) = matches.next() {
        let name = m.captures().iter().find(|c| c.index == tags.name);
        let definition = m
            .captures()
            .iter()
            .find_map(|c| Some((c.node, tags.kinds[c.index as usize]?)));
        let (Some(name), Some((node, kind))) = (name, definition) else {
            continue;
        };
        let better = found
            .get(&node.id())
            .is_none_or(|old| m.pattern_index < old.pattern);
        if better {
            found.insert(
                node.id(),
                Found {
                    node,
                    name: name.node,
                    kind,
                    pattern: m.pattern_index,
                },
            );
        }
    }
    let mut found: Vec<Found> = found.into_values().collect();
    found.sort_by_key(|f| (f.node.start_byte(), std::cmp::Reverse(f.node.end_byte())));

    // The definitions each one is in, outermost first.
    let mut around: Vec<(usize, String)> = Vec::new();
    let mut symbols = Vec::with_capacity(found.len());
    for f in found {
        while around
            .last()
            .is_some_and(|&(end, _)| end <= f.node.start_byte())
        {
            around.pop();
        }
        let whole = &text[f.name.byte_range()];
        let first_line = whole.lines().next().unwrap_or_default();
        let name = first_line.trim();
        if name.is_empty() {
            continue;
        }
        let start = f.name.start_position();
        let lead = first_line.len() - first_line.trim_start().len();
        let column = start.column + lead;
        let kind = match f.kind {
            "method" => "method",
            kind => {
                let node_kind = f.node.kind();
                let word = node_kind.split('_').next().unwrap_or(node_kind);
                NODE_KINDS
                    .iter()
                    .find(|&&known| known == word)
                    .copied()
                    .unwrap_or(kind)
            }
        };
        symbols.push(Symbol {
            name: name.to_string(),
            kind,
            line: start.row as u32,
            bytes: column..column + name.len(),
            container: around
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>()
                .join("."),
        });
        around.push((f.node.end_byte(), name.to_string()));
    }
    symbols
}

/// The color a symbol of `kind` is drawn in, as the editor colors such
/// names: functions as functions, types as types, and so on.
pub fn color(kind: &str) -> Option<SyntaxColor> {
    let capture = match kind {
        "function" | "method" | "test" => "function",
        "macro" => "function.macro",
        "class" | "enum" | "impl" | "interface" | "struct" | "trait" | "type" | "union" => "type",
        "const" | "constant" => "constant",
        "heading" => "text.title",
        _ => return None,
    };
    SyntaxColor::of(capture)
}

// --- the workspace's symbols ----------------------------------------------------

/// A file's symbols, as of its modification time and size.
#[derive(Clone)]
struct Indexed {
    modified: Option<SystemTime>,
    len: u64,
    symbols: Vec<Symbol>,
}

/// What the host keeps of the last indexing, to read only the files that
/// changed the next time.
#[derive(Default)]
pub struct Cache {
    files: Mutex<Arc<HashMap<PathBuf, Indexed>>>,
}

/// Each file's symbols, by path.
pub type FileSymbols = Vec<(PathBuf, Vec<Symbol>)>;

pub struct SymbolIndex {
    host: Link,
    workspace: Workspace,
    /// Shared with an open picker, which keeps its copy until given a new one.
    items: Rc<Vec<Item>>,
    /// Each file's symbols, as of the last indexing to complete.
    files: FileSymbols,
    /// Whether `items` is a whole index, not the first in progress.
    complete: bool,
    indexing: Option<Call>,
    /// Files to index once the indexing in progress is done.
    again: Option<Vec<PathBuf>>,
}

impl SymbolIndex {
    /// An index of `workspace`'s symbols, made on `host`.
    pub fn new(host: &Link, workspace: &Workspace) -> SymbolIndex {
        SymbolIndex {
            host: host.clone(),
            workspace: workspace.clone(),
            items: Rc::new(Vec::new()),
            files: Vec::new(),
            complete: false,
            indexing: None,
            again: None,
        }
    }

    /// Every symbol found, by file, then line, once the first indexing is
    /// complete.
    pub fn items(&self) -> Rc<Vec<Item>> {
        Rc::clone(&self.items)
    }

    /// Whether the first indexing is in progress, or hasn't started.
    pub fn indexing(&self) -> bool {
        !self.complete
    }

    /// Whether indexing ever started.
    pub fn started(&self) -> bool {
        self.complete || self.indexing.is_some()
    }

    /// The symbols named `name`, and the files they're in, as of the last
    /// indexing to complete.
    pub fn named(&self, name: &str) -> Vec<(PathBuf, Symbol)> {
        self.files
            .iter()
            .flat_map(|(path, symbols)| {
                symbols
                    .iter()
                    .filter(|symbol| symbol.name == name)
                    .map(|symbol| (path.clone(), symbol.clone()))
            })
            .collect()
    }

    /// Indexes `files` in the background, reading only those that changed
    /// since the last time, and keeping the current symbols until done.
    pub fn refresh(&mut self, files: Vec<PathBuf>) {
        if self.indexing.is_some() {
            self.again = Some(files);
            return;
        }
        self.indexing = Some(self.host.call(Request::IndexSymbols {
            files,
            progress: !self.complete,
        }));
    }

    /// Takes in what the indexing found since the last call. Returns
    /// whether the symbols changed.
    pub fn poll(&mut self) -> bool {
        let Some(indexing) = self.indexing.take() else {
            return false;
        };
        let mut changed = false;
        loop {
            match indexing.try_recv() {
                Ok(Reply::SymbolsFound(found)) => {
                    let found = items(&self.workspace, &found);
                    Rc::make_mut(&mut self.items).extend(found);
                    changed = true;
                }
                Ok(Reply::Symbols(files)) => {
                    self.items = Rc::new(items(&self.workspace, &files).collect());
                    self.files = files;
                    self.complete = true;
                    changed = true;
                    break;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => {
                    self.indexing = Some(indexing);
                    return changed;
                }
                Err(TryRecvError::Disconnected) => {
                    self.complete = true;
                    break;
                }
            }
        }
        if let Some(files) = self.again.take() {
            self.refresh(files);
        }
        changed
    }

    /// Waits for the indexing in progress to finish, for tests.
    #[cfg(test)]
    pub fn wait(&mut self) {
        while self.indexing.is_some() {
            self.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// `files`' symbols, as the picker lists them.
fn items<'a>(workspace: &'a Workspace, files: &'a FileSymbols) -> impl Iterator<Item = Item> + 'a {
    files.iter().flat_map(|(path, symbols)| {
        let shown = workspace.display_path(path);
        symbols
            .iter()
            .map(move |symbol| Item::workspace_symbol(symbol.clone(), Some(path.clone()), &shown))
    })
}

/// Indexes the symbols in `files`, for the host, reusing those in `cache`
/// for files that haven't changed, on as many threads as there are cores
/// (up to 8). Replies with the symbols found now and then as it goes, if
/// `progress`, and then with all of them, by path. Stops if the call is
/// cancelled, leaving the cache as it was.
pub fn index(files: &[PathBuf], cache: &Cache, progress: bool, responder: &Responder) {
    let old = Arc::clone(&lock(&cache.files));
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8);
    let chunk = files.len().div_ceil(threads).max(1);
    let (sender, receiver) = mpsc::channel::<(PathBuf, Indexed)>();
    let cancelled = responder.cancelled();
    let new = std::thread::scope(|scope| {
        for files in files.chunks(chunk) {
            let sender = sender.clone();
            let old = &old;
            scope.spawn(move || {
                let mut parser = Parser::new();
                for path in files {
                    if cancelled.load(Ordering::Relaxed) {
                        return;
                    }
                    let Some(indexed) = index_file(path, old, &mut parser) else {
                        continue;
                    };
                    if sender.send((path.clone(), indexed)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(sender);

        let mut new = HashMap::new();
        let mut count = 0;
        let mut pending = Vec::new();
        let mut last_sent = Instant::now();
        for (path, mut indexed) in receiver {
            indexed.symbols.truncate(MAX_SYMBOLS - count);
            count += indexed.symbols.len();
            if progress {
                pending.push((path.clone(), indexed.symbols.clone()));
            }
            new.insert(path, indexed);
            if last_sent.elapsed() >= PROGRESS_INTERVAL && !pending.is_empty() {
                if !responder.reply(Reply::SymbolsFound(std::mem::take(&mut pending))) {
                    return None;
                }
                last_sent = Instant::now();
            }
        }
        Some(new)
    });
    let Some(new) = new.filter(|_| !responder.is_cancelled()) else {
        return;
    };
    let mut files: FileSymbols = new
        .iter()
        .map(|(path, indexed)| (path.clone(), indexed.symbols.clone()))
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    *lock(&cache.files) = Arc::new(new);
    responder.reply(Reply::Symbols(files));
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The symbols in the file at `path`, from `cache` if it hasn't changed, or
/// `None` if cue can't outline it.
fn index_file(
    path: &PathBuf,
    cache: &HashMap<PathBuf, Indexed>,
    parser: &mut Parser,
) -> Option<Indexed> {
    let language = language::detect(Some(path), String::new)?;
    let tags = tags_of(language)?;
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok();
    if let Some(cached) = cache.get(path) {
        if modified.is_some() && cached.modified == modified && cached.len == meta.len() {
            return Some(cached.clone());
        }
    }
    let symbols = if meta.len() > MAX_INDEXED_BYTES {
        Vec::new()
    } else {
        let text = fs::read_to_string(path).ok()?;
        outline_with(&tags, parser, &text)
    };
    Some(Indexed {
        modified,
        len: meta.len(),
        symbols,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn outline_of(file: &str, text: &str) -> Vec<(String, &'static str, u32, String)> {
        let language = language::detect(Some(Path::new(file)), String::new).unwrap();
        outline(language, text)
            .into_iter()
            .map(|s| {
                assert_eq!(
                    &text.lines().nth(s.line as usize).unwrap()[s.bytes.clone()],
                    s.name,
                    "{file}"
                );
                (s.name, s.kind, s.line, s.container)
            })
            .collect()
    }

    fn symbol(
        name: &str,
        kind: &'static str,
        line: u32,
        container: &str,
    ) -> (String, &'static str, u32, String) {
        (name.to_string(), kind, line, container.to_string())
    }

    #[test]
    fn every_tags_query_compiles() {
        let mut wrong = Vec::new();
        for language in language::all() {
            let tagged = language.syntax.as_ref().is_some_and(|s| !s.tags.is_empty());
            if tagged && tags_of(language).is_none() {
                wrong.push(language.name);
            }
        }
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    #[test]
    fn outlines_rust_with_what_symbols_are_in() {
        let text = "\
struct Point { x: i32 }

impl Point {
    const ORIGIN: i32 = 0;
    fn new() -> Self { todo!() }
}

pub trait Shape {
    fn area(&self) -> f64;
}

mod geometry {
    pub fn distance() {}
}

macro_rules! square { ($x:expr) => { $x * $x }; }
static COUNT: u32 = 0;
";
        assert_eq!(
            outline_of("a.rs", text),
            [
                symbol("Point", "struct", 0, ""),
                symbol("Point", "impl", 2, ""),
                symbol("ORIGIN", "const", 3, "Point"),
                symbol("new", "method", 4, "Point"),
                symbol("Shape", "trait", 7, ""),
                symbol("area", "method", 8, "Shape"),
                symbol("geometry", "module", 11, ""),
                symbol("distance", "function", 12, "geometry"),
                symbol("square", "macro", 15, ""),
                symbol("COUNT", "constant", 16, ""),
            ]
        );
    }

    #[test]
    fn outlines_other_languages() {
        assert_eq!(
            outline_of(
                "a.py",
                "class A:\n    def f(self):\n        pass\n\ndef g():\n    pass\n"
            ),
            [
                symbol("A", "class", 0, ""),
                symbol("f", "function", 1, "A"),
                symbol("g", "function", 4, ""),
            ]
        );
        assert_eq!(
            outline_of(
                "a.ts",
                "interface Shape {}\nclass Box {\n  size() { return 1; }\n}\nconst go = () => 1;\n"
            ),
            [
                symbol("Shape", "interface", 0, ""),
                symbol("Box", "class", 1, ""),
                symbol("size", "method", 2, "Box"),
                symbol("go", "function", 4, ""),
            ]
        );
        assert_eq!(
            outline_of(
                "a.go",
                "package a\n\ntype T struct{}\n\nfunc (t T) M() {}\n\nfunc F() {}\n"
            ),
            [
                symbol("T", "type", 2, ""),
                symbol("M", "method", 4, ""),
                symbol("F", "function", 6, ""),
            ]
        );
        // Setext headings don't start sections, so nothing is in them.
        assert_eq!(
            outline_of(
                "a.md",
                "Book\n====\n\n# Title\n\nText.\n\n## Part one\n\nMore.\n\n## Part two\n"
            ),
            [
                symbol("Book", "heading", 0, ""),
                symbol("Title", "heading", 3, ""),
                symbol("Part one", "heading", 7, "Title"),
                symbol("Part two", "heading", 11, "Title"),
            ]
        );
        assert_eq!(
            outline_of(
                "a.zig",
                "const std = @import(\"std\");\nconst List = struct {\n    fn push() void {}\n};\npub fn main() void {}\ntest \"adds\" {}\n"
            ),
            [
                symbol("List", "struct", 1, ""),
                symbol("push", "function", 2, "List"),
                symbol("main", "function", 4, ""),
                symbol("adds", "test", 5, ""),
            ]
        );
        assert_eq!(
            outline_of("a.sh", "#!/bin/sh\nbuild() {\n  make\n}\n"),
            [symbol("build", "function", 1, "")]
        );
        // No tags query: nothing.
        let json = language::detect(Some(Path::new("a.json")), String::new).unwrap();
        assert!(tags_of(json).is_none());
        assert!(outline(json, "{\"a\": 1}").is_empty());
    }

    #[test]
    fn indexes_the_workspace_again_only_where_files_changed() {
        let root = std::env::temp_dir()
            .join(format!("cue-symbols-{}", std::process::id()))
            .join("index");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        let root = root.canonicalize().unwrap();
        fs::write(root.join("src/a.rs"), "fn alpha() {}\n").unwrap();
        fs::write(root.join("b.py"), "def beta():\n    pass\n").unwrap();
        fs::write(root.join("notes.txt"), "fn not_code() {}\n").unwrap();
        let files = vec![
            root.join("b.py"),
            root.join("notes.txt"),
            root.join("src/a.rs"),
        ];
        let workspace = Workspace::new([root.clone()]).unwrap();
        let mut index = SymbolIndex::new(&Link::local(), &workspace);
        assert!(index.indexing());
        index.refresh(files.clone());
        index.wait();
        assert!(!index.indexing());
        let texts = |index: &SymbolIndex| -> Vec<String> {
            index.items().iter().map(|item| item.text.clone()).collect()
        };
        assert_eq!(texts(&index), ["beta b.py:1", "alpha src/a.rs:1"]);

        // Changed files are read again; the rest come from before.
        fs::write(root.join("src/a.rs"), "\nfn alpha2() {}\n").unwrap();
        let later = SystemTime::now() + Duration::from_secs(5);
        fs::File::options()
            .write(true)
            .open(root.join("src/a.rs"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        index.refresh(files);
        index.wait();
        assert_eq!(texts(&index), ["beta b.py:1", "alpha2 src/a.rs:2"]);
    }
}
