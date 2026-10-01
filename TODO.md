# bugs:high_prio

# bugs:low_prio

- resizing creates weird wrapping artifacts - same issue in opencode; almost certainly opentui issue. probably just reflow frequency too low; claude code lacks this issue, so solvable in theory

# qol

- indentation guides in editor
- configurable indent from auto guessing indent
- toggle comment (comment token from tree-sitter / language table)
- select next occurrence (ctrl+D style) + highlight other occurrences of the word under the cursor
- bracket matching: highlight the pair, jump to matching bracket
- save all
- `.editorconfig` support (final newline, trailing whitespace, indent)
- CLI: open multiple files (`cue a.rs b.rs` currently errors), read stdin (`git diff | cue -`)

# feat

- maybe: make proper move rather than unix style unified move-rename verb

- nested `$EDITOR` opens in the host instance
	- `cue file` (or an agent's "open in $EDITOR", e.g. claude code ctrl+G) from inside a cue terminal should open a panel in the parent cue via an env var socket, not cue-in-cue
	- `--wait` so it works for git commit messages

- "what changed since i last looked": mark lines that changed on a watcher reload since the panel last showed the file. distinct from git diff; shows what the agent just did, not what's uncommitted

- terminal links: underline paths and URLs under the mouse with ctrl held (needs motion events)

- terminal scrollback search (ctrl+F in a terminal panel)
	- maybe: send selection to terminal

- save workspace layouts
	- maybe do this in like a "sessions" format, similar to claude code and others?
	- to avoid bloat sessions should probably be opt-in
	- would be nice tbh even to have the option to leave sessions in the background, i.e. their terminals stay alive even when the main process is killed. would be really useful in ssh contexts (avoid having to wrap cue in `tmux` or `screen`. nontrivial UX question though.

# bespoke features (think about these carefully)

- clipboard history
- nvim style navigation anchors
- fast keyboard bindings for panel navigation

# larger features

- tree-sitter navigation (no LSP needed)
	- more outlines: languages without a tags query yet (Kotlin, Haskell, Dart, TOML/YAML/JSON keys, ...)
	- expand / shrink selection by syntax node
	- sticky scroll: pin the enclosing fn/impl header at the top of the viewport

- robustness
	- non-UTF-8 files: currently refused outright; latin-1 fallback or read-only lossy view
	- binary / huge files: guard or warn before opening a 2GB log or a binary

- git integrations
	- show staged, unstaged changes
	- allow commits to be performed
	- diff view
	- git log
	- see edit history for any given file
	- git worktree support

- proper config; rebinding, settings

- LSP support
	- "go to definition"
	- inline error reporting
	- just the basics, no debugger, breakpoints, etc.
	- integration w/ autoformatters