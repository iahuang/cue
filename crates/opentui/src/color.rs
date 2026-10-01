use std::ops::{BitOr, BitOrAssign};

const INTENT_RGB: u8 = 0;
const INTENT_INDEXED: u8 = 1;
const INTENT_DEFAULT: u8 = 2;

/// A cell color in OpenTUI's packed format (`ansi.RGBA`).
///
/// Each `u16` holds an 8-bit channel in its low byte and one byte of metadata
/// in its high byte: the palette slot (red) and the color intent (green). The
/// intent tells the renderer whether to emit truecolor, a 256-color index, or
/// the terminal's default color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Rgba(pub opentui_sys::Rgba);

impl Rgba {
    pub const BLACK: Rgba = Rgba::rgb(0, 0, 0);
    pub const WHITE: Rgba = Rgba::rgb(255, 255, 255);
    pub const TRANSPARENT: Rgba = Rgba::rgba(0, 0, 0, 0);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
        Rgba::rgba(r, g, b, 255)
    }

    #[allow(clippy::self_named_constructors)]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        pack(r, g, b, a, INTENT_RGB, 0)
    }

    /// A 256-color palette entry, emitted as `38;5;n`. The RGB snapshot is used
    /// for alpha blending.
    pub const fn indexed(index: u8) -> Rgba {
        let [r, g, b] = ansi256_to_rgb(index);
        pack(r, g, b, 255, INTENT_INDEXED, index)
    }

    /// Palette entry `index`, with `rgb` as what it shows (as the terminal
    /// reported it), for alpha blending and mixing.
    pub const fn indexed_as(index: u8, rgb: [u8; 3]) -> Rgba {
        pack(rgb[0], rgb[1], rgb[2], 255, INTENT_INDEXED, index)
    }

    /// The terminal's configured default color (SGR 39/49). `fallback` is used
    /// for alpha blending.
    pub const fn terminal_default(fallback: [u8; 3]) -> Rgba {
        pack(
            fallback[0],
            fallback[1],
            fallback[2],
            255,
            INTENT_DEFAULT,
            0,
        )
    }

    pub const fn r(self) -> u8 {
        self.0[0] as u8
    }
    pub const fn g(self) -> u8 {
        self.0[1] as u8
    }
    pub const fn b(self) -> u8 {
        self.0[2] as u8
    }
    pub const fn a(self) -> u8 {
        self.0[3] as u8
    }

    /// The palette entry, for a color made with [`Rgba::indexed`].
    pub const fn palette_index(self) -> Option<u8> {
        if (self.0[1] >> 8) as u8 == INTENT_INDEXED {
            Some((self.0[0] >> 8) as u8)
        } else {
            None
        }
    }

    /// Whether this is the terminal's default color.
    pub const fn is_terminal_default(self) -> bool {
        (self.0[1] >> 8) as u8 == INTENT_DEFAULT
    }

    pub(crate) fn as_ptr(&self) -> *const u16 {
        self.0.as_ptr()
    }
}

pub(crate) fn opt_ptr(color: &Option<Rgba>) -> *const u16 {
    color.as_ref().map_or(std::ptr::null(), Rgba::as_ptr)
}

const fn pack(r: u8, g: u8, b: u8, a: u8, intent: u8, slot: u8) -> Rgba {
    Rgba([
        r as u16 | (slot as u16) << 8,
        g as u16 | (intent as u16) << 8,
        b as u16,
        a as u16,
    ])
}

const ANSI16: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00],
    [0x80, 0x00, 0x00],
    [0x00, 0x80, 0x00],
    [0x80, 0x80, 0x00],
    [0x00, 0x00, 0x80],
    [0x80, 0x00, 0x80],
    [0x00, 0x80, 0x80],
    [0xc0, 0xc0, 0xc0],
    [0x80, 0x80, 0x80],
    [0xff, 0x00, 0x00],
    [0x00, 0xff, 0x00],
    [0xff, 0xff, 0x00],
    [0x00, 0x00, 0xff],
    [0xff, 0x00, 0xff],
    [0x00, 0xff, 0xff],
    [0xff, 0xff, 0xff],
];

const fn ansi256_to_rgb(index: u8) -> [u8; 3] {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    if index < 16 {
        ANSI16[index as usize]
    } else if index < 232 {
        let i = index - 16;
        [
            LEVELS[(i / 36) as usize],
            LEVELS[(i / 6 % 6) as usize],
            LEVELS[(i % 6) as usize],
        ]
    } else {
        let v = 8 + (index - 232) * 10;
        [v, v, v]
    }
}

/// Text attributes (`ansi.TextAttributes`), combinable with `|`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Attributes(pub u32);

impl Attributes {
    pub const NONE: Attributes = Attributes(0);
    pub const BOLD: Attributes = Attributes(1 << 0);
    pub const DIM: Attributes = Attributes(1 << 1);
    pub const ITALIC: Attributes = Attributes(1 << 2);
    pub const UNDERLINE: Attributes = Attributes(1 << 3);
    pub const BLINK: Attributes = Attributes(1 << 4);
    pub const INVERSE: Attributes = Attributes(1 << 5);
    pub const HIDDEN: Attributes = Attributes(1 << 6);
    pub const STRIKETHROUGH: Attributes = Attributes(1 << 7);

    pub const fn contains(self, other: Attributes) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for Attributes {
    type Output = Attributes;
    fn bitor(self, rhs: Attributes) -> Attributes {
        Attributes(self.0 | rhs.0)
    }
}

impl BitOrAssign for Attributes {
    fn bitor_assign(&mut self, rhs: Attributes) {
        self.0 |= rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_matches_native_layout() {
        // ansi.zig: packRGBA8(r, g, b, a, packMeta(intent, slot))
        assert_eq!(Rgba::rgba(1, 2, 3, 4).0, [1, 2, 3, 4]);
        assert_eq!(Rgba::indexed(196).0, [255 | 196 << 8, 1 << 8, 0, 255]);
        assert_eq!(Rgba::terminal_default([9, 8, 7]).0, [9, 8 | 2 << 8, 7, 255]);
        assert_eq!(Rgba::indexed(244).r(), 128);
    }
}
