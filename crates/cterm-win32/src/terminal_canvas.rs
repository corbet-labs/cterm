//! Terminal canvas with Direct2D rendering
//!
//! Hardware-accelerated terminal rendering using Direct2D and DirectWrite.

use std::collections::HashMap;

use cterm_core::color::{Color, Rgb};
use cterm_core::{Cell, CellAttrs, CursorStyle, Screen};
use cterm_ui::blink::{cell_foreground_visible, cursor_visible, BlinkPhase};
use cterm_ui::cursor::{cursor_footprint, extra_cursors_visible, resolve_extra_cursor_colors};
use cterm_ui::pane::PaneRect;
use cterm_ui::text_sizing::{
    is_multicell_render_anchor, multicell_is_selected, multicell_render_metrics,
};
use cterm_ui::theme::Theme;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_POINT_2F, D2D_RECT_F,
    D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1RenderTarget,
    ID2D1SolidColorBrush, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
    D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_BITMAP_PROPERTIES, D2D1_FACTORY_OPTIONS,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_ITALIC,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_TEXT_METRICS, DWRITE_TEXT_RANGE,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use crate::dpi::DpiInfo;

/// Cell dimensions
#[derive(Debug, Clone, Copy)]
pub struct CellDimensions {
    pub width: f32,
    pub height: f32,
    pub baseline: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GridPass {
    Background,
    Foreground,
}

#[derive(Clone, Copy)]
struct CellPosition {
    row: usize,
    col: usize,
    absolute_line: usize,
}

impl Default for CellDimensions {
    fn default() -> Self {
        Self {
            width: 8.0,
            height: 16.0,
            baseline: 12.0,
        }
    }
}

/// Terminal renderer using Direct2D
pub struct TerminalRenderer {
    factory: ID2D1Factory,
    dwrite_factory: IDWriteFactory,
    render_target: Option<ID2D1HwndRenderTarget>,
    text_format: Option<IDWriteTextFormat>,
    text_format_bold: Option<IDWriteTextFormat>,
    text_format_italic: Option<IDWriteTextFormat>,
    text_format_bold_italic: Option<IDWriteTextFormat>,
    cell_dims: CellDimensions,
    font_size: f32,
    font_family: String,
    theme: Theme,
    dpi: DpiInfo,
    brush_cache: HashMap<u32, ID2D1SolidColorBrush>,
    hwnd: HWND,
    /// Optional background color override (from template)
    background_override: Option<Rgb>,
    /// Origin of the pane currently being rendered.
    origin_x: f32,
    origin_y: f32,
}

impl TerminalRenderer {
    /// Create a new terminal renderer
    pub fn new(
        hwnd: HWND,
        theme: &Theme,
        font_family: &str,
        font_size: f32,
    ) -> windows::core::Result<Self> {
        // Create D2D factory
        let factory: ID2D1Factory = unsafe {
            D2D1CreateFactory(
                D2D1_FACTORY_TYPE_SINGLE_THREADED,
                Some(&D2D1_FACTORY_OPTIONS::default()),
            )?
        };

        // Create DirectWrite factory
        let dwrite_factory: IDWriteFactory =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };

        let mut renderer = Self {
            factory,
            dwrite_factory,
            render_target: None,
            text_format: None,
            text_format_bold: None,
            text_format_italic: None,
            text_format_bold_italic: None,
            cell_dims: CellDimensions::default(),
            font_size,
            font_family: font_family.to_string(),
            theme: theme.clone(),
            dpi: DpiInfo::system(),
            brush_cache: HashMap::new(),
            hwnd,
            background_override: None,
            origin_x: 0.0,
            origin_y: 0.0,
        };

        renderer.create_device_resources()?;

        Ok(renderer)
    }

    /// Create device-dependent resources
    fn create_device_resources(&mut self) -> windows::core::Result<()> {
        // Get window size
        let mut rect = RECT::default();
        unsafe { GetClientRect(self.hwnd, &mut rect)? };

        // Ensure minimum size of 1x1 to avoid D2D errors
        let width = ((rect.right - rect.left) as u32).max(1);
        let height = ((rect.bottom - rect.top) as u32).max(1);

        // Get DPI
        self.dpi = DpiInfo::for_window(self.hwnd);

        // Create render target properties
        let render_props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: self.dpi.dpi as f32,
            dpiY: self.dpi.dpi as f32,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };

        let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd: self.hwnd,
            pixelSize: D2D_SIZE_U { width, height },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };

        // Create HWND render target
        let render_target = unsafe {
            self.factory
                .CreateHwndRenderTarget(&render_props, &hwnd_props)?
        };

        unsafe {
            render_target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
            render_target.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        }

        self.render_target = Some(render_target);
        self.brush_cache.clear();

        // Create text format
        self.create_text_format()?;

        Ok(())
    }

    /// Create text format and measure cell dimensions
    fn create_text_format(&mut self) -> windows::core::Result<()> {
        let scaled_font_size = self.dpi.scale_f32(self.font_size);

        // Locale for DirectWrite (empty string = user default)
        let locale: Vec<u16> = "".encode_utf16().chain(std::iter::once(0)).collect();

        // Try each font in the comma-separated list until one works
        let font_families: Vec<&str> = self.font_family.split(',').map(|s| s.trim()).collect();

        let mut text_format = None;
        let mut text_format_bold = None;
        let mut text_format_italic = None;
        let mut text_format_bold_italic = None;

        for font_family in &font_families {
            let font_family_wide: Vec<u16> = font_family
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();

            let create_format = |weight, style| unsafe {
                self.dwrite_factory.CreateTextFormat(
                    PCWSTR(font_family_wide.as_ptr()),
                    None,
                    weight,
                    style,
                    DWRITE_FONT_STRETCH_NORMAL,
                    scaled_font_size,
                    PCWSTR(locale.as_ptr()),
                )
            };

            if let (Ok(normal), Ok(bold), Ok(italic), Ok(bold_italic)) = (
                create_format(DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL),
                create_format(DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_STYLE_NORMAL),
                create_format(DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_ITALIC),
                create_format(DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_STYLE_ITALIC),
            ) {
                text_format = Some(normal);
                text_format_bold = Some(bold);
                text_format_italic = Some(italic);
                text_format_bold_italic = Some(bold_italic);
                log::info!("Using font: {}", font_family);
                break;
            }
        }

        // If no font worked, return error
        let text_format = text_format.ok_or_else(|| {
            let msg = format!("No suitable font found in: {}", self.font_family);
            windows::core::Error::new(windows::core::HRESULT(-1), msg)
        })?;
        let text_format_bold = text_format_bold.unwrap();
        let text_format_italic = text_format_italic.unwrap();
        let text_format_bold_italic = text_format_bold_italic.unwrap();

        // Measure cell dimensions using 'M' character
        let test_char: Vec<u16> = "M".encode_utf16().collect();
        let layout: IDWriteTextLayout = unsafe {
            self.dwrite_factory
                .CreateTextLayout(&test_char, &text_format, 1000.0, 1000.0)?
        };

        let mut metrics = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&mut metrics)? };

        self.cell_dims = CellDimensions {
            width: metrics.width,
            height: metrics.height * 1.1,    // Add some line spacing
            baseline: metrics.height * 0.85, // Approximate baseline
        };

        self.text_format = Some(text_format);
        self.text_format_bold = Some(text_format_bold);
        self.text_format_italic = Some(text_format_italic);
        self.text_format_bold_italic = Some(text_format_bold_italic);

        Ok(())
    }

    /// Get or create a solid color brush
    fn get_brush(&mut self, color: Rgb) -> windows::core::Result<ID2D1SolidColorBrush> {
        let key = (color.r as u32) << 16 | (color.g as u32) << 8 | (color.b as u32);

        if let Some(brush) = self.brush_cache.get(&key) {
            return Ok(brush.clone());
        }

        // Clone and cast to parent interface to access methods
        let rt = self.render_target.clone().unwrap();
        let base: ID2D1RenderTarget = rt.cast()?;
        let d2d_color = rgb_to_d2d_color(color);
        let brush = unsafe { base.CreateSolidColorBrush(&d2d_color, None)? };

        self.brush_cache.insert(key, brush.clone());
        Ok(brush)
    }

    /// Resize the render target
    pub fn resize(&mut self, width: u32, height: u32) -> windows::core::Result<()> {
        if let Some(ref rt) = self.render_target {
            let size = D2D_SIZE_U { width, height };
            unsafe { rt.Resize(&size)? };
        }
        Ok(())
    }

    /// Handle DPI change
    pub fn update_dpi(&mut self, dpi: u32) -> windows::core::Result<()> {
        self.dpi = DpiInfo::from_dpi(dpi);
        self.create_device_resources()
    }

    /// Get the cell dimensions
    pub fn cell_dimensions(&self) -> CellDimensions {
        self.cell_dims
    }

    /// Clone the Direct2D/DirectWrite resources used by native window chrome.
    pub fn chrome_resources(
        &self,
    ) -> Option<(ID2D1HwndRenderTarget, IDWriteFactory, IDWriteTextFormat)> {
        Some((
            self.render_target.clone()?,
            self.dwrite_factory.clone(),
            self.text_format.clone()?,
        ))
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

    /// Calculate terminal size in cells
    pub fn terminal_size(&self, width: u32, height: u32) -> (usize, usize) {
        let cols = (width as f32 / self.cell_dims.width).floor() as usize;
        let rows = (height as f32 / self.cell_dims.height).floor() as usize;
        (cols.max(1), rows.max(1))
    }

    /// Begin a window frame using the active terminal's background color.
    pub fn begin_frame(&self, screen: &Screen) {
        let Some(rt) = self.render_target.as_ref() else {
            return;
        };
        let palette = self.resolved_palette(screen);
        let background = if screen.modes.reverse_video {
            palette.foreground
        } else {
            palette.background
        };
        unsafe {
            rt.BeginDraw();
            rt.Clear(Some(&rgb_to_d2d_color(background)));
        }
    }

    /// Render one terminal into a clipped pane rectangle in client pixels.
    pub fn render_pane(
        &mut self,
        screen: &Screen,
        rect: PaneRect,
        active: bool,
        alerted: bool,
        blink_phase: BlinkPhase,
    ) -> windows::core::Result<()> {
        let Some(rt) = self.render_target.clone() else {
            return Ok(());
        };
        if rect.width == 0 || rect.height == 0 {
            return Ok(());
        }

        self.origin_x = rect.x as f32;
        self.origin_y = rect.y as f32;
        let clip = D2D_RECT_F {
            left: self.origin_x,
            top: self.origin_y,
            right: self.origin_x + rect.width as f32,
            bottom: self.origin_y + rect.height as f32,
        };
        let base: ID2D1RenderTarget = rt.cast()?;
        let palette = self.resolved_palette(screen);
        let background = if screen.modes.reverse_video {
            palette.foreground
        } else {
            palette.background
        };
        let background_brush = self.get_brush(background)?;

        unsafe {
            base.FillRectangle(&clip, &background_brush);
            base.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        }
        let draw_result = (|| {
            self.draw_images(screen, cterm_core::ImageLayer::BehindCellBackground)?;
            self.draw_grid(screen, blink_phase, GridPass::Background)?;
            self.draw_images(screen, cterm_core::ImageLayer::BehindText)?;
            self.draw_grid(screen, blink_phase, GridPass::Foreground)?;
            self.draw_images(screen, cterm_core::ImageLayer::AboveText)?;
            self.draw_extra_cursors(screen, blink_phase)?;
            if active {
                self.draw_cursor(screen, blink_phase)?;
            }
            Ok::<(), windows::core::Error>(())
        })();
        unsafe { base.PopAxisAlignedClip() };
        draw_result?;

        let border_color = if alerted {
            palette.ansi[3]
        } else if active {
            palette.cursor
        } else {
            palette.ansi[8]
        };
        let border_brush = self.get_brush(border_color)?;
        let border = D2D_RECT_F {
            left: clip.left + 0.5,
            top: clip.top + 0.5,
            right: (clip.right - 0.5).max(clip.left + 0.5),
            bottom: (clip.bottom - 0.5).max(clip.top + 0.5),
        };
        unsafe { base.DrawRectangle(&border, &border_brush, 1.0, None) };
        Ok(())
    }

    /// End and present the current window frame.
    pub fn end_frame(&self) -> windows::core::Result<()> {
        if let Some(rt) = self.render_target.as_ref() {
            unsafe { rt.EndDraw(None, None)? };
        }
        Ok(())
    }

    /// Render the terminal screen as a single full-window pane.
    pub fn render(
        &mut self,
        screen: &Screen,
        blink_phase: BlinkPhase,
    ) -> windows::core::Result<()> {
        if self.render_target.is_none() {
            return Ok(());
        }
        let mut client = RECT::default();
        unsafe { GetClientRect(self.hwnd, &mut client)? };
        self.begin_frame(screen);
        self.render_pane(
            screen,
            PaneRect::new(
                0,
                0,
                (client.right - client.left).max(1) as u32,
                (client.bottom - client.top).max(1) as u32,
            ),
            true,
            false,
            blink_phase,
        )?;
        self.end_frame()
    }

    /// Draw decoded SIXEL and other inline images through Direct2D.
    fn draw_images(
        &self,
        screen: &Screen,
        layer: cterm_core::ImageLayer,
    ) -> windows::core::Result<()> {
        let rt = self.render_target.clone().unwrap();
        let base: ID2D1RenderTarget = rt.cast()?;
        let bitmap_properties = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };

        for image in screen.visible_images_in_layer(layer) {
            let Some(visible_row) = screen.image_visible_row(image) else {
                continue;
            };
            let Some(pitch) = image
                .pixel_width
                .checked_mul(4)
                .and_then(|pitch| u32::try_from(pitch).ok())
            else {
                log::warn!("Terminal image {} has an invalid Direct2D pitch", image.id);
                continue;
            };
            let Some(pixels) = cterm_ui::rgba_to_premultiplied_bgra(image.data.as_slice()) else {
                log::warn!("Terminal image {} has invalid RGBA data", image.id);
                continue;
            };
            let Ok(width) = u32::try_from(image.pixel_width) else {
                log::warn!("Terminal image {} is too wide for Direct2D", image.id);
                continue;
            };
            let Ok(height) = u32::try_from(image.pixel_height) else {
                log::warn!("Terminal image {} is too tall for Direct2D", image.id);
                continue;
            };

            let bitmap = unsafe {
                base.CreateBitmap(
                    D2D_SIZE_U { width, height },
                    Some(pixels.as_ptr().cast()),
                    pitch,
                    &bitmap_properties,
                )?
            };
            let x = self.origin_x + image.col as f32 * self.cell_dims.width;
            let y = self.origin_y + visible_row as f32 * self.cell_dims.height;
            let destination = D2D_RECT_F {
                left: x,
                top: y,
                right: x + image.pixel_width as f32,
                bottom: y + image.pixel_height as f32,
            };

            unsafe {
                base.DrawBitmap(
                    &bitmap,
                    Some(&destination),
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                    None,
                );
            }
        }

        Ok(())
    }

    /// Draw the terminal grid
    fn draw_grid(
        &mut self,
        screen: &Screen,
        blink_phase: BlinkPhase,
        pass: GridPass,
    ) -> windows::core::Result<()> {
        let grid = screen.grid();
        let rows = grid.height();
        let cols = grid.width();
        let visible_top = screen.visible_row_to_absolute_line(0);

        for row in 0..rows {
            let absolute_line = screen.visible_row_to_absolute_line(row);

            for col in 0..cols {
                if let Some(cell) = screen.get_cell_with_scrollback(absolute_line, col) {
                    if cell.is_wide_spacer()
                        || cell.multicell.as_ref().is_some_and(|multicell| {
                            !is_multicell_render_anchor(multicell, absolute_line, visible_top)
                        })
                    {
                        continue;
                    }
                    self.draw_cell(
                        CellPosition {
                            row,
                            col,
                            absolute_line,
                        },
                        cell,
                        screen,
                        blink_phase,
                        pass,
                    )?;
                }
            }
        }

        Ok(())
    }

    /// Draw a single cell
    fn draw_cell(
        &mut self,
        position: CellPosition,
        cell: &Cell,
        screen: &Screen,
        blink_phase: BlinkPhase,
        pass: GridPass,
    ) -> windows::core::Result<()> {
        let multicell = cell.multicell.as_ref();
        let metrics = multicell.map(multicell_render_metrics);
        let x = self.origin_x + position.col as f32 * self.cell_dims.width;
        let cell_y = self.origin_y + position.row as f32 * self.cell_dims.height;
        let y = metrics.map_or(cell_y, |_| {
            cell_y
                - self.cell_dims.height
                    * f32::from(multicell.expect("metrics require metadata").row_offset)
        });
        let text_x = x + metrics.map_or(0.0, |metrics| {
            metrics.horizontal_offset as f32 * self.cell_dims.width
        });
        let text_y = y + metrics.map_or(0.0, |metrics| {
            metrics.vertical_offset as f32 * self.cell_dims.height
        });

        let attrs = cell.attrs;
        let foreground_visible = cell_foreground_visible(attrs, blink_phase);
        let is_selected = multicell.map_or_else(
            || screen.is_selected(position.absolute_line, position.col),
            |multicell| {
                multicell_is_selected(screen, position.absolute_line, position.col, multicell)
            },
        );
        let (fg, bg) = self.resolve_colors(cell, screen, screen.modes.reverse_video, is_selected);
        let palette = self.resolved_palette(screen);
        let cell_width = if let Some(multicell) = multicell {
            self.cell_dims.width * f32::from(multicell.columns)
        } else if cell.is_wide() {
            self.cell_dims.width * 2.0
        } else {
            self.cell_dims.width
        };
        let cell_height = metrics.map_or(self.cell_dims.height, |metrics| {
            self.cell_dims.height * metrics.rows as f32
        });

        // Get brushes first (this mutably borrows self temporarily)
        let canvas_background = if screen.modes.reverse_video {
            palette.foreground
        } else {
            palette.background
        };
        let bg_brush = if pass == GridPass::Background && (bg != canvas_background || is_selected) {
            Some(self.get_brush(bg)?)
        } else {
            None
        };

        let text = multicell.map_or_else(|| cell.text(), |multicell| multicell.text());
        let has_hyperlink = cell.hyperlink.is_some();
        let needs_fg = pass == GridPass::Foreground
            && foreground_visible
            && (text != " " && text != "\0"
                || attrs.has_underline()
                || has_hyperlink
                || attrs.intersects(CellAttrs::STRIKETHROUGH | CellAttrs::OVERLINE));
        let fg_brush = if needs_fg {
            Some(self.get_brush(fg)?)
        } else {
            None
        };

        let underline_brush = if pass == GridPass::Foreground
            && foreground_visible
            && (attrs.has_underline() || has_hyperlink)
        {
            let color = if has_hyperlink {
                Rgb::new(100, 149, 237)
            } else if let Some(color) = cell.underline_color {
                screen.resolve_color(color, &palette)
            } else {
                fg
            };
            Some(self.get_brush(color)?)
        } else {
            None
        };

        // Clone and cast to parent interface to access methods
        let rt = self.render_target.clone().unwrap();
        let base: ID2D1RenderTarget = rt.cast()?;

        // Draw background if not default
        if let Some(ref brush) = bg_brush {
            let rect = D2D_RECT_F {
                left: x,
                top: y,
                right: x + cell_width,
                bottom: y + cell_height,
            };
            unsafe { base.FillRectangle(&rect, brush) };
        }

        // Draw character
        if pass == GridPass::Foreground
            && foreground_visible
            && text != " "
            && text != "\0"
            && !cell.is_kitty_image_placeholder()
            && !attrs.contains(CellAttrs::HIDDEN)
        {
            let text_format = match (
                attrs.contains(CellAttrs::BOLD),
                attrs.contains(CellAttrs::ITALIC),
            ) {
                (true, true) => self.text_format_bold_italic.as_ref().unwrap(),
                (true, false) => self.text_format_bold.as_ref().unwrap(),
                (false, true) => self.text_format_italic.as_ref().unwrap(),
                (false, false) => self.text_format.as_ref().unwrap(),
            };

            let utf16: Vec<u16> = text.encode_utf16().collect();

            let layout: IDWriteTextLayout = unsafe {
                self.dwrite_factory.CreateTextLayout(
                    &utf16,
                    text_format,
                    (cell_width - (text_x - x)).max(self.cell_dims.width),
                    (cell_height - (text_y - y)).max(self.cell_dims.height),
                )?
            };

            if let Some(metrics) = metrics {
                unsafe {
                    layout.SetFontSize(
                        self.dpi.scale_f32(self.font_size) * metrics.font_scale as f32,
                        DWRITE_TEXT_RANGE {
                            startPosition: 0,
                            length: utf16.len() as u32,
                        },
                    )?;
                }
            }

            let origin = D2D_POINT_2F {
                x: text_x,
                y: text_y,
            };
            unsafe {
                base.DrawTextLayout(
                    origin,
                    &layout,
                    fg_brush.as_ref().unwrap(),
                    Default::default(),
                )
            };
        }

        // Draw underline (also for hyperlinks)
        let visible = foreground_visible && !attrs.contains(CellAttrs::HIDDEN);
        if pass == GridPass::Foreground && visible && (attrs.has_underline() || has_hyperlink) {
            let underline_y =
                y + cell_height - (self.cell_dims.height - self.cell_dims.baseline) + 2.0;
            self.draw_underline_pattern(
                &base,
                underline_brush.as_ref().unwrap(),
                x,
                x + cell_width,
                underline_y,
                attrs,
            );
        }

        // Draw strikethrough
        if pass == GridPass::Foreground && visible && attrs.contains(CellAttrs::STRIKETHROUGH) {
            let strike_y = y + cell_height / 2.0;
            unsafe {
                base.DrawLine(
                    D2D_POINT_2F { x, y: strike_y },
                    D2D_POINT_2F {
                        x: x + cell_width,
                        y: strike_y,
                    },
                    fg_brush.as_ref().unwrap(),
                    1.0,
                    None,
                )
            };
        }

        if pass == GridPass::Foreground && visible && attrs.contains(CellAttrs::OVERLINE) {
            unsafe {
                base.DrawLine(
                    D2D_POINT_2F { x, y: y + 1.0 },
                    D2D_POINT_2F {
                        x: x + cell_width,
                        y: y + 1.0,
                    },
                    fg_brush.as_ref().unwrap(),
                    1.0,
                    None,
                )
            };
        }

        Ok(())
    }

    /// Resolve foreground and background colors from a cell
    fn resolve_colors(
        &self,
        cell: &Cell,
        screen: &Screen,
        reverse_video: bool,
        selected: bool,
    ) -> (Rgb, Rgb) {
        let palette = self.resolved_palette(screen);
        let normal_background = palette.background;

        let mut fg = screen.resolve_color(cell.fg, &palette);
        let mut bg = if cell.bg == Color::Default {
            normal_background
        } else {
            screen.resolve_color(cell.bg, &palette)
        };

        // Handle inverse
        let is_inverted = cell.attrs.contains(CellAttrs::INVERSE) ^ reverse_video ^ selected;
        if is_inverted {
            std::mem::swap(&mut fg, &mut bg);
        }

        // Handle dim
        if cell.attrs.contains(CellAttrs::DIM) {
            fg = Rgb::new(fg.r / 2, fg.g / 2, fg.b / 2);
        }

        // Cornflower blue for hyperlinks with default foreground
        if cell.hyperlink.is_some() && cell.fg == Color::Default && !is_inverted {
            fg = Rgb::new(100, 149, 237);
        }

        (fg, bg)
    }

    fn draw_underline_pattern(
        &self,
        target: &ID2D1RenderTarget,
        brush: &ID2D1SolidColorBrush,
        start_x: f32,
        end_x: f32,
        y: f32,
        attrs: CellAttrs,
    ) {
        let draw_segment = |from_x: f32, from_y: f32, to_x: f32, to_y: f32| unsafe {
            target.DrawLine(
                D2D_POINT_2F {
                    x: from_x,
                    y: from_y,
                },
                D2D_POINT_2F { x: to_x, y: to_y },
                brush,
                1.0,
                None,
            )
        };

        if attrs.contains(CellAttrs::CURLY_UNDERLINE) {
            let mut x = start_x;
            let mut rising = true;
            while x < end_x {
                let next = (x + 2.0).min(end_x);
                let next_y = if rising { y - 1.0 } else { y + 1.0 };
                draw_segment(x, if rising { y + 1.0 } else { y - 1.0 }, next, next_y);
                rising = !rising;
                x = next;
            }
        } else if attrs.contains(CellAttrs::DOTTED_UNDERLINE) {
            let mut x = start_x;
            while x < end_x {
                draw_segment(x, y, (x + 1.0).min(end_x), y);
                x += 3.0;
            }
        } else if attrs.contains(CellAttrs::DASHED_UNDERLINE) {
            let mut x = start_x;
            while x < end_x {
                draw_segment(x, y, (x + 4.0).min(end_x), y);
                x += 6.0;
            }
        } else {
            draw_segment(start_x, y, end_x, y);
            if attrs.contains(CellAttrs::DOUBLE_UNDERLINE) {
                draw_segment(start_x, y + 2.0, end_x, y + 2.0);
            }
        }
    }

    /// Draw the cursor
    fn draw_cursor(
        &mut self,
        screen: &Screen,
        blink_phase: BlinkPhase,
    ) -> windows::core::Result<()> {
        // Check DECTCEM mode for cursor visibility
        if !cursor_visible(screen, blink_phase) {
            return Ok(());
        }

        let cursor_color = self.resolved_palette(screen).cursor;
        self.draw_cursor_cell(
            screen,
            screen.cursor.row,
            screen.cursor.col,
            screen.cursor.style,
            cursor_color,
            self.theme.cursor.text_color,
        )
    }

    fn draw_extra_cursors(
        &mut self,
        screen: &Screen,
        blink_phase: BlinkPhase,
    ) -> windows::core::Result<()> {
        if !extra_cursors_visible(screen, blink_phase) {
            return Ok(());
        }
        let palette = self.resolved_palette(screen);
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
                colors.cursor,
                colors.text,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_cursor_cell(
        &mut self,
        screen: &Screen,
        row: usize,
        col: usize,
        style: CursorStyle,
        cursor_color: Rgb,
        text_color: Rgb,
    ) -> windows::core::Result<()> {
        let footprint = cursor_footprint(screen, row, col);
        let x = self.origin_x + footprint.col as f32 * self.cell_dims.width;
        let y = self.origin_y + footprint.row as f32 * self.cell_dims.height;
        let width = footprint.columns as f32 * self.cell_dims.width;
        let height = footprint.rows as f32 * self.cell_dims.height;
        let brush = self.get_brush(cursor_color)?;

        let rect = match style {
            CursorStyle::Block => D2D_RECT_F {
                left: x,
                top: y,
                right: x + width,
                bottom: y + height,
            },
            CursorStyle::Underline => D2D_RECT_F {
                left: x,
                top: y + height - 2.0,
                right: x + width,
                bottom: y + height,
            },
            CursorStyle::Bar => D2D_RECT_F {
                left: x,
                top: y,
                right: x + 2.0,
                bottom: y + height,
            },
        };

        // Clone and cast to parent interface to access methods
        let rt = self.render_target.clone().unwrap();
        let base: ID2D1RenderTarget = rt.cast()?;

        // Draw filled block cursor
        unsafe {
            base.FillRectangle(&rect, &brush);
        }

        if style != CursorStyle::Block {
            return Ok(());
        }

        // Draw the character under a block cursor with inverted color.
        if let Some(cell) = screen.get_cell(footprint.row, footprint.col) {
            let multicell = cell.multicell.as_ref();
            let metrics = multicell.map(multicell_render_metrics);
            let text = multicell.map_or_else(|| cell.text(), |multicell| multicell.text());

            if text != " "
                && text != "\0"
                && !cell.is_kitty_image_placeholder()
                && !cell.attrs.contains(CellAttrs::HIDDEN)
            {
                let text_brush = self.get_brush(text_color)?;
                let text_x = x + metrics.map_or(0.0, |metrics| {
                    metrics.horizontal_offset as f32 * self.cell_dims.width
                });
                let text_y = y + metrics.map_or(0.0, |metrics| {
                    metrics.vertical_offset as f32 * self.cell_dims.height
                });

                let text_format = self.text_format.as_ref().unwrap();
                let utf16: Vec<u16> = text.encode_utf16().collect();

                let layout: IDWriteTextLayout = unsafe {
                    self.dwrite_factory.CreateTextLayout(
                        &utf16,
                        text_format,
                        (width - (text_x - x)).max(self.cell_dims.width),
                        (height - (text_y - y)).max(self.cell_dims.height),
                    )?
                };

                if let Some(metrics) = metrics {
                    unsafe {
                        layout.SetFontSize(
                            self.dpi.scale_f32(self.font_size) * metrics.font_scale as f32,
                            DWRITE_TEXT_RANGE {
                                startPosition: 0,
                                length: utf16.len() as u32,
                            },
                        )?;
                    }
                }

                let origin = D2D_POINT_2F {
                    x: text_x,
                    y: text_y,
                };
                unsafe {
                    base.DrawTextLayout(origin, &layout, &text_brush, Default::default());
                }
            }
        }

        Ok(())
    }

    fn resolved_palette(&self, screen: &Screen) -> cterm_core::ColorPalette {
        let mut base = self.theme.colors.clone();
        if let Some(background) = self.background_override {
            base.background = background;
        }
        screen.resolved_palette(&base)
    }

    /// Update the theme
    pub fn set_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
        self.brush_cache.clear();
    }

    /// Update font settings
    pub fn set_font(&mut self, family: &str, size: f32) -> windows::core::Result<()> {
        self.font_family = family.to_string();
        self.font_size = size;
        self.create_text_format()
    }

    /// Get current font size
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// Set font size only
    pub fn set_font_size(&mut self, size: f32) -> windows::core::Result<()> {
        self.font_size = size;
        self.create_text_format()
    }
}

/// Convert Rgb to D2D1_COLOR_F
fn rgb_to_d2d_color(rgb: Rgb) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: rgb.r as f32 / 255.0,
        g: rgb.g as f32 / 255.0,
        b: rgb.b as f32 / 255.0,
        a: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rgb_to_d2d_color() {
        let rgb = Rgb::new(255, 128, 0);
        let color = rgb_to_d2d_color(rgb);
        assert_eq!(color.r, 1.0);
        assert!((color.g - 0.5).abs() < 0.01);
        assert_eq!(color.b, 0.0);
        assert_eq!(color.a, 1.0);
    }

    #[test]
    fn test_cell_dimensions_default() {
        let dims = CellDimensions::default();
        assert!(dims.width > 0.0);
        assert!(dims.height > 0.0);
    }
}
