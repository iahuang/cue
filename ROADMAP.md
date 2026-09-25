things i'd like

---

tier 0 - required for use at all

- file nav
  - single or multi-directory workspaces
    - allows a folder to be added to the workspace that already exists as a child or a parent of an existing workspace folder
  - left-hand file tree navigator; expand and collapse
  - search
    - should ignore .gitignore and .git by default
    - quick "go to file" ctrl+P
      - supports that thing where u can find, for instance `abracadabra.rs` by searching `abcr` bc those characters exist in order in the filename
    - workspace-wide search; probably use ripgrep for this. i'd make this a popup modal tbh.
- editor stuff
  - basic syntax highlighting
    - figure out how themes should work. should qedit bundle themes or should it inherit the terminal's therme?
  - find and replace
  - basic language awareness for features like autoclosing brackets, quotes
- command pallete (come up with a sensible shortcut for this. claude says cmd+space is taken)
  - actions such as file save, copy, paste ideally prefixed with like `editor:copy`
  - philosophy: any action which has a keyboard shortcut should also be findable in the command pallete. and likewise you should be able to see the shortcut there.

---

tier 1 - ergonomics

- window organization
  - the space right of the file explorer is divided into "panels". you can vertically or horizontally split a panel in two
  - a panel can represent either a text editor or a terminal (keep this representationally flexible)
  - panels do not have tabs
  - the identity of a panel is not static. when you have a panel focused, and you say open another file through ctrl+P the viewed thing gets replaced
  - panels should be resizable
  - figure out some way of enabling panel reorganization. this is nontrivial i think.
  - panels should maybe require an empty state like if you split a panel, then the twin should default to an empty state from which you can open a file or a terminal or whatever.
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