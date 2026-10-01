//! Finding text in an embedded terminal's screen and scrollback (cue patch).
//!
//! The host does the matching: `build` gives it the screen's text, one line
//! per line of output (soft-wrapped rows joined), and `setMatches` takes the
//! byte ranges it found and turns them into highlighted cells. Highlights
//! keep to the page rows they were found on, as Ghostty's own search
//! results do, so they stay put while the view scrolls and drop off when
//! their rows leave the scrollback. They don't follow text that is
//! rewritten in place; the host searches again when the output changes.

const std = @import("std");
const ghostty = @import("ghostty.zig");

const Node = ghostty.PageList.List.Node;
const Pin = ghostty.PageList.Pin;
const Cell = ghostty.Cell;
const CellCount = ghostty.CellCountInt;

/// The tags highlights carry in the render state.
pub const tag_match: u8 = 0;
pub const tag_current: u8 = 1;

/// A row of the screen, as of the last `build`.
const Row = struct {
    node: *Node,
    serial: u64,
    y: CellCount,
    /// Where its text starts in `Search.text`.
    start: usize,
};

/// The part of a match on one row: columns `x[0]` to `x[1]`, inclusive.
const Span = struct {
    node: *Node,
    serial: u64,
    y: CellCount,
    x: [2]CellCount,
    match: u32,
};

/// Where a match starts. `serial`, `page_y`, and `x` stay the same while
/// the row stays in the scrollback, so the host can tell a match it found
/// before; `row` counts from the top of the scrollback, and shifts as the
/// oldest rows are dropped.
pub const Found = struct {
    serial: u64,
    row: u32,
    page_y: CellCount,
    x: CellCount,
};

pub const Search = struct {
    /// The screen's text, as of the last `build`.
    text: std.ArrayListUnmanaged(u8) = .empty,
    rows: std.ArrayListUnmanaged(Row) = .empty,
    /// The terminal's generation the text is from; matches are only taken
    /// for it.
    generation: u64 = 0,
    spans: std.ArrayListUnmanaged(Span) = .empty,
    current: ?u32 = null,
    /// The cells of one row, for placing the matches in it: matches come in
    /// order, so often several to a row.
    cells: Cells = .{},

    const Cells = struct {
        row: ?usize = null,
        /// The text's byte where each cell's text starts, and the cell.
        starts: std.ArrayListUnmanaged(struct { byte: usize, x: CellCount, wide: bool }) = .empty,
    };

    pub fn deinit(self: *Search, alloc: std.mem.Allocator) void {
        self.text.deinit(alloc);
        self.rows.deinit(alloc);
        self.spans.deinit(alloc);
        self.cells.starts.deinit(alloc);
    }

    /// Drops the text and the matches.
    pub fn clear(self: *Search, alloc: std.mem.Allocator) void {
        self.deinit(alloc);
        self.* = .{};
    }

    /// Reads the text of every row of `screen`, from the top of the
    /// scrollback: a line per line of output, rows wrapped onto the next
    /// joined, blanks at the ends of lines left out.
    pub fn build(self: *Search, alloc: std.mem.Allocator, screen: *ghostty.Screen, generation: u64) std.mem.Allocator.Error!void {
        self.text.clearRetainingCapacity();
        self.rows.clearRetainingCapacity();
        self.generation = generation;
        var it = screen.pages.rowIterator(.right_down, .{ .screen = .{} }, null);
        while (it.next()) |pin| {
            try self.rows.append(alloc, .{
                .node = pin.node,
                .serial = pin.node.serial,
                .y = pin.y,
                .start = self.text.items.len,
            });
            const row = pin.rowAndCell().row;
            const cells = pin.cells(.all);
            for (cells[0..textEnd(row.wrap, cells)]) |*cell| {
                if (isSpacer(cell)) continue;
                try appendCell(alloc, &self.text, pin, cell);
            }
            if (!row.wrap) try self.text.append(alloc, '\n');
        }
    }

    /// Highlights the matches at byte `ranges` (start, end) of the text,
    /// which are in order, and fills `out` with where each starts. Fails if
    /// the terminal changed since the text was read.
    pub fn setMatches(
        self: *Search,
        alloc: std.mem.Allocator,
        generation: u64,
        ranges: []const [2]u32,
        out: []Found,
    ) error{ OutOfMemory, InvalidValue }!void {
        self.spans.clearRetainingCapacity();
        self.current = null;
        self.cells.row = null;
        if (generation != self.generation) return error.InvalidValue;
        for (ranges, out, 0..) |range, *found, i| {
            const start, const end = range;
            if (start >= end or end > self.text.items.len) return error.InvalidValue;
            const first = self.rowOf(start);
            const last = self.rowOf(end - 1);
            const top_x = (try self.cellAt(alloc, first, start)).x;
            const bottom = try self.cellAt(alloc, last, end - 1);
            const bottom_x = bottom.x + @as(CellCount, if (bottom.wide) 1 else 0);
            for (first..last + 1) |r| {
                const row = self.rows.items[r];
                const cols = Pin{ .node = row.node, .y = row.y };
                try self.spans.append(alloc, .{
                    .node = row.node,
                    .serial = row.serial,
                    .y = row.y,
                    .x = .{
                        if (r == first) top_x else 0,
                        if (r == last) bottom_x else @intCast(cols.cells(.all).len - 1),
                    },
                    .match = @intCast(i),
                });
            }
            const row = self.rows.items[first];
            found.* = .{
                .serial = row.serial,
                .row = @intCast(first),
                .page_y = row.y,
                .x = top_x,
            };
        }
    }

    /// Adds the matches on screen to the highlights of `state`'s rows,
    /// replacing any there.
    pub fn highlight(self: *const Search, alloc: std.mem.Allocator, state: *ghostty.RenderState) std.mem.Allocator.Error!void {
        const rows = state.row_data.slice();
        const arenas = rows.items(.arena);
        const pins = rows.items(.pin);
        const serials = rows.items(.serial);
        const highlights = rows.items(.highlights);
        for (highlights) |*list| list.clearRetainingCapacity();
        if (self.spans.items.len == 0) return;

        // The rows of a page on screen are consecutive: find a span's row
        // from the page's first row on screen.
        const Visible = struct { node: *Node, serial: u64, y: CellCount, rows: usize, at: usize };
        var visible: [16]Visible = undefined;
        var count: usize = 0;
        for (pins, serials, 0..) |pin, serial, at| {
            if (count > 0 and visible[count - 1].node == pin.node and visible[count - 1].serial == serial) {
                visible[count - 1].rows += 1;
                continue;
            }
            if (count == visible.len) break;
            visible[count] = .{ .node = pin.node, .serial = serial, .y = pin.y, .rows = 1, .at = at };
            count += 1;
        }

        for (self.spans.items) |span| {
            for (visible[0..count]) |page| {
                if (span.node != page.node or span.serial != page.serial) continue;
                if (span.y < page.y or span.y - page.y >= page.rows) continue;
                const at = page.at + (span.y - page.y);
                var arena = arenas[at].promote(alloc);
                defer arenas[at] = arena.state;
                try highlights[at].append(arena.allocator(), .{
                    .tag = if (self.current == span.match) tag_current else tag_match,
                    .range = span.x,
                });
            }
        }
        state.dirty = .full;
    }

    /// The row whose text has byte `byte`.
    fn rowOf(self: *const Search, byte: usize) usize {
        const rows = self.rows.items;
        var low: usize = 0;
        var high: usize = rows.len;
        while (low < high) {
            const mid = low + (high - low) / 2;
            if (rows[mid].start <= byte) low = mid + 1 else high = mid;
        }
        return low -| 1;
    }

    /// The cell of row `r` whose text has byte `byte`, and whether it's wide.
    fn cellAt(self: *Search, alloc: std.mem.Allocator, r: usize, byte: usize) std.mem.Allocator.Error!struct { x: CellCount, wide: bool } {
        const starts = &self.cells.starts;
        if (self.cells.row != r) {
            self.cells.row = null;
            starts.clearRetainingCapacity();
            const row = self.rows.items[r];
            const pin = Pin{ .node = row.node, .y = row.y };
            var at = row.start;
            for (pin.cells(.all), 0..) |*cell, x| {
                if (isSpacer(cell)) continue;
                try starts.append(alloc, .{ .byte = at, .x = @intCast(x), .wide = cell.wide == .wide });
                at += cellLen(pin, cell);
            }
            self.cells.row = r;
        }
        const items = starts.items;
        if (items.len == 0) return .{ .x = 0, .wide = false };
        // The last cell starting at or before the byte.
        var low: usize = 0;
        var high: usize = items.len;
        while (low < high) {
            const mid = low + (high - low) / 2;
            if (items[mid].byte <= byte) low = mid + 1 else high = mid;
        }
        const cell = items[low -| 1];
        return .{ .x = cell.x, .wide = cell.wide };
    }
};

/// How many of a row's cells have its text: all of them when it wraps onto
/// the next, or else up to the last that isn't blank.
fn textEnd(wraps: bool, cells: []const Cell) usize {
    if (wraps) return cells.len;
    var end = cells.len;
    while (end > 0 and !cells[end - 1].hasText()) end -= 1;
    return end;
}

/// The second half of a wide character, or the blank where one didn't fit
/// at the end of a row: no text of their own.
fn isSpacer(cell: *const Cell) bool {
    return cell.wide == .spacer_tail or cell.wide == .spacer_head;
}

/// A cell's text: its characters, or a space if it's blank.
fn appendCell(alloc: std.mem.Allocator, text: *std.ArrayListUnmanaged(u8), pin: Pin, cell: *const Cell) std.mem.Allocator.Error!void {
    if (!cell.hasText()) return text.append(alloc, ' ');
    var buf: [4]u8 = undefined;
    try text.appendSlice(alloc, encode(cell.codepoint(), &buf));
    if (cell.hasGrapheme()) {
        if (pin.grapheme(cell)) |codepoints| {
            for (codepoints) |codepoint| try text.appendSlice(alloc, encode(codepoint, &buf));
        }
    }
}

/// The length of what `appendCell` appends.
fn cellLen(pin: Pin, cell: *const Cell) usize {
    if (!cell.hasText()) return 1;
    var buf: [4]u8 = undefined;
    var len = encode(cell.codepoint(), &buf).len;
    if (cell.hasGrapheme()) {
        if (pin.grapheme(cell)) |codepoints| {
            for (codepoints) |codepoint| len += encode(codepoint, &buf).len;
        }
    }
    return len;
}

fn encode(codepoint: u21, buf: *[4]u8) []const u8 {
    const len = std.unicode.utf8Encode(codepoint, buf) catch return "\u{FFFD}";
    return buf[0..len];
}
