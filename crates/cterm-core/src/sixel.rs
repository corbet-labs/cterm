//! Sixel graphics decoder.
//!
//! Sixel encodes six vertical pixels in each data byte. The decoder keeps
//! the DEC pixel aspect ratio and raster attributes in pixel space so the
//! resulting image can be handed directly to a renderer.

/// Maximum palette size implemented by current versions of foot.
pub const MAX_SIXEL_COLORS: usize = 1024;

/// Maximum configurable image dimension implemented by current foot.
pub const MAX_SIXEL_DIMENSION: usize = 10_000;

/// Default allocation budget for one decoded image (64 MiB of RGBA data).
///
/// The dimension limit deliberately matches foot, but a 10,000 x 10,000
/// RGBA image would consume 400 MB. The independent byte budget makes the
/// default safe for untrusted terminal output. Applications may raise it
/// explicitly when they also enforce an aggregate image-cache budget.
pub const DEFAULT_SIXEL_MAX_BYTES: usize = 64 * 1024 * 1024;

const BYTES_PER_PIXEL: usize = 4;
const DEFAULT_MAX_REPEAT: usize = MAX_SIXEL_DIMENSION;

/// Decoded sixel image as RGBA pixels.
#[derive(Debug, Clone)]
pub struct SixelImage {
    /// RGBA pixel data (4 bytes per pixel).
    pub data: Vec<u8>,
    /// Image width in pixels.
    pub width: usize,
    /// Image height in pixels.
    pub height: usize,
}

/// Resource and palette limits for a Sixel decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SixelDecoderConfig {
    /// Maximum decoded width. Values above 10,000 are clamped.
    pub max_width: usize,
    /// Maximum decoded height. Values above 10,000 are clamped.
    pub max_height: usize,
    /// Maximum bytes allocated for the RGBA backing image.
    pub max_bytes: usize,
    /// Number of addressable palette entries (2 through 1024).
    pub palette_size: usize,
    /// Maximum source repetitions accepted from one DECGRI sequence.
    pub max_repeat: usize,
}

impl Default for SixelDecoderConfig {
    fn default() -> Self {
        Self {
            max_width: MAX_SIXEL_DIMENSION,
            max_height: MAX_SIXEL_DIMENSION,
            max_bytes: DEFAULT_SIXEL_MAX_BYTES,
            palette_size: MAX_SIXEL_COLORS,
            max_repeat: DEFAULT_MAX_REPEAT,
        }
    }
}

impl SixelDecoderConfig {
    fn normalized(self) -> Self {
        Self {
            max_width: self.max_width.clamp(1, MAX_SIXEL_DIMENSION),
            max_height: self.max_height.clamp(1, MAX_SIXEL_DIMENSION),
            max_bytes: self.max_bytes.max(BYTES_PER_PIXEL),
            palette_size: self.palette_size.clamp(2, MAX_SIXEL_COLORS),
            max_repeat: self.max_repeat.clamp(1, MAX_SIXEL_DIMENSION),
        }
    }
}

/// Decoder output including the final palette.
///
/// Returning the palette separately allows the parser to implement DEC mode
/// 1070 (shared Sixel palettes) without exposing decoder internals.
#[derive(Debug, Clone)]
pub struct SixelDecodeResult {
    /// Decoded image, if the stream established a non-empty image.
    pub image: Option<SixelImage>,
    /// Palette after applying all color definitions in this stream.
    pub palette: Vec<[u8; 4]>,
    /// Whether input was clipped or ignored because a configured limit was
    /// reached or an allocation failed.
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    Normal,
    Repeat,
    Color,
    ColorDef,
    Raster,
}

/// Streaming Sixel decoder.
pub struct SixelDecoder {
    palette: Vec<[u8; 4]>,
    current_color: usize,
    transparent_bg: bool,
    /// Vertical pixel multiplier (Pan).
    pan: usize,
    /// Horizontal pixel multiplier (Pad).
    pad: usize,
    x: usize,
    /// Current band position in output pixels.
    band_y: usize,
    /// RGBA buffer using `allocated_width` as its stride.
    pixels: Vec<u8>,
    allocated_width: usize,
    allocated_height: usize,
    /// Logical image extent, including raster-attribute background.
    image_width: usize,
    image_height: usize,
    repeat_count: usize,
    parse_state: ParseState,
    accum: usize,
    color_params: [usize; 5],
    color_param_idx: usize,
    raster_params: [usize; 4],
    raster_param_idx: usize,
    config: SixelDecoderConfig,
    truncated: bool,
    #[cfg(test)]
    allocation_count: usize,
}

impl SixelDecoder {
    /// Create a decoder with DEC defaults.
    pub fn new() -> Self {
        Self::with_params(&[])
    }

    /// Create a decoder from DCS `P1;P2;P3` parameters.
    pub fn with_params(params: &[u16]) -> Self {
        Self::with_config(params, SixelDecoderConfig::default())
    }

    /// Create a decoder with explicit resource limits.
    pub fn with_config(params: &[u16], config: SixelDecoderConfig) -> Self {
        Self::with_config_and_palette(params, config, &[])
    }

    /// Create a decoder seeded with a caller-owned palette.
    ///
    /// At most `config.palette_size` entries are copied. Missing entries use
    /// the VT340-compatible default palette. The updated palette is available
    /// through [`Self::finish_with_palette`].
    pub fn with_config_and_palette(
        params: &[u16],
        config: SixelDecoderConfig,
        initial_palette: &[[u8; 4]],
    ) -> Self {
        let config = config.normalized();
        let mut palette = Self::default_palette(config.palette_size);
        let copy_count = palette.len().min(initial_palette.len());
        palette[..copy_count].copy_from_slice(&initial_palette[..copy_count]);

        // P1 defaults to 0, whose DEC aspect ratio is 2:1. P2=1 makes
        // untouched pixels transparent. P3 is intentionally ignored.
        let p1 = params.first().copied().unwrap_or(0);
        let pan = match p1 {
            2 => 5,
            3 | 4 => 3,
            7..=9 => 1,
            _ => 2,
        };
        let transparent_bg = params.get(1).copied().unwrap_or(0) == 1;

        Self {
            palette,
            current_color: 0,
            transparent_bg,
            pan,
            pad: 1,
            x: 0,
            band_y: 0,
            pixels: Vec::new(),
            allocated_width: 0,
            allocated_height: 0,
            image_width: 0,
            image_height: 0,
            repeat_count: 1,
            parse_state: ParseState::Normal,
            accum: 0,
            color_params: [0; 5],
            color_param_idx: 0,
            raster_params: [0; 4],
            raster_param_idx: 0,
            config,
            truncated: false,
            #[cfg(test)]
            allocation_count: 0,
        }
    }

    /// Current palette. This is primarily useful for shared-palette mode.
    pub fn palette(&self) -> &[[u8; 4]] {
        &self.palette
    }

    /// Whether any data has been clipped by a configured safety limit.
    pub fn was_truncated(&self) -> bool {
        self.truncated
    }

    /// Initialize the VT340-compatible palette at the requested size.
    pub fn default_palette(size: usize) -> Vec<[u8; 4]> {
        let size = size.clamp(2, MAX_SIXEL_COLORS);
        let mut palette = vec![[0, 0, 0, 255]; size];

        const VT340: [[u8; 4]; 16] = [
            [0, 0, 0, 255],
            [51, 51, 204, 255],
            [204, 33, 33, 255],
            [51, 204, 51, 255],
            [204, 51, 204, 255],
            [51, 204, 204, 255],
            [204, 204, 51, 255],
            [135, 135, 135, 255],
            [66, 66, 66, 255],
            [84, 84, 153, 255],
            [153, 66, 66, 255],
            [84, 153, 84, 255],
            [153, 84, 153, 255],
            [84, 153, 153, 255],
            [153, 153, 84, 255],
            [204, 204, 204, 255],
        ];
        let copy_count = size.min(VT340.len());
        palette[..copy_count].copy_from_slice(&VT340[..copy_count]);

        // Preserve cterm's deterministic extended grayscale defaults.
        let extended = size.saturating_sub(VT340.len());
        if extended > 0 {
            let denominator = extended.saturating_sub(1).max(1);
            for (offset, color) in palette.iter_mut().skip(VT340.len()).enumerate() {
                let gray = (offset * 255 / denominator) as u8;
                *color = [gray, gray, gray, 255];
            }
        }

        palette
    }

    /// Process one byte of Sixel payload.
    pub fn put(&mut self, byte: u8) {
        match self.parse_state {
            ParseState::Normal => self.put_normal(byte),
            ParseState::Repeat => self.put_repeat(byte),
            ParseState::Color => self.put_color(byte),
            ParseState::ColorDef => self.put_color_def(byte),
            ParseState::Raster => self.put_raster(byte),
        }
    }

    fn put_normal(&mut self, byte: u8) {
        match byte {
            b'!' => {
                self.parse_state = ParseState::Repeat;
                self.accum = 0;
            }
            b'#' => {
                self.parse_state = ParseState::Color;
                self.accum = 0;
                self.color_param_idx = 0;
                self.color_params = [0; 5];
            }
            b'"' => {
                self.parse_state = ParseState::Raster;
                self.accum = 0;
                self.raster_param_idx = 0;
                self.raster_params = [0; 4];
            }
            b'$' => self.x = 0,
            b'-' => {
                self.x = 0;
                self.band_y = self.band_y.saturating_add(6usize.saturating_mul(self.pan));
            }
            63..=126 => self.draw_sixel(byte - 63),
            _ => {}
        }
    }

    fn put_repeat(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => self.push_digit(byte),
            63..=126 => {
                self.repeat_count = self.accum.max(1).min(self.config.max_repeat);
                if self.accum > self.config.max_repeat {
                    self.truncated = true;
                }
                self.draw_sixel(byte - 63);
                self.repeat_count = 1;
                self.parse_state = ParseState::Normal;
            }
            _ => {
                self.parse_state = ParseState::Normal;
                self.put_normal(byte);
            }
        }
    }

    fn put_color(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => self.push_digit(byte),
            b';' => {
                self.store_color_param();
                self.parse_state = ParseState::ColorDef;
            }
            _ => {
                self.select_color(self.accum);
                self.parse_state = ParseState::Normal;
                self.put_normal(byte);
            }
        }
    }

    fn put_color_def(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => self.push_digit(byte),
            b';' => self.store_color_param(),
            _ => {
                self.store_color_param();
                self.define_color();
                self.parse_state = ParseState::Normal;
                self.put_normal(byte);
            }
        }
    }

    fn put_raster(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => self.push_digit(byte),
            b';' => self.store_raster_param(),
            _ => {
                self.store_raster_param();
                self.apply_raster_attributes();
                self.parse_state = ParseState::Normal;
                self.put_normal(byte);
            }
        }
    }

    fn push_digit(&mut self, byte: u8) {
        self.accum = self
            .accum
            .saturating_mul(10)
            .saturating_add((byte - b'0') as usize);
    }

    fn store_color_param(&mut self) {
        if self.color_param_idx < self.color_params.len() {
            self.color_params[self.color_param_idx] = self.accum;
            self.color_param_idx += 1;
        }
        self.accum = 0;
    }

    fn store_raster_param(&mut self) {
        if self.raster_param_idx < self.raster_params.len() {
            self.raster_params[self.raster_param_idx] = self.accum;
            self.raster_param_idx += 1;
        }
        self.accum = 0;
    }

    fn select_color(&mut self, requested: usize) {
        self.current_color = requested.min(self.palette.len() - 1);
    }

    fn define_color(&mut self) {
        self.select_color(self.color_params[0]);
        if self.color_param_idx < 5 {
            return;
        }

        match self.color_params[1] {
            // Sixel HLS primary hues are blue=0, red=120 and green=240.
            1 => {
                let hue = self.color_params[2].min(360);
                let lightness = self.color_params[3].min(100);
                let saturation = self.color_params[4].min(100);
                let rotated_hue = (hue + 240) % 360;
                let (r, g, b) = Self::hls_to_rgb(rotated_hue, lightness, saturation);
                self.palette[self.current_color] = [r, g, b, 255];
            }
            2 => {
                let r = ((self.color_params[2].min(100) * 255) / 100) as u8;
                let g = ((self.color_params[3].min(100) * 255) / 100) as u8;
                let b = ((self.color_params[4].min(100) * 255) / 100) as u8;
                self.palette[self.current_color] = [r, g, b, 255];
            }
            _ => {}
        }
    }

    fn apply_raster_attributes(&mut self) {
        let pan = self.raster_params[0].clamp(1, 5);
        let pad = self.raster_params[1].clamp(1, 5);

        // Like foot, retain the original ratio if output already exists.
        if self.image_width == 0 && self.image_height == 0 {
            self.pan = pan;
            self.pad = pad;
        }

        let width = match self.raster_params[2].checked_mul(self.pad) {
            Some(width) => width,
            None => {
                self.truncated = true;
                return;
            }
        };
        let height = match self.raster_params[3].checked_mul(self.pan) {
            Some(height) => height,
            None => {
                self.truncated = true;
                return;
            }
        };
        if width == 0 || height == 0 {
            return;
        }

        let requested_width = self.image_width.max(width);
        let requested_height = self.image_height.max(height);
        if self.ensure_size(requested_width, requested_height) {
            self.image_width = requested_width;
            self.image_height = requested_height;
        } else {
            self.truncated = true;
        }
    }

    /// Convert standard HSL to RGB (H: 0-359, L/S: 0-100).
    fn hls_to_rgb(h: usize, l: usize, s: usize) -> (u8, u8, u8) {
        let h = h as f64 / 360.0;
        let l = l as f64 / 100.0;
        let s = s as f64 / 100.0;

        if s == 0.0 {
            let gray = (l * 255.0) as u8;
            return (gray, gray, gray);
        }

        let q = if l < 0.5 {
            l * (1.0 + s)
        } else {
            l + s - l * s
        };
        let p = 2.0 * l - q;

        fn hue_to_rgb(p: f64, q: f64, mut t: f64) -> f64 {
            if t < 0.0 {
                t += 1.0;
            }
            if t > 1.0 {
                t -= 1.0;
            }
            if t < 1.0 / 6.0 {
                p + (q - p) * 6.0 * t
            } else if t < 1.0 / 2.0 {
                q
            } else if t < 2.0 / 3.0 {
                p + (q - p) * (2.0 / 3.0 - t) * 6.0
            } else {
                p
            }
        }

        let r = (hue_to_rgb(p, q, h + 1.0 / 3.0) * 255.0) as u8;
        let g = (hue_to_rgb(p, q, h) * 255.0) as u8;
        let b = (hue_to_rgb(p, q, h - 1.0 / 3.0) * 255.0) as u8;
        (r, g, b)
    }

    fn draw_sixel(&mut self, sixel: u8) {
        let stripe_height = 6usize.saturating_mul(self.pan);
        let required_height = self.band_y.saturating_add(stripe_height);
        if required_height > self.config.max_height || required_height == 0 {
            self.truncated = true;
            return;
        }

        // Bound repeats before iterating. The byte budget can impose a width
        // smaller than max_width for tall images.
        let budget_pixels = self.config.max_bytes / BYTES_PER_PIXEL;
        let budget_width = budget_pixels / required_height;
        let effective_max_width = self.config.max_width.min(budget_width);
        let repetitions_available = effective_max_width.saturating_sub(self.x) / self.pad;
        let repetitions = self.repeat_count.min(repetitions_available);

        if repetitions < self.repeat_count {
            self.truncated = true;
        }
        if repetitions == 0 {
            return;
        }

        let Some(required_width) = repetitions
            .checked_mul(self.pad)
            .and_then(|width| self.x.checked_add(width))
        else {
            self.truncated = true;
            return;
        };
        if !self.ensure_size(required_width, required_height) {
            self.truncated = true;
            return;
        }

        let color = self.palette[self.current_color];
        for _ in 0..repetitions {
            for column in 0..self.pad {
                for bit in 0..6 {
                    if (sixel >> bit) & 1 != 0 {
                        let first_y = self.band_y + bit * self.pan;
                        for row in 0..self.pan {
                            self.set_pixel(self.x + column, first_y + row, color);
                        }
                    }
                }
            }
            self.x += self.pad;
        }

        self.image_width = self.image_width.max(required_width);
        self.image_height = self.image_height.max(required_height);
    }

    fn ensure_size(&mut self, width: usize, height: usize) -> bool {
        if width == 0 || height == 0 || !self.allocation_allowed(width, height) {
            return false;
        }
        if width <= self.allocated_width && height <= self.allocated_height {
            return true;
        }

        let base_width = width.max(self.allocated_width);
        let base_height = height.max(self.allocated_height);
        let grown_width = Self::grown_dimension(self.allocated_width, width, self.config.max_width);
        let grown_height =
            Self::grown_dimension(self.allocated_height, height, self.config.max_height);

        // Prefer geometric growth. Near the budget retain slack in the axis
        // currently growing before falling back to the exact required extent.
        let candidates = [
            (grown_width, grown_height),
            (grown_width, base_height),
            (base_width, grown_height),
            (base_width, base_height),
        ];
        for (new_width, new_height) in candidates {
            if new_width < width
                || new_height < height
                || new_width < self.allocated_width
                || new_height < self.allocated_height
                || !self.allocation_allowed(new_width, new_height)
            {
                continue;
            }
            return self.reallocate(new_width, new_height);
        }
        false
    }

    fn allocation_allowed(&self, width: usize, height: usize) -> bool {
        if width > self.config.max_width || height > self.config.max_height {
            return false;
        }
        width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
            .is_some_and(|bytes| bytes <= self.config.max_bytes)
    }

    fn grown_dimension(current: usize, required: usize, limit: usize) -> usize {
        let geometric = if current == 0 {
            16
        } else {
            current.saturating_mul(2)
        };
        required.max(geometric).min(limit)
    }

    fn reallocate(&mut self, new_width: usize, new_height: usize) -> bool {
        let Some(new_len) = new_width
            .checked_mul(new_height)
            .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
        else {
            return false;
        };

        let mut new_pixels = Vec::new();
        if new_pixels.try_reserve_exact(new_len).is_err() {
            return false;
        }
        new_pixels.resize(new_len, 0);
        if !self.transparent_bg {
            let background = self.palette[0];
            for pixel in new_pixels.as_chunks_mut::<BYTES_PER_PIXEL>().0 {
                pixel.copy_from_slice(&background);
            }
        }

        if self.allocated_width > 0 {
            let old_row_bytes = self.allocated_width * BYTES_PER_PIXEL;
            let new_row_bytes = new_width * BYTES_PER_PIXEL;
            let rows = self.allocated_height.min(new_height);
            for row in 0..rows {
                let old_start = row * old_row_bytes;
                let new_start = row * new_row_bytes;
                new_pixels[new_start..new_start + old_row_bytes]
                    .copy_from_slice(&self.pixels[old_start..old_start + old_row_bytes]);
            }
        }

        self.pixels = new_pixels;
        self.allocated_width = new_width;
        self.allocated_height = new_height;
        #[cfg(test)]
        {
            self.allocation_count += 1;
        }
        true
    }

    fn set_pixel(&mut self, x: usize, y: usize, color: [u8; 4]) {
        debug_assert!(x < self.allocated_width);
        debug_assert!(y < self.allocated_height);
        let Some(index) = y
            .checked_mul(self.allocated_width)
            .and_then(|offset| offset.checked_add(x))
            .and_then(|offset| offset.checked_mul(BYTES_PER_PIXEL))
        else {
            self.truncated = true;
            return;
        };
        if let Some(pixel) = self.pixels.get_mut(index..index + BYTES_PER_PIXEL) {
            pixel.copy_from_slice(&color);
        } else {
            self.truncated = true;
        }
    }

    fn finish_pending_sequence(&mut self) {
        match self.parse_state {
            ParseState::Color => self.select_color(self.accum),
            ParseState::ColorDef => {
                self.store_color_param();
                self.define_color();
            }
            ParseState::Raster => {
                self.store_raster_param();
                self.apply_raster_attributes();
            }
            ParseState::Normal | ParseState::Repeat => {}
        }
        self.parse_state = ParseState::Normal;
    }

    /// Finalize decoding and return both the image and updated palette.
    pub fn finish_with_palette(mut self) -> SixelDecodeResult {
        self.finish_pending_sequence();

        let image = if self.image_width == 0 || self.image_height == 0 {
            None
        } else if self.image_width > self.allocated_width
            || self.image_height > self.allocated_height
            || self
                .image_width
                .checked_mul(self.image_height)
                .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
                .is_none_or(|bytes| bytes > self.pixels.len())
        {
            // All extent changes normally go through ensure_size(). Keep the
            // consuming API defensive so a rejected hint or future parser
            // change can never turn an allocation failure into a panic.
            self.truncated = true;
            None
        } else {
            let width = self.image_width;
            let height = self.image_height;
            let row_bytes = width * BYTES_PER_PIXEL;

            // Compact the geometric backing stride in place. Top-to-bottom is
            // safe because each destination begins before its source.
            for row in 1..height {
                let source = row * self.allocated_width * BYTES_PER_PIXEL;
                let destination = row * row_bytes;
                self.pixels
                    .copy_within(source..source + row_bytes, destination);
            }
            self.pixels.truncate(row_bytes * height);

            Some(SixelImage {
                data: self.pixels,
                width,
                height,
            })
        };

        SixelDecodeResult {
            image,
            palette: self.palette,
            truncated: self.truncated,
        }
    }

    /// Finalize decoding and return only the image.
    pub fn finish(self) -> Option<SixelImage> {
        self.finish_with_palette().image
    }
}

impl Default for SixelDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_all(decoder: &mut SixelDecoder, bytes: &[u8]) {
        for &byte in bytes {
            decoder.put(byte);
        }
    }

    fn one_to_one() -> SixelDecoder {
        SixelDecoder::with_params(&[7])
    }

    #[test]
    fn dec_default_aspect_ratio_is_two_to_one() {
        let mut decoder = SixelDecoder::new();
        decoder.put(b'~');
        let image = decoder.finish().unwrap();
        assert_eq!((image.width, image.height), (1, 12));
        for pixel in image.data.as_chunks::<4>().0 {
            assert_eq!(pixel[3], 255);
        }
    }

    #[test]
    fn dcs_p1_matches_dec_pixel_aspect_ratios() {
        for (p1, expected_pan) in [
            (0, 2),
            (1, 2),
            (2, 5),
            (3, 3),
            (4, 3),
            (5, 2),
            (6, 2),
            (7, 1),
            (8, 1),
            (9, 1),
            (10, 2),
        ] {
            let mut decoder = SixelDecoder::with_params(&[p1]);
            decoder.put(b'~');
            let image = decoder.finish().unwrap();
            assert_eq!(image.height, 6 * expected_pan, "P1={p1}");
        }
    }

    #[test]
    fn repeat_is_applied_and_bounded() {
        let config = SixelDecoderConfig {
            max_width: 32,
            max_height: 64,
            max_bytes: 32 * 64 * 4,
            ..SixelDecoderConfig::default()
        };
        let mut decoder = SixelDecoder::with_config(&[7], config);
        put_all(&mut decoder, b"!999999999999999999999999999999~");
        let result = decoder.finish_with_palette();
        assert_eq!(result.image.unwrap().width, 32);
        assert!(result.truncated);
    }

    #[test]
    fn color_select_uses_extended_palette() {
        let config = SixelDecoderConfig {
            palette_size: 1024,
            ..SixelDecoderConfig::default()
        };
        let mut palette = SixelDecoder::default_palette(1024);
        palette[700] = [12, 34, 56, 255];
        let mut decoder = SixelDecoder::with_config_and_palette(&[7], config, &palette);
        put_all(&mut decoder, b"#700~");
        let image = decoder.finish().unwrap();
        assert_eq!(&image.data[..4], &[12, 34, 56, 255]);
    }

    #[test]
    fn color_definition_is_returned_for_shared_palette_mode() {
        let mut decoder = one_to_one();
        put_all(&mut decoder, b"#1023;2;100;50;0~");
        let result = decoder.finish_with_palette();
        assert_eq!(result.palette.len(), 1024);
        assert_eq!(result.palette[1023], [255, 127, 0, 255]);
        assert_eq!(&result.image.unwrap().data[..4], &[255, 127, 0, 255]);
    }

    #[test]
    fn sixel_hls_uses_dec_hue_rotation() {
        for (hue, expected) in [
            (0, [0, 0, 255, 255]),
            (120, [255, 0, 0, 255]),
            (240, [0, 255, 0, 255]),
        ] {
            let mut decoder = one_to_one();
            put_all(&mut decoder, format!("#1;1;{hue};50;100~").as_bytes());
            let image = decoder.finish().unwrap();
            assert_eq!(&image.data[..4], &expected, "DEC hue {hue}");
        }
    }

    #[test]
    fn raster_attributes_set_aspect_and_geometry() {
        let mut decoder = one_to_one();
        // Pan=2, Pad=3, Ph=4, Pv=5 => 12x10 output pixels.
        put_all(&mut decoder, b"\"2;3;4;5");
        let image = decoder.finish().unwrap();
        assert_eq!((image.width, image.height), (12, 10));
        assert_eq!(image.data.len(), 12 * 10 * 4);
        assert!(image
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel == &[0, 0, 0, 255]));
    }

    #[test]
    fn raster_hint_expands_for_printed_sixels() {
        let mut decoder = one_to_one();
        put_all(&mut decoder, b"\"2;3;4;5~");
        // The hint is 12x10, but one sixel at Pan=2 is 12 pixels tall.
        let image = decoder.finish().unwrap();
        assert_eq!((image.width, image.height), (12, 12));
        for y in 0..12 {
            for x in 0..3 {
                assert_eq!(image.data[(y * 12 + x) * 4 + 3], 255);
            }
        }
    }

    #[test]
    fn raster_attributes_do_not_change_aspect_after_output() {
        let mut decoder = one_to_one();
        put_all(&mut decoder, b"~\"5;5;0;0~");
        let image = decoder.finish().unwrap();
        assert_eq!((image.width, image.height), (2, 6));
    }

    #[test]
    fn byte_budget_rejects_large_raster_but_keeps_decoding() {
        let config = SixelDecoderConfig {
            max_width: 10_000,
            max_height: 10_000,
            max_bytes: 1024,
            ..SixelDecoderConfig::default()
        };
        let mut decoder = SixelDecoder::with_config(&[7], config);
        put_all(&mut decoder, b"\"1;1;100;100~");
        let result = decoder.finish_with_palette();
        assert!(result.truncated);
        let image = result.image.unwrap();
        assert_eq!((image.width, image.height), (1, 6));
        assert!(image.data.len() <= config.max_bytes);
    }

    #[test]
    fn geometric_growth_avoids_per_pixel_reallocation() {
        let mut decoder = one_to_one();
        for _ in 0..1000 {
            decoder.put(b'~');
        }
        assert!(decoder.allocated_width >= 1000);
        assert!(
            decoder.allocation_count <= 8,
            "{}",
            decoder.allocation_count
        );
        let image = decoder.finish().unwrap();
        assert_eq!((image.width, image.height), (1000, 6));
    }

    #[test]
    fn transparent_background_remains_transparent() {
        let mut decoder = SixelDecoder::with_params(&[7, 1]);
        decoder.put(b'?');
        let image = decoder.finish().unwrap();
        assert_eq!((image.width, image.height), (1, 6));
        assert!(image.data.iter().all(|&component| component == 0));
    }

    #[test]
    fn line_feed_and_carriage_return_preserve_extent() {
        let mut decoder = one_to_one();
        put_all(&mut decoder, b"~~~$??-~");
        let image = decoder.finish().unwrap();
        assert_eq!((image.width, image.height), (3, 12));
    }
}
