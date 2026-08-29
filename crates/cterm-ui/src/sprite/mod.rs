// Copyright (c) 2023-present, Raphael Amorim.
//
// This source code is licensed under the MIT license recorded in
// THIRD_PARTY_LICENSES.md. Adapted from Rio's Sugarloaf sprite renderer.

//! Font-independent glyphs for terminal characters that must tile exactly.

mod block;
mod box_drawing;
mod braille;
mod canvas;
mod legacy;

use std::collections::hash_map::{Entry, HashMap};

use canvas::Canvas;

const MAX_SPRITE_DIMENSION: u32 = 512;

/// An alpha-only bitmap sized to exactly one terminal cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sprite {
    pub width: u16,
    pub height: u16,
    pub bytes: Vec<u8>,
}

/// Return whether cterm can draw this codepoint independently of the font.
#[inline]
pub fn is_drawable(cp: u32) -> bool {
    matches!(
        cp,
        0x2500..=0x259F
            | 0x2800..=0x28FF
            | 0x1FB00..=0x1FB3B
            | 0x1CD00..=0x1CDE5
            | 0x1CEA0
            | 0x1CEA3
            | 0x1CEA8
            | 0x1CEAB
            | 0x1FBE6
            | 0x1FBE7
    )
}

/// Rasterize a built-in glyph into an R8 coverage bitmap.
pub fn rasterize(cp: u32, cell_width: u32, cell_height: u32) -> Option<Sprite> {
    if cell_width == 0
        || cell_height == 0
        || cell_width > MAX_SPRITE_DIMENSION
        || cell_height > MAX_SPRITE_DIMENSION
    {
        return None;
    }

    let mut canvas = Canvas::new(cell_width, cell_height);
    let drawn = match cp {
        0x2500..=0x257F => box_drawing::draw(cp, &mut canvas),
        0x2580..=0x259F => block::draw(cp, &mut canvas),
        0x2800..=0x28FF => braille::draw(cp, &mut canvas),
        0x1FB00..=0x1FB3B
        | 0x1CD00..=0x1CDE5
        | 0x1CEA0
        | 0x1CEA3
        | 0x1CEA8
        | 0x1CEAB
        | 0x1FBE6
        | 0x1FBE7 => legacy::draw(cp, &mut canvas),
        _ => false,
    };
    drawn.then(|| Sprite {
        width: cell_width as u16,
        height: cell_height as u16,
        bytes: canvas.into_bytes(),
    })
}

/// Per-renderer cache for cell-sized built-in glyphs.
#[derive(Debug, Default)]
pub struct SpriteCache {
    sprites: HashMap<(u32, u16, u16), Sprite>,
}

impl SpriteCache {
    pub fn get(&mut self, cp: u32, cell_width: u32, cell_height: u32) -> Option<&Sprite> {
        if !is_drawable(cp) {
            return None;
        }
        let width = u16::try_from(cell_width).ok()?;
        let height = u16::try_from(cell_height).ok()?;
        let key = (cp, width, height);
        match self.sprites.entry(key) {
            Entry::Occupied(entry) => Some(entry.into_mut()),
            Entry::Vacant(entry) => Some(entry.insert(rasterize(cp, cell_width, cell_height)?)),
        }
    }

    pub fn clear(&mut self) {
        self.sprites.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_ranges_rasterize_at_odd_and_even_sizes() {
        for (width, height) in [(9, 19), (12, 24)] {
            let ranges = [
                0x2500..=0x259F,
                0x2800..=0x28FF,
                0x1FB00..=0x1FB3B,
                0x1CD00..=0x1CDE5,
            ];
            for cp in ranges.into_iter().flatten() {
                let sprite = rasterize(cp, width, height)
                    .unwrap_or_else(|| panic!("U+{cp:04X} should rasterize"));
                assert_eq!(sprite.bytes.len(), (width * height) as usize);
            }
            for cp in [0x1CEA0, 0x1CEA3, 0x1CEA8, 0x1CEAB, 0x1FBE6, 0x1FBE7] {
                assert!(rasterize(cp, width, height).is_some());
            }
        }
    }

    #[test]
    fn key_glyphs_have_expected_coverage() {
        let horizontal = rasterize(0x2500, 12, 24).unwrap();
        let middle = (horizontal.height as usize / 2) * horizontal.width as usize;
        assert!(horizontal.bytes[middle..middle + horizontal.width as usize]
            .iter()
            .all(|&alpha| alpha == 0xff));

        let full_block = rasterize(0x2588, 12, 24).unwrap();
        assert!(full_block.bytes.iter().all(|&alpha| alpha == 0xff));

        let full_braille = rasterize(0x28ff, 12, 24).unwrap();
        assert!(full_braille.bytes.contains(&0xff));
    }

    #[test]
    fn rejects_unknown_or_unreasonable_dimensions() {
        assert!(rasterize('A' as u32, 10, 20).is_none());
        assert!(rasterize(0x2500, 0, 20).is_none());
        assert!(rasterize(0x2500, MAX_SPRITE_DIMENSION + 1, 20).is_none());
    }

    #[test]
    fn cache_reuses_rasterized_sprite() {
        let mut cache = SpriteCache::default();
        let first = cache.get(0x2500, 10, 20).unwrap() as *const Sprite;
        let second = cache.get(0x2500, 10, 20).unwrap() as *const Sprite;
        assert_eq!(first, second);
    }
}
