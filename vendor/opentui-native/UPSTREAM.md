# Upstream

This is a fork of `packages/native` from [OpenTUI](https://github.com/sst/opentui), not a mirror. Upstream fixes are ported by hand, not merged.

- Base: `4b1474e0` (2026-09-23, between v0.5.12 and v0.5.13), imported as a git subtree.
- Reviewed through: `d31b2909` (2026-10-14).

Upstream moved its render tree into Zig in `c1bb64ba` (#1479), with a new C ABI (`context.zig`, `session.zig`, `include/opentui.h`). We don't take that, so later fixes rarely apply cleanly. Read the commit, then port the fix and its test against this tree.

To review: `git log --no-merges 'REVIEWED..main' -- packages/native` in an upstream clone, then move "Reviewed through" forward.

## Ported

- `15eb073f` reply to device attribute queries in the embedded terminal
- `e7a62870` close an attributed run at the end of its row (not in compact output, which diffs styles)
- `646f2555` delete Kitty placements by id so iTerm2 keeps the image
- `8742fa72` release replaced text buffer ropes
- `07225659` snap vertical cursor moves to the start of a wide character, with the scroll-margin clamp for short viewports from the same file
- `ae3193f1` measure merged editor chunks instead of adding their widths
- `055e2093` redraw the embedded terminal when default colors change (test rewritten for our default colors)

## Skipped

- `c1bb64ba` and everything on its ABI: `78ca1edb`, `3ab0b954`, `942bc41c`, `d31b2909`
- Already done here: `f0d7d1d3` (Alt on macOS), `17abb968` (embedded terminal default colors)
- Split footer and scrollback, unused: `af4b9592`, `f813c7a1`, `7221adcb`
- Unused features: `e4eb894f` (placeholder), `0450fadc` (tmux probes), `786d423e` (auto remote mode), `6fe23b83`, `6f26ed24` (OSC 8 detection)
- Perf or refactors that conflict with our big-file work: `ca25b668`, `e535b30e`, `8df82955`, `ff2b969b`, `19869ee1`, `205a48b7`, `ddb7fbd6`
- `d60641be` combining mark that starts a chunk: needs upstream's rewritten draw loop; rare
- `acf13584`, `78976e0e` highlight leftovers: cue replaces highlights after each reparse, so these only show until then
- `542275a3` control characters as caret notation: needs the new ABI. Here instead, controls take one cell and output writes their Control Picture (U+FFFD for C1), so they never reach the terminal (`utf8.isShownControl`, `utf8.shownCodepoint`)
