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
