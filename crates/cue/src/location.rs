//! Places in files as programs print them, such as `src/main.rs:12:5`, for
//! opening from the command line, the picker, or a click in a terminal.
//!
//! The forms understood are those compilers, linters, test runners, and
//! search tools print: `path:line`, `path:line:column` (and a colon after,
//! before the message), `path(line)` and `path(line,column)` (MSVC and
//! TypeScript), and Python's `File "path", line N`. In a terminal, OSC 8
//! hyperlinks and `http`/`https` URLs are understood too.

use std::path::PathBuf;

/// A line, and maybe a column, in a file. Both are 0-based; the column
/// counts characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub column: Option<u32>,
}

impl Position {
    /// From 1-based numbers as programs print them; 0 counts as 1.
    pub fn printed(line: u32, column: Option<u32>) -> Position {
        Position {
            line: line.saturating_sub(1),
            column: column.map(|column| column.saturating_sub(1)),
        }
    }
}

/// Splits a position printed after a path off the end of `text`:
/// `:line`, `:line:column`, either with a colon after, or `(line)` and
/// `(line,column)`. The path left may be empty, as in `:12`.
pub fn split_position(text: &str) -> (&str, Option<Position>) {
    let trimmed = text.strip_suffix(':').unwrap_or(text);
    if let Some(inner) = trimmed.strip_suffix(')') {
        if let Some((path, numbers)) = inner.rsplit_once('(') {
            let (line, column) = match numbers.split_once(',') {
                Some((line, column)) => (line, Some(column.trim())),
                None => (numbers, None),
            };
            if let Some(position) = numbered(line, column) {
                return (path, Some(position));
            }
        }
    }
    let Some((rest, last)) = trimmed.rsplit_once(':') else {
        return (text, None);
    };
    let Some(last) = number(last) else {
        return (text, None);
    };
    if let Some((path, line)) = rest.rsplit_once(':') {
        if let Some(line) = number(line) {
            return (path, Some(Position::printed(line, Some(last))));
        }
    }
    (rest, Some(Position::printed(last, None)))
}

fn numbered(line: &str, column: Option<&str>) -> Option<Position> {
    let line = number(line)?;
    let column = match column {
        Some(column) => Some(number(column)?),
        None => None,
    };
    Some(Position::printed(line, column))
}

/// A run of ASCII digits as a number.
fn number(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// What a click in a terminal points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A web page, for the system to open.
    Url(String),
    /// A file, and maybe a place in it.
    File(PathBuf, Option<Position>),
}

/// Characters that end a path or URL printed in text, besides whitespace.
const DELIMITERS: &[char] = &['"', '\'', '`', '<', '>', '|'];
/// Characters around a path or URL that aren't part of it, as in
/// `(src/main.rs)` or `see src/main.rs.`.
const LEADING: &[char] = &['(', '[', '{'];
const TRAILING: &[char] = &[')', ']', '}', ',', '.', ';', ':', '!', '?'];

/// What's at byte `offset` of `line`, a line of text in a terminal: a URL,
/// or a path `resolve` finds a file at, with the position printed after it,
/// if any.
pub fn target_in_line(
    line: &str,
    offset: usize,
    resolve: impl Fn(&str) -> Option<PathBuf>,
) -> Option<Target> {
    if offset >= line.len() || !line.is_char_boundary(offset) {
        return None;
    }
    let is_break = |c: char| c.is_whitespace() || DELIMITERS.contains(&c);
    if line[offset..].starts_with(is_break) {
        return None;
    }
    let start = line[..offset]
        .char_indices()
        .rev()
        .find(|&(_, c)| is_break(c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let end = line[offset..]
        .find(is_break)
        .map_or(line.len(), |i| offset + i);
    let word = &line[start..end];

    if let Some(scheme) = ["https://", "http://", "file://"]
        .iter()
        .filter_map(|scheme| word.find(scheme))
        .min()
    {
        return target_of_link(trim_url(&word[scheme..]), resolve);
    }

    // Python: `File "src/main.py", line 12, in main`.
    let python = || {
        let rest = line[end..].strip_prefix('"')?.strip_prefix(", line ")?;
        let digits = rest.split(|c: char| !c.is_ascii_digit()).next()?;
        Some(Position::printed(number(digits)?, None))
    };

    // The word, and the word less what may surround it; each also with
    // what follows its last colons dropped, for `path:12:message`.
    let mut candidates = vec![word];
    let inner = word.trim_start_matches(LEADING);
    let mut trimmed = inner;
    loop {
        if trimmed != word {
            candidates.push(trimmed);
        }
        match trimmed.strip_suffix(TRAILING) {
            Some(shorter) if !shorter.is_empty() => trimmed = shorter,
            _ => break,
        }
    }
    for candidate in candidates.clone() {
        let mut rest = candidate;
        while let Some((before, _)) = rest.rsplit_once(':') {
            candidates.push(before);
            rest = before;
        }
    }
    for candidate in candidates {
        let (path, position) = split_position(candidate);
        if path.is_empty() {
            continue;
        }
        if let Some(file) = resolve(path) {
            return Some(Target::File(file, position.or_else(python)));
        }
    }
    None
}

/// A URL with what likely follows it in prose dropped: trailing
/// punctuation, and a closing bracket it didn't open.
fn trim_url(url: &str) -> &str {
    let mut url = url;
    loop {
        let Some(last) = url.chars().last() else {
            return url;
        };
        let unopened = match last {
            ')' => !url.contains('('),
            ']' => !url.contains('['),
            '}' => !url.contains('{'),
            _ => false,
        };
        if unopened || matches!(last, ',' | '.' | ';' | ':' | '!' | '?') {
            url = &url[..url.len() - last.len_utf8()];
        } else {
            return url;
        }
    }
}

/// What a link points at: an `http` or `https` page, or a file (a
/// `file://` URL, whose host is ignored), with a position after its path
/// if one is printed there. Other schemes aren't opened.
pub fn target_of_link(uri: &str, resolve: impl Fn(&str) -> Option<PathBuf>) -> Option<Target> {
    if uri.starts_with("https://") || uri.starts_with("http://") {
        return Some(Target::Url(uri.to_string()));
    }
    let rest = uri.strip_prefix("file://")?;
    // After the host, if any.
    let path = &rest[rest.find('/')?..];
    let path = percent_decode(path.split(['?', '#']).next().unwrap_or(path));
    if let Some(file) = resolve(&path) {
        return Some(Target::File(file, None));
    }
    let (path, position) = split_position(&path);
    Some(Target::File(resolve(path)?, position))
}

/// The file `path` names, as printed in a terminal: absolute, from the
/// home folder (`~/`), or relative to one of `folders`, the first that has
/// it. A diff's `a/` and `b/` prefixes are dropped if the path isn't found
/// with them.
pub fn find_file(path: &str, folders: &[PathBuf]) -> Option<PathBuf> {
    let unprefixed = path.strip_prefix("a/").or_else(|| path.strip_prefix("b/"));
    for path in std::iter::once(path).chain(unprefixed) {
        let named = match path.strip_prefix("~/") {
            Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
            None => PathBuf::from(path),
        };
        let found = if named.is_absolute() {
            named.is_file().then_some(named)
        } else {
            folders
                .iter()
                .map(|folder| folder.join(&named))
                .find(|candidate| candidate.is_file())
        };
        if let Some(found) = found {
            return Some(crate::document::resolve(&found));
        }
    }
    None
}

/// `%XX` escapes decoded, as in a `file://` URL's path. Invalid ones, and
/// escapes that don't make UTF-8, are kept as they are.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn at(line: u32, column: Option<u32>) -> Option<Position> {
        Some(Position::printed(line, column))
    }

    #[test]
    fn splits_positions_printed_after_paths() {
        for (text, path, position) in [
            ("src/main.rs", "src/main.rs", None),
            ("src/main.rs:12", "src/main.rs", at(12, None)),
            ("src/main.rs:12:5", "src/main.rs", at(12, Some(5))),
            ("src/main.rs:12:5:", "src/main.rs", at(12, Some(5))),
            ("src/main.rs:12:", "src/main.rs", at(12, None)),
            ("src/a.ts(12,5)", "src/a.ts", at(12, Some(5))),
            ("src/a.ts(12, 5):", "src/a.ts", at(12, Some(5))),
            ("src/a.cpp(12)", "src/a.cpp", at(12, None)),
            (":42", "", at(42, None)),
            (":0", "", at(1, None)),
            ("a:b:c", "a:b:c", None),
            ("f(x)", "f(x)", None),
            ("main.rs:", "main.rs:", None),
            ("x:12a", "x:12a", None),
        ] {
            assert_eq!(split_position(text), (path, position), "{text}");
        }
        assert_eq!(
            at(1, Some(1)).unwrap(),
            Position {
                line: 0,
                column: Some(0)
            }
        );
    }

    /// Resolves paths that are in `files`, as if under /root.
    fn resolver(files: &'static [&'static str]) -> impl Fn(&str) -> Option<PathBuf> {
        move |path| {
            let path = path.strip_prefix("/root/").unwrap_or(path);
            files.contains(&path).then(|| Path::new("/root").join(path))
        }
    }

    fn file(path: &str, position: Option<Position>) -> Option<Target> {
        Some(Target::File(Path::new("/root").join(path), position))
    }

    #[test]
    fn finds_what_a_click_in_a_terminal_line_points_at() {
        let resolve = resolver(&["src/main.rs", "src/a.ts", "app.py", "src/[id].tsx"]);
        let target = |line: &str, needle: &str| {
            let offset = line.find(needle).unwrap();
            target_in_line(line, offset, &resolve)
        };
        let main = |line, column| file("src/main.rs", at(line, column));
        assert_eq!(target("  --> src/main.rs:12:5", "main"), main(12, Some(5)));
        assert_eq!(target("  --> src/main.rs:12:5", "12"), main(12, Some(5)));
        assert_eq!(target("src/main.rs:12:fn main() {", "src"), main(12, None));
        assert_eq!(
            target("src/main.rs:12:5: warning", "src"),
            main(12, Some(5))
        );
        assert_eq!(
            target("see (src/main.rs).", "main"),
            file("src/main.rs", None)
        );
        assert_eq!(
            target("at f (/root/src/main.rs:3:9)", "main"),
            main(3, Some(9))
        );
        assert_eq!(
            target("src/a.ts(4,2): error TS1", "a.ts"),
            file("src/a.ts", at(4, Some(2)))
        );
        assert_eq!(
            target("modified: `src/main.rs`", "main"),
            file("src/main.rs", None)
        );
        assert_eq!(
            target("  File \"app.py\", line 7, in <module>", "app"),
            file("app.py", at(7, None))
        );
        assert_eq!(
            target("src/[id].tsx:2", "id"),
            file("src/[id].tsx", at(2, None))
        );
        // Not a file, or not on a word.
        assert_eq!(target("error: src/missing.rs:1", "missing"), None);
        assert_eq!(target_in_line("a  b", 1, &resolve), None);
        assert_eq!(target_in_line("a", 5, &resolve), None);
    }

    #[test]
    fn finds_urls_and_links() {
        let resolve = resolver(&["src/main.rs", "a b.rs"]);
        let line = "docs at https://example.com/a_(b)?q=1, and (https://x.dev/y).";
        assert_eq!(
            target_in_line(line, line.find("example").unwrap(), &resolve),
            Some(Target::Url("https://example.com/a_(b)?q=1".into()))
        );
        assert_eq!(
            target_in_line(line, line.find("x.dev").unwrap(), &resolve),
            Some(Target::Url("https://x.dev/y".into()))
        );
        assert_eq!(
            target_of_link("file://host/root/a%20b.rs", &resolve),
            file("a b.rs", None)
        );
        assert_eq!(
            target_of_link("file:///root/src/main.rs:3:4", &resolve),
            file("src/main.rs", at(3, Some(4)))
        );
        assert_eq!(target_of_link("file:///root/nope", &resolve), None);
        assert_eq!(target_of_link("mailto:a@b.c", &resolve), None);
    }
}
