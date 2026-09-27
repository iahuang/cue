# bugs:high_prio

# bugs:low_prio

- resizing creates weird wrapping artifacts - same issue in opencode; almost certainly opentui issue. probably just reflow frequency too low; claude code lacks this issue, so solvable in theory

# qol

- display number of index/n_matches in within-file and workspace search
- syntax highlighting in workspace search (possibly nontrivial)

- indentation guides in editor
- that thing where when scrolling thru the file explorer, the directory descendants stay sticked at the top so you always know where u are
- click below last line should put ur cursor at the end of the buffer
- header controls for panels; close button at least, maybe a meatball menu too?

- allow scrolling past the end of the file

- make sure currently focused file is automatically revealed in the file tree

# feat

- context menus

- file ops
  - rename, move, copy, delete in file tree
  - new file (create)
  - new file (unsaved buffer)
  - file picker dialog for open, save emulating the native OS one

- dim gitignored files, folders in the file tree and also exclude them from the search + file picker
- support for nerdfonts

- spaces versus tabs; ideally infer based on existing file content

- file watcher to reflect updates to the filetree as well as to currently open buffers. follow vscode/zed convention--don't update the file if there r unsaved changes; instead enter "unsaved with conficts" mode where the next save will prompt you to either overwrite or defer to disk.

- select + tab to indent, shift-tab to deindent

- nerdfont support

- panel history nav

- save workspace layouts
	- maybe do this in like a "sessions" format, similar to claude code and others?
	- to avoid bloat sessions should probably be opt-in
	- would be nice tbh even to have the option to leave sessions in the background, i.e. their terminals stay alive even when the main process is killed. would be really useful in ssh contexts (avoid having to wrap qedit in `tmux` or `screen`. nontrivial UX question though.

# bespoke features (think about these carefully)

- clipboard history
- nvim style navigation anchors
- fast keyboard bindings for panel navigation

# larger features

- truecolor theming support
	- ship a few well known themes
	- default to a theme based off the terminal's current theme colors. extra granularity (e.g. dimmed text, alternate background colors maybe automatically build variants based off of reported bg/fg colors gathered from terminal reporting)

- git integrations
	- show staged, unstaged changes
	- allow commits to be performed
	- diff view
	- git log
	- see edit history for any given file
	- git worktree support

- image viewer thru kitty graphics protocol

- proper config; rebinding, settings

- LSP support
	- "go to definition"
	- inline error reporting
	- just the basics, no debugger, breakpoints, etc.
	- integration w/ autoformatters