things i'd like

---

tier 0 - required for use at all

- file nav
  - single or multi-directory workspaces (partial: single folder works via `qedit FOLDER`; the workspace already holds a list of roots, but there's no way to add a second yet)
    - allows a folder to be added to the workspace that already exists as a child or a parent of an existing workspace folder
  - left-hand file tree navigator; expand and collapse (done)
  - search
    - should ignore .gitignore and .git by default
    - quick "go to file" ctrl+P (done: fuzzy matching with nucleo, skips .gitignored files and .git, recently opened files first so ctrl+P enter goes back to the previous file)
      - supports that thing where u can find, for instance `abracadabra.rs` by searching `abcr` bc those characters exist in order in the filename
    - workspace-wide search; probably use ripgrep for this. i'd make this a popup modal tbh. (done: ctrl+shift+F opens a large popup that searches as you type with ripgrep's engine, in-process. results are zed-style excerpts (each file's path, then matching lines with 2 lines of context), read-only. up/down step through matches, enter opens the file with the match selected. alt+C/W/R toggle case, whole word, regex. unsaved edits are searched. reopening keeps the query and selected match.)
- editor stuff
  - basic syntax highlighting (done: tree-sitter grammars for Rust, TOML, Markdown, JSON, Python, JavaScript, TypeScript/TSX, Go, C, C++, shell, YAML, HTML, CSS, and Zig, with their bundled highlight queries. it reparses incrementally as you type and highlights only the lines on screen. not yet: languages inside others, like code blocks in Markdown or scripts in HTML.)
    - figure out how themes should work. should qedit bundle themes or should it inherit the terminal's therme? (for now: syntax colors are the terminal's own 16 ANSI colors, so they follow its theme)
  - find and replace (done: ctrl+F opens a find bar floating at the top right of the editor, as in VS Code, that highlights every match as you type and selects the nearest one; the selection, if on one line, becomes the query. enter / shift+enter or ctrl+G / ctrl+shift+G step through matches, alt+C/W/R toggle case, whole word, regex, like workspace search. ctrl+H (cmd+alt+F), or clicking its ▸, adds a replace row: enter replaces the current match and moves to the next, alt+enter replaces all as one undo step, and `$1` / `${name}` expand regex groups. the last query carries over to other files.)
  - basic language awareness for features like autoclosing brackets, quotes
- command pallete (done): ctrl+K (cmd+K too, though most mac terminals eat it to clear the screen). typing `>` in ctrl+P switches to commands, like sublime/vscode. ctrl+shift+P is awkward to hit, and ctrl+space arrives as a NUL byte that also toggles macOS input sources.
  - actions such as file save, copy, paste ideally prefixed with like `editor:copy` (done: every shortcut is a named command like `editor:copy`, listed with its shortcut in `qedit --help` and in the palette)
  - philosophy: any action which has a keyboard shortcut should also be findable in the command pallete. and likewise you should be able to see the shortcut there.

---

tier 1 - ergonomics

- window organization
  - the space right of the file explorer is divided into "panels". you can vertically or horizontally split a panel in two (done: ctrl+\ splits right, ctrl+shift+\ splits down (legacy terminals send it as ctrl+\, so there it's in the palette), ctrl+W closes a panel and gives its room to its neighbor. ctrl+alt+arrows move between panels, and left of the leftmost is the file tree. the layout is a split tree. each panel has a header naming its file, dimmed when inactive; one status bar along the bottom shows the active panel's line/col, messages, and prompts.)
  - a panel can represent either a text editor or a terminal (keep this representationally flexible) (partial: editors only. open files are documents the app owns, and panels hold views of them, so a file stays open when no panel shows it. the same file can be open in two panels: shared text and undo, separate cursor and scroll.)
  - panels do not have tabs
  - the identity of a panel is not static. when you have a panel focused, and you say open another file through ctrl+P the viewed thing gets replaced (done: each panel remembers its cursor and scroll in every file it has shown)
  - panels should be resizable (done: drag the divider between side-by-side panels, or the header of the lower of two stacked ones)
  - figure out some way of enabling panel reorganization. this is nontrivial i think.
  - panels should maybe require an empty state like if you split a panel, then the twin should default to an empty state from which you can open a file or a terminal or whatever. (done: a new split is empty and lists the shortcuts for opening something. closing the last panel empties it.)
- tabs
  - tabs highest-level rather than be. they're really "layouts" in the strict sense. you only see one tab at a time. switching tabs switches the panel layouts
- embedded terminal (see above)
  - not really special in any layout sense, unlike some editors where the terminal has to go on the bottom or on the side or whatever
  - lingering questions:
    - should we allow the same terminal instance to appear multiple times in a layout?
    - how does one open a terminal?

the nesting i think if had to put it would be

```
application
  file explorer
    workspace folder 1
    workspace folder 2
  main area
    tab 1
      vertically stacked panels
        panel: file
        panel: editor
      panel: terminal
    tab 2
      panel: terminal
  status bar (bottom)
```

---

tier 2 - functionality (think about this later)

- LSP support
- settings menus
- autocomplete
- diff views, that kind of thing

^^^^

claude's feedback on the above plan, all of which i agree on

The plan holds together, and the architecture is sound. Tabs as layouts is the tmux-windows / Vim-tabpages model, which is a proven design, and "panels have no tabs, content gets replaced" is a clean, opinionated choice. Here's where I'd push, and my take on your open questions.

**Separate sessions from panels.** This one decision answers both of your terminal questions. Keep a flat list of *sessions* (open buffers, running terminals) that exist independently of the layout, and make panels just views that point at a session. Then:

- Since panels replace their content, pressing ctrl+P in a panel showing a terminal shouldn't kill the shell. The terminal goes back to the session list, and something like `terminal:switch` in the palette brings it back. Without this, the no-tabs rule makes terminals fragile.
- Same buffer in two panels: yes, allow it. Viewing two spots in one file side by side is useful, and it's cheap because the rope is shared and each view has its own scroll and cursor.
- Same terminal in two panels: I'd say no. A PTY has exactly one size, so two views of different dimensions conflict. tmux handles this by shrinking to the smallest client, and it's ugly. The simple rule is that a terminal lives in at most one panel, and opening it elsewhere moves it there.
- Opening a terminal: from the empty-state panel, from `terminal:new` in the palette, and with a split-with-terminal shortcut. The empty state you described is the natural home for it.

**Panel reorganization** is easier than it looks if the layout is a split tree: internal nodes are H/V splits with ratios, leaves are panels. Moving a panel means removing a leaf and inserting it next to a target leaf in a direction. The genuinely fiddly part is the drag UI: grab a panel's header, then show drop zones on the target (four edges to split, center to swap), like VS Code or a tiling WM. That's a natural mouse-first interaction.

**Theming.** Default to the terminal's 16-color ANSI palette for syntax highlighting. It automatically matches whatever Ghostty or kitty theme you use, and it's surprisingly workable if you map highlight groups semantically (keywords, strings, comments, and so on). Offer truecolor themes as an opt-in. You can also query the terminal's background with OSC 11 to pick light or dark variants.

**Fuzzy finding.** What you described is subsequence matching, and the quality comes from the scoring: bonuses for matches at word boundaries, camelCase humps, path separators, and consecutive runs. Use `nucleo`, the matcher Helix built. It's in Rust, fast, and already does fzf-quality scoring.

**Keybinding gotchas:**

- In the legacy terminal protocol, lots of combinations are indistinguishable: ctrl+shift+P vs ctrl+P, ctrl+I vs Tab, ctrl+M vs Enter. Enable the kitty keyboard protocol, which Ghostty, kitty, WezTerm, and recent iTerm2 support, and design the default bindings to degrade gracefully without it. This matters directly for your "every action shows its shortcut" rule, since a displayed shortcut the terminal can't deliver is a lie.

**Overlapping workspace folders.** Allowing a child of an existing folder means canonicalizing paths and deduplicating everywhere: file-watcher registration, ripgrep search (otherwise you get every hit twice), and ctrl+P results. The simplest approach is to show both roots in the tree but have search and indexing operate on the set of unique real paths.