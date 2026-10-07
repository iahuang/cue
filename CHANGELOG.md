# Changelog

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
