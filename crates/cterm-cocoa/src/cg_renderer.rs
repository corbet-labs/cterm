//! CoreGraphics-based terminal renderer
//!
//! Renders terminal content using CoreGraphics for text drawing.
//! This is simpler than Metal but sufficient for basic functionality.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_app_kit::{NSFont, NSFontManager, NSFontTraitMask, NSGraphicsContext};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use cterm_core::cell::CellAttrs;
use cterm_core::color::{Color, ColorPalette, Rgb};
use cterm_core::drcs::DrcsGlyph;
use cterm_core::TerminalImage;
use cterm_core::{CursorStyle, Terminal};
use cterm_ui::blink::{cell_foreground_visible, cursor_visible, BlinkPhase};
use cterm_ui::cursor::{extra_cursors_visible, resolve_extra_cursor_colors};
use cterm_ui::theme::Theme;

/// CoreGraphics renderer for terminal display
pub struct CGRenderer {
    font_name: String,
    font: Retained<NSFont>,
    bold_font: Retained<NSFont>,
    italic_font: Retained<NSFont>,
    bold_italic_font: Retained<NSFont>,
    theme: Theme,
    cell_width: f64,
    cell_height: f64,
    /// Whether bold text uses bright ANSI colors
    bold_is_bright: bool,
    /// Optional background color override (from template)
    background_override: Option<Rgb>,
}

impl CGRenderer {
    fn load_font(font_name: &str, font_size: f64) -> Retained<NSFont> {
        NSFont::fontWithName_size(&NSString::from_str(font_name), font_size)
            .or_else(|| NSFont::fontWithName_size(&NSString::from_str("Menlo"), font_size))
            .unwrap_or_else(|| NSFont::monospacedSystemFontOfSize_weight(font_size, 0.0))
    }

    /// Measure the exact cell size used by the renderer for this font.
    pub fn measure_cell_size(font_name: &str, font_size: f64) -> (f64, f64) {
        let font = Self::load_font(font_name, font_size);
        (Self::get_advance_for_glyph(&font), font_size * 1.2)
    }

    /// Create a new CoreGraphics renderer
    pub fn new(
        mtm: MainThreadMarker,
        font_name: &str,
        font_size: f64,
        theme: &Theme,
        bold_is_bright: bool,
    ) -> Self {
        let font = Self::load_font(font_name, font_size);

        // Create bold/italic/bold-italic variants via NSFontManager
        let fm = NSFontManager::sharedFontManager(mtm);
        let bold_font = fm.convertFont_toHaveTrait(&font, NSFontTraitMask::BoldFontMask);
        let italic_font = fm.convertFont_toHaveTrait(&font, NSFontTraitMask::ItalicFontMask);
        let bold_italic_font =
            fm.convertFont_toHaveTrait(&bold_font, NSFontTraitMask::ItalicFontMask);

        // Calculate cell dimensions using font metrics
        let cell_width = Self::get_advance_for_glyph(&font);
        let cell_height = font_size * 1.2; // Line height

        log::debug!(
            "CGRenderer: font_size={}, cell_width={}, cell_height={}",
            font_size,
            cell_width,
            cell_height
        );

        Self {
            font_name: font_name.to_string(),
            font,
            bold_font,
            italic_font,
            bold_italic_font,
            theme: theme.clone(),
            cell_width,
            cell_height,
            bold_is_bright,
            background_override: None,
        }
    }

    /// Return the current font size in points.
    pub fn font_size(&self) -> f64 {
        self.font.pointSize()
    }

    /// Rebuild the renderer fonts and cell metrics for a new point size.
    pub fn set_font_size(&mut self, mtm: MainThreadMarker, font_size: f64) {
        let font = Self::load_font(&self.font_name, font_size);
        let fm = NSFontManager::sharedFontManager(mtm);
        let bold_font = fm.convertFont_toHaveTrait(&font, NSFontTraitMask::BoldFontMask);
        let italic_font = fm.convertFont_toHaveTrait(&font, NSFontTraitMask::ItalicFontMask);
        let bold_italic_font =
            fm.convertFont_toHaveTrait(&bold_font, NSFontTraitMask::ItalicFontMask);

        self.cell_width = Self::get_advance_for_glyph(&font);
        self.cell_height = font_size * 1.2;
        self.font = font;
        self.bold_font = bold_font;
        self.italic_font = italic_font;
        self.bold_italic_font = bold_italic_font;

        log::debug!(
            "CGRenderer: font_size={}, cell_width={}, cell_height={}",
            font_size,
            self.cell_width,
            self.cell_height
        );
    }

    /// Set an optional background color override (hex string like "#1a1b26")
    pub fn set_background_override(&mut self, color: Option<&str>) {
        self.background_override = color.and_then(|hex| {
            let hex = hex.trim_start_matches('#');
            if hex.len() == 6 {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some(Rgb::new(r, g, b))
            } else {
                None
            }
        });
    }

    /// Get the advance width for a character
    fn get_advance_for_glyph(font: &NSFont) -> f64 {
        // Use 'M' width as cell width for monospace
        let advancement: NSSize = unsafe {
            let glyph: u32 = msg_send![font, glyphWithName: &*NSString::from_str("M")];
            msg_send![font, advancementForGlyph: glyph]
        };
        if advancement.width > 0.0 {
            advancement.width
        } else {
            // Fallback: estimate based on font size
            font.pointSize() * 0.6
        }
    }

    /// Get cell dimensions
    pub fn cell_size(&self) -> (f64, f64) {
        (self.cell_width, self.cell_height)
    }

    /// Render the terminal content
    pub fn render(&self, terminal: &Terminal, bounds: NSRect, blink_phase: BlinkPhase) {
        let Some(_context) = NSGraphicsContext::currentContext() else {
            log::warn!("No graphics context");
            return;
        };

        let screen = terminal.screen();
        let cols = screen.width();
        let rows = screen.height();
        let mut base_palette = self.theme.colors.clone();
        if let Some(background) = self.background_override {
            base_palette.background = background;
        }
        let palette = screen.resolved_palette(&base_palette);
        let normal_background = &palette.background;

        // Draw background
        self.draw_background(bounds, screen.modes.reverse_video, &palette);

        self.render_images(screen, cterm_core::ImageLayer::BehindCellBackground);

        for render_foreground in [false, true] {
            if render_foreground {
                self.render_images(screen, cterm_core::ImageLayer::BehindText);
            }

            // Draw cells in separate background and foreground passes so
            // negative-z Kitty images can sit precisely between them.
            for row in 0..rows {
                // Get absolute line for scrollback access and selection checking
                let absolute_line = screen.visible_row_to_absolute_line(row);

                for col in 0..cols {
                    if let Some(cell) = screen.get_cell_with_scrollback(absolute_line, col) {
                        // Skip wide char spacers - background handled by the wide cell
                        if cell.is_wide_spacer() {
                            continue;
                        }

                        let x = col as f64 * self.cell_width;
                        let y = row as f64 * self.cell_height;
                        let foreground_visible = cell_foreground_visible(cell.attrs, blink_phase);

                        // Check if cell is selected
                        let is_selected = screen.is_selected(absolute_line, col);

                        // XOR selection with INVERSE attribute to determine if colors should be inverted
                        let is_inverted = cell.attrs.contains(CellAttrs::INVERSE)
                            ^ is_selected
                            ^ screen.modes.reverse_video;

                        // Apply bold_is_bright: map base ANSI fg colors to bright variants
                        let fg = if self.bold_is_bright && cell.attrs.contains(CellAttrs::BOLD) {
                            match cell.fg {
                                Color::Ansi(ansi) => Color::Ansi(ansi.bright()),
                                Color::Indexed(idx @ 0..=7) => Color::Indexed(idx + 8),
                                other => other,
                            }
                        } else {
                            cell.fg
                        };

                        // Determine actual foreground and background colors
                        let (fg_color, bg_color) = if is_inverted {
                            // Inverted: swap foreground and background
                            let fg_rgb = if cell.bg.is_default() {
                                *normal_background
                            } else {
                                Self::color_to_rgb(screen, &cell.bg, &palette)
                            };
                            let bg_rgb = if fg.is_default() {
                                palette.foreground
                            } else {
                                Self::color_to_rgb(screen, &fg, &palette)
                            };
                            (fg_rgb, bg_rgb)
                        } else {
                            let bg = if cell.bg.is_default() {
                                *normal_background
                            } else {
                                Self::color_to_rgb(screen, &cell.bg, &palette)
                            };
                            (Self::color_to_rgb(screen, &fg, &palette), bg)
                        };

                        // Apply dim (SGR 2) — halve foreground brightness
                        let fg_color = if cell.attrs.contains(CellAttrs::DIM) {
                            Rgb::new(
                                (fg_color.r as f64 * 0.5) as u8,
                                (fg_color.g as f64 * 0.5) as u8,
                                (fg_color.b as f64 * 0.5) as u8,
                            )
                        } else {
                            fg_color
                        };

                        // Cornflower blue for hyperlinks with default foreground
                        let fg_color =
                            if cell.hyperlink.is_some() && fg.is_default() && !is_inverted {
                                Rgb::new(100, 149, 237)
                            } else {
                                fg_color
                            };

                        // Use double width for wide characters
                        let char_width = if cell.is_wide() {
                            self.cell_width * 2.0
                        } else {
                            self.cell_width
                        };

                        // Draw cell background if not default or if selected/inverted
                        if !render_foreground
                            && (!cell.bg.is_default()
                                || is_inverted
                                || is_selected
                                || screen.modes.reverse_video)
                        {
                            self.draw_cell_background_sized(x, y, char_width, &bg_color);
                        }

                        // Draw character
                        if render_foreground
                            && foreground_visible
                            && cell.text() != " "
                            && cell.text() != "\0"
                            && !cell.is_kitty_image_placeholder()
                            && !cell.attrs.contains(CellAttrs::HIDDEN)
                        {
                            // DRCS is defined for individual character positions,
                            // while normal text may be a full grapheme cluster.
                            let drcs = cell.single_char().and_then(|c| screen.get_drcs_for_char(c));
                            if let Some(glyph) = drcs {
                                self.draw_drcs_glyph(glyph, x, y, &fg_color);
                            } else {
                                self.draw_text_rgb(cell.text(), x, y, &fg_color, cell.attrs);
                            }
                        }

                        // Draw underlines (regular underline attributes or hyperlinks)
                        let has_hyperlink = cell.hyperlink.is_some();
                        let visible = foreground_visible && !cell.attrs.contains(CellAttrs::HIDDEN);
                        if render_foreground
                            && visible
                            && (cell.attrs.has_underline() || has_hyperlink)
                        {
                            // Use hyperlink color (blue) for hyperlinks, otherwise use underline color or fg
                            let underline_color = if has_hyperlink {
                                Rgb {
                                    r: 100,
                                    g: 149,
                                    b: 237,
                                } // Cornflower blue for hyperlinks
                            } else if let Some(ref uc) = cell.underline_color {
                                Self::color_to_rgb(screen, uc, &palette)
                            } else {
                                fg_color
                            };

                            self.draw_underline(
                                x,
                                y,
                                char_width,
                                &underline_color,
                                &cell.attrs,
                                has_hyperlink,
                            );
                        }

                        // Draw strikethrough
                        if render_foreground
                            && visible
                            && cell.attrs.contains(CellAttrs::STRIKETHROUGH)
                        {
                            self.draw_strikethrough(x, y, char_width, &fg_color);
                        }

                        // Draw overline
                        if render_foreground && visible && cell.attrs.contains(CellAttrs::OVERLINE)
                        {
                            self.draw_overline(x, y, char_width, &fg_color);
                        }
                    }
                }
            }
        }

        self.render_images(screen, cterm_core::ImageLayer::AboveText);

        if extra_cursors_visible(screen, blink_phase) {
            for cursor in screen.extra_cursors() {
                let colors = resolve_extra_cursor_colors(
                    screen,
                    &palette,
                    self.theme.cursor.text_color,
                    cursor.row,
                    cursor.col,
                );
                self.draw_cursor_cell(
                    screen,
                    cursor.row,
                    cursor.col,
                    cursor.shape.resolve(screen.cursor.style),
                    &colors.cursor,
                    &colors.text,
                );
            }
        }

        // Draw main cursor (only when visible and not scrolled back).
        let cursor = &screen.cursor;
        if cursor_visible(screen, blink_phase) {
            self.draw_cursor_cell(
                screen,
                cursor.row,
                cursor.col,
                cursor.style,
                &palette.cursor,
                &self.theme.cursor.text_color,
            );
        }

        // Draw scrollbar overlay when there is scrollback content
        let scrollback_len = screen.scrollback().len();
        if scrollback_len > 0 {
            self.draw_scrollbar(screen, bounds);
        }
    }

    /// Draw a thin scrollbar overlay on the right edge of the terminal
    fn draw_scrollbar(&self, screen: &cterm_core::Screen, bounds: NSRect) {
        let scrollback_len = screen.scrollback().len();
        let rows = screen.height();
        let total_lines = scrollback_len + rows;
        let view_height = bounds.size.height;

        // Scrollbar geometry
        let bar_width: f64 = 6.0;
        let bar_inset: f64 = 2.0;
        let bar_x = bounds.origin.x + bounds.size.width - bar_width - bar_inset;
        let min_thumb_height: f64 = 20.0;

        // Thumb height proportional to visible fraction
        let thumb_height = (rows as f64 / total_lines as f64 * view_height).max(min_thumb_height);

        // Thumb position: scroll_offset=0 means at bottom, scroll_offset=scrollback_len means at top
        let scrollable = view_height - thumb_height;
        let fraction = if scrollback_len > 0 {
            screen.scroll_offset as f64 / scrollback_len as f64
        } else {
            0.0
        };
        // In macOS coordinate system, y=0 is bottom. fraction=0 (at bottom) should
        // place thumb at y=0, fraction=1 (at top) at y=scrollable.
        // But our rendering uses flipped coordinates (y=0 at top), so:
        let thumb_y = (1.0 - fraction) * scrollable;

        // Draw thumb with rounded corners
        let opacity = if screen.scroll_offset > 0 { 0.5 } else { 0.25 };
        let Some(context) = NSGraphicsContext::currentContext() else {
            return;
        };
        unsafe {
            let cg_context = context.CGContext();
            let cg_context: *mut std::ffi::c_void = Retained::as_ptr(&cg_context).cast_mut().cast();

            #[repr(C)]
            #[derive(Copy, Clone)]
            struct CGPoint {
                x: f64,
                y: f64,
            }
            #[repr(C)]
            #[derive(Copy, Clone)]
            struct CGSize {
                width: f64,
                height: f64,
            }
            #[repr(C)]
            #[derive(Copy, Clone)]
            struct CGRect {
                origin: CGPoint,
                size: CGSize,
            }

            type CGPathRef = *const std::ffi::c_void;

            extern "C" {
                fn CGContextSetRGBFillColor(
                    c: *mut std::ffi::c_void,
                    r: f64,
                    g: f64,
                    b: f64,
                    a: f64,
                );
                fn CGContextFillRect(c: *mut std::ffi::c_void, rect: CGRect);
                fn CGContextSaveGState(c: *mut std::ffi::c_void);
                fn CGContextRestoreGState(c: *mut std::ffi::c_void);
                fn CGContextAddPath(c: *mut std::ffi::c_void, path: CGPathRef);
                fn CGContextClip(c: *mut std::ffi::c_void);
                fn CGPathCreateWithRoundedRect(
                    rect: CGRect,
                    corner_width: f64,
                    corner_height: f64,
                    transform: *const std::ffi::c_void,
                ) -> CGPathRef;
                fn CGPathRelease(path: CGPathRef);
            }

            if !cg_context.is_null() {
                let rect = CGRect {
                    origin: CGPoint {
                        x: bar_x,
                        y: thumb_y,
                    },
                    size: CGSize {
                        width: bar_width,
                        height: thumb_height,
                    },
                };
                let radius = bar_width / 2.0;
                let path = CGPathCreateWithRoundedRect(rect, radius, radius, std::ptr::null());
                CGContextSaveGState(cg_context);
                CGContextAddPath(cg_context, path);
                CGContextClip(cg_context);
                CGContextSetRGBFillColor(cg_context, 0.5, 0.5, 0.5, opacity);
                CGContextFillRect(cg_context, rect);
                CGContextRestoreGState(cg_context);
                CGPathRelease(path);
            }
        }
    }

    /// Render terminal images (Sixel graphics, etc.)
    fn render_images(&self, screen: &cterm_core::Screen, layer: cterm_core::ImageLayer) {
        for image in screen.visible_images_in_layer(layer) {
            if let Some(visible_row) = screen.image_visible_row(image) {
                let x = image.col as f64 * self.cell_width;
                let y = visible_row as f64 * self.cell_height;

                // Calculate display size (preserve aspect ratio, fit to pixel dimensions)
                let width = image.pixel_width as f64;
                let height = image.pixel_height as f64;

                self.draw_image(image, x, y, width, height);
            }
        }
    }

    /// Draw a terminal image at the specified position
    fn draw_image(&self, image: &TerminalImage, x: f64, y: f64, width: f64, height: f64) {
        // CoreGraphics FFI declarations
        #[repr(C)]
        #[derive(Copy, Clone)]
        struct CGPoint {
            x: f64,
            y: f64,
        }

        #[repr(C)]
        #[derive(Copy, Clone)]
        struct CGSize {
            width: f64,
            height: f64,
        }

        #[repr(C)]
        #[derive(Copy, Clone)]
        struct CGRect {
            origin: CGPoint,
            size: CGSize,
        }

        extern "C" {
            fn CGDataProviderCreateWithData(
                info: *mut std::ffi::c_void,
                data: *const u8,
                size: usize,
                release_callback: *const std::ffi::c_void,
            ) -> *mut std::ffi::c_void;

            fn CGImageCreate(
                width: usize,
                height: usize,
                bits_per_component: usize,
                bits_per_pixel: usize,
                bytes_per_row: usize,
                color_space: *mut std::ffi::c_void,
                bitmap_info: u32,
                provider: *mut std::ffi::c_void,
                decode: *const f64,
                should_interpolate: bool,
                intent: u32,
            ) -> *mut std::ffi::c_void;

            fn CGContextDrawImage(
                context: *mut std::ffi::c_void,
                rect: CGRect,
                image: *mut std::ffi::c_void,
            );

            fn CGImageRelease(image: *mut std::ffi::c_void);
            fn CGDataProviderRelease(provider: *mut std::ffi::c_void);
            fn CGColorSpaceCreateDeviceRGB() -> *mut std::ffi::c_void;
            fn CGColorSpaceRelease(color_space: *mut std::ffi::c_void);
            fn CGContextSaveGState(context: *mut std::ffi::c_void);
            fn CGContextRestoreGState(context: *mut std::ffi::c_void);
            fn CGContextTranslateCTM(context: *mut std::ffi::c_void, tx: f64, ty: f64);
            fn CGContextScaleCTM(context: *mut std::ffi::c_void, sx: f64, sy: f64);
        }

        unsafe {
            let data_ptr = image.data.as_ptr();
            let data_len = image.data.len();

            let cg_color_space = CGColorSpaceCreateDeviceRGB();
            if cg_color_space.is_null() {
                log::warn!("Failed to create CGColorSpace");
                return;
            }

            let provider = CGDataProviderCreateWithData(
                std::ptr::null_mut(),
                data_ptr,
                data_len,
                std::ptr::null(),
            );

            if provider.is_null() {
                log::warn!("Failed to create CGDataProvider for image");
                CGColorSpaceRelease(cg_color_space);
                return;
            }

            // Create CGImage
            // kCGImageAlphaLast = 3, kCGBitmapByteOrderDefault = 0
            const K_CG_IMAGE_ALPHA_LAST: u32 = 3;
            let cg_image = CGImageCreate(
                image.pixel_width,
                image.pixel_height,
                8,                     // bits per component
                32,                    // bits per pixel
                image.pixel_width * 4, // bytes per row
                cg_color_space,
                K_CG_IMAGE_ALPHA_LAST,
                provider,
                std::ptr::null(), // no decode array
                true,             // interpolate
                0,                // rendering intent (default)
            );

            CGDataProviderRelease(provider);
            CGColorSpaceRelease(cg_color_space);

            if cg_image.is_null() {
                log::warn!("Failed to create CGImage");
                return;
            }

            // Get current graphics context
            if let Some(context) = NSGraphicsContext::currentContext() {
                let cg_context = context.CGContext();
                let cg_context: *mut std::ffi::c_void =
                    Retained::as_ptr(&cg_context).cast_mut().cast();

                if !cg_context.is_null() {
                    CGContextSaveGState(cg_context);

                    // In a flipped view, we need to flip the image back
                    // Since our view is flipped (origin at top-left), but CGImage
                    // draws with origin at bottom-left, we need to flip vertically
                    CGContextTranslateCTM(cg_context, x, y + height);
                    CGContextScaleCTM(cg_context, 1.0, -1.0);

                    let draw_rect = CGRect {
                        origin: CGPoint { x: 0.0, y: 0.0 },
                        size: CGSize { width, height },
                    };

                    CGContextDrawImage(cg_context, draw_rect, cg_image);
                    CGContextRestoreGState(cg_context);
                }
            }

            CGImageRelease(cg_image);
        }
    }

    fn draw_background(&self, bounds: NSRect, reverse_video: bool, palette: &ColorPalette) {
        let bg = if reverse_video {
            &palette.foreground
        } else {
            &palette.background
        };
        unsafe {
            let color = Self::ns_color(bg.r, bg.g, bg.b);
            let _: () = msg_send![&*color, setFill];
            let _: () = msg_send![class!(NSBezierPath), fillRect: bounds];
        }
    }

    fn draw_cell_background_sized(&self, x: f64, y: f64, width: f64, rgb: &Rgb) {
        let rect = NSRect::new(NSPoint::new(x, y), NSSize::new(width, self.cell_height));
        unsafe {
            let ns_color = Self::ns_color(rgb.r, rgb.g, rgb.b);
            let _: () = msg_send![&*ns_color, setFill];
            let _: () = msg_send![class!(NSBezierPath), fillRect: rect];
        }
    }

    fn draw_text_rgb(&self, value: &str, x: f64, y: f64, rgb: &Rgb, attrs: CellAttrs) {
        let text = NSString::from_str(value);

        let font = match (
            attrs.contains(CellAttrs::BOLD),
            attrs.contains(CellAttrs::ITALIC),
        ) {
            (true, true) => &self.bold_italic_font,
            (true, false) => &self.bold_font,
            (false, true) => &self.italic_font,
            (false, false) => &self.font,
        };

        unsafe {
            let ns_color = Self::ns_color(rgb.r, rgb.g, rgb.b);

            // Use the actual string keys for NSAttributedString attributes
            let font_key = NSString::from_str("NSFont");
            let color_key = NSString::from_str("NSColor");

            let keys: [&AnyObject; 2] = [
                std::mem::transmute::<&NSString, &AnyObject>(&font_key),
                std::mem::transmute::<&NSString, &AnyObject>(&color_key),
            ];
            let values: [&AnyObject; 2] = [&**font, &*ns_color];

            let dict: Retained<AnyObject> = msg_send![
                class!(NSDictionary),
                dictionaryWithObjects: values.as_ptr(),
                forKeys: keys.as_ptr(),
                count: 2usize
            ];

            // In a flipped view, drawAtPoint places text with point as top-left of the text
            let point = NSPoint::new(x, y);
            let _: () = msg_send![&*text, drawAtPoint: point, withAttributes: &*dict];
        }
    }

    /// Draw a DRCS (soft font) glyph
    fn draw_drcs_glyph(&self, glyph: &DrcsGlyph, x: f64, y: f64, rgb: &Rgb) {
        // Calculate scaling factors to fit glyph into cell
        let scale_x = self.cell_width / glyph.width as f64;
        let scale_y = self.cell_height / glyph.height as f64;

        unsafe {
            let ns_color = Self::ns_color(rgb.r, rgb.g, rgb.b);
            let _: () = msg_send![&*ns_color, setFill];

            // Draw each pixel of the glyph as a small rectangle
            for gy in 0..glyph.height {
                for gx in 0..glyph.width {
                    if glyph.get_pixel(gx, gy) {
                        let px = x + gx as f64 * scale_x;
                        let py = y + gy as f64 * scale_y;
                        let rect = NSRect::new(
                            NSPoint::new(px, py),
                            NSSize::new(scale_x.ceil(), scale_y.ceil()),
                        );
                        let _: () = msg_send![class!(NSBezierPath), fillRect: rect];
                    }
                }
            }
        }
    }

    fn draw_cursor(&self, x: f64, y: f64, width: f64, style: CursorStyle, cursor_color: &Rgb) {
        let rect = match style {
            CursorStyle::Block => {
                NSRect::new(NSPoint::new(x, y), NSSize::new(width, self.cell_height))
            }
            CursorStyle::Underline => NSRect::new(
                NSPoint::new(x, y + self.cell_height - 2.0),
                NSSize::new(width, 2.0),
            ),
            CursorStyle::Bar => NSRect::new(NSPoint::new(x, y), NSSize::new(2.0, self.cell_height)),
        };
        unsafe {
            let color = Self::ns_color_alpha(cursor_color.r, cursor_color.g, cursor_color.b, 0.7);
            let _: () = msg_send![&*color, setFill];
            let _: () = msg_send![class!(NSBezierPath), fillRect: rect];
        }
    }

    fn draw_cursor_cell(
        &self,
        screen: &cterm_core::Screen,
        row: usize,
        col: usize,
        style: CursorStyle,
        cursor_color: &Rgb,
        text_color: &Rgb,
    ) {
        let x = col as f64 * self.cell_width;
        let y = row as f64 * self.cell_height;
        let width = screen
            .get_cell(row, col)
            .filter(|cell| cell.is_wide())
            .map_or(self.cell_width, |_| self.cell_width * 2.0);
        self.draw_cursor(x, y, width, style, cursor_color);

        if style == CursorStyle::Block {
            if let Some(cell) = screen.get_cell(row, col) {
                if cell.text() != " "
                    && cell.text() != "\0"
                    && !cell.is_kitty_image_placeholder()
                    && !cell.attrs.contains(CellAttrs::HIDDEN)
                {
                    self.draw_text_rgb(cell.text(), x, y, text_color, cell.attrs);
                }
            }
        }
    }

    /// Draw underline for a cell
    fn draw_underline(
        &self,
        x: f64,
        y: f64,
        width: f64,
        rgb: &Rgb,
        attrs: &CellAttrs,
        is_hyperlink: bool,
    ) {
        let underline_y = y + self.cell_height - 2.0;
        let thickness = 1.0;

        unsafe {
            let color = Self::ns_color(rgb.r, rgb.g, rgb.b);
            let _: () = msg_send![&*color, setStroke];

            // For hyperlinks or regular underline, draw a simple line
            if is_hyperlink || attrs.contains(CellAttrs::UNDERLINE) {
                let path: Retained<AnyObject> = msg_send![class!(NSBezierPath), bezierPath];
                let _: () = msg_send![&*path, setLineWidth: thickness];
                let _: () = msg_send![&*path, moveToPoint: NSPoint::new(x, underline_y)];
                let _: () = msg_send![&*path, lineToPoint: NSPoint::new(x + width, underline_y)];
                let _: () = msg_send![&*path, stroke];
            } else if attrs.contains(CellAttrs::DOUBLE_UNDERLINE) {
                // Double underline: two lines
                let path: Retained<AnyObject> = msg_send![class!(NSBezierPath), bezierPath];
                let _: () = msg_send![&*path, setLineWidth: thickness];
                let _: () = msg_send![&*path, moveToPoint: NSPoint::new(x, underline_y)];
                let _: () = msg_send![&*path, lineToPoint: NSPoint::new(x + width, underline_y)];
                let _: () = msg_send![&*path, moveToPoint: NSPoint::new(x, underline_y - 2.0)];
                let _: () =
                    msg_send![&*path, lineToPoint: NSPoint::new(x + width, underline_y - 2.0)];
                let _: () = msg_send![&*path, stroke];
            } else if attrs.contains(CellAttrs::CURLY_UNDERLINE) {
                // Curly underline: approximate with small waves
                let path: Retained<AnyObject> = msg_send![class!(NSBezierPath), bezierPath];
                let _: () = msg_send![&*path, setLineWidth: thickness];
                let wave_width = width / 4.0;
                let wave_height = 1.5;
                let _: () = msg_send![&*path, moveToPoint: NSPoint::new(x, underline_y)];
                for i in 0..4 {
                    let x1 = x + (i as f64 + 0.5) * wave_width;
                    let y1 = underline_y
                        + if i % 2 == 0 {
                            -wave_height
                        } else {
                            wave_height
                        };
                    let x2 = x + (i as f64 + 1.0) * wave_width;
                    let y2 = underline_y;
                    let _: () = msg_send![&*path, curveToPoint: NSPoint::new(x2, y2),
                        controlPoint1: NSPoint::new(x1, y1),
                        controlPoint2: NSPoint::new(x1, y1)];
                }
                let _: () = msg_send![&*path, stroke];
            } else if attrs.contains(CellAttrs::DOTTED_UNDERLINE) {
                // Dotted underline
                let path: Retained<AnyObject> = msg_send![class!(NSBezierPath), bezierPath];
                let _: () = msg_send![&*path, setLineWidth: thickness];
                let pattern: [f64; 2] = [2.0, 2.0];
                let _: () =
                    msg_send![&*path, setLineDash: pattern.as_ptr(), count: 2usize, phase: 0.0f64];
                let _: () = msg_send![&*path, moveToPoint: NSPoint::new(x, underline_y)];
                let _: () = msg_send![&*path, lineToPoint: NSPoint::new(x + width, underline_y)];
                let _: () = msg_send![&*path, stroke];
            } else if attrs.contains(CellAttrs::DASHED_UNDERLINE) {
                // Dashed underline
                let path: Retained<AnyObject> = msg_send![class!(NSBezierPath), bezierPath];
                let _: () = msg_send![&*path, setLineWidth: thickness];
                let pattern: [f64; 2] = [4.0, 2.0];
                let _: () =
                    msg_send![&*path, setLineDash: pattern.as_ptr(), count: 2usize, phase: 0.0f64];
                let _: () = msg_send![&*path, moveToPoint: NSPoint::new(x, underline_y)];
                let _: () = msg_send![&*path, lineToPoint: NSPoint::new(x + width, underline_y)];
                let _: () = msg_send![&*path, stroke];
            }
        }
    }

    /// Draw strikethrough for a cell
    fn draw_strikethrough(&self, x: f64, y: f64, width: f64, rgb: &Rgb) {
        let strike_y = y + self.cell_height * 0.5;
        unsafe {
            let color = Self::ns_color(rgb.r, rgb.g, rgb.b);
            let _: () = msg_send![&*color, setStroke];
            let path: Retained<AnyObject> = msg_send![class!(NSBezierPath), bezierPath];
            let _: () = msg_send![&*path, setLineWidth: 1.0f64];
            let _: () = msg_send![&*path, moveToPoint: NSPoint::new(x, strike_y)];
            let _: () = msg_send![&*path, lineToPoint: NSPoint::new(x + width, strike_y)];
            let _: () = msg_send![&*path, stroke];
        }
    }

    /// Draw overline for a cell
    fn draw_overline(&self, x: f64, y: f64, width: f64, rgb: &Rgb) {
        let overline_y = y + 1.0;
        unsafe {
            let color = Self::ns_color(rgb.r, rgb.g, rgb.b);
            let _: () = msg_send![&*color, setStroke];
            let path: Retained<AnyObject> = msg_send![class!(NSBezierPath), bezierPath];
            let _: () = msg_send![&*path, setLineWidth: 1.0f64];
            let _: () = msg_send![&*path, moveToPoint: NSPoint::new(x, overline_y)];
            let _: () = msg_send![&*path, lineToPoint: NSPoint::new(x + width, overline_y)];
            let _: () = msg_send![&*path, stroke];
        }
    }

    fn ns_color(r: u8, g: u8, b: u8) -> Retained<AnyObject> {
        Self::ns_color_alpha(r, g, b, 1.0)
    }

    fn ns_color_alpha(r: u8, g: u8, b: u8, a: f64) -> Retained<AnyObject> {
        unsafe {
            msg_send![
                class!(NSColor),
                colorWithRed: r as f64 / 255.0,
                green: g as f64 / 255.0,
                blue: b as f64 / 255.0,
                alpha: a
            ]
        }
    }

    fn color_to_rgb(screen: &cterm_core::Screen, color: &Color, palette: &ColorPalette) -> Rgb {
        screen.resolve_color(*color, palette)
    }

    /// Update theme colors
    pub fn set_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
    }

    /// Render IME marked text (composition text) at cursor position
    pub fn render_marked_text(&self, text: &str, cursor_row: usize, cursor_col: usize) {
        if text.is_empty() {
            return;
        }

        let x = cursor_col as f64 * self.cell_width;
        let y = cursor_row as f64 * self.cell_height;

        // Calculate the width of the marked text
        let char_count: usize = text.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
        let text_width = char_count as f64 * self.cell_width;

        // Draw background for marked text (slightly different from regular background)
        let bg_rect = NSRect::new(
            NSPoint::new(x, y),
            NSSize::new(text_width, self.cell_height),
        );
        unsafe {
            // Use a light yellow background for marked text
            let bg_color = Self::ns_color_alpha(255, 255, 200, 0.9);
            let _: () = msg_send![&*bg_color, setFill];
            let _: () = msg_send![class!(NSBezierPath), fillRect: bg_rect];
        }

        // Draw the marked text
        let ns_text = NSString::from_str(text);
        unsafe {
            // Use dark text color for marked text
            let text_color = Self::ns_color(0, 0, 0);

            let font_key = NSString::from_str("NSFont");
            let color_key = NSString::from_str("NSColor");

            let keys: [&AnyObject; 2] = [
                std::mem::transmute::<&NSString, &AnyObject>(&font_key),
                std::mem::transmute::<&NSString, &AnyObject>(&color_key),
            ];
            let values: [&AnyObject; 2] = [&*self.font, &*text_color];

            let dict: Retained<AnyObject> = msg_send![
                class!(NSDictionary),
                dictionaryWithObjects: values.as_ptr(),
                forKeys: keys.as_ptr(),
                count: 2usize
            ];

            let point = NSPoint::new(x, y);
            let _: () = msg_send![&*ns_text, drawAtPoint: point, withAttributes: &*dict];
        }

        // Draw underline to indicate composition
        let underline_rect = NSRect::new(
            NSPoint::new(x, y + self.cell_height - 2.0),
            NSSize::new(text_width, 2.0),
        );
        unsafe {
            let underline_color = Self::ns_color(0, 100, 200);
            let _: () = msg_send![&*underline_color, setFill];
            let _: () = msg_send![class!(NSBezierPath), fillRect: underline_rect];
        }
    }
}
