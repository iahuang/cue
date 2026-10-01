a post-agentic terminal code editor

```
curl -fsSL https://s3.ianhuang.dev/cue/install.sh | bash
```

**guiding principles**

- people don't write code anymore: navigation first; editing second.
- native mouse support; muscle memory optional.
- single binary; works out of the box.
- minimalistic and functional.

**features**

- multi panel editing
- `ripgrep` search
- go to symbol, in the file (`ctrl+r`) or the workspace (`ctrl+shift+r`), from tree-sitter; go to line (`ctrl+l`)
- terminal buffers; `ctrl+click` opens a path (`src/main.rs:12:5`) or URL printed in one
- find in a terminal's output and scrollback (`ctrl+shift+f` or `cmd+f`), with the editor's case, word, and regex options
- `cue src/main.rs:12:5` opens a file at a line and column
- unsaved changes survive crashes and dropped ssh sessions, and are offered back next time
- image viewer (kitty graphics protocol)
- settings and key bindings in `~/.config/cue/config.toml`; Open Settings in the command palette (`ctrl+k`) makes one with every setting and its default