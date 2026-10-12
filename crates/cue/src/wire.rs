//! Messages as they go over a stream between the app and its host: each
//! serialized ([`postcard`]), after its length, a `u64`.
//!
//! The stream is shared by everything, so a big message (a PDF's bytes,
//! the picker's list of files) mustn't hold up what's waited on as it's
//! typed, such as a terminal's output, behind it. So the writer keeps two
//! queues: a message is interactive, written whole as soon as it can be, or
//! bulk ([`Message::bulk`]), written in chunks, one at a time, and only
//! while no interactive message waits. Bulk messages of different lanes
//! take turns, chunk by chunk, so a small file that's opened doesn't wait
//! for the whole list of files to go first.
//!
//! What must stay in order does: each message may say what it's in order
//! with (its [`Message::lane`], such as the call a reply is for), and one
//! that comes while a bulk message of its lane is still being written goes
//! after it, in that lane. A message with no lane ([`Message::barrier`])
//! goes after every bulk message written before it, so that the host's
//! notices (files changed on disk) never overtake a reply sent before them.
//!
//! A chunk's length has its top bits set: [`BULK`] for every chunk of a
//! bulk message, with [`LAST`] too for its last; the lane follows the
//! length. A message written whole has neither, which is how the handshake
//! is written too, by builds that know nothing of chunks.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Receiver;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Set in a chunk's length: it's part of a bulk message.
const BULK: u64 = 1 << 63;
/// Set in a chunk's length: it's the last part of its message.
const LAST: u64 = 1 << 62;
/// The bits of a length that are the length.
const LENGTH: u64 = LAST - 1;
/// The longest a message can be, to tell a stream that isn't one of
/// messages from one that is.
const MAX_MESSAGE: u64 = 1 << 36;
/// The most of a bulk message written at once.
const CHUNK: usize = if cfg!(test) { 16 } else { 32 * 1024 };

/// What a message is kept in order with: messages of the same lane arrive
/// in the order they were sent.
pub type Lane = u64;

/// A message, as sent over a stream.
pub trait Message: Serialize + DeserializeOwned + Send + 'static {
    /// Whether it's big: written in chunks, behind interactive messages.
    fn bulk(&self) -> bool {
        false
    }

    /// What it's kept in order with, if anything (see the module's docs).
    fn lane(&self) -> Option<Lane> {
        None
    }

    /// Whether it goes after every bulk message sent before it.
    fn barrier(&self) -> bool {
        false
    }
}

/// Reads a message written whole: its length, then it.
pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut len = [0; 8];
    reader.read_exact(&mut len)?;
    let len = u64::from_le_bytes(len);
    if len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Not a message."));
    }
    let mut frame = vec![0; len as usize];
    reader.read_exact(&mut frame)?;
    postcard::from_bytes(&frame).map_err(io::Error::other)
}

/// Writes a message whole: its length, then it.
pub fn write_frame<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let frame = postcard::to_stdvec(message).map_err(io::Error::other)?;
    writer.write_all(&(frame.len() as u64).to_le_bytes())?;
    writer.write_all(&frame)
}

/// Takes each message from `stream`, until it ends, or `take` says to stop.
pub fn read_frames<T: DeserializeOwned>(stream: UnixStream, take: impl FnMut(T) -> bool) {
    if let Err(err) = read_all(BufReader::new(stream), take) {
        if err.kind() != io::ErrorKind::UnexpectedEof {
            eprintln!("cue: a message from the other end can't be read: {err}");
        }
    }
}

/// Takes each message from `reader`, whole or put together from its
/// chunks, until it ends, or `take` says to stop.
fn read_all<T: DeserializeOwned>(
    mut reader: impl Read,
    mut take: impl FnMut(T) -> bool,
) -> io::Result<()> {
    // The bulk messages under way, by lane.
    let mut partial: HashMap<Lane, Vec<u8>> = HashMap::new();
    loop {
        let mut len = [0; 8];
        reader.read_exact(&mut len)?;
        let len = u64::from_le_bytes(len);
        let size = len & LENGTH;
        if size > MAX_MESSAGE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Not a message."));
        }
        let lane = match len & BULK {
            0 => None,
            _ => {
                let mut lane = [0; 8];
                reader.read_exact(&mut lane)?;
                Some(u64::from_le_bytes(lane))
            }
        };
        let mut frame = vec![0; size as usize];
        reader.read_exact(&mut frame)?;
        let whole = match lane {
            None => frame,
            Some(lane) => {
                let so_far = partial.entry(lane).or_default();
                so_far.extend_from_slice(&frame);
                if so_far.len() as u64 > MAX_MESSAGE {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Not a message."));
                }
                if len & LAST == 0 {
                    continue;
                }
                partial.remove(&lane).unwrap_or_default()
            }
        };
        let message = postcard::from_bytes(&whole).map_err(io::Error::other)?;
        if !take(message) {
            return Ok(());
        }
    }
}

/// Sends what comes from `outbox` over `stream`, until it's all sent, or
/// the other end's gone, interactive messages first (see the module's
/// docs). Then the connection's done with, both ways: on either end,
/// nothing's left to read once nothing's left to send.
pub fn write_frames<T: Message>(stream: UnixStream, outbox: Receiver<T>) {
    let mut writer = BufWriter::new(&stream);
    let _ = send(&mut writer, &outbox);
    let _ = writer.flush();
    drop(writer);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Sends what comes from `outbox` to `writer`, until it's all sent.
fn send<T: Message>(writer: &mut impl Write, outbox: &Receiver<T>) -> io::Result<()> {
    let mut queues = Queues::default();
    let mut open = true;
    while open || !queues.is_empty() {
        if queues.is_empty() {
            match outbox.recv() {
                Ok(message) => queues.push(message)?,
                Err(_) => open = false,
            }
        }
        // What's come meanwhile goes in the same write.
        while let Ok(message) = outbox.try_recv() {
            queues.push(message)?;
        }
        while let Some(frame) = queues.interactive.pop_front() {
            writer.write_all(&(frame.len() as u64).to_le_bytes())?;
            writer.write_all(&frame)?;
        }
        queues.write_chunk(writer)?;
        writer.flush()?;
    }
    Ok(())
}

/// What's waiting to be written.
#[derive(Default)]
struct Queues {
    /// Interactive messages, serialized, in order.
    interactive: VecDeque<Vec<u8>>,
    /// The lanes with bulk messages to write, in turn, the first next.
    lanes: VecDeque<Lane>,
    /// Each lane's bulk messages, serialized, the first written so far.
    bulk: HashMap<Lane, (VecDeque<Vec<u8>>, usize)>,
    /// Messages that wait for the bulk messages before them, with the
    /// number of those still to be written.
    held: VecDeque<(Vec<u8>, usize)>,
    /// Bulk messages written in all, to tell when those before a held
    /// message are.
    written: usize,
    /// Bulk messages queued in all.
    queued: usize,
}

impl Queues {
    fn is_empty(&self) -> bool {
        self.interactive.is_empty() && self.lanes.is_empty() && self.held.is_empty()
    }

    /// Queues `message` as the module's docs say.
    fn push<T: Message>(&mut self, message: T) -> io::Result<()> {
        let frame = postcard::to_stdvec(&message).map_err(io::Error::other)?;
        let lane = message.lane();
        let busy = lane.is_some_and(|lane| self.bulk.contains_key(&lane));
        if message.bulk() || busy {
            let lane = lane.unwrap_or(0);
            let (frames, _) = self.bulk.entry(lane).or_insert_with(|| {
                self.lanes.push_back(lane);
                (VecDeque::new(), 0)
            });
            frames.push_back(frame);
            self.queued += 1;
        } else if message.barrier() && self.queued > self.written {
            self.held.push_back((frame, self.queued));
        } else {
            self.interactive.push_back(frame);
        }
        Ok(())
    }

    /// Writes the next chunk of the next lane's bulk message, if there's
    /// one, then the held messages it lets go.
    fn write_chunk(&mut self, writer: &mut impl Write) -> io::Result<()> {
        let Some(lane) = self.lanes.pop_front() else {
            return Ok(());
        };
        let (frames, offset) = self.bulk.get_mut(&lane).expect("a lane has its messages");
        let frame = frames.front().expect("a lane has a message");
        let end = (*offset + CHUNK).min(frame.len());
        let mut len = BULK | (end - *offset) as u64;
        if end == frame.len() {
            len |= LAST;
        }
        writer.write_all(&len.to_le_bytes())?;
        writer.write_all(&lane.to_le_bytes())?;
        writer.write_all(&frame[*offset..end])?;
        *offset = end;
        if end == frame.len() {
            frames.pop_front();
            *offset = 0;
            self.written += 1;
        }
        match frames.is_empty() {
            true => {
                self.bulk.remove(&lane);
            }
            // Its turn's over.
            false => self.lanes.push_back(lane),
        }
        while self
            .held
            .front()
            .is_some_and(|&(_, after)| after <= self.written)
        {
            let (frame, _) = self.held.pop_front().unwrap();
            writer.write_all(&(frame.len() as u64).to_le_bytes())?;
            writer.write_all(&frame)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::sync::mpsc;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    enum Test {
        Small(u64, String),
        Big(u64, String),
        Notice(String),
    }

    impl Message for Test {
        fn bulk(&self) -> bool {
            matches!(self, Test::Big(..))
        }

        fn lane(&self) -> Option<Lane> {
            match self {
                Test::Small(lane, _) | Test::Big(lane, _) => Some(*lane),
                Test::Notice(_) => None,
            }
        }

        fn barrier(&self) -> bool {
            matches!(self, Test::Notice(_))
        }
    }

    /// What `sent` arrives as, all queued before any is written.
    fn arrives(sent: &[Test]) -> Vec<Test> {
        let (outbox, inbox) = mpsc::channel();
        for message in sent {
            outbox.send(message.clone()).unwrap();
        }
        drop(outbox);
        let mut bytes = Vec::new();
        send(&mut bytes, &inbox).unwrap();
        let mut got = Vec::new();
        let ended = read_all(&bytes[..], |message| {
            got.push(message);
            true
        });
        assert_eq!(ended.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        got
    }

    #[test]
    fn interactive_messages_overtake_bulk_ones_of_other_lanes() {
        let big = Test::Big(1, "x".repeat(100));
        let small = Test::Small(2, "typed".into());
        assert_eq!(arrives(&[big.clone(), small.clone()]), [small, big]);
    }

    #[test]
    fn a_lane_stays_in_order() {
        let big = Test::Big(1, "x".repeat(100));
        let after = Test::Small(1, "end".into());
        let other = Test::Small(2, "other".into());
        assert_eq!(
            arrives(&[big.clone(), after.clone(), other.clone()]),
            [other, big, after]
        );
    }

    #[test]
    fn bulk_lanes_take_turns() {
        let long = Test::Big(1, "x".repeat(200));
        let short = Test::Big(2, "y".repeat(20));
        assert_eq!(arrives(&[long.clone(), short.clone()]), [short, long]);
    }

    #[test]
    fn a_barrier_waits_for_the_bulk_before_it() {
        let big = Test::Big(1, "x".repeat(100));
        let notice = Test::Notice("changed".into());
        let small = Test::Small(2, "typed".into());
        assert_eq!(
            arrives(&[big.clone(), notice.clone(), small.clone()]),
            [small, big, notice]
        );
    }

    #[test]
    fn a_whole_message_reads_as_the_handshake_does() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &Test::Notice("hi".into())).unwrap();
        let read: Test = read_frame(&mut &bytes[..]).unwrap();
        assert_eq!(read, Test::Notice("hi".into()));
    }
}
