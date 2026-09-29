# Vendored code

## `opentui-native/`

The Zig core of [OpenTUI](https://github.com/anomalyco/opentui) (`packages/native`
upstream), vendored as a squashed `git subtree` of a `packages/native`-only
split. MIT licensed; see `OPENTUI-LICENSE`.

Current upstream commit: `4b1474e009bf78365a7a4866e83a9e522533ef42`
(the squash commit message names the split commit, not this one).

To update, in a clone of upstream:

```sh
git fetch origin
git subtree split --prefix=packages/native origin/main -b native-only
```

Then from this repository:

```sh
git subtree pull --prefix=vendor/opentui-native /path/to/opentui native-only --squash
```

and update the commit above. The split is deterministic, so repeated splits
share history and pulls merge without conflicts.

### Local changes

Not upstreamed. Check each still applies (or was fixed upstream) after a pull.

- `src/lib.zig`: `createEditorView` allocates the view from `globalAllocator`
  instead of `globalArena`. The view's layout arenas sat on the never-freeing
  global arena, so every relayout (one per edit) leaked the whole layout,
  about 700 bytes per line of the document.
- `src/text-buffer-view.zig`: `VirtualLineOutput.reserveLines` presizes the
  layout arrays to the line count before each relayout, instead of growing
  them by doubling inside an arena.
- `src/rope.zig`: `rebuildMarkerCache` looks up each marker list once and
  presizes it from the root's marker counts, instead of a hash lookup per
  marker.
- `src/text-buffer.zig`, `src/lib.zig`: `editBufferBytesToCursors` converts
  byte offsets into the text to cursor positions in one pass, for placing
  regex matches (tabs and wide characters make columns differ from bytes).
  `editBufferGetContentEpoch` exposes the text buffer's change counter, and
  `textBufferStartHighlightsTransaction`/`End...` batch highlight updates.
- `src/text-buffer.zig`, `src/syntax-style.zig`: where highlights overlap,
  `rebuildLineSpans` layers their styles by priority (colors from higher
  priorities replace lower ones, attributes are OR'd) instead of using only
  the highest priority one's, so a find match's background keeps the syntax
  color of the text under it. `SyntaxStyle.layeredStyleId` makes the layered
  styles and redoes them when a layer is redefined; `getStyleCount` leaves
  them out. Without a syntax style set, the highest priority still wins.
  `setSyntaxStyle` rebuilds the spans of lines with overlapping highlights.
- `src/editor-view.zig`: `setWrapMode` scrolls the viewport back to the left
  when wrapping, since `ensureCursorVisible` never moves it sideways while
  wrapped, and keeps the text at the top of the viewport there, since
  rewrapping renumbers the rows. `makeCursorVisible`, while wrapped, puts the
  cursor on the target row itself instead of at its logical column in the
  row's line, which in a line wrapped over several rows could be out of view
  and scroll the viewport straight back.
  `getVisualEOL` (End) treats a line wider than the viewport as one overwide
  grapheme only while wrapping; unwrapped, End did nothing on such lines.
- `src/embedded-terminal/`, `src/lib.zig`: for cue's terminals.
  `setHostPalette` (`embeddedTerminalSetHostPalette`) composes default and
  palette colors as the host terminal's own (SGR 39/49 and indexed colors)
  instead of RGB from Ghostty's palette, so shells follow the host's theme;
  inverse video and the selection become the inverse attribute there. Palette
  slots a program redefined (OSC 4) stay RGB. Composing skips cells outside
  the buffer's scissor rect (`set` ignores it). `encodeKey` treats Alt as Alt
  on macOS, since the host terminal already decided what Option is. New
  exports: `embeddedTerminalScrollToBottom`, `embeddedTerminalIsAlternateScreen`,
  and `embeddedTerminalGetTitle`.
