//! Go to Definition, by name: what a name in a file refers to, as near as
//! its syntax tree tells without resolving names as a compiler would.
//!
//! The candidates are the definitions with the name that tags queries find
//! (see [`crate::symbols`]), in the file and across the workspace, ranked
//! by how well they fit the way the name is used: `Point::new` prefers a
//! `new` in `Point`, a name in a type a type, a call a function, and the
//! file it's in other files.

use std::cmp::Reverse;
use std::ops::Range;
use std::path::{Path, PathBuf};

use tree_sitter::{Node, Tree};

use crate::symbols::Symbol;

/// A name used somewhere, to find the definition of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    /// Its bytes in the text.
    pub bytes: Range<usize>,
    /// Its line, 0-based, and its bytes in the line, as a [`Symbol`]'s.
    pub line: u32,
    pub columns: Range<usize>,
    /// The last part of what it's qualified with: `Point` in `Point::new`,
    /// `geometry` in `geometry::area`; for `self.area()`, the type it's in.
    pub qualifier: Option<String>,
    pub role: Role,
}

/// How a name is used, as far as its place in the tree says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Called, as `f()`, `p.area()`, or `square!()`.
    Call,
    /// Named as a type.
    Type,
    Other,
}

/// The name at byte `byte` of `text`, which `tree` is the tree of, if
/// there's one there: an identifier, as the grammar has it, not a keyword
/// or part of a comment or string.
pub fn reference_at(tree: &Tree, text: &str, byte: usize) -> Option<Reference> {
    if byte >= text.len() {
        return None;
    }
    let node = tree.root_node().descendant_for_byte_range(byte, byte + 1)?;
    if !is_name(node, text) {
        return None;
    }
    let name = &text[node.byte_range()];
    let start = node.start_position();
    let qualifier = node
        .parent()
        .filter(|parent| qualifies(parent.kind()))
        .and_then(|_| node.prev_named_sibling())
        .and_then(|before| last_part(&text[before.byte_range()]))
        .and_then(|qualifier| match qualifier.as_str() {
            "self" | "Self" | "this" | "cls" => enclosing_type(node, text),
            _ => Some(qualifier),
        });
    Some(Reference {
        name: name.to_string(),
        bytes: node.byte_range(),
        line: start.row as u32,
        columns: start.column..start.column + name.len(),
        qualifier,
        role: role(node),
    })
}

/// Whether `node` is a name: grammars call theirs `identifier`,
/// `type_identifier`, `field_identifier`, and so on; Ruby's capitalized
/// ones `constant`, Bash's `word`, and PHP's `name`.
fn is_name(node: Node, text: &str) -> bool {
    let kind = node.kind();
    let named = kind.contains("identifier") || matches!(kind, "constant" | "word" | "name");
    let name = &text[node.byte_range()];
    named
        && node.is_named()
        && node.child_count() == 0
        && !name.is_empty()
        && !name.contains(char::is_whitespace)
}

/// Whether a node of `kind` qualifies the name it ends with by what comes
/// before: `a.b`, `a::b`, `a->b`.
fn qualifies(kind: &str) -> bool {
    matches!(
        kind,
        "attribute"
            | "call"
            | "field_access"
            | "field_expression"
            | "member_access_expression"
            | "member_expression"
            | "method_invocation"
            | "nested_identifier"
            | "nested_type_identifier"
            | "qualified_identifier"
            | "qualified_type"
            | "scoped_call_expression"
            | "scoped_identifier"
            | "scoped_type_identifier"
            | "selector_expression"
    )
}

/// Whether a node of `kind` calls something.
fn calls(kind: &str) -> bool {
    kind.contains("call") || kind.contains("invocation") || kind == "new_expression"
}

/// How the name `node` is used: called, as itself or the end of what's
/// called, or named as a type.
fn role(node: Node) -> Role {
    let kind = node.kind();
    let own = match kind.contains("type") || kind == "constant" {
        true => Role::Type,
        false => Role::Other,
    };
    let mut outer = node;
    while let Some(parent) = outer.parent() {
        if calls(parent.kind()) {
            let argument = parent
                .child_by_field_name("arguments")
                .is_some_and(|arguments| {
                    arguments.start_byte() <= outer.start_byte()
                        && outer.end_byte() <= arguments.end_byte()
                });
            return if argument { own } else { Role::Call };
        }
        if !qualifies(parent.kind()) || outer.prev_named_sibling().is_none() {
            break;
        }
        outer = parent;
    }
    own
}

/// The name of the type, class, or impl `node` is in, if any.
fn enclosing_type(node: Node, text: &str) -> Option<String> {
    const KINDS: &[&str] = &[
        "class",
        "enum",
        "extension",
        "impl",
        "interface",
        "object",
        "protocol",
        "struct",
        "trait",
    ];
    let mut up = node.parent();
    while let Some(n) = up {
        let kind = n.kind();
        let words = kind.split('_').collect::<Vec<_>>();
        if words.iter().any(|word| KINDS.contains(word)) {
            let name = n
                .child_by_field_name("name")
                .or_else(|| n.child_by_field_name("type"));
            if let Some(name) = name {
                return last_part(&text[name.byte_range()]);
            }
        }
        up = n.parent();
    }
    None
}

/// The last name in a path such as `crate::symbols`, `self.items`, or
/// `Point<T>`.
fn last_part(path: &str) -> Option<String> {
    let last = path
        .rsplit(['.', ':', '>'])
        .find(|part| !part.trim().is_empty())
        .unwrap_or(path);
    let name: String = last
        .trim()
        .chars()
        .take_while(|&c| c.is_alphanumeric() || c == '_' || c == '$')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Languages whose files use each other's definitions are one family:
/// JavaScript's and TypeScript's, and C's and C++'s, as through headers.
pub fn family(language: &str) -> &str {
    match language {
        "TypeScript" | "TSX" => "JavaScript",
        "C++" => "C",
        name => name,
    }
}

/// A definition a name may refer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The file it's in; `None` for the file the name is in, if that has
    /// no path.
    pub path: Option<PathBuf>,
    pub symbol: Symbol,
    /// How well it fits the name's use: higher fits better.
    score: i32,
}

/// The definitions a name may refer to.
#[derive(Debug, Default)]
pub struct Found {
    /// Best first.
    pub candidates: Vec<Candidate>,
    /// The name is where it's defined, which isn't a candidate.
    pub at_definition: bool,
}

impl Found {
    /// The candidate to go to without asking: the only one, or the one
    /// that fits best by a margin.
    pub fn best(&self) -> Option<&Candidate> {
        match self.candidates.as_slice() {
            [only] => Some(only),
            [first, second, ..] if first.score > second.score => Some(first),
            _ => None,
        }
    }
}

/// Ranks the definitions of `reference`'s name: those in `outline`, of the
/// file it's in, which is at `here`, and those `elsewhere`.
pub fn rank(
    reference: &Reference,
    here: Option<&Path>,
    outline: &[Symbol],
    elsewhere: Vec<(PathBuf, Symbol)>,
) -> Found {
    // The file it's in counts most, then those beside it.
    const SAME_FILE: i32 = 2;
    const SAME_FOLDER: i32 = 1;
    let mut found = Found::default();
    for symbol in outline.iter().filter(|s| s.name == reference.name) {
        if symbol.line == reference.line && symbol.bytes == reference.columns {
            found.at_definition = true;
            continue;
        }
        found.candidates.push(Candidate {
            path: here.map(Path::to_path_buf),
            score: fit(reference, symbol, here) + SAME_FILE,
            symbol: symbol.clone(),
        });
    }
    let folder = here.and_then(Path::parent);
    for (path, symbol) in elsewhere {
        if symbol.name != reference.name || Some(path.as_path()) == here {
            continue;
        }
        let near = folder.is_some() && path.parent() == folder;
        found.candidates.push(Candidate {
            score: fit(reference, &symbol, Some(&path)) + if near { SAME_FOLDER } else { 0 },
            path: Some(path),
            symbol,
        });
    }
    found.candidates.sort_by_cached_key(|c| {
        let here = c.path.as_deref() == here;
        (Reverse(c.score), !here, c.path.clone(), c.symbol.line)
    });
    found
}

/// How well `symbol`, in the file at `path`, fits `reference`'s use of
/// its name.
fn fit(reference: &Reference, symbol: &Symbol, path: Option<&Path>) -> i32 {
    // What it's qualified with names the type or module it's in.
    const QUALIFIED: i32 = 8;
    // The kind of definition its use calls for.
    const KIND: i32 = 4;
    let callable = matches!(symbol.kind, "function" | "method" | "macro" | "test");
    let typelike = matches!(
        symbol.kind,
        "class" | "enum" | "interface" | "struct" | "trait" | "type" | "union"
    );
    let mut score = 0;
    if let Some(qualifier) = &reference.qualifier {
        let container = symbol.container.rsplit('.').next().unwrap_or_default();
        let module = path
            .and_then(Path::file_stem)
            .is_some_and(|stem| stem == qualifier.as_str());
        if container == qualifier || module {
            score += QUALIFIED;
        }
    }
    score += match reference.role {
        Role::Call if callable => KIND,
        // A constructor, as `Point(1, 2)` in Python or Rust.
        Role::Call if typelike => KIND / 2,
        Role::Type if typelike => KIND,
        _ => 0,
    };
    // An impl names the type it's for, which is defined elsewhere.
    if symbol.kind == "impl" {
        score -= KIND;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language;
    use crate::symbols;

    fn parse(file: &str, text: &str) -> Tree {
        let language = language::detect(Some(Path::new(file)), String::new).unwrap();
        let grammar = (language.syntax.as_ref().unwrap().grammar)();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&grammar).unwrap();
        parser.parse(text, None).unwrap()
    }

    /// The reference at the first `needle` in `text`, `skip` bytes in.
    fn reference(file: &str, text: &str, needle: &str, skip: usize) -> Option<Reference> {
        let tree = parse(file, text);
        reference_at(&tree, text, text.find(needle).unwrap() + skip)
    }

    fn outline(file: &str, text: &str) -> Vec<Symbol> {
        let language = language::detect(Some(Path::new(file)), String::new).unwrap();
        symbols::outline(language, text)
    }

    const RUST: &str = "\
struct Point { x: i32 }

impl Point {
    fn new() -> Self { Point { x: 0 } }
    fn norm(&self) -> i32 { self.len() }
    fn len(&self) -> i32 { 0 }
}

fn new() {}

fn main() {
    // Point::new in a comment
    let p: Point = Point::new();
    p.norm();
    new();
    let s = \"new\";
}
";

    #[test]
    fn finds_names_and_how_theyre_used() {
        let at = |needle: &str, skip: usize| reference("a.rs", RUST, needle, skip);
        let r = at("Point::new()", 8).unwrap();
        assert_eq!(r.name, "new");
        assert_eq!(r.qualifier.as_deref(), Some("Point"));
        assert_eq!(r.role, Role::Call);
        assert_eq!(r.line, 12);
        assert_eq!(&RUST.lines().nth(12).unwrap()[r.columns.clone()], "new");
        assert_eq!(&RUST[r.bytes.clone()], "new");

        let r = at("Point = ", 0).unwrap();
        assert_eq!(
            (r.name.as_str(), r.role, r.qualifier),
            ("Point", Role::Type, None)
        );
        let r = at("p.norm", 2).unwrap();
        assert_eq!((r.name.as_str(), r.role), ("norm", Role::Call));
        assert_eq!(r.qualifier.as_deref(), Some("p"));
        // `self` is the type the method's in.
        let r = at("self.len", 5).unwrap();
        assert_eq!(r.qualifier.as_deref(), Some("Point"));
        let r = at("    new();", 4).unwrap();
        assert_eq!((r.role, r.qualifier), (Role::Call, None));

        // Not in comments, strings, keywords, or between names.
        assert_eq!(at("Point::new in", 0), None);
        assert_eq!(at("\"new\"", 1), None);
        assert_eq!(at("fn main", 0), None);
        assert_eq!(at("let p", 3), None);
    }

    #[test]
    fn ranks_by_qualifier_kind_and_place() {
        let symbols = outline("a.rs", RUST);
        let here = Path::new("/w/src/a.rs");
        let names = |found: &Found| -> Vec<(String, u32)> {
            found
                .candidates
                .iter()
                .map(|c| (c.symbol.container.clone(), c.symbol.line))
                .collect()
        };

        // `Point::new` is the one in `Point`.
        let r = reference("a.rs", RUST, "Point::new()", 8).unwrap();
        let found = rank(&r, Some(here), &symbols, Vec::new());
        assert_eq!(names(&found), [("Point".into(), 3), (String::new(), 8)]);
        assert_eq!(found.best().unwrap().symbol.line, 3);
        assert!(!found.at_definition);

        // `new()` is either: neither is in a type.
        let r = reference("a.rs", RUST, "    new();", 4).unwrap();
        let found = rank(&r, Some(here), &symbols, Vec::new());
        assert_eq!(found.candidates.len(), 2);
        assert!(found.best().is_none());

        // A type is the struct, not its impl.
        let r = reference("a.rs", RUST, "Point = ", 0).unwrap();
        let found = rank(&r, Some(here), &symbols, Vec::new());
        assert_eq!(found.best().unwrap().symbol.kind, "struct");

        // On a definition, it's the others.
        let r = reference("a.rs", RUST, "fn len", 3).unwrap();
        let found = rank(&r, Some(here), &symbols, Vec::new());
        assert!(found.at_definition);
        assert!(found.candidates.is_empty());

        // Elsewhere: beside the file before farther away, and a module
        // qualifies as a type does.
        let other = |path: &str, line: u32| {
            let symbol = Symbol {
                name: "norm".into(),
                kind: "function",
                line,
                bytes: 3..7,
                container: String::new(),
            };
            (PathBuf::from(path), symbol)
        };
        let r = reference("a.rs", RUST, "p.norm", 2).unwrap();
        let found = rank(
            &r,
            Some(here),
            &[],
            vec![other("/w/lib/b.rs", 1), other("/w/src/c.rs", 2)],
        );
        assert_eq!(
            found.best().unwrap().path.as_deref(),
            Some(Path::new("/w/src/c.rs"))
        );
        let text = "fn f() { geometry::norm(); }\n";
        let r = reference("a.rs", text, "norm", 0).unwrap();
        let found = rank(
            &r,
            Some(here),
            &[],
            vec![other("/w/src/c.rs", 2), other("/w/lib/geometry.rs", 1)],
        );
        assert_eq!(
            found.best().unwrap().path.as_deref(),
            Some(Path::new("/w/lib/geometry.rs"))
        );
    }

    #[test]
    fn finds_names_in_other_languages() {
        let py =
            "class A:\n    def f(self):\n        return self.g()\n    def g(self):\n        pass\n";
        let r = reference("a.py", py, "g()", 0).unwrap();
        assert_eq!((r.role, r.qualifier.as_deref()), (Role::Call, Some("A")));
        let found = rank(&r, None, &outline("a.py", py), Vec::new());
        assert_eq!(found.best().unwrap().symbol.line, 3);

        let ts = "interface Shape {}\nfunction area(s: Shape) { return s.size(); }\n";
        let r = reference("a.ts", ts, "Shape)", 0).unwrap();
        assert_eq!(r.role, Role::Type);
        let r = reference("a.ts", ts, "size", 0).unwrap();
        assert_eq!((r.role, r.qualifier.as_deref()), (Role::Call, Some("s")));

        let go = "package a\n\nfunc F() {}\n\nfunc G() { F() }\n";
        let r = reference("a.go", go, "F() }", 0).unwrap();
        let found = rank(&r, None, &outline("a.go", go), Vec::new());
        assert_eq!(found.best().unwrap().symbol.line, 2);
    }

    #[test]
    fn last_parts_of_paths() {
        assert_eq!(last_part("crate::symbols").as_deref(), Some("symbols"));
        assert_eq!(last_part("self.items").as_deref(), Some("items"));
        assert_eq!(last_part("Point<T>").as_deref(), Some("Point"));
        assert_eq!(last_part("p->next").as_deref(), Some("next"));
        assert_eq!(last_part("()"), None);
    }
}
