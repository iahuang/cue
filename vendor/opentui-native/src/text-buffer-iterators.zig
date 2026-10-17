const std = @import("std");
const Allocator = std.mem.Allocator;
const seg_mod = @import("text-buffer-segment.zig");
const mem_registry_mod = @import("mem-registry.zig");
const utf8 = @import("utf8.zig");

const Segment = seg_mod.Segment;
const UnifiedRope = seg_mod.UnifiedRope;
const TextChunk = seg_mod.TextChunk;
const MemRegistry = mem_registry_mod.MemRegistry;

pub const LineInfo = struct {
    line_idx: u32,
    col_offset: u32,
    width_cols: u32,
    seg_start: u32,
    seg_end: u32,
};

pub const Coords = struct {
    row: u32,
    col: u32,
};

/// This is the most efficient way to iterate lines and their content
pub fn walkLinesAndSegments(
    rope: *const UnifiedRope,
    ctx: *anyopaque,
    segment_callback: *const fn (ctx: *anyopaque, line_idx: u32, chunk: *const TextChunk, chunk_idx_in_line: u32) void,
    line_end_callback: *const fn (ctx: *anyopaque, line_info: LineInfo) void,
) void {
    walkLines(rope, null, ctx, segment_callback, line_end_callback);
}

/// Lines `first` to `last` (inclusive) of a text, and where the first starts.
pub const LineRange = struct {
    first: u32,
    last: u32,
    /// The rope weight before line `first`.
    start_weight: u32,
    /// The index of line `first`'s linestart marker.
    leaf_index: u32,
};

/// Lines `first` to `last` of `rope`, for `walkLineRange`. Null if there's
/// no line `first`.
pub fn findLineRange(rope: *UnifiedRope, first: u32, last: u32) ?LineRange {
    const marker = rope.getMarker(.linestart, first) orelse return null;
    return .{ .first = first, .last = last, .start_weight = marker.global_weight, .leaf_index = marker.leaf_index };
}

/// Like `walkLinesAndSegments`, for the lines of `range` only, walking just
/// their segments. Line infos' `seg_start` and `seg_end` count from the
/// first line's start.
pub fn walkLineRange(
    rope: *const UnifiedRope,
    range: LineRange,
    ctx: *anyopaque,
    segment_callback: *const fn (ctx: *anyopaque, line_idx: u32, chunk: *const TextChunk, chunk_idx_in_line: u32) void,
    line_end_callback: *const fn (ctx: *anyopaque, line_info: LineInfo) void,
) void {
    walkLines(rope, range, ctx, segment_callback, line_end_callback);
}

fn walkLines(
    rope: *const UnifiedRope,
    range: ?LineRange,
    ctx: *anyopaque,
    segment_callback: *const fn (ctx: *anyopaque, line_idx: u32, chunk: *const TextChunk, chunk_idx_in_line: u32) void,
    line_end_callback: *const fn (ctx: *anyopaque, line_info: LineInfo) void,
) void {
    if (rope.count() == 0) {
        return;
    }

    const WalkContext = struct {
        user_ctx: *anyopaque,
        seg_callback: *const fn (ctx: *anyopaque, line_idx: u32, chunk: *const TextChunk, chunk_idx_in_line: u32) void,
        line_callback: *const fn (ctx: *anyopaque, line_info: LineInfo) void,
        current_line_idx: u32 = 0,
        current_col_offset: u32 = 0,
        line_start_seg: u32 = 0,
        current_seg_idx: u32 = 0,
        line_width_cols: u32 = 0,
        chunk_idx_in_line: u32 = 0,
        /// The last line to walk, when walking a range.
        last_line: ?u32 = null,
        done: bool = false,

        fn walker(walk_ctx_ptr: *anyopaque, seg: *const Segment, idx: u32) UnifiedRope.Node.WalkerResult {
            const walk_ctx = @as(*@This(), @ptrCast(@alignCast(walk_ctx_ptr)));

            if (seg.asText()) |chunk| {
                walk_ctx.seg_callback(walk_ctx.user_ctx, walk_ctx.current_line_idx, chunk, walk_ctx.chunk_idx_in_line);
                walk_ctx.chunk_idx_in_line += 1;
                walk_ctx.line_width_cols += chunk.width_cols;
            } else if (seg.isBreak()) {
                walk_ctx.line_callback(walk_ctx.user_ctx, .{
                    .line_idx = walk_ctx.current_line_idx,
                    .col_offset = walk_ctx.current_col_offset,
                    .width_cols = walk_ctx.line_width_cols,
                    .seg_start = walk_ctx.line_start_seg,
                    .seg_end = idx, // Don't include the break
                });

                if (walk_ctx.last_line == walk_ctx.current_line_idx) {
                    walk_ctx.done = true;
                    return .{ .keep_walking = false };
                }
                walk_ctx.current_line_idx += 1;
                walk_ctx.current_col_offset += walk_ctx.line_width_cols + 1;
                walk_ctx.line_start_seg = idx + 1;
                walk_ctx.line_width_cols = 0;
                walk_ctx.chunk_idx_in_line = 0;
            }

            walk_ctx.current_seg_idx = idx + 1;
            return .{};
        }
    };

    var walk_ctx: WalkContext = .{
        .user_ctx = ctx,
        .seg_callback = segment_callback,
        .line_callback = line_end_callback,
    };
    if (range) |lines| {
        walk_ctx.current_line_idx = lines.first;
        walk_ctx.current_col_offset = lines.start_weight;
        walk_ctx.last_line = lines.last;
        rope.walk_from(lines.leaf_index, &walk_ctx, WalkContext.walker) catch {};
    } else {
        rope.walk(&walk_ctx, WalkContext.walker) catch {};
    }
    if (walk_ctx.done) return;

    // Emit final line if we have content after last break OR if we had at least one break
    // (A trailing break creates an empty final line)
    const had_breaks = walk_ctx.current_line_idx > 0;
    const has_content_after_break = walk_ctx.line_start_seg < walk_ctx.current_seg_idx;

    if (has_content_after_break or had_breaks) {
        line_end_callback(ctx, .{
            .line_idx = walk_ctx.current_line_idx,
            .col_offset = walk_ctx.current_col_offset,
            .width_cols = walk_ctx.line_width_cols,
            .seg_start = walk_ctx.line_start_seg,
            .seg_end = walk_ctx.current_seg_idx,
        });
    }
}

/// A leaf of the rope, and what comes before it.
pub const LeafStart = struct {
    segment: *const Segment,
    index: u32,
    /// The rope weight before it.
    weight: u32,
    /// The plain text's bytes before it (lines joined by `\n`).
    bytes: u32,
    /// The line it's on.
    row: u32,
};

fn plainBytes(metrics: anytype) u32 {
    return metrics.custom.total_bytes + metrics.custom.newline_count;
}

/// The leaf that rope weight `weight` falls in, past leaves without weight.
/// Null past the end.
pub fn seekWeight(rope: *const UnifiedRope, weight: u32) ?LeafStart {
    return seek(rope, .weight, weight);
}

/// The leaf that byte `byte` of the plain text falls in. Null past the end.
pub fn seekByte(rope: *const UnifiedRope, byte: u32) ?LeafStart {
    return seek(rope, .bytes, byte);
}

fn seek(rope: *const UnifiedRope, comptime by: enum { weight, bytes }, target: u32) ?LeafStart {
    var node = rope.root;
    var at: LeafStart = .{ .segment = undefined, .index = 0, .weight = 0, .bytes = 0, .row = 0 };
    while (true) {
        switch (node.*) {
            .branch => |*b| {
                const left = b.left.metrics();
                const before = if (by == .weight) at.weight else at.bytes;
                const size = if (by == .weight) left.weight() else plainBytes(left);
                if (target < before + size) {
                    node = b.left;
                } else {
                    at.index += left.count;
                    at.weight += left.weight();
                    at.bytes += plainBytes(left);
                    at.row += left.custom.newline_count;
                    node = b.right;
                }
            },
            .leaf => |*l| {
                const metrics = node.metrics();
                const before = if (by == .weight) at.weight else at.bytes;
                const size = if (by == .weight) metrics.weight() else plainBytes(metrics);
                if (target >= before + size) return null;
                at.segment = &l.data;
                return at;
            },
        }
    }
}

pub fn getLineCount(rope: *const UnifiedRope) u32 {
    const metrics = rope.root.metrics();
    return metrics.custom.linestart_count;
}

pub fn getMaxLineWidth(rope: *const UnifiedRope) u32 {
    const metrics = rope.root.metrics();
    return metrics.custom.max_line_width_cols;
}

pub fn getTotalWidth(rope: *const UnifiedRope) u32 {
    const metrics = rope.root.metrics();
    return metrics.custom.total_width_cols;
}

/// Optimized O(1) implementation using linestart marker lookups
/// Note: Rope weight includes newlines (each .brk adds +1), but col is still display width
/// Takes mutable rope for lazy marker cache rebuilding
pub fn coordsToOffset(rope: *UnifiedRope, row: u32, col: u32) ?u32 {
    const linestart_count = rope.markerCount(.linestart);
    if (row >= linestart_count) return null;

    const marker = rope.getMarker(.linestart, row) orelse return null;
    const line_start_weight = marker.global_weight;
    const line_width = lineWidthAt(rope, row);

    if (col > line_width) return null;

    return line_start_weight + col;
}

/// Optimized O(log n) implementation using binary search on linestart markers
/// Note: Rope weight includes newlines, so valid offsets are 0..totalWeight() inclusive
/// Takes mutable rope for lazy marker cache rebuilding
/// TODO: Should clamp to min/max offset and always return valid coords
pub fn offsetToCoords(rope: *UnifiedRope, offset: u32) ?Coords {
    const linestart_count = rope.markerCount(.linestart);
    if (linestart_count == 0) return null;

    const total_weight = rope.totalWeight();
    if (offset > total_weight) return null;

    var left: u32 = 0;
    var right: u32 = linestart_count;

    while (left < right) {
        const mid = left + (right - left) / 2;
        const marker = rope.getMarker(.linestart, mid) orelse return null;
        const line_start_weight = marker.global_weight;

        if (offset < line_start_weight) {
            right = mid;
        } else {
            const next_line_start_weight = if (mid + 1 < linestart_count) blk: {
                const next_marker = rope.getMarker(.linestart, mid + 1) orelse return null;
                break :blk next_marker.global_weight;
            } else blk: {
                // Last line: ends at total weight
                break :blk total_weight;
            };

            // Offset belongs to this line if it's before the next line starts
            // (newline offset at end of non-final line maps to col==line_width)
            if (offset < next_line_start_weight or (offset == total_weight and mid + 1 == linestart_count)) {
                return .{
                    .row = mid,
                    .col = offset - line_start_weight,
                };
            }
            left = mid + 1;
        }
    }

    return null;
}

/// Note: Returns display width only (excludes newline weight)
/// Takes mutable rope for lazy marker cache rebuilding
pub fn lineWidthAt(rope: *UnifiedRope, row: u32) u32 {
    const linestart_count = rope.markerCount(.linestart);
    if (row >= linestart_count) return 0;

    const line_marker = rope.getMarker(.linestart, row) orelse return 0;
    const line_start_weight = line_marker.global_weight;
    if (row + 1 < linestart_count) {
        // Non-final line: width = (next_line_start - current_start - 1_for_newline)
        const next_marker = rope.getMarker(.linestart, row + 1) orelse return 0;
        const next_line_start_weight = next_marker.global_weight;
        // Guard against underflow (adjacent linestart markers or empty line)
        if (next_line_start_weight <= line_start_weight) return 0;
        return next_line_start_weight - line_start_weight - 1;
    } else {
        // Final line: width = total_weight - line_start (total weight includes all previous newlines)
        const total_weight = rope.totalWeight();
        return total_weight - line_start_weight;
    }
}

pub const GraphemeBounds = struct {
    start: u32,
    end: u32,
};

/// Takes mutable rope for lazy marker cache rebuilding
fn getTextUnitBoundsAt(rope: *UnifiedRope, mem_registry: *const MemRegistry, row: u32, col: u32, tab_width: u8, width_method: utf8.WidthMethod, cluster_graphemes: bool) ?GraphemeBounds {
    const line_width = lineWidthAt(rope, row);
    if (col >= line_width) return null;

    const linestart = rope.getMarker(.linestart, row) orelse return null;
    var seg_idx = linestart.leaf_index + 1;
    var cols_before: u32 = 0;

    while (seg_idx < rope.count()) : (seg_idx += 1) {
        const seg = rope.get(seg_idx) orelse break;
        if (seg.isBreak() or seg.isLineStart()) break;
        if (seg.asText()) |chunk| {
            const next_cols = cols_before + chunk.width_cols;
            if (col < next_cols) {
                const local_col: u32 = col - cols_before;
                const bytes = chunk.getBytes(mem_registry);
                const is_ascii = (chunk.flags & TextChunk.Flags.ASCII_ONLY) != 0;
                const pos = if (cluster_graphemes)
                    utf8.findGraphemePosByWidth(bytes, local_col, tab_width, is_ascii, false, width_method)
                else
                    utf8.findPosByWidth(bytes, local_col, tab_width, is_ascii, false, width_method);
                if (pos.byte_offset >= bytes.len) return null;
                const width = if (cluster_graphemes)
                    utf8.getGraphemeWidthAt(bytes, pos.byte_offset, tab_width, width_method)
                else
                    utf8.getWidthAt(bytes, pos.byte_offset, tab_width, width_method);
                const start = cols_before + pos.columns_used;
                return .{ .start = start, .end = start + width };
            }
            cols_before = next_cols;
        }
    }
    return null;
}

pub fn getGraphemeBoundsAt(rope: *UnifiedRope, mem_registry: *const MemRegistry, row: u32, col: u32, tab_width: u8, width_method: utf8.WidthMethod) ?GraphemeBounds {
    return getTextUnitBoundsAt(rope, mem_registry, row, col, tab_width, width_method, true);
}

pub fn getCursorUnitBoundsAt(rope: *UnifiedRope, mem_registry: *const MemRegistry, row: u32, col: u32, tab_width: u8, width_method: utf8.WidthMethod) ?GraphemeBounds {
    return getTextUnitBoundsAt(rope, mem_registry, row, col, tab_width, width_method, false);
}

pub fn getGraphemeWidthAt(rope: *UnifiedRope, mem_registry: *const MemRegistry, row: u32, col: u32, tab_width: u8, width_method: utf8.WidthMethod) u32 {
    const bounds = getCursorUnitBoundsAt(rope, mem_registry, row, col, tab_width, width_method) orelse return 0;
    return bounds.end -| col;
}

/// Takes mutable rope for lazy marker cache rebuilding
pub fn getPrevGraphemeWidth(rope: *UnifiedRope, mem_registry: *const MemRegistry, row: u32, col: u32, tab_width: u8, width_method: utf8.WidthMethod) u32 {
    if (col == 0) return 0;

    const line_width = lineWidthAt(rope, row);
    const clamped_col: u32 = @min(col, line_width);

    const linestart = rope.getMarker(.linestart, row) orelse return 0;
    var seg_idx = linestart.leaf_index + 1;
    var cols_before: u32 = 0;
    var prev_chunk: ?struct { chunk: TextChunk, cols_before: u32 } = null;

    while (seg_idx < rope.count()) : (seg_idx += 1) {
        const seg = rope.get(seg_idx) orelse break;
        if (seg.isBreak() or seg.isLineStart()) break;
        if (seg.asText()) |chunk| {
            const next_cols = cols_before + chunk.width_cols;

            if (clamped_col <= next_cols) {
                if (clamped_col == cols_before and prev_chunk != null) {
                    // Exactly at chunk boundary - get last grapheme from previous chunk
                    const pc = prev_chunk.?;
                    const bytes = pc.chunk.getBytes(mem_registry);
                    const prev = utf8.getPrevGraphemeStart(bytes, bytes.len, tab_width, width_method);
                    if (prev) |res| {
                        return res.width;
                    }
                    return 0;
                }

                const bytes = chunk.getBytes(mem_registry);
                const is_ascii = (chunk.flags & TextChunk.Flags.ASCII_ONLY) != 0;
                const local_col: u32 = clamped_col - cols_before;

                const here = utf8.findPosByWidth(bytes, local_col, tab_width, is_ascii, false, width_method);

                const grapheme_start_col = here.columns_used;

                // Check for integer underflow: if grapheme_start_col > local_col, we're in the middle of a grapheme
                // that spans beyond local_col. This can happen with multi-codepoint graphemes.
                if (grapheme_start_col > local_col) {
                    // We're in the middle of a wide grapheme cluster - need to look at previous chunk or grapheme
                    if (prev_chunk) |pc| {
                        const prev_bytes = pc.chunk.getBytes(mem_registry);
                        const prev = utf8.getPrevGraphemeStart(prev_bytes, prev_bytes.len, tab_width, width_method);
                        if (prev) |res| return res.width;
                    }
                    return 0;
                }

                const offset_into_grapheme = local_col - grapheme_start_col;

                if (offset_into_grapheme > 0) {
                    // We need to jump back: offset_into_grapheme + width of previous grapheme
                    const prev = utf8.getPrevGraphemeStart(bytes, @intCast(here.byte_offset), tab_width, width_method);
                    if (prev) |res| {
                        const total_distance = offset_into_grapheme + res.width;
                        return total_distance;
                    }
                    return offset_into_grapheme;
                }

                const prev = utf8.getPrevGraphemeStart(bytes, @intCast(here.byte_offset), tab_width, width_method);
                if (prev) |res| {
                    return res.width;
                }
                return 0;
            }

            prev_chunk = .{ .chunk = chunk.*, .cols_before = cols_before };
            cols_before = next_cols;
        }
    }
    return 0;
}

/// Extract text between display-width offsets into a buffer
/// Automatically snaps to grapheme boundaries:
/// - start_offset excludes graphemes that start before it
/// - end_offset includes graphemes that start before it
/// Returns number of bytes written to out_buffer
/// Walks only the segments from the one `start_offset` falls in to the one
/// `end_offset` does, rather than the whole rope.
pub fn extractTextBetweenOffsets(
    rope: *const UnifiedRope,
    mem_registry: *const MemRegistry,
    tab_width: u8,
    start_offset: u32,
    end_offset: u32,
    out_buffer: []u8,
    width_method: utf8.WidthMethod,
) usize {
    if (start_offset >= end_offset) return 0;
    if (out_buffer.len == 0) return 0;

    const first = rope.findIndexByWeight(start_offset) orelse return 0;

    const Context = struct {
        mem_registry: *const MemRegistry,
        tab_width: u8,
        out_buffer: []u8,
        out_index: usize = 0,
        col_offset: u32,
        start: u32,
        end: u32,
        width_method: utf8.WidthMethod,

        fn walker(ctx_ptr: *anyopaque, seg: *const Segment, idx: u32) UnifiedRope.Node.WalkerResult {
            _ = idx;
            const ctx = @as(*@This(), @ptrCast(@alignCast(ctx_ptr)));
            if (ctx.col_offset >= ctx.end or ctx.out_index >= ctx.out_buffer.len) return .{ .keep_walking = false };

            if (seg.asText()) |chunk| {
                ctx.text(chunk);
            } else if (seg.isBreak()) {
                // A break is only ever followed by another line, so the
                // newline is in the text when its offset is in the range,
                // even for empty logical lines.
                if (ctx.col_offset >= ctx.start) {
                    ctx.out_buffer[ctx.out_index] = '\n';
                    ctx.out_index += 1;
                }
                ctx.col_offset += 1;
            }
            return .{};
        }

        fn text(ctx: *@This(), chunk: *const TextChunk) void {
            const chunk_start_offset = ctx.col_offset;
            const chunk_end_offset = chunk_start_offset + chunk.width_cols;
            ctx.col_offset = chunk_end_offset;

            // Skip chunk if it's entirely outside range
            if (chunk_end_offset <= ctx.start or chunk_start_offset >= ctx.end) return;

            const chunk_bytes = chunk.getBytes(ctx.mem_registry);
            const is_ascii_only = (chunk.flags & TextChunk.Flags.ASCII_ONLY) != 0;

            const local_start_col: u32 = if (ctx.start > chunk_start_offset) ctx.start - chunk_start_offset else 0;
            const local_end_col: u32 = @min(ctx.end - chunk_start_offset, chunk.width_cols);

            var byte_start: u32 = 0;
            var byte_end: u32 = @intCast(chunk_bytes.len);

            if (local_start_col > 0) {
                const start_result = utf8.findGraphemePosByWidth(chunk_bytes, local_start_col, ctx.tab_width, is_ascii_only, false, ctx.width_method);
                byte_start = start_result.byte_offset;
            }

            if (local_end_col < chunk.width_cols) {
                const end_result = utf8.findGraphemePosByWidth(chunk_bytes, local_end_col, ctx.tab_width, is_ascii_only, true, ctx.width_method);
                byte_end = end_result.byte_offset;
            }

            if (byte_start < byte_end and byte_start < chunk_bytes.len) {
                const actual_end = @min(byte_end, @as(u32, @intCast(chunk_bytes.len)));
                const selected_bytes = chunk_bytes[byte_start..actual_end];
                const copy_len = @min(selected_bytes.len, ctx.out_buffer.len - ctx.out_index);

                if (copy_len > 0) {
                    @memcpy(ctx.out_buffer[ctx.out_index .. ctx.out_index + copy_len], selected_bytes[0..copy_len]);
                    ctx.out_index += copy_len;
                }
            }
        }
    };

    var ctx: Context = .{
        .mem_registry = mem_registry,
        .tab_width = tab_width,
        .out_buffer = out_buffer,
        .col_offset = first.start_weight,
        .start = start_offset,
        .end = end_offset,
        .width_method = width_method,
    };

    rope.walk_from(first.index, &ctx, Context.walker) catch {};

    return ctx.out_index;
}
