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

# feat

- file ops
- rename, move, copy, delete in file tree
- new file (create)
- new file (unsaved buffer)
- file picker for open, save

- dim gitignored files, folders in the file tree and also exclude them from the search + file picker
- support for nerdfonts

- spaces versus tabs; ideally infer based on existing file content

- file watcher to reflect updates to the filetree as well as to currently open buffers. follow vscode/zed convention--don't update the file if there r unsaved changes; instead enter "unsaved with conficts" mode where the next save will prompt you to either overwrite or defer to disk.

- select + tab to indent, shift-tab to deindent

# long-term

- truecolor theming support
