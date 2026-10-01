const std = @import("std");
const buffer = @import("../buffer.zig");
const compositor = @import("compositor.zig");
const ghostty = @import("ghostty.zig");
const search = @import("search.zig");

pub const Found = search.Found;
pub const SearchColors = compositor.SearchColors;

pub const Error = error{
    InvalidValue,
    ProcessingFailed,
    ResponseOverflow,
} || std.mem.Allocator.Error || buffer.BufferError;

pub const Cursor = struct {
    x: u16 = 0,
    y: u16 = 0,
    has_value: bool = false,
    visible: bool = false,
    blinking: bool = false,
    wide_tail: bool = false,
    style: u8 = 1,
    color: ?struct { r: u8, g: u8, b: u8 } = null,
};

pub const Options = struct {
    cols: u16,
    rows: u16,
    max_scrollback: usize = 10_000,
};

pub const response_limit = 1024 * 1024;

pub const EmbeddedTerminal = struct {
    allocator: std.mem.Allocator,
    terminal: ghostty.Terminal,
    stream: ghostty.TerminalStream,
    render_state: ghostty.RenderState = .empty,
    cols: u16,
    rows: u16,
    responses: std.ArrayListUnmanaged(u8) = .empty,
    response_error: ?Error = null,
    /// The text the program last put on the clipboard (OSC 52), until
    /// taken; the host puts it on its own.
    clipboard: std.ArrayListUnmanaged(u8) = .empty,
    clipboard_pending: bool = false,
    mouse_last_cell: ?ghostty.Coordinate = null,
    force_redraw: bool = true,
    transparent_background: bool = false,
    host_palette: bool = false,
    /// Counts changes to the screen's contents, so search results are only
    /// taken for the text they were found in.
    generation: u64 = 0,
    search: search.Search = .{},
    search_colors: SearchColors = .{},

    pub fn init(io: std.Io, allocator: std.mem.Allocator, options: Options) Error!*EmbeddedTerminal {
        if (options.cols == 0 or options.rows == 0) return error.InvalidValue;

        const self = try allocator.create(EmbeddedTerminal);
        errdefer allocator.destroy(self);

        self.* = .{
            .allocator = allocator,
            .terminal = try .init(io, allocator, .{
                .cols = options.cols,
                .rows = options.rows,
                .max_scrollback_bytes = options.max_scrollback,
            }),
            .stream = undefined,
            .cols = options.cols,
            .rows = options.rows,
        };
        errdefer self.terminal.deinit(allocator);

        var handler = self.terminal.vtHandler();
        handler.effects.write_pty = &writePty;
        handler.effects.clipboard_write = &clipboardWrite;
        self.stream = .init(.{ .allocator = allocator, .handler = handler });
        return self;
    }

    pub fn deinit(self: *EmbeddedTerminal) void {
        const allocator = self.allocator;
        self.stream.deinit();
        self.render_state.deinit(allocator);
        self.search.deinit(allocator);
        self.terminal.deinit(allocator);
        self.responses.deinit(allocator);
        self.clipboard.deinit(allocator);
        allocator.destroy(self);
    }

    pub fn write(self: *EmbeddedTerminal, bytes: []const u8) Error!void {
        self.generation +%= 1;
        self.stream.handler.semantic_failure = false;
        self.stream.nextSlice(bytes);
        if (self.stream.handler.semantic_failure) return error.ProcessingFailed;
    }

    pub fn resize(self: *EmbeddedTerminal, cols: u16, rows: u16) Error!void {
        if (cols == 0 or rows == 0) return error.InvalidValue;
        self.generation +%= 1;
        self.stream.handler.resize(.{ .cols = cols, .rows = rows }) catch |err| switch (err) {
            error.InvalidValue => return error.InvalidValue,
            error.OutOfMemory => return error.OutOfMemory,
        };
        self.cols = cols;
        self.rows = rows;
        self.mouse_last_cell = null;
    }

    pub fn scroll(self: *EmbeddedTerminal, delta: i32) void {
        self.terminal.scrollViewport(.{ .delta = delta });
    }

    pub fn scrollToBottom(self: *EmbeddedTerminal) void {
        self.terminal.scrollViewport(.bottom);
    }

    /// The viewport's top row, counted from the top of the scrollback, and
    /// how many rows there are in all.
    pub fn viewportRow(self: *EmbeddedTerminal) struct { row: usize, total: usize } {
        const scrollbar = self.terminal.screens.active.pages.scrollbar();
        return .{ .row = scrollbar.offset, .total = scrollbar.total };
    }

    /// Scrolls the viewport's top to `row`, counted from the top of the
    /// scrollback, or as near as it goes.
    pub fn scrollToRow(self: *EmbeddedTerminal, row: usize) void {
        self.terminal.scrollViewport(.{ .row = row });
    }

    /// Reads the screen's text, for the host to search (see `search.zig`).
    /// Returns its length; `searchText` has it until the screen changes.
    pub fn buildSearch(self: *EmbeddedTerminal) Error!usize {
        try self.search.build(self.allocator, self.terminal.screens.active, self.generation);
        return self.search.text.items.len;
    }

    pub fn searchText(self: *EmbeddedTerminal) []const u8 {
        return self.search.text.items;
    }

    /// Highlights the matches at byte `ranges` of the text from the last
    /// `buildSearch`, and puts where each starts in `out`. Fails if the
    /// screen changed since.
    pub fn setSearchMatches(self: *EmbeddedTerminal, ranges: []const [2]u32, out: []Found) Error!void {
        self.force_redraw = true;
        return self.search.setMatches(self.allocator, self.generation, ranges, out);
    }

    /// Highlights match `index` as the current one, or none.
    pub fn setSearchCurrent(self: *EmbeddedTerminal, index: ?u32) void {
        self.search.current = index;
        self.force_redraw = true;
    }

    pub fn clearSearch(self: *EmbeddedTerminal) void {
        self.search.clear(self.allocator);
        self.force_redraw = true;
    }

    pub fn setSearchColors(self: *EmbeddedTerminal, colors: SearchColors) void {
        self.search_colors = colors;
        self.force_redraw = true;
    }

    pub fn isAlternateScreen(self: *EmbeddedTerminal) bool {
        return self.terminal.screens.active_key == .alternate;
    }

    /// The title set by escape sequences (OSC 0/2), or "".
    pub fn title(self: *EmbeddedTerminal) [:0]const u8 {
        return self.terminal.getTitle() orelse "";
    }

    pub fn setSelection(self: *EmbeddedTerminal, start: ghostty.Coordinate, end: ghostty.Coordinate) Error!void {
        const screen = self.terminal.screens.active;
        const start_pin = screen.pages.pin(.{ .viewport = start }) orelse return error.InvalidValue;
        const end_pin = screen.pages.pin(.{ .viewport = end }) orelse return error.InvalidValue;
        try screen.select(ghostty.Selection.init(start_pin, end_pin, false));
    }

    pub fn clearSelection(self: *EmbeddedTerminal) void {
        self.terminal.screens.active.clearSelection();
    }

    pub fn selectedText(self: *EmbeddedTerminal) Error![:0]const u8 {
        const screen = self.terminal.screens.active;
        const selection = screen.selection orelse return try self.allocator.dupeZ(u8, "");
        return try screen.selectionString(self.allocator, .{ .sel = selection });
    }

    /// What's at viewport cell `at`, for the host to open: the URI of an
    /// OSC 8 hyperlink there (`is_link` set), or else the text of its line,
    /// joined across soft wraps, with `offset` set to the byte in it where
    /// the cell's text starts (maxInt if the cell is blank past the end).
    /// Free with `freeSelectedText`.
    pub fn lineAt(self: *EmbeddedTerminal, at: ghostty.Coordinate, offset: *usize, is_link: *bool) Error![:0]const u8 {
        offset.* = std.math.maxInt(usize);
        is_link.* = false;
        const screen = self.terminal.screens.active;
        const pin = screen.pages.pin(.{ .viewport = at }) orelse return error.InvalidValue;
        const page = pin.node.page();
        const cell = page.getRowAndCell(pin.x, pin.y).cell;
        if (cell.hyperlink) {
            if (page.lookupHyperlink(cell)) |id| {
                const link = page.hyperlink_set.get(page.memory, id);
                is_link.* = true;
                offset.* = 0;
                return try self.allocator.dupeZ(u8, link.uri.slice(page.memory));
            }
        }
        const selection = screen.selectLine(.{
            .pin = pin,
            .whitespace = null,
            .semantic_prompt_boundary = false,
        }) orelse return try self.allocator.dupeZ(u8, "");
        var map: ghostty.StringMap = undefined;
        const text = try screen.selectionString(self.allocator, .{ .sel = selection, .trim = false, .map = &map });
        defer map.deinit(self.allocator);
        for (0..map.map.count()) |i| {
            const mapped = map.map.get(i) orelse continue;
            if (mapped.node == pin.node and mapped.x == pin.x and mapped.y == pin.y) {
                offset.* = i;
                break;
            }
        }
        return text;
    }

    pub fn freeSelectedText(self: *EmbeddedTerminal, text: [:0]const u8) void {
        self.allocator.free(text);
    }

    pub fn invalidate(self: *EmbeddedTerminal) void {
        self.force_redraw = true;
    }

    pub fn setTransparentBackground(self: *EmbeddedTerminal, transparent: bool) void {
        self.transparent_background = transparent;
        self.force_redraw = true;
    }

    /// Composes default and palette colors as the host terminal's own
    /// (SGR 39/49 and indexed colors), so they follow its theme, rather
    /// than as RGB from Ghostty's palette. Inverse video and the selection
    /// become the inverse attribute, which the host applies.
    pub fn setHostPalette(self: *EmbeddedTerminal, enabled: bool) void {
        self.host_palette = enabled;
        self.force_redraw = true;
    }

    pub fn compose(self: *EmbeddedTerminal, target: *buffer.OptimizedBuffer, x: i32, y: i32) Error!void {
        self.render_state.update(self.allocator, &self.terminal) catch |err| {
            self.render_state.deinit(self.allocator);
            self.render_state = .empty;
            self.force_redraw = true;
            return err;
        };
        try self.search.highlight(self.allocator, &self.render_state);
        if (self.force_redraw) {
            self.render_state.dirty = .full;
            self.force_redraw = false;
        }
        try compositor.compose(self.allocator, &self.render_state, target, x, y, .{
            .transparent_background = self.transparent_background,
            .host_palette = self.host_palette,
            .search = self.search_colors,
        });
    }

    pub fn cursor(self: *EmbeddedTerminal) Cursor {
        const state = self.render_state.cursor;
        const viewport = state.viewport orelse return .{ .visible = state.visible };
        const cursor_color = self.render_state.colors.cursor orelse self.render_state.colors.foreground;
        return .{
            .x = viewport.x,
            .y = viewport.y,
            .has_value = true,
            .visible = state.visible,
            .blinking = state.blinking,
            .wide_tail = viewport.wide_tail,
            .style = switch (state.visual_style) {
                .bar => 0,
                .block => 1,
                .underline => 2,
                .block_hollow => 3,
            },
            .color = .{ .r = cursor_color.r, .g = cursor_color.g, .b = cursor_color.b },
        };
    }

    pub fn encodeKey(self: *EmbeddedTerminal, key: ghostty.Key) Error![]u8 {
        var options: ghostty.KeyEncodeOptions = .fromTerminal(&self.terminal);
        // Keys come from the host terminal, which already decided whether
        // Option is Alt: a key reported with Alt is meant as Alt.
        options.macos_option_as_alt = .true;
        var output: std.Io.Writer.Allocating = .init(self.allocator);
        errdefer output.deinit();
        ghostty.encodeKey(&output.writer, key.event(), options) catch return error.OutOfMemory;
        return output.toOwnedSlice();
    }

    pub fn encodeMouse(self: *EmbeddedTerminal, mouse: ghostty.Mouse) Error![]u8 {
        const MouseSize = @FieldType(ghostty.MouseEncodeOptions, "size");
        const size: MouseSize = .{
            .screen = .{ .width = self.cols, .height = self.rows },
            .cell = .{ .width = 1, .height = 1 },
            .padding = .{},
        };
        var options = ghostty.MouseEncodeOptions.fromTerminal(&self.terminal, size);
        // OpenTUI receives terminal-cell coordinates, not physical pixels.
        if (options.format == .sgr_pixels) return self.allocator.alloc(u8, 0);
        options.any_button_pressed = mouse.any_button_pressed;
        options.last_cell = &self.mouse_last_cell;

        var output: std.Io.Writer.Allocating = .init(self.allocator);
        errdefer output.deinit();
        ghostty.encodeMouse(&output.writer, mouse.event(), options) catch return error.OutOfMemory;
        return output.toOwnedSlice();
    }

    pub fn encodePaste(self: *EmbeddedTerminal, input: []const u8) Error![]u8 {
        const copy = try self.allocator.dupe(u8, input);
        defer self.allocator.free(copy);
        const parts = ghostty.encodePaste(copy, .fromTerminal(&self.terminal));

        var output: std.Io.Writer.Allocating = .init(self.allocator);
        errdefer output.deinit();
        for (parts) |part| output.writer.writeAll(part) catch return error.OutOfMemory;
        return output.toOwnedSlice();
    }

    pub fn encodeFocus(self: *EmbeddedTerminal, focused: bool) Error![]u8 {
        if (!self.terminal.modes.get(.focus_event)) return self.allocator.alloc(u8, 0);

        var output: std.Io.Writer.Allocating = .init(self.allocator);
        errdefer output.deinit();
        ghostty.encodeFocus(&output.writer, if (focused) .gained else .lost) catch return error.OutOfMemory;
        return output.toOwnedSlice();
    }

    pub fn freeEncoded(self: *EmbeddedTerminal, bytes: []u8) void {
        self.allocator.free(bytes);
    }

    pub fn drainResponses(self: *EmbeddedTerminal, output: []u8) Error!usize {
        if (self.response_error) |err| {
            self.response_error = null;
            return err;
        }
        const count = @min(output.len, self.responses.items.len);
        @memcpy(output[0..count], self.responses.items[0..count]);
        std.mem.copyForwards(u8, self.responses.items[0 .. self.responses.items.len - count], self.responses.items[count..]);
        self.responses.items.len -= count;
        return count;
    }

    /// The text the program put on the clipboard since this was last
    /// called with room for it, or null. Stays pending when `output` is too
    /// short; `required` is then set to its length.
    pub fn takeClipboard(self: *EmbeddedTerminal, output: []u8, required: *usize) ?usize {
        required.* = 0;
        if (!self.clipboard_pending) return null;
        const text = self.clipboard.items;
        required.* = text.len;
        if (text.len > output.len) return null;
        @memcpy(output[0..text.len], text);
        self.clipboard_pending = false;
        self.clipboard.clearRetainingCapacity();
        return text.len;
    }

    /// Keeps the latest write for the host. All destinations (clipboard,
    /// selection, primary) mean the one clipboard there. Clearing one is
    /// ignored, so a program can't wipe what the user copied elsewhere.
    fn clipboardWrite(handler: *ghostty.TerminalStream.Handler, request: ghostty.ClipboardWrite) ghostty.ClipboardWriteResult {
        const self: *EmbeddedTerminal = @fieldParentPtr("terminal", handler.terminal);
        const text = for (request.contents) |content| {
            if (std.mem.eql(u8, content.mime, "text/plain")) break content.data;
        } else return .unsupported;
        self.clipboard.clearRetainingCapacity();
        self.clipboard.appendSlice(self.allocator, text) catch {
            self.clipboard_pending = false;
            return .io_error;
        };
        self.clipboard_pending = true;
        return .success;
    }

    fn writePty(handler: *ghostty.TerminalStream.Handler, data: [:0]const u8) void {
        const self: *EmbeddedTerminal = @fieldParentPtr("terminal", handler.terminal);
        if (self.response_error != null) return;
        if (data.len > response_limit - @min(self.responses.items.len, response_limit)) {
            self.response_error = error.ResponseOverflow;
            return;
        }
        self.responses.appendSlice(self.allocator, data) catch {
            self.response_error = error.OutOfMemory;
        };
    }
};
