# Changelog

## Unreleased

- Make workspace search's matches easier to see in syntax-colored lines, in find's colors (`503e6d1`)
- Recently viewed (Ctrl+Shift+-) puts what it goes to first, as Cmd+Tab does, so taps go back and forth between two files (`9d6ba9c`)
- Checkpoints: `+` at the top of the changes view takes one of the workspace's folders, named if you like, and the changes, diffs, and gutter then show what changed since, until `✕`; clicking the row picks an older one. They're kept in cue's state folder, not the repository, the last 20 per folder, and work in folders that aren't repositories (`HASH`)
- Each repository in the changes view is one row, with its branch and how far it is from the branch it tracks; clicking it pops up Commit, Log, Switch Branch, and syncing beside it, in place of the buttons over it. Show Git Log, Switch Branch…, and Sync with Remote are in the palette too (`HASH`)

## v0.4.6 — 2026-10-18

- Tab switches the sidebar between the files and the changes while it has focus (`a7250c5`)
- Fast scrolling, typing, and find in big files (a keystroke in a 100k-line file went from about 130 ms to under 1 ms); find highlights now show only in the panel with the find bar, and only near the view (`d44f6a8`)
- Use about half the memory for files with many lines (a 100 MB log with 1M lines went from about 1 GB to 685 MB) (`339ec05`)
- Highlight JSON Lines files (`.jsonl`, `.ndjson`) (`d703ec3`)
- Port fixes from upstream OpenTUI: fish no longer stalls at startup in cue's terminals, bold or underlined text at a row's right edge no longer leaks into the next row, Kitty images stay put in iTerm2 when they move, reloading a file no longer keeps the old text in memory, Up and Down no longer stop inside a tab or wide character, emoji typed in parts take their right width, and terminals redraw when a program changes their colors (`81719f1`)
- Show control characters in files as one-cell symbols like `␛` and `␇`, which the cursor stops on, instead of sending them to the terminal, where a log's color codes recolored the screen and text after some controls went missing (`12fa7a5`)
- Autocomplete as you type (or with Ctrl+Space): words of the file and other open files, the language's keywords and common builtins, LaTeX commands with their `\`, and names the workspace defines; Tab takes one, and `editor.autocomplete = false` keeps it to Ctrl+Space (`aeb4004`)
- Clean Up Idle Terminals command: closes the terminals no panel shows that aren't running anything (`0a0a773`)

## v0.4.5 — 2026-10-16

- Scratchpad: Ctrl+; floats a panel of its own over every tab, which takes what you open while it's out, hides with Esc or a click outside it, resizes by its frame or in layout mode, and keeps its contents in sessions (`fc33201`)
- Pop a panel's content with Ctrl+Shift+W instead of Ctrl+0 (and no longer Cmd+0) (`3eb9524`)
- Move tabs with `{` and `}`, close them with `W`, and go to one with `1` to `9` in layout mode, with undo for moves (`83800de`)

## v0.4.4 — 2026-10-16

- Highlight the cursor's line in the focused editor while nothing is selected, and make faint selections easier to see (`8ac3695`)
- Rename the editor menu's Copy Reference to Copy Line Reference, and Toggle Line Comment to Toggle Comment (`2c29d17`)
- `cue` no longer resumes the folder's session on its own (use `cue --resume`), so `--fresh` is gone, along with the startup note about sessions running in the background (`de4b161`)
- Filter which files workspace search looks in, with comma-separated globs like `*.tsx, !node_modules`, which can also pick workspace folders by name (Tab switches to the filter) (`07a8941`)
- Go back and forward with Ctrl+- and Ctrl+= (or the mouse's buttons) through every panel and tab, focusing the panel again, with focus moving between panels counting too; the header's buttons still go through the panel's own history (`03f9bdc`)
- Zoom a panel to fill its tab with Ctrl+Shift+Enter or from its header's menu, framed, with the other panels hidden until it's zoomed out; moving to another panel shows that one instead (`e997495`)
- Arrange panels from the keyboard in layout mode (Ctrl+J): move between panels with the arrows or `hjkl`, trade places with Shift, resize with Alt, even out, flip, split, zoom, and cut, copy, and paste panels between tabs, with undo (`9cb0629`)

## v0.4.3 — 2026-10-14

- Mark terminals whose bell rings or that send a notification (OSC 9 or 777), with a status bar badge that goes to them (`a3bff59`)
- Fix laggy PDF zooming on high-resolution screens, and pages going blank when zoomed far in (`37fc2c7`)
- Copy Reference in the editor's context menu, which copies the selection's or cursor line's location, like `src/main.rs:257-265` (`818eaa3`)
- Find in PDFs with Ctrl+F, highlighting matches on the pages (`78ea5c0`)
- Smaller debug builds, keeping only line tables (`8c6f7ee`)
- Fix stutter zooming out of PDFs on high-resolution screens (`7541939`)

## v0.4.2 — 2026-10-11

- Embedded PDF viewer (`c3deea1`)
- Build releases with Rust 1.92 (`5f06f75`)

## v0.4.1 — 2026-10-10

- Use temporary binary when launched from NFS to avoid SIGBUS on remote instances (`c991ebf`)
- Find in reader mode (`691f95b`)
- New themes: Lichen and Slate (`fa2bbe0`)
- Automatically scroll while drag selecting offscreen (`691f95b`)

## v0.4.0 — 2026-10-09

- Add custom theme "Tidepool" (`58045b1`)
- Change a file's indentation from the status bar, offering to convert its lines between tabs and spaces, and remember the choice for the file in the session (`a08ab49`)
- Toggle word wrap, or go to a line, by clicking it in the status bar (`a08ab49`)
- Tree-sitter Go to Definition (`b22c2b3`)
- In-editor context menu (`fb84db0`)

## v0.3.13 — 2026-10-08

- Fix a crash diffing a file with a lone carriage return in it (`4aecc87`)
- Show messages as toasts over the bottom right, not in the status bar (`6f30bf3`)
- Ask for a tab's, terminal's, or session's new name, or a new branch's, in a box at the top rather than in the status bar (`6f30bf3`)
- Drop messages that only say what's already on screen, such as "Copied 12 characters." or "Nothing to undo." (`6f30bf3`)
- Start new terminals in the workspace folder with the tree's selection even when the tree isn't focused (`bd63178`)
- Open a folder to an empty panel rather than an untitled file (`7e29bb0`)
- Write crash reports to `~/.local/state/cue/crashes`, including for sessions in the background, and say where they are on the next start (`1f0ff06`)
- Show messages in the corner `ui.toast_position` says, and say "Mark set." again (`450dda1`)

## v0.3.12 — 2026-10-08

- Name sessions with Rename Session (`a07c1e9`)
- Give the enclosing terminal window a relevant title (`ad31b9e`)
- Add shortcut for clear terminal (`c335616`)
- Add a context menu to panel headers (`721ee30`)
- Open the file picker by clicking the file's name in a panel's header (`721ee30`)
- Dynamic git fetch/pull/push button (`0e7659f`)
- Add a context menu to the git log: copy a commit's hash or message, check it out, branch from it, or revert it (`2a7a91f`)

## v0.3.11 — 2026-10-07

- Refresh stashes in the commit view when one is replaced by another (`943d7bd`)
- Switch to the right branch when a tag shares its name (`6b22062`)
- Keep ignored files, and older stashes, when switching branches (`d7dc9c9`)
- Prevent applying or popping a stash while a commit is in progress (`b94691b`)
- Allow committing a merge with no staged changes (`58632d8`)
- Turn off amending as soon as a branch switch finishes (`0078654`)
- Save and stash unsaved edits when there are no other changes (`1b00f03`)
- Bring changes along unstaged when switching branches if what was staged no longer applies (`6379408`)
- Refuse to switch branches over new files the branch also has, rather than stashing them (`d984544`)
- Bring changes along when switching branches with a translated git (`ca8b759`)
- Offer only a remote's branch, not a new branch of the same name, when its name is typed in the branch picker (`2469d3f`)
- List remotes' branches correctly when a remote's name has a `/` in it (`f7310af`)

## v0.3.10 — 2026-10-07

- Indent new lines and closing brackets as you type (`334f3de`)
- Add a commit view for staging and committing changes (`ac5f5af`)
- Limit hover highlights in the Git sidebar views to rows and buttons that can be clicked (`ac5f5af`)
- Add stashing, and applying, popping, and dropping stashes, to the commit view (`f22b3c2`)
- Switch and create branches from the branch name in the Git sidebar views, bringing uncommitted changes along (`fe137dd`)
- Keep what's staged when switching branches, and wait for a commit in progress to finish first (`926b0fd`)
- Turn off amending when switching branches (`4f8a21d`)

## v0.3.9 — 2026-10-06

- Use VS Code word separators for double-click selection in the editor (`fd6db5a`)
- Swap keybindings for set mark and clear mark (`4d0112b`)
- Select between caret positions when drag selecting in the editor (`86cb2ec`)

## v0.3.8 — 2026-10-06

- Treat buffers edited back to their saved text as unmodified (`fd4cdda`)
- Add unsaved file filtering and a corresponding command palette action (`fac771f`)

## v0.3.7 — 2026-10-06

- Add configurable lines per scroll, defaulting to 2 (`0c2aa9c`)
- Add Save All, line comment toggling, and configurable indentation guides (`e78cf8f`)

## v0.3.6 — 2026-10-05

- Add mouse hover states (`60e36de`)
- Add Quick Look previews in the sidebar with Space or Alt+click (`98f2cdb`)

## v0.3.5 — 2026-10-05

- Track cursor position as part of panel history (`8416862`)
- Add a persistent mark to jump back to (`1d93a19`)
- Toggle terminal key passthrough with Ctrl+Space (`eb15d08`)
- Refine interface wording and help text (`aef78cb`)
- Allow marking and jumping back to terminals (`30e664d`)

## v0.3.4 — 2026-10-05

- Add Git log and commit diffs (`f752b80`)
- Add Git gutter markers and compact folder trees (`f752b80`)

## v0.3.3 — 2026-10-04

- Add text selection in pickers and file dialogs (`0ba34f7`)
- Add text selection in workspace search and find fields (`c281ecc`)
- Add Git changes viewer and status indicators (`c05c40c`)
- Add live diffs for changed files (`c8f1fa6`)

## v0.3.2 — 2026-10-03

- Simplify the status bar (`94e313c`)
- Add Markdown reader mode (`2f5a84f`)
- Add side-by-side Markdown preview with synchronized scrolling (`2f5a84f`)
- Render typeset math in Markdown reader mode (`b086914`)
- Make scrolling independent of cursor position (`e566303`)

## v0.3.1 — 2026-10-02

- Refine interface wording, help, and errors (`c72ea65`)
- Fix terminal double-click selection (`78918f5`)
- Add searchable full-screen session picker (`5e31d57`)
- Add session badge with detach and end actions (`5e31d57`)

## v0.3.0 — 2026-10-02

- Add experimental frame pacing, compact output, and network simulation (`cb23d64`)
- Add Pop to close panel content and go back; sort themes by appearance (`3580be6`)
- Add resumable sessions with saved layouts and unsaved edits (`063de78`)
- Keep terminal programs running in detached sessions (`063de78`)

## v0.2.7 — 2026-10-01

- Improve terminal labels, preview titles, and status text (`43081ab`)

## v0.2.6 — 2026-10-01

- Change panel-history forward shortcut to Ctrl+= (`a54350c`)
- Add terminal previews in the picker (`2e3eaf1`)

## v0.2.5 — 2026-10-01

- Add terminal find; fix wrapped-file match scrolling and syntax colors (`03411ce`)
- Add TOML settings and customizable keybindings (`f9dda25`)
- Add adaptive terminal colors, built-in themes, and live theme selection (`3c750fe`)
- Simplify settings and theme labels; use a nonblinking line cursor (`6a72edd`)
- Trim the README (`8b384f5`)

## v0.2.4 — 2026-09-30

- Add Markdown inline and injected-language highlighting (`524cae9`)
- Expand language support, including LaTeX and Mermaid (`a32b03c`)
- Run Cue shortcuts from terminals with Ctrl+Shift (`5bd9f1e`)
- Add file/workspace symbol navigation and Go to Line (`ca817f3`)
- Open terminal links and file positions with Ctrl+click or the CLI (`ca817f3`)
- Recover unsaved edits after crashes or dropped SSH sessions (`ca817f3`)

## v0.2.3 — 2026-09-29

- Add multi-folder workspaces (`e8707f0`)

## v0.2.2 — 2026-09-29

- Add image viewer with zoom and pan (`0e8815b`)
- Fix Cargo lockfile release version (`a181edf`)

## v0.2.1 — 2026-09-29

- Add indentation detection and indent/outdent commands (`5e00d07`)
- Add panel history and header navigation buttons (`5e00d07`)
- Add sticky folders in the file tree and editing improvements (`5e00d07`)

## v0.2.0 — 2026-09-28

- Add button-based confirmation dialogs (`71696c7`)
- Update release builds and installer tooling (`71696c7`)
- Watch files, reload external changes, and detect save conflicts (`3cac80d`)
- Make Nerd Font icons optional (`8db600c`)

## v0.1.0 — 2026-09-28

- Initialize the repository and vendor OpenTUI with license notes (`905323a`, `b0971c6`, `c39d8f2`)
- Add Rust bindings and safe wrappers for OpenTUI (`4f236d3`)
- Add the original qedit editor with navigation and atomic saves (`05e2be0`)
- Fix editor layout memory leak (`3a084e0`)
- Improve large-file editing performance (`63045ea`)
- Add grouped undo/redo, selection, clipboard, and word/line movement (`1113b1f`)
- Link OpenTUI statically for standalone macOS and Linux binaries (`13c56ca`)
- Add line numbers and current-line highlighting (`cca7d3a`)
- Add README, roadmap, and development instructions (`eafc856`, `677d940`)
- Add shared editor views and clipped drawing to OpenTUI (`901e748`)
- Add file tree, workspace navigation, open-file state, and named commands (`6876c98`)
- Add fuzzy file and command pickers (`9a00290`)
- Add incremental workspace search with ripgrep (`a6364de`)
- Remove the word-wrap shortcut (`9bec127`)
- Add in-file find and replace (`7f1bdcd`)
- Add tree-sitter syntax highlighting (`7642913`)
- Fix wrapping and scrolling; improve query-field editing (`37f3519`)
- Fix horizontal scrolling and dotfile visibility (`515b5b2`)
- Add split panels, shared documents, and draggable dividers (`6ff8ba6`)
- Fix panel dragging and add split/swap drops (`bf0c143`)
- Add embedded terminals (`ef11a8c`)
- Enable word wrap by default (`49b96da`)
- Add terminal clear, close, and rename commands (`e56705e`)
- Add file dialogs, untitled buffers, and safe closing (`54f8299`)
- Simplify empty-panel suggestions and rename New File command (`d921c58`)
- Honor ignore files in search/pickers and dim ignored tree entries (`4aea607`)
- Add syntax highlighting in workspace search results (`2cb3ec7`)
- Add context menus and file-tree operations (`2ff4c90`)
- Revise terminal shortcuts and confirmation handling (`6fdef4a`)
- Add tabs with independent panel layouts (`224d08c`)
- Rename qedit to cue (`166f6b1`)
- Add release workflows and installation scripts (`f62c72c`)
