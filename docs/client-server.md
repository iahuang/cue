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
- **Sessions are the host's.** It keeps their files (`session.toml`,
  unsaved text, terminal screens) and their lock; the client sends what
  changes (layout, unsaved text, screens) and asks for them back to
  restore.
- **Recovery copies stay the client's**, a safety net for unsaved text
  that doesn't depend on the host.
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
  ControlMaster, and agents just work. The server listens only on unix
  sockets, in folders checked to be the user's own, as tmux does: theirs,
  not a link, and no one else's to write to (`/tmp/cue-UID/` is fine on
  macOS, which has no `XDG_RUNTIME_DIR`, but on a shared Linux machine
  another user can make that folder first).

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

Everything shares one stream, so a big reply (a PDF, the picker's list)
mustn't hold up terminal output and acks behind it: big replies go in
chunks, and each side's writer keeps two queues, sending from the bulk one
only while the interactive one's empty.

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
- [x] 1.5 leftovers: the host resolves paths as it loads and saves
  them (`document::resolve`), and says where each file really is,
  symlinks followed (`Loaded::real`, `Written::real`), so a symlink and
  its file open as one document; the client only normalizes paths
  (`document::normalize`), and `same_file` is gone. The file named at
  startup opens as any other, once loaded, and a line given with it is
  gone to then (`App::go_to`). Recovery and session restore still wait,
  but for one round trip in all (`load_all`, `images_now`, and the
  session's texts and screens asked for at once), not one each.
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
- [x] Small checks: the app keeps what the host's disk change notices
  said went (`App::gone`), which the picker's recent files, panels'
  history, and the dialog's starting folder go by; what it never heard
  of is taken to be there. A file opened from the changes list that
  isn't there says it was deleted once the host's loaded nothing
  (`Opening::existing`). The file dialog goes up from a starting folder
  that went. Following a link in reader mode, and adding a folder to the
  workspace, ask the host what's there (`Request::Look`). Terminals
  start in their saved folder, or the workspace's if that went, as the
  host finds (`Start::Shell`'s `or`).
- [x] 1.7 Images and PDFs: the host reads their bytes (`Request::ReadFile`,
  unless they're as last read) and the client decodes and renders them,
  so zooming and paging never wait on it. Opening one waits for it as
  opening a file does; a change on disk reads it again. Quick Look asks
  the host what's at a path (`Request::Peek`: a folder's entries, an
  image's bytes, a file's text, or why there's nothing to show), and
  shows it once it's said.
- [x] 1.8 Terminals: the host runs each pty in a thread of its own
  (`pty::serve`), which reads the program's output, writes input, resizes,
  and reaps it. A terminal is one long call (`Request::Terminal`): replies
  carry output, how the program's doing (busy, its name, its folders,
  looked at as output comes and every 250ms), and its exit; controls go by
  the call's id (`Tell::Term`); dropping the call hangs up. The VT stays
  in the client for now, fed the output as bytes, and the main loop no
  longer polls ptys. Handing over to a new process releases each pty
  through the host first, taking in what it wrote meanwhile, then
  snapshots the screen.
- [ ] Later for terminals: the VT on the host, with screens sent as
  snapshots and diffs rather than bytes (needs the host to keep the VT for
  detached sessions anyway, and spares the wire a `cat` of a big log).
  `Event::Released` hands over a raw fd, which only means anything in
  the same process: re-exec becomes the server's own business in step 2.
- [x] 1.9 Sessions are the host's: it holds the session's lock and files
  (`Request::Session` with a `SessionCall`, answered by `session::run`),
  and the app keeps a `SessionLink`: the session's id, folder, and name,
  and which file has each document's unsaved text. A save is one call
  (`SessionCall::Save` with a `Snapshot`: the texts and screens that
  changed, under names the client gives them, then the state, then
  pruning). Periodic saves don't wait for it (`App::session_saving`),
  one at a time; saves before leaving do. Taking, renaming, and ending a
  session still wait: they're rare. Recovery copies stay the client's. Settings are classified
  in `config.rs`'s docs: `[ui]` and `[keys]` the client's, the rest the
  host's; one file and one reload until step 2.
- [x] 1.10 What stays the client's, by design: the clipboard (OSC 52,
  and copies to it); opening a file or link outside cue (`open`,
  `xdg-open`), which across a network will mean fetching the file
  first; the terminal's colors and capabilities, which the host will
  need for embedded terminals' color queries once the VT is on it; the
  client's settings file (reading it, Open Settings making it); and
  recovery copies.
- [x] Leftovers: `document::resolve` and `same_file`, the small checks,
  the blocking loads at startup and restore, and periodic session saves
  (see above).
- [ ] Left for later: Ctrl+hover and Ctrl+click on a path in a
  terminal look for the file on the client (`location::find_file`), on
  every hover. That wants either a batch ask per hover or to move with
  the VT to the host. `Workspace::new` canonicalizes the folders it's
  started with, on the client, before there's a host: step 3's
  handshake should have the host do it.

Step 1 is done but for those: the app touches the machine only through
the host, apart from what's the client's by design.

Notes from 1.1 and 1.2:

- Watching is now asynchronous, and a watch that starts after a file
  changes misses the change (FSEvents especially). Tests call
  `Link::settle` (test-only), which round-trips through the host thread.
- The client turns listed paths into picker items on the main thread; for
  200k files that may be noticeable. Measure before moving it.
- The walker reads `config::get()` for excludes. That's the host's config,
  which matches the config decision, but once the host is a separate
  process it needs its own config reload.
- `Workspace::add_root` canonicalizes on the client; it's only for
  starting now, and folders added later come from the host
  (`Workspace::add_folder`).
- Tests mostly expect the host to answer at once. `host::at_once()`
  (test-only, on by default, per thread) makes the tree, the file dialog,
  and file operations wait for their answers as they ask; their own tests
  turn it off to see what shows meanwhile. The host's threads also get
  the test's settings (`Tell::Config`, test-only), since in tests each
  thread has its own.
- `Symbol::kind` is a `&'static str`; it's interned as it's read (2.1).
- Git status is still scheduled by the client (`Git::poll` decides when to
  call). See settled questions.

### 2. Host in the session server

Move the host into the existing session server process, and the app into
the client. The client stops borrowing the tty's fds; the two talk over the
unix socket with a serialized protocol (serde + a compact format, versioned
by build id).

One host process per cue, as one server per cue today: it lives while a
client's connected, or while it's a session with programs running.

1. Wire format: every message serde, framed and written with postcard.
   `Link::local` talks to its host thread over a socketpair, so every
   test goes through serialization ("same path locally").
2. The host as a process of its own (`cue --host`), listening on the
   session's socket; `Link::connect`. One client at a time: a new one
   takes over, and the old one hears it moved.
3. Terminals' screens on the host: the native core allows one thread per
   process, so the host keeps a VT per terminal on a thread of its own,
   fed what the program writes, answering its queries while no client's
   there. A client that attaches gets each screen as a snapshot, then
   output as bytes (as `Terminal::adopt` takes them now). Not in the
   in-process host, whose process has the client's VTs.
4. The app in the client: the `cue` the shell ran shows the app in its own
   terminal, and connects to (or starts) the host. Detaching saves the
   session and exits; attaching restores it from the host, terminals
   included. No more handing stdio over, and no headless app.
5. Upgrades: a client of another build has the host re-exec into its
   binary, which adopts the ptys, their screens, and the session's lock.
6. Config: the host loads its own, and reloads when told to.
7. Edits as operations: the client streams each document's edits to the
   host as versioned operations, which it acks, and the host's copy is
   what sessions keep and what saving writes. Whole texts go only on
   opening and reloading. Step 4's editing while disconnected needs this.
8. Terminals' queries answered by the host only: with the client's colors
   pushed to it, the host's emulator answers every query and the client's
   none, so a query that comes as a client attaches or leaves gets one
   answer, and the two emulators can't drift (colors, a size the client
   hasn't sent yet). Until then, a client leaves in two steps
   (`App::leave`): the host stops sending its terminals' output
   (`Request::LetGo`, ending once the last is sent), the client answers
   what came, then leaves; from then on the hub's emulator answers.

Progress:

- [x] 2.1 Every message is serde (`Request`, `Reply`, `Tell`, `Notice`,
  and what they carry), framed by its length (a `u64`) and written with
  postcard. `Link::over` is the app's end of a stream: a thread writes
  what it's asked, another reads and routes replies; once the host's
  gone, the calls left end. `serve_stream` is the host's end. The local
  host is on the other end of a socketpair, so every test goes through
  the wire. Two types needed help: `io::ErrorKind` goes as one of the
  kinds the app tells apart (`error_kind`), and `Symbol` is read through
  an owned twin, its kind interned among the known ones. Tests' settings
  reach the local host's threads directly (`Link::set_config`), not as a
  message. Raw fds (`Start::Adopt`, `Event::Released`) still cross as
  numbers, meaningful only in the same process: 2.3 and 2.5 replace them.
- [x] 2.2 `cue --host` (`host::serve_listener`) serves the clients that
  connect to the socket it's handed (`CUE_HOST_FD`), one at a time: each
  says its build first (`Hello`), and one of another build is refused
  (2.5 makes that a re-exec). A client that connects takes over: the one
  before hears `Notice::Moved`, and its calls end. The host keeps its
  `State` (the symbols cache, the session and its lock) from one client to
  the next, but a connection's watches and terminals end with it, for now
  (2.3). It ends when the client it serves is gone, or the first doesn't
  come within 10s, and removes its socket. `Link::connect` connects;
  `Link::spawn` starts one on a socket in `/tmp/cue-UID/`, setsid, and
  connects. The app uses one with `CUE_HOST_PROCESS=1` (experimental),
  the in-process host otherwise. Under the flag: a restart can't adopt
  shells (`Terminal::release` gives up, so they start over), and
  terminals' `CUE_PID` is the host's, so a cue run in one isn't told it's
  nested; the host loads its settings once, at start.
- [x] 2.3 Terminals are the host's, not a connection's (`term_hub`): a
  thread owns each pty's controls and the call that hears of it, keyed by
  a `TermId` that the first reply says (`Event::Started`). Cancelling a
  terminal's call hangs up, as when the app closes it; a connection that
  ends (gone, or moved) only stops hearing of its terminals. A client
  attaches with `Start::Attach(id)`, taking the terminal from the call
  before. In a host process the hub keeps an emulator per terminal, fed
  what the program writes: one attaching gets `Event::Screen` (a full
  snapshot, `terminal::full_snapshot`) first, then its status and exit,
  then output; and while no call hears of it, the hub's emulator answers
  the program's queries (with the default colors: the client's aren't
  pushed yet). The in-process host has none (the core's the app's), and
  hangs up on its terminals when the app's gone. A host process goes on
  without a client while it keeps a session with terminals running,
  looking every second whether it still does. The app shows an
  `Event::Screen` in place of what it had (`Terminal::show_screen`, which
  `Terminal::adopt` now uses too), but doesn't attach yet: that's 2.4,
  which also needs a way to leave without hanging up (dropping the app
  cancels its terminals' calls).
- [x] 2.4 With `CUE_HOST_PROCESS=1`, the `cue` the shell ran is the app
  (`main::show`), in its own terminal: it picks a session as before
  (`client::target`), then starts a host process, or for a live session
  connects to the one running it, on the session's socket, which a host
  listens on while it keeps one. A client that connects takes over once
  the one before has saved and left: that one hears `Notice::Moved`, is
  served until it leaves (`Link::leave`, after which dropping its calls
  doesn't hang up), or 5s, and exits saying the session moved; what the
  new one asks meanwhile waits, so it reads the session as last saved.
  Restoring asks the host which terminals still run (`Request::Terminals`)
  and attaches to those (`TerminalState::host_id`, `Terminal::attach`); the
  rest start over below their screens, which the host's emulator is given
  too (`Start::Shell`'s `above`). Detaching, or the terminal going with
  programs running, leaves; quitting hangs up, waiting until the host's
  heard (`Link::closed`). `cue --end` asks the host (`end_session`), which
  tells its client `Notice::Ended`, hangs up, removes the session, and
  ends. A host doesn't take a client in one of its own terminals
  (`Hello::inside`). Once the host's gone, or the link's left, calls end
  unanswered, and the app says the host stopped (`Notice::Gone`). Also
  fixed: a session's unknown keys went over the wire as TOML values, which
  postcard can't read back; they go as text now. Not yet: Restart, and a
  client of another build attaching (2.5); without the flag, the old
  server path is unchanged.

### 3. ssh

`cue host:path` and `cue host` (a launcher: file browser plus the host's
sessions). scp's rule decides what's remote: a `:` before any `/`. Bootstrap
the server binary over ssh, then run the protocol over the ssh channel.

Large transfers to watch: the file list for the picker (paths relative to
roots, compressed, then kept up to date from disk change notices rather
than sent again), and search results. Chunked replies and the writers'
two queues (see Protocol shape) come in here, before anything's slow.
The host pushes git status (see below).

### 4. Resumable transport

Sequence numbers, acks, replay, reconnection without a new handshake; a
disconnected UI (buffers editable with edits queued, terminals visibly
stale, popups saying offline). Later, a UDP transport keyed over ssh, as
mosh and Eternal Terminal do.

Taking over must still leave one writer. Taking over ends the old
client's claim to resume: one that comes back after another took over
hears `Notice::Moved`, doesn't send the edits the host never acked, and
puts them in its recovery copies instead, which are for exactly that.

### 5. GUI renderer

## Settled questions

- Fuzzy file matching stays on the client, which has the whole list
  already: on the host it'd put a round trip in every keystroke in the
  picker. Step 3 makes the list cheap to send. Matching on the host,
  debounced, only if a list's too big to send.
- Git status: the host refreshes it on its own watch events (the
  worktree's, and `.git/index` and `HEAD`), settled, and pushes repos as
  snapshots. The client stops scheduling it.
- Search excerpts stay highlighted on the host. It parses the whole file,
  so lines in a block comment or a long string come out right, which the
  client can't do from an excerpt; and what goes on the wire is a capture
  index per range (`SyntaxColor`, a byte), not colors, so the client's
  theme still picks them.
