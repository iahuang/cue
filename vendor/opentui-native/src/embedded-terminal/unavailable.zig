const buffer = @import("../buffer.zig");

pub const Error = error{Unsupported};

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

pub const Found = struct {
    serial: u64 = 0,
    row: u32 = 0,
    page_y: u16 = 0,
    x: u16 = 0,
};

pub const SearchColors = struct {
    match_fg: ?buffer.RGBA = null,
    match_bg: buffer.RGBA = .{ 0, 0, 0, 0 },
    current_fg: ?buffer.RGBA = null,
    current_bg: buffer.RGBA = .{ 0, 0, 0, 0 },
};

pub const EmbeddedTerminal = struct {
    pub fn init(_: anytype, _: anytype, _: anytype) Error!*EmbeddedTerminal {
        return error.Unsupported;
    }

    pub fn deinit(_: *EmbeddedTerminal) void {}
    pub fn write(_: *EmbeddedTerminal, _: []const u8) Error!void {
        return error.Unsupported;
    }
    pub fn resize(_: *EmbeddedTerminal, _: u16, _: u16) Error!void {
        return error.Unsupported;
    }
    pub fn scroll(_: *EmbeddedTerminal, _: i32) void {}
    pub fn scrollToBottom(_: *EmbeddedTerminal) void {}
    pub fn viewportRow(_: *EmbeddedTerminal) struct { row: usize, total: usize } {
        return .{ .row = 0, .total = 0 };
    }
    pub fn scrollToRow(_: *EmbeddedTerminal, _: usize) void {}
    pub fn buildSearch(_: *EmbeddedTerminal) Error!usize {
        return error.Unsupported;
    }
    pub fn searchText(_: *EmbeddedTerminal) []const u8 {
        return "";
    }
    pub fn buildSnapshot(_: *EmbeddedTerminal, _: bool) Error!usize {
        return error.Unsupported;
    }
    pub fn snapshotBytes(_: *EmbeddedTerminal) []const u8 {
        return "";
    }
    pub fn setSearchMatches(_: *EmbeddedTerminal, _: []const [2]u32, _: []Found) Error!void {
        return error.Unsupported;
    }
    pub fn setSearchCurrent(_: *EmbeddedTerminal, _: ?u32) void {}
    pub fn clearSearch(_: *EmbeddedTerminal) void {}
    pub fn setSearchColors(_: *EmbeddedTerminal, _: SearchColors) void {}
    pub fn setDefaultColors(_: *EmbeddedTerminal, _: buffer.RGBA, _: buffer.RGBA, _: ?*const [16]buffer.RGBA) void {}
    pub fn isAlternateScreen(_: *EmbeddedTerminal) bool {
        return false;
    }
    pub fn title(_: *EmbeddedTerminal) [:0]const u8 {
        return "";
    }
    pub fn setSelection(_: *EmbeddedTerminal, _: anytype, _: anytype) Error!void {
        return error.Unsupported;
    }
    pub fn clearSelection(_: *EmbeddedTerminal) void {}
    pub fn selectWord(_: *EmbeddedTerminal, _: anytype) Error!void {
        return error.Unsupported;
    }
    pub fn selectedText(_: *EmbeddedTerminal) Error![:0]const u8 {
        return error.Unsupported;
    }
    pub fn lineAt(_: *EmbeddedTerminal, _: anytype, _: *usize, _: *bool) Error![:0]const u8 {
        return error.Unsupported;
    }
    pub fn freeSelectedText(_: *EmbeddedTerminal, _: [:0]const u8) void {}
    pub fn invalidate(_: *EmbeddedTerminal) void {}
    pub fn setTransparentBackground(_: *EmbeddedTerminal, _: bool) void {}
    pub fn setHostPalette(_: *EmbeddedTerminal, _: bool) void {}
    pub fn compose(_: *EmbeddedTerminal, _: *buffer.OptimizedBuffer, _: i32, _: i32) Error!void {
        return error.Unsupported;
    }
    pub fn cursor(_: *EmbeddedTerminal) Cursor {
        return .{};
    }
    pub fn encodePaste(_: *EmbeddedTerminal, _: []const u8) Error![]u8 {
        return error.Unsupported;
    }
    pub fn encodeFocus(_: *EmbeddedTerminal, _: bool) Error![]u8 {
        return error.Unsupported;
    }
    pub fn freeEncoded(_: *EmbeddedTerminal, _: []u8) void {}
    pub fn takeClipboard(_: *EmbeddedTerminal, _: []u8, required: *usize) ?usize {
        required.* = 0;
        return null;
    }
    pub fn drainResponses(_: *EmbeddedTerminal, _: []u8) Error!usize {
        return error.Unsupported;
    }
};
