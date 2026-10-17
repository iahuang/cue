const std = @import("std");
const edit_buffer = @import("../edit-buffer.zig");
const text_buffer_view = @import("../text-buffer-view.zig");
const iter_mod = @import("../text-buffer-iterators.zig");
const text_buffer = @import("../text-buffer.zig");
const gp = @import("../grapheme.zig");
const link = @import("../link.zig");

const EditBuffer = edit_buffer.EditBuffer;
const TextBufferView = text_buffer_view.TextBufferView;
const WrapMode = text_buffer_view.WrapMode;

const snippets = [_][]const u8{
    "a",              " ",          "word ",     "\n",           "\n\n",
    "two words",      "\t",         "x\ty",      "世界",           "日本語のテキスト",
    "emoji 👋🏽 here", "long-unbroken-token-that-wraps", "line\nbreak", "end\n", "  indented\n  more",
    "é",              "a\u{301}",  "",          "\nnew line at start", "trailing ",
};

/// Bytes of a virtual line's chunks, joined.
fn vlineText(view: *TextBufferView, vline: text_buffer_view.VirtualLine, out: *std.ArrayListUnmanaged(u8)) !void {
    out.clearRetainingCapacity();
    for (vline.chunks.items) |vchunk| {
        const bytes = vchunk.chunk.getBytes(view.text_buffer.memRegistry());
        try out.appendSlice(std.testing.allocator, bytes[vchunk.byte_start_in_chunk..][0..vchunk.byte_len]);
    }
}

fn expectSameLayout(incremental: *TextBufferView, full: *TextBufferView) !void {
    const a = incremental.getVirtualLines();
    const b = full.getVirtualLines();
    try std.testing.expectEqual(b.len, a.len);
    var a_text: std.ArrayListUnmanaged(u8) = .empty;
    defer a_text.deinit(std.testing.allocator);
    var b_text: std.ArrayListUnmanaged(u8) = .empty;
    defer b_text.deinit(std.testing.allocator);
    for (a, b) |va, vb| {
        try std.testing.expectEqual(vb.width_cols, va.width_cols);
        try std.testing.expectEqual(vb.document_cell_offset, va.document_cell_offset);
        try std.testing.expectEqual(vb.source_line, va.source_line);
        try std.testing.expectEqual(vb.source_col_start, va.source_col_start);
        try vlineText(incremental, va, &a_text);
        try vlineText(full, vb, &b_text);
        try std.testing.expectEqualStrings(b_text.items, a_text.items);
    }
    const ia = incremental.getCachedLineInfo();
    const ib = full.getCachedLineInfo();
    try std.testing.expectEqualSlices(u32, ib.line_start_cols, ia.line_start_cols);
    try std.testing.expectEqualSlices(u32, ib.line_width_cols, ia.line_width_cols);
    try std.testing.expectEqualSlices(u32, ib.line_sources, ia.line_sources);
    try std.testing.expectEqualSlices(u32, ib.line_wraps, ia.line_wraps);
    const wa = incremental.getWrapInfo();
    const wb = full.getWrapInfo();
    try std.testing.expectEqualSlices(u32, wb.line_first_vline, wa.line_first_vline);
    try std.testing.expectEqualSlices(u32, wb.line_vline_counts, wa.line_vline_counts);
}

fn randomCursor(eb: *EditBuffer, random: std.Random) edit_buffer.Cursor {
    const rope = eb.getTextBuffer().rope();
    const row = random.uintLessThan(u32, eb.getTextBuffer().lineCount());
    const col = random.uintAtMost(u32, iter_mod.lineWidthAt(rope, row));
    return .{ .row = row, .col = col };
}

fn plainText(eb: *EditBuffer) ![]u8 {
    const tb = eb.getTextBuffer();
    const out = try std.testing.allocator.alloc(u8, tb.getByteSize() + 1);
    errdefer std.testing.allocator.free(out);
    const len = tb.getPlainTextIntoBuffer(out);
    return std.testing.allocator.realloc(out, len);
}

/// The change log takes the text at `epoch`, `before`, to the text now:
/// replaying the changes on it, what they didn't insert is the text now.
fn expectChangesSince(eb: *EditBuffer, epoch: u64, before: []const u8) !void {
    const allocator = std.testing.allocator;
    var changes: [64]text_buffer.TextChange = undefined;
    const count = eb.getTextBuffer().changesSince(epoch, &changes) orelse return error.ChangesNotKnown;
    const after = try plainText(eb);
    defer allocator.free(after);
    var text: std.ArrayListUnmanaged(u8) = .empty;
    defer text.deinit(allocator);
    try text.appendSlice(allocator, before);
    // Whether each byte is one of the text before, or was inserted.
    var kept: std.ArrayListUnmanaged(bool) = .empty;
    defer kept.deinit(allocator);
    try kept.appendNTimes(allocator, true, before.len);
    for (changes[0..count]) |change| {
        try std.testing.expect(change.start <= change.old_end and change.old_end <= text.items.len);
        const inserted = change.new_end - change.start;
        const old_len = change.old_end - change.start;
        try text.replaceRange(allocator, change.start, old_len, &.{});
        try kept.replaceRange(allocator, change.start, old_len, &.{});
        for (0..inserted) |_| {
            try text.insert(allocator, change.start, 0);
            try kept.insert(allocator, change.start, false);
        }
    }
    try std.testing.expectEqual(after.len, text.items.len);
    for (text.items, kept.items, after) |byte, was_kept, now| {
        if (was_kept) try std.testing.expectEqual(now, byte);
    }
}

/// Offsets and bytes convert one to the other and back.
fn expectPositionsRoundTrip(eb: *EditBuffer, random: std.Random) !void {
    const tb = eb.getTextBuffer();
    const text = try plainText(eb);
    defer std.testing.allocator.free(text);
    var bytes: [8]u32 = undefined;
    var cursors: [8]edit_buffer.Cursor = undefined;
    for (&bytes, &cursors) |*byte, *cursor| {
        const at = randomCursor(eb, random);
        try eb.setCursor(at.row, at.col);
        cursor.* = eb.getPrimaryCursor();
        byte.* = tb.offsetToByte(cursor.offset);
        // As many line breaks before it as its row.
        try std.testing.expectEqual(@as(usize, cursor.row), std.mem.count(u8, text[0..byte.*], "\n"));
    }
    std.mem.sort(u32, &bytes, {}, std.sort.asc(u32));
    var positions: [8]text_buffer.UnifiedTextBuffer.Position = undefined;
    tb.byteOffsetsToPositions(&bytes, &positions);
    for (bytes, positions) |byte, position| {
        try std.testing.expectEqual(byte, tb.offsetToByte(position.offset));
        try std.testing.expectEqual(@as(usize, position.row), std.mem.count(u8, text[0..byte], "\n"));
    }
    // Past the end is the end.
    tb.byteOffsetsToPositions(&.{@intCast(text.len + 5)}, positions[0..1]);
    try std.testing.expectEqual(tb.rope().totalWeight(), positions[0].offset);
}

fn fuzz(mode: WrapMode, width: u32, first_line_offset: u32, seed: u64) !void {
    const pool = gp.initGlobalPool(std.testing.allocator);
    defer gp.deinitGlobalPool();
    const link_pool = link.initGlobalLinkPool(std.testing.allocator);
    defer link.deinitGlobalLinkPool();

    var eb = try EditBuffer.init(std.testing.allocator, pool, link_pool, .unicode, null);
    defer eb.deinit();
    var view = try TextBufferView.init(std.testing.allocator, eb.getTextBuffer());
    defer view.deinit();
    view.setWrapMode(mode);
    view.setWrapWidth(width);
    view.setFirstLineOffset(first_line_offset);

    try eb.setText("first line of the text\nsecond\n\nfourth line, which is longer than the width\n\tlast");
    _ = view.getVirtualLines();
    var full = try TextBufferView.init(std.testing.allocator, eb.getTextBuffer());
    defer full.deinit();
    var long_way = try TextBufferView.init(std.testing.allocator, eb.getTextBuffer());
    defer long_way.deinit();
    for ([_]*TextBufferView{ full, long_way }) |other| {
        other.setWrapMode(mode);
        other.setWrapWidth(width);
        other.setFirstLineOffset(first_line_offset);
    }

    var prng = std.Random.DefaultPrng.init(seed);
    const random = prng.random();
    var relaid = false;
    var epoch = eb.getTextBuffer().getContentEpoch();
    var before = try plainText(eb);
    defer std.testing.allocator.free(before);
    for (0..300) |_| {
        switch (random.uintLessThan(u8, 13)) {
            0...5 => {
                const at = randomCursor(eb, random);
                try eb.setCursor(at.row, at.col);
                try eb.insertText(snippets[random.uintLessThan(usize, snippets.len)]);
            },
            6, 7 => {
                const from = randomCursor(eb, random);
                const to = randomCursor(eb, random);
                try eb.deleteRange(from, to);
            },
            8 => {
                const at = randomCursor(eb, random);
                try eb.setCursor(at.row, at.col);
                try eb.backspace();
            },
            9 => {
                const at = randomCursor(eb, random);
                try eb.setCursor(at.row, at.col);
                try eb.deleteForward();
            },
            10 => _ = eb.undo() catch {},
            11 => _ = eb.undoSteps(random.uintAtMost(u32, 4) + 1, null) catch {},
            else => _ = eb.redoSteps(random.uintAtMost(u32, 2) + 1, null) catch {},
        }
        // Sometimes several edits come before a layout.
        if (random.uintLessThan(u8, 4) == 0) continue;

        try expectChangesSince(eb, epoch, before);
        std.testing.allocator.free(before);
        before = try plainText(eb);
        epoch = eb.getTextBuffer().getContentEpoch();

        // Sometimes the width changes, which lays out with the word
        // layouts kept from before.
        const view_width = if (mode != .none and random.uintLessThan(u8, 8) == 0) width + 3 else width;
        if (view_width != width) {
            view.setWrapWidth(view_width);
            _ = view.getVirtualLines();
        }
        view.setWrapWidth(width);
        _ = view.getVirtualLines();
        relaid = relaid or view.relaid_vlines > 0;

        // Views take memory slots that are never given back, so these two
        // are kept, and laid out all over again each time.
        full.virtual_lines_dirty = true;
        try expectSameLayout(view, full);

        // Lines that fit are laid out as wrapping them would.
        text_buffer_view.skip_fitting_lines = true;
        defer text_buffer_view.skip_fitting_lines = false;
        long_way.virtual_lines_dirty = true;
        try expectSameLayout(full, long_way);
        try expectPositionsRoundTrip(eb, random);
    }
    // The edits were laid out incrementally, not all over again.
    try std.testing.expect(relaid);
}

test "incremental layout matches a full one - no wrap" {
    for (0..4) |seed| try fuzz(.none, 0, 0, seed);
}

test "incremental layout matches a full one - char wrap" {
    for (0..4) |seed| try fuzz(.char, 9, 0, seed);
    try fuzz(.char, 9, 4, 7);
}

test "incremental layout matches a full one - word wrap" {
    for (0..4) |seed| try fuzz(.word, 11, 0, seed);
    try fuzz(.word, 11, 5, 9);
    try fuzz(.word, 1, 0, 11);
}

test "incremental layout - edits on several lines merge" {
    const pool = gp.initGlobalPool(std.testing.allocator);
    defer gp.deinitGlobalPool();
    const link_pool = link.initGlobalLinkPool(std.testing.allocator);
    defer link.deinitGlobalLinkPool();

    var eb = try EditBuffer.init(std.testing.allocator, pool, link_pool, .unicode, null);
    defer eb.deinit();
    var view = try TextBufferView.init(std.testing.allocator, eb.getTextBuffer());
    defer view.deinit();
    try eb.setText("a\nb\nc\nd\ne\nf");
    _ = view.getVirtualLines();

    try eb.setCursor(4, 1);
    try eb.insertText("\nE");
    try eb.setCursor(1, 0);
    try eb.deleteRange(.{ .row = 1, .col = 0 }, .{ .row = 2, .col = 1 });
    try std.testing.expectEqual(@as(?@import("../text-buffer.zig").LineChange, .{ .start = 1, .old_end = 4, .new_end = 4 }), eb.getTextBuffer().getViewLineChange(view.view_id));

    const vlines = view.getVirtualLines();
    try std.testing.expect(view.relaid_vlines > 0);
    try std.testing.expectEqual(@as(usize, 6), vlines.len);
    var full = try TextBufferView.init(std.testing.allocator, eb.getTextBuffer());
    defer full.deinit();
    try expectSameLayout(view, full);
}
