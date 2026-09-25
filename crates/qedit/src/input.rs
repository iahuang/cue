//! Turns terminal input bytes into key events.
//!
//! OpenTUI's native core only writes to the terminal; key parsing lives in its
//! TypeScript layer. This is a small parser for what qedit needs. It accepts
//! the three encodings the renderer can switch the terminal into: legacy
//! sequences, xterm's modifyOtherKeys (`CSI 27;m;c ~`), and the kitty keyboard
//! protocol (`CSI c;m u`). Bracketed paste arrives as one event. Everything
//! else (replies to the renderer's capability queries) is returned as
//! [`Event::Reply`] for the renderer.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    pub const NONE: Mods = Mods {
        shift: false,
        alt: false,
        ctrl: false,
    };
    pub const CTRL: Mods = Mods {
        shift: false,
        alt: false,
        ctrl: true,
    };

    /// Decodes an xterm/kitty modifier parameter (1 + bitmask).
    fn from_param(param: u32) -> Mods {
        let bits = param.saturating_sub(1);
        Mods {
            shift: bits & 1 != 0,
            alt: bits & 2 != 0,
            ctrl: bits & 4 != 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Delete,
    Insert,
    Esc,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub mods: Mods,
}

impl Key {
    pub fn new(code: KeyCode, mods: Mods) -> Key {
        Key { code, mods }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    Paste(String),
    /// A terminal reply (DECRPM, DA, OSC color, cursor report, ...), to be
    /// passed to `Renderer::process_capability_response`.
    Reply(Vec<u8>),
}

const ESC: u8 = 0x1b;
const PASTE_END: &[u8] = b"\x1b[201~";

enum Token {
    Event(Event),
    PasteStart,
    Ignored,
}

#[derive(Default)]
pub struct Parser {
    pending: Vec<u8>,
    /// `Some` between bracketed-paste start and end markers.
    paste: Option<Vec<u8>>,
}

impl Parser {
    pub fn new() -> Parser {
        Parser::default()
    }

    /// Parses as many complete events as `bytes` (plus earlier leftovers)
    /// contain. Incomplete sequences wait for more input or [`Parser::flush`].
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            let rest = &self.pending[i..];
            if let Some(paste) = &mut self.paste {
                match find(rest, PASTE_END) {
                    Some(end) => {
                        paste.extend_from_slice(&rest[..end]);
                        let text = String::from_utf8_lossy(paste).into_owned();
                        events.push(Event::Paste(text));
                        self.paste = None;
                        i += end + PASTE_END.len();
                        continue;
                    }
                    None => {
                        // Keep a possible partial end marker for the next read.
                        let keep = (PASTE_END.len() - 1).min(rest.len());
                        paste.extend_from_slice(&rest[..rest.len() - keep]);
                        i += rest.len() - keep;
                        break;
                    }
                }
            }
            match parse_one(rest) {
                Some((token, len)) => {
                    i += len;
                    match token {
                        Token::Event(event) => events.push(event),
                        Token::PasteStart => self.paste = Some(Vec::new()),
                        Token::Ignored => {}
                    }
                }
                None => break,
            }
        }
        self.pending.drain(..i);
        events
    }

    /// Whether an incomplete sequence is waiting for more input.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty() && self.paste.is_none()
    }

    /// Called when input has gone quiet: resolves a leftover incomplete
    /// sequence. A lone ESC is the Escape key.
    pub fn flush(&mut self) -> Vec<Event> {
        if self.paste.is_some() || self.pending.is_empty() {
            return Vec::new();
        }
        let mut events = Vec::new();
        if self.pending.remove(0) == ESC {
            events.push(Event::Key(Key::new(KeyCode::Esc, Mods::NONE)));
        }
        events.extend(self.feed(&[]));
        events.extend(self.flush());
        events
    }
}

/// Parses one token from the start of `b`. `None` if `b` is an incomplete prefix.
fn parse_one(b: &[u8]) -> Option<(Token, usize)> {
    if b[0] != ESC {
        return parse_plain(b);
    }
    let &next = b.get(1)?;
    match next {
        b'[' => parse_csi(b),
        b'O' => {
            let &code = b.get(2)?;
            let key = match code {
                b'A' => KeyCode::Up,
                b'B' => KeyCode::Down,
                b'C' => KeyCode::Right,
                b'D' => KeyCode::Left,
                b'H' => KeyCode::Home,
                b'F' => KeyCode::End,
                _ => return Some((Token::Ignored, 3)),
            };
            Some((key_token(key, Mods::NONE), 3))
        }
        // OSC: terminated by BEL or ST.
        b']' => {
            let end = b[2..].iter().position(|&c| c == 0x07).map(|p| p + 3);
            let st = find(&b[2..], b"\x1b\\").map(|p| p + 4);
            let end = match (end, st) {
                (Some(a), Some(b)) => a.min(b),
                (a, b) => a.or(b)?,
            };
            Some((Token::Event(Event::Reply(b[..end].to_vec())), end))
        }
        // DCS, APC, PM, SOS: terminated by ST.
        b'P' | b'_' | b'^' | b'X' => {
            let end = find(&b[2..], b"\x1b\\")? + 4;
            Some((Token::Event(Event::Reply(b[..end].to_vec())), end))
        }
        ESC => Some((key_token(KeyCode::Esc, Mods::NONE), 1)),
        _ => {
            // ESC followed by a key is Alt+key.
            let (token, len) = parse_plain(&b[1..])?;
            let token = match token {
                Token::Event(Event::Key(mut key)) => {
                    key.mods.alt = true;
                    Token::Event(Event::Key(key))
                }
                other => other,
            };
            Some((token, len + 1))
        }
    }
}

fn parse_plain(b: &[u8]) -> Option<(Token, usize)> {
    let key = match b[0] {
        b'\r' | b'\n' => Key::new(KeyCode::Enter, Mods::NONE),
        b'\t' => Key::new(KeyCode::Tab, Mods::NONE),
        0x7f | 0x08 => Key::new(KeyCode::Backspace, Mods::NONE),
        c @ 0x01..=0x1a => Key::new(KeyCode::Char((b'a' + c - 1) as char), Mods::CTRL),
        0x00 | 0x1c..=0x1f => return Some((Token::Ignored, 1)),
        lead => {
            let width = match lead {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => return Some((Token::Ignored, 1)),
            };
            if b.len() < width {
                return None;
            }
            return match std::str::from_utf8(&b[..width]) {
                Ok(s) => Some((
                    key_token(KeyCode::Char(s.chars().next().unwrap()), Mods::NONE),
                    width,
                )),
                Err(_) => Some((Token::Ignored, 1)),
            };
        }
    };
    Some((Token::Event(Event::Key(key)), 1))
}

fn parse_csi(b: &[u8]) -> Option<(Token, usize)> {
    // Parameter and intermediate bytes, then one final byte.
    let mut i = 2;
    while i < b.len() && (0x20..=0x3f).contains(&b[i]) {
        i += 1;
    }
    let &final_byte = b.get(i)?;
    let len = i + 1;
    if !(0x40..=0x7e).contains(&final_byte) {
        return Some((Token::Ignored, i));
    }
    let reply = || Some((Token::Event(Event::Reply(b[..len].to_vec())), len));

    let params = &b[2..i];
    if matches!(params.first(), Some(b'?' | b'>' | b'=' | b'<'))
        || params.iter().any(|c| (0x20..=0x2f).contains(c))
        || matches!(final_byte, b'R' | b't' | b'c' | b'n' | b'y')
    {
        return reply();
    }

    // "1;5:1" -> [[1], [5, 1]]
    let fields: Vec<Vec<u32>> = std::str::from_utf8(params)
        .unwrap_or("")
        .split(';')
        .map(|f| f.split(':').map(|n| n.parse().unwrap_or(0)).collect())
        .collect();
    let field = |i: usize, j: usize| {
        fields
            .get(i)
            .and_then(|f| f.get(j))
            .copied()
            .filter(|&n| n != 0)
    };
    let mods = Mods::from_param(field(1, 0).unwrap_or(1));

    let code = match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'Z' => {
            let mods = Mods {
                shift: true,
                ..mods
            };
            return Some((key_token(KeyCode::Tab, mods), len));
        }
        b'~' => match field(0, 0) {
            Some(1 | 7) => KeyCode::Home,
            Some(2) => KeyCode::Insert,
            Some(3) => KeyCode::Delete,
            Some(4 | 8) => KeyCode::End,
            Some(5) => KeyCode::PageUp,
            Some(6) => KeyCode::PageDown,
            Some(200) => return Some((Token::PasteStart, len)),
            // modifyOtherKeys: CSI 27 ; mods ; codepoint ~
            Some(27) => match field(2, 0) {
                Some(cp) => return Some((codepoint_token(cp, mods), len)),
                None => return Some((Token::Ignored, len)),
            },
            _ => return Some((Token::Ignored, len)),
        },
        // Kitty: CSI codepoint[:alternates] ; mods[:event] u. Event 3 is a release.
        b'u' => {
            if field(1, 1) == Some(3) {
                return Some((Token::Ignored, len));
            }
            match field(0, 0) {
                Some(cp) => return Some((codepoint_token(cp, mods), len)),
                None => return Some((Token::Ignored, len)),
            }
        }
        // Focus in/out.
        b'I' | b'O' => return Some((Token::Ignored, len)),
        _ => return reply(),
    };
    Some((key_token(code, mods), len))
}

fn codepoint_token(cp: u32, mods: Mods) -> Token {
    let code = match cp {
        13 => KeyCode::Enter,
        9 => KeyCode::Tab,
        8 | 127 => KeyCode::Backspace,
        27 => KeyCode::Esc,
        // Kitty's private-use range: keypad and media keys qedit doesn't use.
        57344..=63743 => return Token::Ignored,
        _ => match char::from_u32(cp) {
            Some(c) => KeyCode::Char(c),
            None => return Token::Ignored,
        },
    };
    key_token(code, mods)
}

fn key_token(code: KeyCode, mods: Mods) -> Token {
    Token::Event(Event::Key(Key::new(code, mods)))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(events: Vec<Event>) -> Vec<Key> {
        events
            .into_iter()
            .map(|e| match e {
                Event::Key(k) => k,
                other => panic!("expected a key, got {other:?}"),
            })
            .collect()
    }

    fn key(code: KeyCode) -> Key {
        Key::new(code, Mods::NONE)
    }

    fn ctrl(c: char) -> Key {
        Key::new(KeyCode::Char(c), Mods::CTRL)
    }

    #[test]
    fn text_and_control_keys() {
        let mut p = Parser::new();
        assert_eq!(
            keys(p.feed(b"hi\r\t\x7f\x11")),
            [
                key(KeyCode::Char('h')),
                key(KeyCode::Char('i')),
                key(KeyCode::Enter),
                key(KeyCode::Tab),
                key(KeyCode::Backspace),
                ctrl('q'),
            ]
        );
    }

    #[test]
    fn utf8_split_across_reads() {
        let mut p = Parser::new();
        let bytes = "é✓".as_bytes();
        assert!(p.feed(&bytes[..1]).is_empty());
        assert_eq!(keys(p.feed(&bytes[1..4])), [key(KeyCode::Char('é'))]);
        assert_eq!(keys(p.feed(&bytes[4..])), [key(KeyCode::Char('✓'))]);
    }

    #[test]
    fn arrows_in_every_encoding() {
        let mut p = Parser::new();
        let shift_ctrl = Mods {
            shift: true,
            ctrl: true,
            alt: false,
        };
        assert_eq!(
            keys(p.feed(b"\x1b[A\x1bOB\x1b[1;6C\x1b[D\x1b[3~\x1b[H\x1b[4~")),
            [
                key(KeyCode::Up),
                key(KeyCode::Down),
                Key::new(KeyCode::Right, shift_ctrl),
                key(KeyCode::Left),
                key(KeyCode::Delete),
                key(KeyCode::Home),
                key(KeyCode::End),
            ]
        );
    }

    #[test]
    fn kitty_and_modify_other_keys() {
        let mut p = Parser::new();
        assert_eq!(
            keys(p.feed(b"\x1b[113;5u\x1b[27;5;113~\x1b[13u\x1b[27u\x1b[97;1:3u")),
            [ctrl('q'), ctrl('q'), key(KeyCode::Enter), key(KeyCode::Esc)],
            "the key release (event 3) is dropped"
        );
    }

    #[test]
    fn alt_and_lone_escape() {
        let mut p = Parser::new();
        let alt_x = Key::new(
            KeyCode::Char('x'),
            Mods {
                alt: true,
                ..Mods::NONE
            },
        );
        assert_eq!(keys(p.feed(b"\x1bx")), [alt_x]);
        assert!(p.feed(b"\x1b").is_empty(), "could still become a sequence");
        assert_eq!(keys(p.flush()), [key(KeyCode::Esc)]);
    }

    #[test]
    fn terminal_replies_are_passed_through() {
        let mut p = Parser::new();
        let input: &[u8] = b"\x1b]11;rgb:1717/1717/1a1a\x07\x1b[?2027;0$y\x1b[12;40Rq\x1b[?1u\x1bP>|kitty\x1b\\\x1b[?62;22c";
        let events = p.feed(input);
        let replies: Vec<&[u8]> = events
            .iter()
            .filter_map(|e| match e {
                Event::Reply(r) => Some(r.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(
            replies,
            [
                &b"\x1b]11;rgb:1717/1717/1a1a\x07"[..],
                b"\x1b[?2027;0$y",
                b"\x1b[12;40R",
                b"\x1b[?1u",
                b"\x1bP>|kitty\x1b\\",
                b"\x1b[?62;22c",
            ]
        );
        assert!(events.contains(&Event::Key(key(KeyCode::Char('q')))));
    }

    #[test]
    fn bracketed_paste_split_across_reads() {
        let mut p = Parser::new();
        assert!(p.feed(b"\x1b[200~line one\n\x1b[A two\x1b[2").is_empty());
        assert_eq!(p.flush(), [], "no ESC key while a paste is open");
        assert_eq!(
            p.feed(b"01~x"),
            [
                Event::Paste("line one\n\x1b[A two".into()),
                Event::Key(key(KeyCode::Char('x')))
            ]
        );
    }
}
