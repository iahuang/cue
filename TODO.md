# bugs:high_prio

# bugs:low_prio

- resizing creates weird wrapping artifacts - same issue in opencode; almost certainly opentui issue. probably just reflow frequency too low; claude code lacks this issue, so solvable in theory

# qol

- indentation guides in editor
- configurable indent from auto guessing indent

# feat

- maybe: make proper move rather than unix style unified move-rename verb
- dim gitignored files, folders in the file tree and also exclude them from the search + file picker

- save workspace layouts
	- maybe do this in like a "sessions" format, similar to claude code and others?
	- to avoid bloat sessions should probably be opt-in
	- would be nice tbh even to have the option to leave sessions in the background, i.e. their terminals stay alive even when the main process is killed. would be really useful in ssh contexts (avoid having to wrap cue in `tmux` or `screen`. nontrivial UX question though.

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