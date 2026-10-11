# Client/server split

Status: experiment, on the `client-server` branch. Breaking changes are fine.

## Goal

Split cue into a **server** that owns everything on the machine being edited
(files, git, search, terminals, sessions) and a **client** that owns
everything on screen. These combinations should all work:

| client                 | server              | how                                |
| ---------------------- | ------------------- | ---------------------------------- |
| local TUI              | local               | today's use, through the same path |
| local TUI              | remote, over ssh    | `cue host:path`                    |
| remote TUI (over ssh)  | remote              | today's remote use                 |
| local GUI (later)      | local or remote     | a second renderer                  |

What we want from it: typing that doesn't wait on the network, surviving
flaky links (planes, VPNs), and better OS integration later, while keeping
the property that running everything remotely costs O(1) network per
operation (search, indexing, and git stay server-side).

## Decisions

- **One client at a time.** A new client takes over from the old one, as
  attaching a session does today.
- **Buffers are replicated.** The client holds a full copy of each open
  document, so typing, undo, autoindent, and highlighting are local. The
  server's copy is the one that counts: edits stream to it as versioned
  operations and are acked. There's one writer, so no CRDT: the only
  conflict is the file changing on disk, which cue handles already.
  Unsaved edits live on the server too, so a dead laptop loses nothing and
  another client can attach. Files over a size threshold open read-only,
  paged by viewport.
- **Terminals sync state, not bytes.** The VT (ghostty-vt) and scrollback
  live on the server; clients get screen snapshots and diffs, as in mosh.
  A `cat` of a huge log costs a frame, and a reconnect costs one snapshot.
  Local echo prediction is a possible later addition.
- **PDFs and images ship to the client**, which renders them, so zooming
  and paging don't wait on the network.
- **Config:** the server's, except `[ui]` and `[keys]`, which are the
  client's.
- **Client core vs. renderer.** Inside the client, the model (layout, tabs,
  buffer replicas, keymap, commands) is split from drawing (OpenTUI cells
  today, maybe GPUI later), so a GUI is a new renderer, not a new client.
- **Same path locally.** Local use goes through the same protocol, so the
  remote path never becomes the untested one.
- **Exact build match** between client and server. The client uploads its
  matching server binary over ssh (static musl builds for linux x86_64 and
  aarch64); the server never downloads anything. A server of another build
  re-execs into the uploaded one, adopting its state, as sessions do today.
- **System `ssh`**, not a library: `~/.ssh/config`, ProxyJump,
  ControlMaster, and agents just work. The server listens only on a unix
  socket in the user's runtime dir, mode 0700.

## Protocol shape

Three kinds of message, all plain data (no `Rc`, fds, or closures) so they
can be serialized as-is later:

- **Calls**: client → server, with an id. The server answers with a stream
  of replies under that id, then an end. Dropping the client's handle
  cancels the call; the server sees a cancel and stops the work. A reply
  for a call that's gone is dropped, which covers staleness (search results
  for an old query, a preview for an item no longer selected).
- **Tells**: client → server, no answer (what to watch, for instance).
- **Notices**: server → client, unasked (files changed on disk, later
  terminal screens and git status).

Replies reach the main loop as events through `wake`, not `async`/`await`:
every fs or process operation becomes "ask, and handle the answer when it
comes", with a loading state while it doesn't.

Later, for reconnection: session ids, sequence numbers and acks, and replay
buffers on both sides, so a dropped link resumes instead of starting over.
State the server pushes (repos, screens) should be idempotent snapshots or
diffs against an acked version, not a log of commands.

## Steps

### 1. The host boundary, in-process

Put a message API (`host.rs`) between the app and everything that touches
the machine, with the host on its own thread in the same process. Each
subsystem moves behind it one at a time; the app keeps working throughout.

1. Skeleton: calls, tells, notices, cancellation, an in-process transport
   with a demultiplexing reader thread (as a socket would need).
2. What runs in the background already: file watching, the file index,
   workspace search, git status, git sync, commits.
3. Symbol indexing and Go to Definition's reads.
4. The file tree's listings and file operations (rename, move, duplicate,
   trash, new file/folder), and the file dialog's listings.
5. Documents: load, save, revert, disk checks, `resolve`. This is where
   the edit-streaming design comes in; at first, load/save as whole texts.
6. The rest of git: stage, unstage, stash, branches, switch, log, commit
   details, file bases for diffs.
7. Images and PDFs: read their bytes through the host.
8. Terminals: ptys and the VT on the host, screens to the client.
9. Settings, recovery copies, sessions: decide per item which side owns it.
10. What stays local: clipboard, opening files externally (which then means
    fetching them first), the terminal's colors and capabilities (pushed to
    the server for embedded terminals' color queries).

Done when nothing outside the host touches the file system or spawns
processes, other than the client-local items in 10.

Progress:

- [x] 1.1 `host.rs`: `Link` (client end), `Call` (cancels on drop, and keeps
  the host alive), `Responder` (host end, with a cancelled flag for work
  that checks as it goes), and a reader thread that routes replies by call
  id and wakes the main loop.
- [x] 1.2 Watching (`Tell::Watch`, `Notice::DiskChanged`; the host does the
  settling and drops folders that don't exist), the file index, workspace
  search, git status, git sync, commits.
- [x] 1.3 Symbol indexing. The host keeps the per-file cache (modification
  times and symbols); the client keeps each file's symbols for Go to
  Definition and builds the picker's items.
- [x] 1.4 The tree lists folders through the host (`Request::ListFolders`)
  and keeps the listings, showing a folder as last listed until a new
  listing comes; a reveal waits for the listing that shows its path. Disk
  change notices carry what the host saw of each path (`Changes::dirs`),
  so the tree decides whether to list again without looking itself. File
  operations (`Request::FileOp`: create, move, copy, trash) run on the
  host; the file dialog stays open until one's done, showing its error if
  it failed. The dialog lists folders through the host
  (`Request::ReadDir`) and answers "is it there, is it a folder" from its
  listing; Enter before the listing comes is taken once it does, and what
  that leads to (Save As, then quit, say) reaches the main loop through
  `App::take_deferred`.
- [x] 1.5 Documents, as whole texts. Opening: a file not open yet is
  loaded by the host (`Request::Load`) and shown in the panel it was
  opened from once it comes, then what follows opening it (go to a line,
  select a match, read a Markdown file) runs (`App::open_and`); a newer
  open in the same panel replaces one on its way. Saving: the host writes
  only over the file as the document last had it (`Request::Save` with
  the stamp expected), so checking and writing take one round trip; text
  typed while a save is on its way stays unsaved; Save All carries on
  file by file as each is written, and what follows it (quitting, say)
  reaches the main loop through `App::take_deferred`. Changes on disk are
  checked by the host (`Request::CheckDisk`), and a check against a stamp
  the document no longer has is dropped. Revert loads through the host.
- [ ] 1.5 leftovers: `document::resolve` and `same_file` still
  canonicalize on the client; the
  first file at startup, crash recovery, and session restore load through
  the host but wait for it (`load_now`), until sessions move (1.9).
- [x] 1.6 The rest of git, through one request (`Request::Git` with a
  `GitCall`, answered by `git::run`): staging, stashing, reverting,
  branches, and switching from the app, which says how it went once git
  answers (`GitThen`); Amend's last message and a stash's files in the
  commit view; the log's pages, a commit's files, and copying a message;
  both sides of a commit diff, which shows empty until they come. What a
  file was in the last commit, for diffs and the gutter, is still read
  when a draw first wants it, but now that wakes the app to ask the host,
  and the diff shows once it's said (the caches' keys note whether it
  has). Matching open files to a repository no longer canonicalizes on the
  client.
- [ ] Small checks still on the client, for a sweep later: the picker's
  recent files (`is_file`), the tree's "was deleted" check for the
  changes view, the dialog's starting folder, following a link in reader
  mode, and whether an image or a commit in a panel's history is still
  there.
- [x] 1.7 Images and PDFs: the host reads their bytes (`Request::ReadFile`,
  unless they're as last read) and the client decodes and renders them,
  so zooming and paging never wait on it. Opening one waits for it as
  opening a file does; a change on disk reads it again. Quick Look asks
  the host what's at a path (`Request::Peek`: a folder's entries, an
  image's bytes, a file's text, or why there's nothing to show), and
  shows it once it's said.
- [ ] 1.8 onwards.

Notes from 1.1 and 1.2:

- Watching is now asynchronous, and a watch that starts after a file
  changes misses the change (FSEvents especially). Tests call
  `Link::settle` (test-only), which round-trips through the host thread.
- The client turns listed paths into picker items on the main thread; for
  200k files that may be noticeable. Measure before moving it.
- The walker reads `config::get()` for excludes. That's the host's config,
  which matches the config decision, but once the host is a separate
  process it needs its own config reload.
- `Workspace::add_root` canonicalizes on the client; it moves with the tree
  (1.4).
- Tests mostly expect the host to answer at once. `host::at_once()`
  (test-only, on by default, per thread) makes the tree, the file dialog,
  and file operations wait for their answers as they ask; their own tests
  turn it off to see what shows meanwhile. The host's threads also get
  the test's settings (`Tell::Config`, test-only), since in tests each
  thread has its own.
- Still on the client, for later steps: `document::resolve` and
  `same_file` (canonicalizing), the "was it deleted" check before opening
  from the tree, `Workspace::add_root`, and the dialog's starting folder.
- `Symbol::kind` is a `&'static str`; it needs to become owned or an enum
  to be serialized (step 2).
- Git status is still scheduled by the client (`Git::poll` decides when to
  call). See open questions.

### 2. Host in the session server

Move the host into the existing session server process, and the app into
the client. The client stops borrowing the tty's fds; the two talk over the
unix socket with a serialized protocol (serde + a compact format, versioned
by build id).

### 3. ssh

`cue host:path` and `cue host` (a launcher: file browser plus the host's
sessions). scp's rule decides what's remote: a `:` before any `/`. Bootstrap
the server binary over ssh, then run the protocol over the ssh channel.

Large transfers to watch: the file list for the picker (send paths relative
to roots, maybe compressed, maybe match server-side instead), and search
results.

### 4. Resumable transport

Sequence numbers, acks, replay, reconnection without a new handshake; a
disconnected UI (buffers editable with edits queued, terminals visibly
stale, popups saying offline). Later, a UDP transport keyed over ssh, as
mosh and Eternal Terminal do.

### 5. GUI renderer

## Open questions

- Fuzzy file matching client-side (needs the whole list) or server-side
  (a round trip per keystroke in the picker)?
- Git status scheduling: today the client decides when to ask; the host
  could instead refresh on its own watch events and push repos.
- Syntax highlighting of search excerpts happens server-side now; keep it
  there (CPU on the server, colors on the wire) or highlight on the client?
