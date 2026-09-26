const std = @import("std");
const Allocator = std.mem.Allocator;
const buffer = @import("buffer.zig");
const events = @import("event-emitter.zig");

pub const RGBA = buffer.RGBA;

/// Foreground, background, and text attributes for a syntax highlight style.
/// Color intent (rgb, indexed, default) is embedded in the RGBA values.
pub const StyleDefinition = struct {
    fg: ?RGBA,
    bg: ?RGBA,
    attributes: u32,
};

pub const SyntaxStyleError = error{
    OutOfMemory,
    InvalidId,
    StyleNotFound,
};

pub const Event = enum { Destroy };

pub const SyntaxStyle = struct {
    allocator: Allocator,
    global_allocator: Allocator,
    arena: *std.heap.ArenaAllocator,

    name_to_id: std.StringHashMapUnmanaged(u32),
    id_to_style: std.AutoHashMapUnmanaged(u32, StyleDefinition),
    next_id: u32,

    merged_cache: std.StringHashMapUnmanaged(StyleDefinition),

    /// Styles made by `layeredStyleId`, keyed by their layers' ids joined
    /// with ':', and each one's layers, to redo it when a layer changes.
    layered_ids: std.StringHashMapUnmanaged(u32),
    layers: std.AutoHashMapUnmanaged(u32, []const u32),

    emitter: events.EventEmitter(Event),

    pub fn init(global_allocator: Allocator) SyntaxStyleError!*SyntaxStyle {
        const self = global_allocator.create(SyntaxStyle) catch return SyntaxStyleError.OutOfMemory;
        errdefer global_allocator.destroy(self);

        const internal_arena = global_allocator.create(std.heap.ArenaAllocator) catch return SyntaxStyleError.OutOfMemory;
        errdefer global_allocator.destroy(internal_arena);
        internal_arena.* = std.heap.ArenaAllocator.init(global_allocator);

        const internal_allocator = internal_arena.allocator();

        self.* = .{
            .allocator = internal_allocator,
            .global_allocator = global_allocator,
            .arena = internal_arena,
            .name_to_id = .empty,
            .id_to_style = .empty,
            .next_id = 1, // Start from 1, 0 can be used as "invalid"
            .merged_cache = .empty,
            .layered_ids = .empty,
            .layers = .empty,
            .emitter = events.EventEmitter(Event).init(internal_allocator),
        };

        return self;
    }

    pub fn deinit(self: *SyntaxStyle) void {
        const global_allocator = self.global_allocator;
        defer global_allocator.destroy(self);

        self.emitter.emit(.Destroy);
        self.emitter.deinit();
        self.arena.deinit();
        global_allocator.destroy(self.arena);
        self.* = undefined;
    }

    fn putStyle(self: *SyntaxStyle, name: []const u8, definition: StyleDefinition) SyntaxStyleError!u32 {
        if (self.name_to_id.get(name)) |existing_id| {
            try self.id_to_style.put(self.allocator, existing_id, definition);
            self.merged_cache.clearRetainingCapacity();
            var it = self.layers.iterator();
            while (it.next()) |entry| {
                try self.id_to_style.put(self.allocator, entry.key_ptr.*, self.layer(entry.value_ptr.*));
            }
            return existing_id;
        }

        const id = self.next_id;
        self.next_id += 1;

        const owned_name = self.allocator.dupe(u8, name) catch return SyntaxStyleError.OutOfMemory;

        try self.name_to_id.put(self.allocator, owned_name, id);
        try self.id_to_style.put(self.allocator, id, definition);

        return id;
    }

    pub fn registerStyle(self: *SyntaxStyle, name: []const u8, fg: ?RGBA, bg: ?RGBA, attributes: u32) SyntaxStyleError!u32 {
        return self.registerStyleDefinition(name, .{
            .fg = fg,
            .bg = bg,
            .attributes = attributes,
        });
    }

    pub fn registerStyleDefinition(self: *SyntaxStyle, name: []const u8, definition: StyleDefinition) SyntaxStyleError!u32 {
        return self.putStyle(name, definition);
    }

    pub fn resolveById(self: *const SyntaxStyle, id: u32) ?StyleDefinition {
        return self.id_to_style.get(id);
    }

    pub fn resolveByName(self: *const SyntaxStyle, name: []const u8) ?u32 {
        return self.name_to_id.get(name);
    }

    pub fn getStyleByName(self: *const SyntaxStyle, name: []const u8) ?StyleDefinition {
        const id = self.resolveByName(name) orelse return null;
        return self.resolveById(id);
    }

    pub fn mergeStyles(self: *SyntaxStyle, ids: []const u32) SyntaxStyleError!StyleDefinition {
        var cache_key_buffer: [512]u8 = undefined;
        var writer: std.Io.Writer = .fixed(&cache_key_buffer);

        for (ids, 0..) |id, i| {
            if (i > 0) writer.writeByte(':') catch return SyntaxStyleError.OutOfMemory;
            writer.print("{d}", .{id}) catch return SyntaxStyleError.OutOfMemory;
        }

        const cache_key = writer.buffered();

        if (self.merged_cache.get(cache_key)) |cached| {
            return cached;
        }

        var merged: StyleDefinition = .{
            .fg = null,
            .bg = null,
            .attributes = 0,
        };

        for (ids) |id| {
            if (self.resolveById(id)) |style| {
                if (style.fg) |fg| {
                    merged.fg = fg;
                }
                if (style.bg) |bg| {
                    merged.bg = bg;
                }
                // Attributes are OR'd together
                merged.attributes |= style.attributes;
            }
        }

        const owned_cache_key = self.allocator.dupe(u8, cache_key) catch return SyntaxStyleError.OutOfMemory;
        self.merged_cache.put(self.allocator, owned_cache_key, merged) catch return SyntaxStyleError.OutOfMemory;

        return merged;
    }

    /// `ids`' styles applied in order: each one's colors replace the ones
    /// before, and attributes are OR'd.
    fn layer(self: *const SyntaxStyle, ids: []const u32) StyleDefinition {
        var layered: StyleDefinition = .{ .fg = null, .bg = null, .attributes = 0 };
        for (ids) |id| {
            const style = self.resolveById(id) orelse continue;
            if (style.fg) |fg| layered.fg = fg;
            if (style.bg) |bg| layered.bg = bg;
            layered.attributes |= style.attributes;
        }
        return layered;
    }

    /// The id of a style that is `ids`' styles layered in order (see
    /// `layer`), made on first use and redone when one of them is
    /// redefined. It has no name.
    pub fn layeredStyleId(self: *SyntaxStyle, ids: []const u32) SyntaxStyleError!u32 {
        var key_buffer: [512]u8 = undefined;
        var writer: std.Io.Writer = .fixed(&key_buffer);
        for (ids, 0..) |id, i| {
            if (i > 0) writer.writeByte(':') catch return SyntaxStyleError.OutOfMemory;
            writer.print("{d}", .{id}) catch return SyntaxStyleError.OutOfMemory;
        }
        const key = writer.buffered();
        if (self.layered_ids.get(key)) |id| return id;

        const owned_key = self.allocator.dupe(u8, key) catch return SyntaxStyleError.OutOfMemory;
        const owned_ids = self.allocator.dupe(u32, ids) catch return SyntaxStyleError.OutOfMemory;
        const id = self.next_id;
        try self.id_to_style.put(self.allocator, id, self.layer(ids));
        try self.layers.put(self.allocator, id, owned_ids);
        try self.layered_ids.put(self.allocator, owned_key, id);
        self.next_id += 1;
        return id;
    }

    pub fn clearCache(self: *SyntaxStyle) void {
        self.merged_cache.clearRetainingCapacity();
    }

    pub fn getCacheSize(self: *const SyntaxStyle) usize {
        return self.merged_cache.count();
    }

    pub fn getStyleCount(self: *const SyntaxStyle) usize {
        // Layered styles are made, not registered.
        return self.id_to_style.count() - self.layers.count();
    }

    pub fn onDestroy(self: *SyntaxStyle, ctx: *anyopaque, handle: *const fn (*anyopaque) void) SyntaxStyleError!void {
        self.emitter.on(.Destroy, .{ .ctx = ctx, .handle = handle }) catch return SyntaxStyleError.OutOfMemory;
    }

    pub fn offDestroy(self: *SyntaxStyle, ctx: *anyopaque, handle: *const fn (*anyopaque) void) void {
        self.emitter.off(.Destroy, .{ .ctx = ctx, .handle = handle });
    }
};
