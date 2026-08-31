//! Terminal rendering widget using Cairo

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{
    gdk, gio, glib, pango, DrawingArea, EventControllerKey, EventControllerScroll, GestureClick,
};
use parking_lot::Mutex;

use cterm_app::config::Config;
use cterm_core::cell::CellAttrs;
use cterm_core::color::{Color, ColorPalette, Rgb};
use cterm_core::mouse::{
    encode_mouse_event, MouseButton, MouseEvent, MouseModifiers, MousePosition,
};
use cterm_core::screen::{ClipboardOperation, CursorStyle, MouseEncoding, MouseMode, ScreenConfig};
use cterm_core::term::{Key, Modifiers, Terminal, TerminalEvent};
use cterm_core::KeyEventKind;
use cterm_ui::blink::{
    cell_foreground_visible, cursor_visible, BlinkClock, BlinkNeeds, BlinkPhase,
    BLINK_POLL_INTERVAL,
};
use cterm_ui::sprite::{Sprite, SpriteCache};
use cterm_ui::theme::Theme;

use crate::keyboard::{
    associated_text_for_gdk_key, gtk_state_to_modifiers, keyval_to_key, reported_key_from_gdk,
    should_route_enhanced_key, ReportedKey,
};

/// Cell dimensions calculated from font metrics
#[derive(Debug, Clone, Copy)]
pub struct CellDimensions {
    pub width: f64,
    pub height: f64,
}

impl CellDimensions {
    /// Reject Pango's missing-font sentinel metrics before they become GTK
    /// size requests. Some headless fontconfig setups return large positive
    /// values rather than an error when no usable face is installed.
    pub(crate) fn checked(width: f64, height: f64) -> Option<Self> {
        const MAX_CELL_DIMENSION: f64 = 256.0;

        if width.is_finite()
            && height.is_finite()
            && (1.0..=MAX_CELL_DIMENSION).contains(&width)
            && (1.0..=MAX_CELL_DIMENSION).contains(&height)
        {
            Some(Self { width, height })
        } else {
            None
        }
    }

    pub(crate) fn conservative_fallback(font_size: f64) -> Self {
        let font_size = if font_size.is_finite() {
            font_size.clamp(6.0, 72.0)
        } else {
            12.0
        };
        Self {
            width: font_size * 0.75,
            height: font_size * 1.5,
        }
    }
}

/// Callback type for terminal events
type EventCallback = Rc<RefCell<Option<Box<dyn Fn()>>>>;
/// Callback type for title change events
type TitleCallback = Rc<RefCell<Option<Box<dyn Fn(&str)>>>>;
/// Callback type for file transfer events
type FileTransferCallback = Rc<RefCell<Option<Box<dyn Fn(cterm_core::FileTransferOperation)>>>>;

/// Preedit (input method composition) state
#[derive(Default, Clone)]
struct PreeditState {
    text: String,
    cursor_pos: i32,
    active: bool,
}

/// Terminal widget wrapping GTK drawing area
pub struct TerminalWidget {
    drawing_area: DrawingArea,
    terminal: Arc<Mutex<Terminal>>,
    theme: Theme,
    font_family: String,
    font_size: Rc<RefCell<f64>>,
    default_font_size: f64,
    cell_dims: Rc<RefCell<CellDimensions>>,
    sprite_cache: Rc<RefCell<SpriteCache>>,
    blink_clock: Rc<RefCell<BlinkClock>>,
    blink_started: Instant,
    /// Optional background color override (from template)
    background_override: Rc<RefCell<Option<cterm_core::color::Rgb>>>,
    /// Input method preedit (composition) state
    preedit: Rc<RefCell<PreeditState>>,
    on_exit: EventCallback,
    on_bell: EventCallback,
    on_title_change: TitleCallback,
    on_file_transfer: FileTransferCallback,
    /// Command channel for daemon I/O — None for local PTY sessions
    daemon_cmd_tx: Option<tokio::sync::mpsc::UnboundedSender<DaemonCommand>>,
}

impl TerminalWidget {
    /// Get the widget for adding to containers
    pub fn widget(&self) -> &DrawingArea {
        &self.drawing_area
    }

    /// Get the current cell dimensions
    #[allow(dead_code)]
    pub fn cell_dimensions(&self) -> CellDimensions {
        *self.cell_dims.borrow()
    }

    /// Destroy the daemon session (kill the PTY process).
    /// Called when a tab is explicitly closed by the user.
    pub fn destroy_session(&self) {
        if let Some(ref tx) = self.daemon_cmd_tx {
            let _ = tx.send(DaemonCommand::Destroy);
        }
    }

    /// Detach from the daemon session WITHOUT killing the remote PTY.
    /// Called when the user disconnects from a remote — the session stays
    /// alive on the server and can be reattached later.
    pub fn detach_session(&self) {
        if let Some(ref tx) = self.daemon_cmd_tx {
            let _ = tx.send(DaemonCommand::Detach);
        }
    }

    /// Tell the daemon to clear the bell/alert state for this session.
    pub fn clear_alert(&self) {
        if let Some(ref tx) = self.daemon_cmd_tx {
            let _ = tx.send(DaemonCommand::ClearAlert);
        }
    }

    /// Set a custom title on the daemon (persists across reconnects)
    pub fn set_custom_title(&self, title: &str) {
        if let Some(ref tx) = self.daemon_cmd_tx {
            let _ = tx.send(DaemonCommand::SetTitle(title.to_string()));
        }
    }

    /// Set the tab color on the daemon (persists across reconnects)
    pub fn set_tab_color_on_daemon(&self, color: &str) {
        if let Some(ref tx) = self.daemon_cmd_tx {
            let _ = tx.send(DaemonCommand::SetTabColor(color.to_string()));
        }
    }

    /// Set the template name on the daemon (persists across reconnects)
    pub fn set_template_name_on_daemon(&self, name: &str) {
        if let Some(ref tx) = self.daemon_cmd_tx {
            let _ = tx.send(DaemonCommand::SetTemplateName(name.to_string()));
        }
    }

    /// Report native window visibility to the local terminal or owning daemon.
    pub fn set_window_visibility(&self, visibility: cterm_core::WindowVisibility) {
        update_window_visibility(&self.terminal, self.daemon_cmd_tx.as_ref(), visibility);
    }

    fn setup_visibility_reporting(&self) {
        let terminal = Arc::clone(&self.terminal);
        let sender = self.daemon_cmd_tx.clone();
        self.drawing_area.connect_map(move |_| {
            update_window_visibility(
                &terminal,
                sender.as_ref(),
                cterm_core::WindowVisibility::Visible,
            );
        });

        let terminal = Arc::clone(&self.terminal);
        let sender = self.daemon_cmd_tx.clone();
        self.drawing_area.connect_unmap(move |_| {
            update_window_visibility(
                &terminal,
                sender.as_ref(),
                cterm_core::WindowVisibility::Hidden,
            );
        });
    }

    /// Set callback for when the terminal process exits
    pub fn set_on_exit<F: Fn() + 'static>(&self, callback: F) {
        *self.on_exit.borrow_mut() = Some(Box::new(callback));
    }

    /// Set callback for when the terminal rings the bell
    pub fn set_on_bell<F: Fn() + 'static>(&self, callback: F) {
        *self.on_bell.borrow_mut() = Some(Box::new(callback));
    }

    /// Set callback for when the terminal title changes
    pub fn set_on_title_change<F: Fn(&str) + 'static>(&self, callback: F) {
        *self.on_title_change.borrow_mut() = Some(Box::new(callback));
    }

    /// Set callback for when a file is received
    pub fn set_on_file_transfer<F: Fn(cterm_core::FileTransferOperation) + 'static>(
        &self,
        callback: F,
    ) {
        *self.on_file_transfer.borrow_mut() = Some(Box::new(callback));
    }

    /// Get the terminal for file transfer operations
    pub fn terminal(&self) -> &Arc<Mutex<Terminal>> {
        &self.terminal
    }

    /// Get the current working directory of the foreground process (if any)
    #[cfg(unix)]
    pub fn foreground_cwd(&self) -> Option<String> {
        self.terminal
            .lock()
            .foreground_cwd()
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// Write a string to the terminal (for paste operations)
    pub fn write_str(&self, s: &str) {
        let mut term = self.terminal.lock();
        if let Err(e) = term.write_str(s) {
            log::error!("Failed to write to terminal: {}", e);
        }
    }

    /// Set an optional background color override (hex string like "#1a1b26")
    #[allow(dead_code)]
    pub fn set_background_override(&self, color: Option<&str>) {
        let rgb = color.and_then(parse_rgb);
        *self.background_override.borrow_mut() = rgb;
        let palette = frontend_palette(&self.theme, rgb);
        self.terminal.lock().set_base_palette(palette.clone());
        if let Some(sender) = &self.daemon_cmd_tx {
            let _ = sender.send(DaemonCommand::SetPalette(palette));
        }
        // Trigger redraw to apply new background
        self.drawing_area.queue_draw();
    }

    /// Increase font size (zoom in)
    pub fn zoom_in(&self) {
        let mut font_size = self.font_size.borrow_mut();
        *font_size = (*font_size + 1.0).min(72.0);
        let new_size = *font_size;
        drop(font_size);
        self.update_cell_dimensions(new_size);
        self.trigger_resize();
    }

    /// Decrease font size (zoom out)
    pub fn zoom_out(&self) {
        let mut font_size = self.font_size.borrow_mut();
        *font_size = (*font_size - 1.0).max(6.0);
        let new_size = *font_size;
        drop(font_size);
        self.update_cell_dimensions(new_size);
        self.trigger_resize();
    }

    /// Reset font size to default
    pub fn zoom_reset(&self) {
        *self.font_size.borrow_mut() = self.default_font_size;
        self.update_cell_dimensions(self.default_font_size);
        self.trigger_resize();
    }

    /// Update cell dimensions after font size change
    fn update_cell_dimensions(&self, font_size: f64) {
        let new_dims = calculate_cell_dimensions(&self.font_family, font_size);
        *self.cell_dims.borrow_mut() = new_dims;
        self.sprite_cache.borrow_mut().clear();
    }

    /// Reset the terminal (soft reset - keeps scrollback)
    pub fn reset(&self) {
        let mut term = self.terminal.lock();
        soft_reset_screen(term.screen_mut());
        drop(term);
        self.drawing_area.queue_draw();
    }

    /// Clear scrollback buffer and fully reset the terminal
    pub fn clear_scrollback_and_reset(&self) {
        let mut term = self.terminal.lock();
        term.screen_mut().reset();
        drop(term);
        self.drawing_area.queue_draw();
    }

    /// Send a signal to the terminal process
    pub fn send_signal(&self, signal: i32) {
        let term = self.terminal.lock();
        if let Err(e) = term.send_signal(signal) {
            log::error!("Failed to send signal {}: {}", signal, e);
        }
    }

    /// Send focus event to terminal if focus events mode is enabled (DECSET 1004)
    /// `focused`: true for focus in (\x1b[I), false for focus out (\x1b[O)
    pub fn send_focus_event(&self, focused: bool) {
        let mut term = self.terminal.lock();
        if term.screen().modes.focus_events {
            let sequence = if focused { b"\x1b[I" } else { b"\x1b[O" };
            if let Err(e) = term.write(sequence) {
                log::error!("Failed to send focus event: {}", e);
            }
        }
    }

    /// Search for text in terminal buffer (scrollback + visible)
    ///
    /// Returns the number of matches found. If matches are found, scrolls to the first match.
    pub fn find(&self, pattern: &str, case_sensitive: bool, regex: bool) -> usize {
        let term = self.terminal.lock();
        let results = term.find(pattern, case_sensitive, regex);
        let count = results.len();

        if let Some(first) = results.first() {
            // Need to release the lock before we can take mutable lock
            let line_idx = first.line;
            drop(term);

            let mut term = self.terminal.lock();
            term.scroll_to_line(line_idx);
            self.drawing_area.queue_draw();
        }

        count
    }

    /// Search and return all matches (for iteration/highlighting)
    #[allow(dead_code)]
    pub fn find_all(
        &self,
        pattern: &str,
        case_sensitive: bool,
        regex: bool,
    ) -> Vec<cterm_core::SearchResult> {
        let term = self.terminal.lock();
        term.find(pattern, case_sensitive, regex)
    }

    /// Scroll to a specific search result
    #[allow(dead_code)]
    pub fn scroll_to_result(&self, result: &cterm_core::SearchResult) {
        let mut term = self.terminal.lock();
        term.scroll_to_line(result.line);
        drop(term);
        self.drawing_area.queue_draw();
    }

    /// Scroll the local viewport up by a fixed number of physical rows.
    pub fn scroll_viewport_up(&self, lines: usize) {
        self.terminal.lock().scroll_viewport_up(lines);
        self.drawing_area.queue_draw();
    }

    /// Scroll the local viewport down by a fixed number of physical rows.
    pub fn scroll_viewport_down(&self, lines: usize) {
        self.terminal.lock().scroll_viewport_down(lines);
        self.drawing_area.queue_draw();
    }

    /// Scroll one viewport upward or downward.
    pub fn scroll_viewport_page(&self, up: bool) {
        let mut terminal = self.terminal.lock();
        let lines = terminal.rows().max(1);
        if up {
            terminal.scroll_viewport_up(lines);
        } else {
            terminal.scroll_viewport_down(lines);
        }
        drop(terminal);
        self.drawing_area.queue_draw();
    }

    /// Jump to the oldest retained row or back to the live bottom.
    pub fn scroll_viewport_edge(&self, top: bool) {
        let mut terminal = self.terminal.lock();
        if top {
            terminal.scroll_viewport_up(usize::MAX);
        } else {
            terminal.scroll_viewport_to_bottom();
        }
        drop(terminal);
        self.drawing_area.queue_draw();
    }

    /// Navigate between OSC 133 prompt markers.
    pub fn scroll_to_shell_prompt(&self, previous: bool) {
        let mut terminal = self.terminal.lock();
        if previous {
            terminal.scroll_to_previous_prompt();
        } else {
            terminal.scroll_to_next_prompt();
        }
        drop(terminal);
        self.drawing_area.queue_draw();
    }

    /// Convert pixel coordinates to cell (row, col) coordinates
    ///
    /// Returns (visible_row, col) where visible_row is the row on screen (0 = top)
    #[allow(dead_code)]
    pub fn pixel_to_cell(&self, x: f64, y: f64) -> (usize, usize) {
        let dims = self.cell_dims.borrow();
        let col = (x / dims.width).floor() as usize;
        let row = (y / dims.height).floor() as usize;
        (row, col)
    }

    /// Convert pixel coordinates to absolute line index
    ///
    /// Returns (absolute_line, col) where absolute_line accounts for scrollback
    #[allow(dead_code)]
    pub fn pixel_to_absolute(&self, x: f64, y: f64) -> (usize, usize) {
        let (visible_row, col) = self.pixel_to_cell(x, y);
        let term = self.terminal.lock();
        let absolute_line = term.screen().visible_row_to_absolute_line(visible_row);
        (absolute_line, col)
    }

    /// Start a new selection at the given pixel coordinates
    #[allow(dead_code)]
    pub fn start_selection(&self, x: f64, y: f64) {
        let (line, col) = self.pixel_to_absolute(x, y);
        let mut term = self.terminal.lock();
        term.screen_mut()
            .start_selection(line, col, cterm_core::SelectionMode::Char);
        drop(term);
        self.drawing_area.queue_draw();
    }

    /// Extend the current selection to the given pixel coordinates
    #[allow(dead_code)]
    pub fn extend_selection(&self, x: f64, y: f64) {
        let (line, col) = self.pixel_to_absolute(x, y);
        let mut term = self.terminal.lock();
        term.screen_mut().extend_selection(line, col);
        drop(term);
        self.drawing_area.queue_draw();
    }

    /// Clear the current selection
    #[allow(dead_code)]
    pub fn clear_selection(&self) {
        let mut term = self.terminal.lock();
        term.screen_mut().clear_selection();
        drop(term);
        self.drawing_area.queue_draw();
    }

    /// Get the selected text (if any)
    pub fn get_selected_text(&self) -> Option<String> {
        let term = self.terminal.lock();
        term.screen().get_selected_text()
    }

    /// Copy the current selection to clipboard
    pub fn copy_selection(&self) {
        if let Some(text) = self.get_selected_text() {
            if let Some(display) = gdk::Display::default() {
                let clipboard = display.clipboard();
                clipboard.set_text(&text);
            }
        }
    }

    /// Copy the current selection to clipboard as HTML
    pub fn copy_selection_html(&self) {
        let term = self.terminal.lock();
        let html = term.screen().get_selected_html(&self.theme.colors);
        let text = term.screen().get_selected_text();
        drop(term);

        if let (Some(html), Some(_text)) = (html, text) {
            if let Some(display) = gdk::Display::default() {
                let clipboard = display.clipboard();
                // GTK4 clipboard can hold multiple formats via ContentProvider
                // For simplicity, we set HTML as text - most apps will interpret it
                // A full implementation would use ContentProvider with multiple MIME types
                clipboard.set_text(&html);
                log::debug!("Copied {} chars as HTML to clipboard", html.len());
            }
        }
    }

    /// Select all text in the terminal
    pub fn select_all(&self) {
        let mut term = self.terminal.lock();
        let total_lines = term.screen().total_lines();
        let width = term.screen().width();

        // Select from the first line to the last line
        term.screen_mut()
            .start_selection(0, 0, cterm_core::screen::SelectionMode::Char);
        term.screen_mut()
            .extend_selection(total_lines.saturating_sub(1), width.saturating_sub(1));
        drop(term);

        self.drawing_area.queue_draw();
    }

    /// Copy the current selection to primary selection (Unix only)
    #[cfg(unix)]
    #[allow(dead_code)]
    pub fn copy_selection_to_primary(&self) {
        if let Some(text) = self.get_selected_text() {
            if let Some(display) = gdk::Display::default() {
                let primary = display.primary_clipboard();
                primary.set_text(&text);
            }
        }
    }

    /// Paste from primary selection (Unix middle-click paste)
    #[cfg(unix)]
    #[allow(dead_code)]
    pub fn paste_primary(&self) {
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let primary = display.primary_clipboard();
        let terminal = Arc::clone(&self.terminal);
        let drawing_area = self.drawing_area.clone();

        primary.read_text_async(None::<&gio::Cancellable>, move |result| {
            if let Ok(Some(text)) = result {
                let mut term = terminal.lock();
                // Use bracketed paste if enabled
                let paste_text = if term.screen().modes.bracketed_paste {
                    format!("\x1b[200~{}\x1b[201~", text)
                } else {
                    text.to_string()
                };
                let _ = term.write_str(&paste_text);
                drawing_area.queue_draw();
            }
        });
    }

    /// Trigger a resize to recalculate terminal dimensions
    fn trigger_resize(&self) {
        // Force a resize by getting current size
        let width = self.drawing_area.width();
        let height = self.drawing_area.height();

        let dims = self.cell_dims.borrow();
        let cols = ((width as f64) / dims.width).floor() as usize;
        let rows = ((height as f64) / dims.height).floor() as usize;
        drop(dims);

        if cols > 0 && rows > 0 {
            let pixel_width = width.clamp(1, u16::MAX as i32) as u16;
            let pixel_height = height.clamp(1, u16::MAX as i32) as u16;
            let mut term = self.terminal.lock();
            term.resize_with_pixels(cols, rows, pixel_width, pixel_height);
            drop(term);
            if let Some(ref tx) = self.daemon_cmd_tx {
                let _ = tx.send(DaemonCommand::Resize {
                    cols: cols as u32,
                    rows: rows as u32,
                    pixel_width: u32::from(pixel_width),
                    pixel_height: u32::from(pixel_height),
                });
            }
        }

        self.drawing_area.queue_draw();
    }

    /// Set up the draw function
    fn setup_drawing(&self) {
        let terminal = Arc::clone(&self.terminal);
        let theme = self.theme.clone();
        let font_family = self.font_family.clone();
        let font_size = Rc::clone(&self.font_size);
        let cell_dims = Rc::clone(&self.cell_dims);
        let background_override = Rc::clone(&self.background_override);
        let preedit = Rc::clone(&self.preedit);
        let sprite_cache = Rc::clone(&self.sprite_cache);
        let blink_clock = Rc::clone(&self.blink_clock);

        self.drawing_area
            .set_draw_func(move |_area, cr, _width, _height| {
                let font_size = *font_size.borrow();
                let dims = *cell_dims.borrow();
                let bg_override = *background_override.borrow();
                let preedit_state = preedit.borrow().clone();
                let render_config = RenderConfig {
                    font_family: &font_family,
                    font_size,
                    cell_dims: dims,
                    background_override: bg_override,
                };
                let mut sprites = sprite_cache.borrow_mut();
                let blink_phase = blink_clock.borrow().phase();
                draw_terminal(
                    cr,
                    &terminal,
                    &theme,
                    &render_config,
                    &preedit_state,
                    &mut sprites,
                    blink_phase,
                );
            });
    }

    /// Drive blink phases independently of PTY output and invalidate only on
    /// phase edges that are relevant to the currently visible screen.
    fn setup_blink_clock(&self) {
        let drawing_area = self.drawing_area.downgrade();
        let terminal = Arc::clone(&self.terminal);
        let blink_clock = Rc::clone(&self.blink_clock);
        let started = self.blink_started;

        glib::timeout_add_local(BLINK_POLL_INTERVAL, move || {
            let Some(drawing_area) = drawing_area.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let needs = BlinkNeeds::for_screen(terminal.lock().screen());
            if blink_clock.borrow_mut().update(started.elapsed(), needs) {
                drawing_area.queue_draw();
            }
            glib::ControlFlow::Continue
        });
    }

    /// Set up input handling
    fn setup_input(&self) {
        let terminal = Arc::clone(&self.terminal);
        let cell_dims = Rc::clone(&self.cell_dims);

        // Keyboard input — we manage the IM context explicitly so that
        // Japanese/CJK composition works reliably with IBus/Fcitx.
        let key_controller = EventControllerKey::new();
        // Disable the controller's built-in IM handling; we call
        // filter_keypress ourselves so we can control the priority.
        key_controller.set_im_context(None::<&gtk4::IMContext>);

        // Create our own IM context
        let im_context = gtk4::IMMulticontext::new();
        im_context.set_client_widget(Some(&self.drawing_area));

        // IM commit: receives confirmed text from the input method
        let terminal_commit = Arc::clone(&terminal);
        let drawing_area_commit = self.drawing_area.clone();
        im_context.connect_commit(move |_, text| {
            let mut term = terminal_commit.lock();
            term.scroll_viewport_to_bottom();
            let encoded = term.handle_text_input(text);
            if !encoded.is_empty() {
                if let Err(e) = term.write(&encoded) {
                    log::error!("Failed to write IM text to PTY: {}", e);
                }
            }
            drawing_area_commit.queue_draw();
        });

        // IM preedit: display composition text while the user is typing
        let preedit_changed = Rc::clone(&self.preedit);
        let drawing_area_preedit = self.drawing_area.clone();
        im_context.connect_preedit_changed(move |im| {
            let (text, _attrs, cursor_pos) = im.preedit_string();
            let mut state = preedit_changed.borrow_mut();
            state.text = text.to_string();
            state.cursor_pos = cursor_pos;
            state.active = !state.text.is_empty();
            drawing_area_preedit.queue_draw();
        });

        let preedit_end = Rc::clone(&self.preedit);
        let drawing_area_preedit_end = self.drawing_area.clone();
        im_context.connect_preedit_end(move |_| {
            let mut state = preedit_end.borrow_mut();
            state.text.clear();
            state.cursor_pos = 0;
            state.active = false;
            drawing_area_preedit_end.queue_draw();
        });

        let enhanced_keys: Rc<RefCell<HashMap<u32, ReportedKey>>> =
            Rc::new(RefCell::new(HashMap::new()));

        // Manage IM focus when the DrawingArea gains/loses keyboard focus
        let im_focus = im_context.clone();
        let enhanced_focus = Rc::clone(&enhanced_keys);
        self.drawing_area.connect_has_focus_notify(move |widget| {
            if widget.has_focus() {
                im_focus.focus_in();
            } else {
                im_focus.focus_out();
                // A compositor may not deliver key-up after focus moves.
                // Do not classify the next physical press as a repeat.
                enhanced_focus.borrow_mut().clear();
            }
        });
        // If the drawing area already has focus, activate IM immediately
        if self.drawing_area.has_focus() {
            im_context.focus_in();
        }

        // Key press handler
        let terminal_key = Arc::clone(&terminal);
        let im_key = im_context.clone();
        let enhanced_keys_down = Rc::clone(&enhanced_keys);
        key_controller.connect_key_pressed(move |controller, keyval, keycode, state| {
            // Reset scroll to bottom on any user input
            {
                let mut term = terminal_key.lock();
                if !term.is_at_bottom() {
                    term.scroll_viewport_to_bottom();
                }
            }

            let has_ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);

            // Let the IM context try to handle the key first. This must happen
            // before enhanced-character routing so triggers such as Ctrl+Space
            // and active composition continue to belong to the input method.
            if let Some(event) = controller.current_event() {
                if im_key.filter_keypress(&event) {
                    return glib::Propagation::Stop;
                }
            }

            let modifiers = gtk_state_to_modifiers(state);
            let reported_key = reported_key_from_gdk(controller, keyval, keycode);
            if let Some(reported) = reported_key {
                let mut term = terminal_key.lock();
                if should_route_enhanced_key(&term, reported.key, modifiers) {
                    let already_pressed = enhanced_keys_down.borrow().get(&keycode).copied();
                    let (kind, reported) = match already_pressed {
                        Some(pressed) => (KeyEventKind::Repeat, pressed),
                        None => {
                            enhanced_keys_down.borrow_mut().insert(keycode, reported);
                            (KeyEventKind::Press, reported)
                        }
                    };
                    let associated_text =
                        associated_text_for_gdk_key(reported.key, keyval, modifiers);
                    let metadata = reported
                        .metadata()
                        .with_associated_text(associated_text.as_deref());
                    if let Some(bytes) =
                        term.handle_key_event_with_metadata(reported.key, modifiers, kind, metadata)
                    {
                        if let Err(e) = term.write(&bytes) {
                            log::error!("Failed to write enhanced key event to PTY: {e}");
                        }
                    }
                    return glib::Propagation::Stop;
                }
            }

            let has_alt = state.contains(gdk::ModifierType::ALT_MASK);

            // Handle special keys (arrows, function keys, etc.)
            if let Some(key) = keyval_to_key(keyval) {
                let mut term = terminal_key.lock();
                if let Some(bytes) = term.handle_key(key, modifiers) {
                    if let Err(e) = term.write(&bytes) {
                        log::error!("Failed to write to PTY: {}", e);
                    }
                    return glib::Propagation::Stop;
                }
            }

            // Get the character for this key
            if let Some(c) = keyval.to_unicode() {
                // Handle Ctrl+letter -> control character
                if has_ctrl && !has_alt {
                    let mut term = terminal_key.lock();
                    let ctrl_char = match c.to_ascii_lowercase() {
                        'a'..='z' => Some(c.to_ascii_lowercase() as u8 - b'a' + 1),
                        '[' | '3' => Some(0x1b), // Escape
                        '\\' | '4' => Some(0x1c),
                        ']' | '5' => Some(0x1d),
                        '^' | '6' => Some(0x1e),
                        '_' | '7' | '/' => Some(0x1f),
                        ' ' | '2' | '@' => Some(0x00), // Ctrl-Space/Ctrl-@
                        '?' | '8' => Some(0x7f),       // DEL
                        _ => None,
                    };

                    if let Some(byte) = ctrl_char {
                        if let Err(e) = term.write(&[byte]) {
                            log::error!("Failed to write to PTY: {}", e);
                        }
                        return glib::Propagation::Stop;
                    }
                }

                // Handle Alt+key -> ESC + key
                if has_alt && !has_ctrl {
                    let mut term = terminal_key.lock();
                    let mut buf = vec![0x1b]; // ESC
                    let mut char_buf = [0u8; 4];
                    let s = c.encode_utf8(&mut char_buf);
                    buf.extend_from_slice(s.as_bytes());
                    if let Err(e) = term.write(&buf) {
                        log::error!("Failed to write to PTY: {}", e);
                    }
                    return glib::Propagation::Stop;
                }

                // Regular character without Ctrl/Alt: IM didn't handle it,
                // so write directly to the PTY.
                if !has_ctrl && !has_alt {
                    let mut term = terminal_key.lock();
                    let mut buf = [0u8; 4];
                    let s = c.encode_utf8(&mut buf);
                    let encoded = term.handle_text_input(s);
                    if !encoded.is_empty() {
                        if let Err(e) = term.write(&encoded) {
                            log::error!("Failed to write to PTY: {}", e);
                        }
                    }
                    return glib::Propagation::Stop;
                }
            }

            glib::Propagation::Proceed
        });

        // Key release handler — report releases for physical keys routed by
        // kitty event mode; all other releases still reach the IM context.
        let im_release = im_context.clone();
        let terminal_release = Arc::clone(&terminal);
        let enhanced_keys_up = Rc::clone(&enhanced_keys);
        key_controller.connect_key_released(move |controller, _keyval, keycode, state| {
            if let Some(reported) = enhanced_keys_up.borrow_mut().remove(&keycode) {
                let modifiers = gtk_state_to_modifiers(state);
                let mut term = terminal_release.lock();
                if let Some(bytes) = term.handle_reported_key_release_with_metadata(
                    reported.key,
                    modifiers,
                    reported.metadata(),
                ) {
                    if let Err(e) = term.write(&bytes) {
                        log::error!("Failed to write enhanced key release to PTY: {e}");
                    }
                }
                return;
            }
            if let Some(event) = controller.current_event() {
                im_release.filter_keypress(&event);
            }
        });

        self.drawing_area.add_controller(key_controller);

        // Selection state: tracks whether we're in a drag operation
        let selecting = Rc::new(RefCell::new(false));

        // Mouse-forwarding state (for applications that enable mouse tracking).
        // `last_cell` is the pointer's current cell, needed by the scroll handler
        // (GTK scroll events carry no coordinates). `pressed_button` is the button
        // currently held, so motion can be reported as a drag and released cleanly.
        let last_position = Rc::new(RefCell::new(MousePosition::default()));
        let pressed_button: Rc<RefCell<Option<MouseButton>>> = Rc::new(RefCell::new(None));

        // Mouse click for selection
        let click_controller = GestureClick::new();
        click_controller.set_button(gdk::BUTTON_PRIMARY);

        let terminal_click = Arc::clone(&terminal);
        let cell_dims_click = Rc::clone(&cell_dims);
        let drawing_area_click = self.drawing_area.clone();
        let selecting_pressed = Rc::clone(&selecting);
        let pressed_button_click = Rc::clone(&pressed_button);

        click_controller.connect_pressed(move |gesture, n_press, x, y| {
            drawing_area_click.grab_focus();

            let dims = cell_dims_click.borrow();
            let col = (x / dims.width).floor() as usize;
            let row = (y / dims.height).floor() as usize;
            drop(dims);

            let state = gesture
                .current_event()
                .map(|e| e.modifier_state())
                .unwrap_or_else(gdk::ModifierType::empty);

            // Ctrl+click to open hyperlinks
            if state.contains(gdk::ModifierType::CONTROL_MASK) {
                let term = terminal_click.lock();
                if let Some(uri) = term
                    .screen()
                    .get_cell(row, col)
                    .and_then(|c| c.hyperlink.as_ref())
                    .map(|h| h.uri.clone())
                {
                    drop(term);
                    if let Err(e) = open::that(&uri) {
                        log::error!("Failed to open URL {}: {}", uri, e);
                    }
                    return;
                }
            }

            // Forward to a mouse-tracking application unless Shift is held (Shift
            // always falls through to local text selection).
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            if !shift {
                let mut term = terminal_click.lock();
                if mouse_tracking_active(&term)
                    && report_mouse(
                        &mut term,
                        MouseEvent::Press(MouseButton::Left),
                        MousePosition::new(col, row, x.floor() as i32, y.floor() as i32),
                        gtk_state_to_mouse_mods(state),
                    )
                {
                    drop(term);
                    *pressed_button_click.borrow_mut() = Some(MouseButton::Left);
                    return;
                }
            }

            // Determine selection mode based on click count
            let mode = match n_press {
                2 => cterm_core::SelectionMode::Word,
                3 => cterm_core::SelectionMode::Line,
                _ => cterm_core::SelectionMode::Char,
            };

            // Start selection
            let mut term = terminal_click.lock();
            let line = term.screen().visible_row_to_absolute_line(row);
            term.screen_mut().start_selection(line, col, mode);
            drop(term);

            *selecting_pressed.borrow_mut() = true;
            drawing_area_click.queue_draw();
        });

        let terminal_released = Arc::clone(&terminal);
        let cell_dims_released = Rc::clone(&cell_dims);
        let drawing_area_released = self.drawing_area.clone();
        let selecting_released = Rc::clone(&selecting);
        let pressed_button_released = Rc::clone(&pressed_button);

        click_controller.connect_released(move |gesture, _n_press, x, y| {
            // If this press was forwarded to a mouse-tracking app, report the release
            // and skip the selection-finalize path.
            let reported_button = pressed_button_released.borrow_mut().take();
            if let Some(button) = reported_button {
                let dims = cell_dims_released.borrow();
                let col = (x / dims.width).floor() as usize;
                let row = (y / dims.height).floor() as usize;
                drop(dims);
                let state = gesture
                    .current_event()
                    .map(|e| e.modifier_state())
                    .unwrap_or_else(gdk::ModifierType::empty);
                let mut term = terminal_released.lock();
                report_mouse(
                    &mut term,
                    MouseEvent::Release(button),
                    MousePosition::new(col, row, x.floor() as i32, y.floor() as i32),
                    gtk_state_to_mouse_mods(state),
                );
                return;
            }

            *selecting_released.borrow_mut() = false;

            // Check if selection is empty (same start and end) and clear it
            // Only clear char/block selections - word/line selections are never "empty"
            // since they select at minimum the clicked word/line
            let term = terminal_released.lock();
            if let Some(selection) = &term.screen().selection {
                if selection.anchor == selection.end
                    && matches!(
                        selection.mode,
                        cterm_core::SelectionMode::Char | cterm_core::SelectionMode::Block
                    )
                {
                    drop(term);
                    let mut term = terminal_released.lock();
                    term.screen_mut().clear_selection();
                    drawing_area_released.queue_draw();
                } else {
                    // Copy selection to primary clipboard (Unix behavior)
                    #[cfg(unix)]
                    if let Some(text) = term.screen().get_selected_text() {
                        if let Some(display) = gdk::Display::default() {
                            let primary = display.primary_clipboard();
                            primary.set_text(&text);
                        }
                    }
                }
            }
        });

        self.drawing_area.add_controller(click_controller);

        // Right-click for hyperlink context menu
        {
            let right_click = GestureClick::new();
            right_click.set_button(gdk::BUTTON_SECONDARY);

            let terminal_rc = Arc::clone(&terminal);
            let cell_dims_rc = Rc::clone(&cell_dims);
            let drawing_area_rc = self.drawing_area.clone();
            let pressed_button_rc = Rc::clone(&pressed_button);

            right_click.connect_pressed(move |gesture, _n_press, x, y| {
                let dims = cell_dims_rc.borrow();
                let col = (x / dims.width).floor() as usize;
                let row = (y / dims.height).floor() as usize;
                drop(dims);

                let state = gesture
                    .current_event()
                    .map(|e| e.modifier_state())
                    .unwrap_or_else(gdk::ModifierType::empty);
                let shift = state.contains(gdk::ModifierType::SHIFT_MASK);

                // Forward to a mouse-tracking app unless Shift is held.
                if !shift {
                    let mut term = terminal_rc.lock();
                    if mouse_tracking_active(&term)
                        && report_mouse(
                            &mut term,
                            MouseEvent::Press(MouseButton::Right),
                            MousePosition::new(col, row, x.floor() as i32, y.floor() as i32),
                            gtk_state_to_mouse_mods(state),
                        )
                    {
                        drop(term);
                        *pressed_button_rc.borrow_mut() = Some(MouseButton::Right);
                        return;
                    }
                }

                let term = terminal_rc.lock();
                let uri = term
                    .screen()
                    .get_cell(row, col)
                    .and_then(|c| c.hyperlink.as_ref())
                    .map(|h| h.uri.clone());
                drop(term);

                if let Some(uri) = uri {
                    // Build context menu for hyperlink
                    let menu = gio::Menu::new();
                    menu.append(Some("Open URL"), Some(&format!("win.open-url::{}", uri)));
                    menu.append(Some("Copy URL"), Some(&format!("win.copy-url::{}", uri)));

                    let popover = gtk4::PopoverMenu::from_model(Some(&menu));
                    popover.set_parent(&drawing_area_rc);
                    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                    popover.popup();
                }
            });

            // Report the release of a forwarded right-button press.
            let terminal_rr = Arc::clone(&terminal);
            let cell_dims_rr = Rc::clone(&cell_dims);
            let pressed_button_rr = Rc::clone(&pressed_button);
            right_click.connect_released(move |gesture, _n_press, x, y| {
                if *pressed_button_rr.borrow() != Some(MouseButton::Right) {
                    return;
                }
                *pressed_button_rr.borrow_mut() = None;
                let dims = cell_dims_rr.borrow();
                let col = (x / dims.width).floor() as usize;
                let row = (y / dims.height).floor() as usize;
                drop(dims);
                let state = gesture
                    .current_event()
                    .map(|e| e.modifier_state())
                    .unwrap_or_else(gdk::ModifierType::empty);
                let mut term = terminal_rr.lock();
                report_mouse(
                    &mut term,
                    MouseEvent::Release(MouseButton::Right),
                    MousePosition::new(col, row, x.floor() as i32, y.floor() as i32),
                    gtk_state_to_mouse_mods(state),
                );
            });

            self.drawing_area.add_controller(right_click);
        }

        // Middle-click paste from primary selection (Unix only)
        #[cfg(unix)]
        {
            let middle_click_controller = GestureClick::new();
            middle_click_controller.set_button(gdk::BUTTON_MIDDLE);

            let terminal_middle = Arc::clone(&terminal);
            let cell_dims_middle = Rc::clone(&cell_dims);
            let drawing_area_middle = self.drawing_area.clone();
            let pressed_button_middle = Rc::clone(&pressed_button);

            middle_click_controller.connect_pressed(move |gesture, _n_press, x, y| {
                let state = gesture
                    .current_event()
                    .map(|e| e.modifier_state())
                    .unwrap_or_else(gdk::ModifierType::empty);
                let shift = state.contains(gdk::ModifierType::SHIFT_MASK);

                // Forward to a mouse-tracking app unless Shift is held.
                if !shift {
                    let dims = cell_dims_middle.borrow();
                    let col = (x / dims.width).floor() as usize;
                    let row = (y / dims.height).floor() as usize;
                    drop(dims);
                    let mut term = terminal_middle.lock();
                    if mouse_tracking_active(&term)
                        && report_mouse(
                            &mut term,
                            MouseEvent::Press(MouseButton::Middle),
                            MousePosition::new(col, row, x.floor() as i32, y.floor() as i32),
                            gtk_state_to_mouse_mods(state),
                        )
                    {
                        drop(term);
                        *pressed_button_middle.borrow_mut() = Some(MouseButton::Middle);
                        return;
                    }
                }

                let Some(display) = gdk::Display::default() else {
                    return;
                };
                let primary = display.primary_clipboard();
                let terminal = Arc::clone(&terminal_middle);
                let drawing_area = drawing_area_middle.clone();

                primary.read_text_async(None::<&gio::Cancellable>, move |result| {
                    if let Ok(Some(text)) = result {
                        let mut term = terminal.lock();
                        // Use bracketed paste if enabled
                        let paste_text = if term.screen().modes.bracketed_paste {
                            format!("\x1b[200~{}\x1b[201~", text)
                        } else {
                            text.to_string()
                        };
                        let _ = term.write_str(&paste_text);
                        drawing_area.queue_draw();
                    }
                });
            });

            // Report the release of a forwarded middle-button press.
            let terminal_mr = Arc::clone(&terminal);
            let cell_dims_mr = Rc::clone(&cell_dims);
            let pressed_button_mr = Rc::clone(&pressed_button);
            middle_click_controller.connect_released(move |gesture, _n_press, x, y| {
                if *pressed_button_mr.borrow() != Some(MouseButton::Middle) {
                    return;
                }
                *pressed_button_mr.borrow_mut() = None;
                let dims = cell_dims_mr.borrow();
                let col = (x / dims.width).floor() as usize;
                let row = (y / dims.height).floor() as usize;
                drop(dims);
                let state = gesture
                    .current_event()
                    .map(|e| e.modifier_state())
                    .unwrap_or_else(gdk::ModifierType::empty);
                let mut term = terminal_mr.lock();
                report_mouse(
                    &mut term,
                    MouseEvent::Release(MouseButton::Middle),
                    MousePosition::new(col, row, x.floor() as i32, y.floor() as i32),
                    gtk_state_to_mouse_mods(state),
                );
            });

            self.drawing_area.add_controller(middle_click_controller);
        }

        // Mouse motion for drag selection and hyperlink hover
        let motion_controller = gtk4::EventControllerMotion::new();

        let terminal_motion = Arc::clone(&terminal);
        let cell_dims_motion = Rc::clone(&cell_dims);
        let drawing_area_motion = self.drawing_area.clone();
        let selecting_motion = Rc::clone(&selecting);
        let last_position_motion = Rc::clone(&last_position);
        let pressed_button_motion = Rc::clone(&pressed_button);

        motion_controller.connect_motion(move |controller, x, y| {
            let dims = cell_dims_motion.borrow();
            let col = (x / dims.width).floor() as usize;
            let row = (y / dims.height).floor() as usize;
            drop(dims);

            // Track the pointer cell for the scroll handler (scroll events carry no
            // coordinates), and detect whether we moved to a new cell.
            let position = MousePosition::new(col, row, x.floor() as i32, y.floor() as i32);
            let previous_position = *last_position_motion.borrow();
            *last_position_motion.borrow_mut() = position;

            let state = controller
                .current_event()
                .map(|e| e.modifier_state())
                .unwrap_or_else(gdk::ModifierType::empty);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);

            // If a button was forwarded to a mouse-tracking app, this drag belongs to
            // the app: report motion (on cell change, to avoid flooding) and swallow it.
            if !shift {
                if let Some(button) = *pressed_button_motion.borrow() {
                    let mut term = terminal_motion.lock();
                    if mouse_position_changed(
                        term.screen().modes.mouse_encoding,
                        previous_position,
                        position,
                    ) {
                        report_mouse(
                            &mut term,
                            MouseEvent::Motion(Some(button)),
                            position,
                            gtk_state_to_mouse_mods(state),
                        );
                    }
                    return;
                }

                let mut term = terminal_motion.lock();
                if term.screen().modes.mouse_mode == MouseMode::AnyEvent {
                    if mouse_position_changed(
                        term.screen().modes.mouse_encoding,
                        previous_position,
                        position,
                    ) {
                        report_mouse(
                            &mut term,
                            MouseEvent::Motion(None),
                            position,
                            gtk_state_to_mouse_mods(state),
                        );
                    }
                    return;
                }
            }

            // Selection drag
            if *selecting_motion.borrow() {
                let mut term = terminal_motion.lock();
                let line = term.screen().visible_row_to_absolute_line(row);
                term.screen_mut().extend_selection(line, col);
                drop(term);
                drawing_area_motion.queue_draw();
                return;
            }

            // Check for hyperlink under cursor
            let term = terminal_motion.lock();
            let has_link = term
                .screen()
                .get_cell(row, col)
                .and_then(|c| c.hyperlink.as_ref())
                .is_some();
            let uri = term
                .screen()
                .get_cell(row, col)
                .and_then(|c| c.hyperlink.as_ref())
                .map(|h| h.uri.clone());
            drop(term);

            if has_link {
                drawing_area_motion.set_cursor_from_name(Some("pointer"));
                if let Some(uri) = uri {
                    drawing_area_motion.set_tooltip_text(Some(&uri));
                }
            } else {
                drawing_area_motion.set_cursor_from_name(Some("text"));
                drawing_area_motion.set_tooltip_text(None);
            }
        });

        self.drawing_area.add_controller(motion_controller);

        // Scroll handling
        let scroll_controller =
            EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        let terminal_scroll = Arc::clone(&terminal);
        let drawing_area_scroll = self.drawing_area.clone();
        let last_position_scroll = Rc::clone(&last_position);

        // Lines of cursor-key / viewport movement per wheel notch.
        const SCROLL_LINES: usize = 3;

        scroll_controller.connect_scroll(move |controller, _dx, dy| {
            let up = dy < 0.0;
            let state = controller
                .current_event()
                .map(|e| e.modifier_state())
                .unwrap_or_else(gdk::ModifierType::empty);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);

            let mut term = terminal_scroll.lock();

            // Shift+wheel always scrolls cterm's own scrollback, overriding any
            // application mouse/alternate-scroll handling (xterm/VTE convention).
            if !shift {
                // 1) Application is tracking the mouse: forward a wheel report.
                if mouse_tracking_active(&term) {
                    let position = *last_position_scroll.borrow();
                    let button = if up {
                        MouseButton::WheelUp
                    } else {
                        MouseButton::WheelDown
                    };
                    report_mouse(
                        &mut term,
                        MouseEvent::Press(button),
                        position,
                        gtk_state_to_mouse_mods(state),
                    );
                    return glib::Propagation::Stop;
                }

                // 2) Alternate screen + alternate-scroll: translate the wheel into
                //    cursor-key input so pagers (less/man) scroll.
                if term.screen().modes.alternate_screen && term.screen().modes.alternate_scroll {
                    let key = if up { Key::Up } else { Key::Down };
                    if let Some(bytes) = term.handle_key(key, Modifiers::empty()) {
                        for _ in 0..SCROLL_LINES {
                            let _ = term.write(&bytes);
                        }
                    }
                    return glib::Propagation::Stop;
                }
            }

            // 3) Default: scroll cterm's local scrollback viewport.
            if up {
                term.scroll_viewport_up(SCROLL_LINES);
            } else {
                term.scroll_viewport_down(SCROLL_LINES);
            }
            drop(term);
            drawing_area_scroll.queue_draw();
            glib::Propagation::Stop
        });

        self.drawing_area.add_controller(scroll_controller);
    }

    /// Set up file drag-and-drop
    fn setup_drop(&self) {
        let drop_target = gtk4::DropTarget::new(gio::File::static_type(), gdk::DragAction::COPY);
        let terminal = Arc::clone(&self.terminal);
        let drawing_area = self.drawing_area.clone();

        drop_target.connect_drop(move |_, value, _, _| {
            let file = match value.get::<gio::File>() {
                Ok(f) => f,
                Err(_) => return false,
            };
            let Some(path) = file.path() else {
                return false;
            };
            let info = match cterm_app::file_drop::FileDropInfo::from_path(&path) {
                Ok(info) => info,
                Err(e) => {
                    log::error!("Failed to read dropped file info: {}", e);
                    return false;
                }
            };

            // Get the parent window
            let Some(root) = drawing_area.root() else {
                return false;
            };
            let Some(window) = root.downcast_ref::<gtk4::Window>() else {
                return false;
            };

            let terminal = Arc::clone(&terminal);
            let info = std::rc::Rc::new(info);
            let info_for_cb = std::rc::Rc::clone(&info);

            crate::dialogs::show_file_drop_dialog(window, &info, move |choice| {
                use cterm_app::file_drop::{build_pty_input, FileDropAction};

                let action = match choice {
                    crate::dialogs::FileDropChoice::PastePath => FileDropAction::PastePath,
                    crate::dialogs::FileDropChoice::PasteContents => FileDropAction::PasteContents,
                    crate::dialogs::FileDropChoice::CreateViaBase64(name) => {
                        FileDropAction::CreateViaBase64 { filename: name }
                    }
                    crate::dialogs::FileDropChoice::CreateViaPrintf(name) => {
                        FileDropAction::CreateViaPrintf { filename: name }
                    }
                    crate::dialogs::FileDropChoice::Cancel => return,
                };

                let use_bracketed = matches!(action, FileDropAction::PasteContents);

                match build_pty_input(&info_for_cb, action) {
                    Ok(text) => {
                        let mut term = terminal.lock();
                        if use_bracketed && term.screen().modes.bracketed_paste {
                            let paste = format!("\x1b[200~{}\x1b[201~", text);
                            let _ = term.write_str(&paste);
                        } else {
                            let _ = term.write_str(&text);
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to build PTY input for dropped file: {}", e);
                    }
                }
            });

            true
        });

        self.drawing_area.add_controller(drop_target);
    }

    /// Set up resize handling for daemon-backed sessions.
    /// Resizes the local terminal and also notifies the daemon.
    fn setup_daemon_resize(&self, cmd_tx: tokio::sync::mpsc::UnboundedSender<DaemonCommand>) {
        let terminal = Arc::clone(&self.terminal);
        let cell_dims = Rc::clone(&self.cell_dims);

        self.drawing_area
            .connect_resize(move |_area, width, height| {
                let dims = cell_dims.borrow();
                let cols = ((width as f64) / dims.width).floor() as usize;
                let rows = ((height as f64) / dims.height).floor() as usize;
                drop(dims);

                if cols > 0 && rows > 0 {
                    // Resize local terminal (screen buffer)
                    let mut term = terminal.lock();
                    term.resize_with_pixels(
                        cols,
                        rows,
                        width.clamp(1, u16::MAX as i32) as u16,
                        height.clamp(1, u16::MAX as i32) as u16,
                    );
                    drop(term);

                    // Notify daemon of resize via command channel
                    let _ = cmd_tx.send(DaemonCommand::Resize {
                        cols: cols as u32,
                        rows: rows as u32,
                        pixel_width: width.max(1) as u32,
                        pixel_height: height.max(1) as u32,
                    });
                }
            });
    }

    /// Create a terminal widget backed by a daemon session.
    ///
    /// The Terminal has no PTY — input goes through the write callback to the
    /// daemon, and output is streamed from the daemon and parsed locally.
    pub fn from_daemon(
        session: cterm_client::SessionHandle,
        config: &Config,
        theme: &Theme,
    ) -> Self {
        let font_family = config.appearance.font.family.clone();
        let font_size = config.appearance.font.size;
        let cell_dims = calculate_cell_dimensions(&font_family, font_size);

        let drawing_area = DrawingArea::new();
        drawing_area.set_can_focus(true);
        drawing_area.set_focusable(true);
        drawing_area.add_css_class("terminal");
        drawing_area.set_vexpand(true);
        drawing_area.set_hexpand(true);

        // Keep enough space for a usable split without forcing every pane to
        // retain the initial 80x24 window size.
        let min_width = (cell_dims.width * 8.0).ceil() as i32;
        let min_height = (cell_dims.height * 3.0).ceil() as i32;
        drawing_area.set_size_request(min_width, min_height);

        // Capture session ID before session is consumed
        let sid = session.session_id().to_string();

        // Set up command channel — write/resize callbacks send to the background I/O thread
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<DaemonCommand>();

        // Create a Terminal with no PTY — write callback forwards via channel
        let mut terminal = Terminal::new(80, 24, ScreenConfig::default());
        configure_terminal_cursor(&mut terminal, config);
        terminal.set_base_palette(frontend_palette(theme, None));
        terminal.set_frontend_state(cterm_core::FrontendState {
            appearance: theme.appearance(),
            ..Default::default()
        });
        terminal.screen_mut().set_cell_width_hint(cell_dims.width);
        terminal.screen_mut().set_cell_height_hint(cell_dims.height);
        let write_tx = cmd_tx.clone();
        terminal.set_write_fn(Box::new(move |data: &[u8]| {
            let _ = write_tx.send(DaemonCommand::Write(data.to_vec()));
            Ok(())
        }));

        let terminal = Arc::new(Mutex::new(terminal));
        let cell_dims = Rc::new(RefCell::new(cell_dims));

        let widget = Self {
            drawing_area: drawing_area.clone(),
            terminal: Arc::clone(&terminal),
            theme: theme.clone(),
            font_family,
            font_size: Rc::new(RefCell::new(font_size)),
            default_font_size: font_size,
            cell_dims,
            sprite_cache: Rc::new(RefCell::new(SpriteCache::default())),
            blink_clock: Rc::new(RefCell::new(BlinkClock::default())),
            blink_started: Instant::now(),
            background_override: Rc::new(RefCell::new(None)),
            on_exit: Rc::new(RefCell::new(None)),
            on_bell: Rc::new(RefCell::new(None)),
            on_title_change: Rc::new(RefCell::new(None)),
            preedit: Rc::new(RefCell::new(PreeditState::default())),
            on_file_transfer: Rc::new(RefCell::new(None)),
            daemon_cmd_tx: Some(cmd_tx.clone()),
        };

        let daemon_socket = session.socket_path().map(|p| p.to_owned());
        widget.setup_drawing();
        widget.setup_blink_clock();
        widget.setup_visibility_reporting();
        widget.setup_input();
        widget.setup_drop();
        widget.setup_daemon_reader(
            sid,
            cmd_rx,
            daemon_socket,
            frontend_palette(theme, None),
            cterm_core::FrontendState {
                appearance: theme.appearance(),
                ..Default::default()
            },
            false,
        );
        widget.setup_daemon_resize(cmd_tx);

        widget
    }

    /// Create a terminal widget backed by a reconnected daemon session.
    ///
    /// Like `from_daemon`, but also applies an initial screen snapshot so the
    /// terminal shows the correct content immediately before streaming begins.
    pub fn from_daemon_with_screen(
        recon: cterm_app::daemon_reconnect::ReconnectedSession,
        config: &Config,
        theme: &Theme,
    ) -> Self {
        let font_family = config.appearance.font.family.clone();
        let font_size = config.appearance.font.size;
        let cell_dims = calculate_cell_dimensions(&font_family, font_size);

        let drawing_area = DrawingArea::new();
        drawing_area.set_can_focus(true);
        drawing_area.set_focusable(true);
        drawing_area.add_css_class("terminal");
        drawing_area.set_vexpand(true);
        drawing_area.set_hexpand(true);

        let min_width = (cell_dims.width * 8.0).ceil() as i32;
        let min_height = (cell_dims.height * 3.0).ceil() as i32;
        drawing_area.set_size_request(min_width, min_height);

        // Create a Terminal with no PTY
        let mut terminal = Terminal::new(80, 24, ScreenConfig::default());
        configure_terminal_cursor(&mut terminal, config);
        terminal.set_base_palette(frontend_palette(theme, None));
        terminal.set_frontend_state(cterm_core::FrontendState {
            appearance: theme.appearance(),
            ..Default::default()
        });
        terminal.screen_mut().set_cell_width_hint(cell_dims.width);
        terminal.screen_mut().set_cell_height_hint(cell_dims.height);

        // Apply screen snapshot BEFORE wrapping in Arc<Mutex<>>
        recon.apply_screen(&mut terminal);

        // Capture session ID before session is consumed
        let sid = recon.handle.session_id().to_string();

        // Set up command channel — write/resize callbacks send to the background I/O thread
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<DaemonCommand>();

        // Set up write callback to forward input via channel
        let write_tx = cmd_tx.clone();
        terminal.set_write_fn(Box::new(move |data: &[u8]| {
            let _ = write_tx.send(DaemonCommand::Write(data.to_vec()));
            Ok(())
        }));

        let terminal = Arc::new(Mutex::new(terminal));
        let cell_dims = Rc::new(RefCell::new(cell_dims));

        let widget = Self {
            drawing_area: drawing_area.clone(),
            terminal: Arc::clone(&terminal),
            theme: theme.clone(),
            font_family,
            font_size: Rc::new(RefCell::new(font_size)),
            default_font_size: font_size,
            cell_dims,
            sprite_cache: Rc::new(RefCell::new(SpriteCache::default())),
            blink_clock: Rc::new(RefCell::new(BlinkClock::default())),
            blink_started: Instant::now(),
            background_override: Rc::new(RefCell::new(None)),
            on_exit: Rc::new(RefCell::new(None)),
            on_bell: Rc::new(RefCell::new(None)),
            on_title_change: Rc::new(RefCell::new(None)),
            preedit: Rc::new(RefCell::new(PreeditState::default())),
            on_file_transfer: Rc::new(RefCell::new(None)),
            daemon_cmd_tx: Some(cmd_tx.clone()),
        };

        let daemon_socket = recon.handle.socket_path().map(|p| p.to_owned());
        widget.setup_drawing();
        widget.setup_blink_clock();
        widget.setup_visibility_reporting();
        widget.setup_input();
        widget.setup_drop();
        widget.setup_daemon_reader(
            sid,
            cmd_rx,
            daemon_socket,
            frontend_palette(theme, None),
            cterm_core::FrontendState {
                appearance: theme.appearance(),
                ..Default::default()
            },
            true,
        );
        widget.setup_daemon_resize(cmd_tx);

        widget
    }

    /// Set up the daemon output reader — streams raw PTY output from the daemon
    /// and feeds it through the local terminal parser.
    ///
    /// Creates a fresh daemon connection in its own tokio runtime because tonic
    /// channels are tied to the runtime that created them.
    ///
    /// `daemon_socket` specifies which socket to connect to. For remote (SSH-tunneled)
    /// sessions this is the local forwarded socket; for local sessions it's None.
    fn setup_daemon_reader(
        &self,
        session_id: String,
        cmd_rx: tokio::sync::mpsc::UnboundedReceiver<DaemonCommand>,
        daemon_socket: Option<std::path::PathBuf>,
        base_palette: ColorPalette,
        frontend_state: cterm_core::FrontendState,
        release_snapshot_attachment: bool,
    ) {
        let drawing_area = self.drawing_area.clone();

        let (tx, rx) = std::sync::mpsc::channel::<PtyMessage>();

        // Spawn I/O thread with its own tokio runtime and fresh daemon connection
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime for daemon reader");

            rt.block_on(async move {
                // Create a fresh connection to the same daemon (local or SSH-forwarded)
                let conn = match if let Some(ref path) = daemon_socket {
                    cterm_client::DaemonConnection::connect_unix(path, false).await
                } else {
                    cterm_client::DaemonConnection::connect_local().await
                } {
                    Ok(c) => c,
                    Err(e) => {
                        log::error!("Failed to connect to daemon for output stream: {}", e);
                        let _ = tx.send(PtyMessage::Exited);
                        return;
                    }
                };
                // The tab already has its screen applied; this reader only needs
                // the PTY stream. Skip the snapshot (avoids re-transferring full
                // scrollback) and pass 0×0 to leave the daemon size unchanged.
                let session = match conn.attach_session_no_snapshot(&session_id, 0, 0).await {
                    Ok(s) => s,
                    Err(e) => {
                        log::error!(
                            "Failed to attach to session {} for output stream: {}",
                            session_id,
                            e
                        );
                        let _ = tx.send(PtyMessage::Exited);
                        return;
                    }
                };

                // `from_daemon_with_screen` arrived with one attachment used
                // only to fetch the initial snapshot. The reader attachment is
                // now live, so balance that temporary count without dropping
                // the stream we are about to use.
                if release_snapshot_attachment {
                    if let Err(error) = session.detach().await {
                        log::warn!(
                            "Failed to release snapshot attachment for {session_id}: {error}"
                        );
                    }
                }

                if let Err(error) = session.set_base_palette(&base_palette).await {
                    log::warn!("Failed to synchronize frontend palette with daemon: {error}");
                }
                if let Err(error) = session.set_frontend_state(frontend_state).await {
                    log::warn!("Failed to synchronize frontend state with daemon: {error}");
                }

                // Spawn command handler — drains write/resize/destroy commands and forwards to daemon
                let cmd_session = session.clone();
                tokio::spawn(async move {
                    let mut cmd_rx = cmd_rx;

                    // Try to open a streaming-input RPC for low-latency keystroke
                    // delivery. Falls back to batched fire-and-forget write_input
                    // calls if the daemon doesn't support StreamInput.
                    let input_stream = if cmd_session.supports_stream_input().await {
                        match cmd_session.open_input_stream().await {
                            Ok(tx) => {
                                log::debug!("Using StreamInput for low-latency writes");
                                Some(tx)
                            }
                            Err(e) => {
                                log::warn!(
                                    "Failed to open input stream, falling back: {}",
                                    e
                                );
                                None
                            }
                        }
                    } else {
                        log::debug!(
                            "Daemon does not support StreamInput, using batched write_input"
                        );
                        None
                    };

                    let mut pushback: Option<DaemonCommand> = None;
                    loop {
                        let cmd = match pushback.take() {
                            Some(c) => c,
                            None => match cmd_rx.recv().await {
                                Some(c) => c,
                                None => break,
                            },
                        };

                        match cmd {
                            DaemonCommand::Write(data) => {
                                if let Some(ref tx) = input_stream {
                                    if tx.send(data).is_err() {
                                        log::error!("Input stream closed unexpectedly");
                                        break;
                                    }
                                } else {
                                    // Fallback: batch consecutive writes, fire-and-forget the RPC
                                    let mut batch = data;
                                    while let Ok(next) = cmd_rx.try_recv() {
                                        match next {
                                            DaemonCommand::Write(more) => {
                                                batch.extend_from_slice(&more)
                                            }
                                            other => {
                                                pushback = Some(other);
                                                break;
                                            }
                                        }
                                    }
                                    let s = cmd_session.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = s.write_input(&batch).await {
                                            log::error!("Failed to write to daemon: {}", e);
                                        }
                                    });
                                }
                            }
                            DaemonCommand::Resize {
                                cols,
                                rows,
                                pixel_width,
                                pixel_height,
                            } => {
                                if let Err(e) = cmd_session
                                    .resize_with_pixels(
                                        cols,
                                        rows,
                                        pixel_width,
                                        pixel_height,
                                    )
                                    .await
                                {
                                    log::error!("Failed to resize daemon session: {}", e);
                                }
                            }
                            DaemonCommand::Destroy => {
                                log::info!("Destroying daemon session");
                                if let Err(e) = cmd_session.destroy().await {
                                    log::error!("Failed to destroy daemon session: {}", e);
                                }
                                break;
                            }
                            DaemonCommand::Detach => {
                                log::info!("Detaching from daemon session");
                                if let Err(e) = cmd_session.detach().await {
                                    log::error!("Failed to detach from daemon session: {}", e);
                                }
                                break;
                            }
                            DaemonCommand::SetTitle(title) => {
                                if let Err(e) = cmd_session.set_custom_title(&title).await {
                                    log::error!("Failed to set custom title: {}", e);
                                }
                            }
                            DaemonCommand::SetTabColor(color) => {
                                if let Err(e) =
                                    cmd_session.set_metadata(None, Some(&color), None).await
                                {
                                    log::error!("Failed to set tab color: {}", e);
                                }
                            }
                            DaemonCommand::SetTemplateName(name) => {
                                if let Err(e) =
                                    cmd_session.set_metadata(None, None, Some(&name)).await
                                {
                                    log::error!("Failed to set template name: {}", e);
                                }
                            }
                            DaemonCommand::ClearAlert => {
                                if let Err(e) = cmd_session.clear_alert().await {
                                    log::error!("Failed to clear alert: {}", e);
                                }
                            }
                            DaemonCommand::SetPalette(palette) => {
                                if let Err(error) = cmd_session.set_base_palette(&palette).await {
                                    log::error!("Failed to update daemon palette: {error}");
                                }
                            }
                            DaemonCommand::SetFrontendState(state) => {
                                if let Err(error) = cmd_session.set_frontend_state(state).await {
                                    log::error!("Failed to update daemon frontend state: {error}");
                                }
                            }
                        }
                    }
                });

                // Notify used to cancel the output stream when process exits
                let exit_notify = Arc::new(tokio::sync::Notify::new());

                // Subscribe to event stream (process exit, etc.)
                let tx_events = tx.clone();
                let event_session = session.clone();
                let exit_notify_event = Arc::clone(&exit_notify);
                tokio::spawn(async move {
                    match event_session.stream_events().await {
                        Ok(mut stream) => {
                            use tokio_stream::StreamExt;
                            while let Some(result) = stream.next().await {
                                if let Ok(event) = result {
                                    match event.event {
                                        Some(cterm_proto::proto::terminal_event::Event::ProcessExited(_)) => {
                                            log::info!("Daemon reports process exited");
                                            exit_notify_event.notify_one();
                                            let _ = tx_events.send(PtyMessage::Exited);
                                            break;
                                        }
                                        Some(cterm_proto::proto::terminal_event::Event::Bell(_)) => {
                                            let _ = tx_events.send(PtyMessage::Bell);
                                        }
                                        Some(cterm_proto::proto::terminal_event::Event::SessionPrompt(prompt)) => {
                                            // Show a native dialog off the GTK main thread, then
                                            // send the user's reply back to the daemon.
                                            let p = prompt.clone();
                                            let (accept, secret) = tokio::task::spawn_blocking(move || {
                                                crate::ssh_prompt::show_ssh_prompt(&p)
                                            })
                                            .await
                                            .unwrap_or((false, None));
                                            let _ = event_session
                                                .respond_prompt(&prompt.prompt_id, accept, secret)
                                                .await;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("Failed to start daemon event stream: {}", e);
                        }
                    }
                });

                // Read output stream, cancellable by process exit notification
                tokio::select! {
                    _ = exit_notify.notified() => {
                        log::info!("Process exited, stopping output stream");
                    }
                    _ = async {
                        match session.stream_output().await {
                            Ok(mut stream) => {
                                use tokio_stream::StreamExt;
                                while let Some(result) = stream.next().await {
                                    match result {
                                        Ok(chunk) => {
                                            if tx.send(PtyMessage::Data(chunk.data)).is_err() {
                                                break;
                                            }
                                        }
                                        Err(e) => {
                                            log::error!("Daemon stream error: {}", e);
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                log::error!("Failed to start daemon output stream: {}", e);
                            }
                        }
                    } => {}
                }
                let _ = tx.send(PtyMessage::Exited);
            });
        });

        // Process messages on main thread
        let terminal_main = Arc::clone(&self.terminal);
        let on_exit = Rc::clone(&self.on_exit);
        let on_bell = Rc::clone(&self.on_bell);
        let on_title_change = Rc::clone(&self.on_title_change);
        let on_file_transfer = Rc::clone(&self.on_file_transfer);
        let blink_clock = Rc::clone(&self.blink_clock);
        let blink_started = self.blink_started;
        glib::timeout_add_local(Duration::from_millis(10), move || {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    PtyMessage::Data(data) => {
                        let mut term = terminal_main.lock();
                        let events = term.process_mirror(&data);
                        let mut content_changed = false;

                        for event in events {
                            match event {
                                TerminalEvent::ClipboardRequest(op) => {
                                    if let Some(display) = gdk::Display::default() {
                                        let clipboard = display.clipboard();
                                        match op {
                                            ClipboardOperation::Set { selection: _, data } => {
                                                if let Ok(text) = String::from_utf8(data) {
                                                    clipboard.set_text(&text);
                                                }
                                            }
                                            ClipboardOperation::Query { selection } => {
                                                let terminal_clip = Arc::clone(&terminal_main);
                                                let sel = selection;
                                                clipboard.read_text_async(
                                                    None::<&gio::Cancellable>,
                                                    move |result| {
                                                        let text = result
                                                            .ok()
                                                            .flatten()
                                                            .map(|s| s.to_string())
                                                            .unwrap_or_default();
                                                        let mut term = terminal_clip.lock();
                                                        let _ = term.send_clipboard_response(
                                                            sel,
                                                            text.as_bytes(),
                                                        );
                                                    },
                                                );
                                            }
                                        }
                                    }
                                }
                                TerminalEvent::Bell => {
                                    if let Some(ref callback) = *on_bell.borrow() {
                                        callback();
                                    }
                                }
                                TerminalEvent::TitleChanged(ref title) => {
                                    if let Some(ref callback) = *on_title_change.borrow() {
                                        callback(title);
                                    }
                                }
                                TerminalEvent::DesktopNotification(ref notification) => {
                                    crate::desktop_notification::handle(notification);
                                }
                                TerminalEvent::ContentChanged => content_changed = true,
                                TerminalEvent::ProcessExited(_) => {}
                            }
                        }

                        if term.screen().bell {
                            term.screen_mut().bell = false;
                            if let Some(ref callback) = *on_bell.borrow() {
                                callback();
                            }
                        }

                        let transfers = term.screen_mut().take_file_transfers();
                        drop(term);

                        for transfer in transfers {
                            if let Some(ref callback) = *on_file_transfer.borrow() {
                                callback(transfer);
                            }
                        }

                        if content_changed {
                            blink_clock
                                .borrow_mut()
                                .rearm_cursor(blink_started.elapsed());
                            terminal_main.lock().screen_mut().dirty = false;
                            drawing_area.queue_draw();
                        }
                    }
                    PtyMessage::Bell => {
                        if let Some(ref callback) = *on_bell.borrow() {
                            callback();
                        }
                    }
                    PtyMessage::Exited => {
                        log::info!("Daemon session stream ended");
                        if let Some(ref callback) = *on_exit.borrow() {
                            callback();
                        }
                        return glib::ControlFlow::Break;
                    }
                }
            }
            if terminal_main.lock().expire_synchronized_update() {
                terminal_main.lock().screen_mut().dirty = false;
                drawing_area.queue_draw();
            }
            glib::ControlFlow::Continue
        });
    }
}

/// Reset application-controlled state without discarding scrollback or the
/// native cursor defaults supplied by this frontend.
fn soft_reset_screen(screen: &mut cterm_core::Screen) {
    screen.cursor.reset_protocol_state();
    screen.style = cterm_core::cell::CellStyle::default();
    screen.modes = cterm_core::screen::TerminalModes {
        auto_wrap: true,
        reverse_wrap: true,
        modify_other_keys: 1,
        show_cursor: true,
        ..Default::default()
    };
    screen.reset_scroll_region();
    screen.dirty = true;
}

/// Calculate cell dimensions using Pango font metrics
fn calculate_cell_dimensions(font_family: &str, font_size: f64) -> CellDimensions {
    // Get the default font map and create a context
    let font_map = pangocairo::FontMap::default();
    let context = font_map.create_context();

    // Try the requested font first, then fall back to generic monospace
    let fonts_to_try = [font_family.to_string(), "monospace".to_string()];

    for font_name in &fonts_to_try {
        let font_desc =
            pango::FontDescription::from_string(&format!("{} {}", font_name, font_size));

        if let Some(font) = font_map.load_font(&context, &font_desc) {
            let metrics = font.metrics(None);
            // Use the approximate char width for monospace fonts
            let char_width = metrics.approximate_char_width() as f64 / pango::SCALE as f64;
            // Height is ascent + descent with some line spacing
            let ascent = metrics.ascent() as f64 / pango::SCALE as f64;
            let descent = metrics.descent() as f64 / pango::SCALE as f64;
            let height = ascent + descent;

            // Validate that we got sensible metrics. In particular, reject
            // Pango's missing-font sentinels before multiplying them into a
            // multi-gigabyte Wayland shared-memory surface.
            if let Some(dimensions) = CellDimensions::checked(char_width, height * 1.1) {
                log::debug!(
                    "Using font '{}' at {}pt: cell={}x{}",
                    font_name,
                    font_size,
                    dimensions.width,
                    dimensions.height
                );
                return dimensions;
            }
            log::warn!(
                "Ignoring invalid metrics for font '{}': width={}, height={}",
                font_name,
                char_width,
                height
            );
        }
    }

    // Last resort: use a Pango layout to measure a character directly
    let layout = pango::Layout::new(&context);
    let font_desc = pango::FontDescription::from_string(&format!("monospace {}", font_size));
    layout.set_font_description(Some(&font_desc));
    layout.set_text("M");

    let (width, height) = layout.pixel_size();
    if let Some(dimensions) = CellDimensions::checked(width as f64, height as f64 * 1.1) {
        log::warn!(
            "Font metrics unavailable, using layout measurement: {}x{}",
            width,
            height
        );
        return dimensions;
    }

    let fallback = CellDimensions::conservative_fallback(font_size);
    log::error!(
        "No usable font metrics; using conservative cell size {}x{}",
        fallback.width,
        fallback.height
    );
    fallback
}

/// Messages from PTY reader thread
enum PtyMessage {
    Data(Vec<u8>),
    Bell,
    Exited,
}

/// Commands sent to the daemon I/O thread
enum DaemonCommand {
    Write(Vec<u8>),
    Resize {
        cols: u32,
        rows: u32,
        pixel_width: u32,
        pixel_height: u32,
    },
    /// Kill the remote PTY and shut down the I/O loop.
    Destroy,
    /// Tell the daemon to detach (keeping the remote PTY alive) and shut down
    /// the I/O loop. Used by the right-click "Disconnect" path so a closed tab
    /// does not terminate the underlying remote session.
    Detach,
    SetTitle(String),
    SetTabColor(String),
    SetTemplateName(String),
    ClearAlert,
    SetPalette(ColorPalette),
    SetFrontendState(cterm_core::FrontendState),
}

fn update_window_visibility(
    terminal: &Arc<Mutex<Terminal>>,
    sender: Option<&tokio::sync::mpsc::UnboundedSender<DaemonCommand>>,
    visibility: cterm_core::WindowVisibility,
) {
    let mut terminal = terminal.lock();
    let mut state = terminal.screen().frontend_state();
    if state.visibility == visibility {
        return;
    }
    state.visibility = visibility;
    if sender.is_some() {
        let _ = terminal.set_frontend_state_collecting(state);
    } else {
        terminal.set_frontend_state(state);
    }
    drop(terminal);
    if let Some(sender) = sender {
        let _ = sender.send(DaemonCommand::SetFrontendState(state));
    }
}

/// Rendering parameters for draw_terminal
struct RenderConfig<'a> {
    font_family: &'a str,
    font_size: f64,
    cell_dims: CellDimensions,
    background_override: Option<cterm_core::color::Rgb>,
}

fn configure_terminal_cursor(terminal: &mut Terminal, config: &Config) {
    terminal.screen_mut().configure_cursor(
        config.appearance.cursor_style.core_style(),
        config.appearance.cursor_blink,
    );
}

/// Draw the terminal contents
fn draw_terminal(
    cr: &cairo::Context,
    terminal: &Arc<Mutex<Terminal>>,
    theme: &Theme,
    config: &RenderConfig<'_>,
    preedit: &PreeditState,
    sprite_cache: &mut SpriteCache,
    blink_phase: BlinkPhase,
) {
    let term = terminal.lock();
    let screen = term.screen();
    let palette = frontend_palette(theme, config.background_override);
    let palette = screen.resolved_palette(&palette);
    let palette = &palette;

    // Dynamic OSC colors override the configured theme/template palette.
    let normal_background = &palette.background;
    let bg = if screen.modes.reverse_video {
        &palette.foreground
    } else {
        normal_background
    };
    let (r, g, b) = bg.to_f64();
    cr.set_source_rgb(r, g, b);
    cr.paint().ok();

    // Create Pango layout for text rendering
    let pango_context = pangocairo::functions::create_context(cr);
    let layout = pango::Layout::new(&pango_context);

    // Set font
    let font_desc = pango::FontDescription::from_string(&format!(
        "{} {}",
        config.font_family, config.font_size
    ));
    layout.set_font_description(Some(&font_desc));

    // Use pre-calculated cell dimensions
    let cell_width = config.cell_dims.width;
    let cell_height = config.cell_dims.height;

    // Draw cells - use absolute line indices to render scrollback content
    let grid = screen.grid();
    let scroll_offset = screen.scroll_offset;
    let rows = grid.height();
    let cols = grid.width();

    for row_idx in 0..rows {
        let y = row_idx as f64 * cell_height;
        let absolute_line = screen.visible_row_to_absolute_line(row_idx);

        for col_idx in 0..cols {
            let cell = if let Some(c) = screen.get_cell_with_scrollback(absolute_line, col_idx) {
                c
            } else {
                continue;
            };
            let x = col_idx as f64 * cell_width;
            let foreground_visible = cell_foreground_visible(cell.attrs, blink_phase);

            // Skip wide char spacers
            if cell.attrs.contains(CellAttrs::WIDE_SPACER) {
                continue;
            }

            // Check if this cell is selected
            let is_selected = screen.is_selected(absolute_line, col_idx);

            // Determine if cell has INVERSE attribute (XOR with selection)
            let is_inverted =
                cell.attrs.contains(CellAttrs::INVERSE) ^ is_selected ^ screen.modes.reverse_video;
            let char_width = if cell.attrs.contains(CellAttrs::WIDE) {
                cell_width * 2.0
            } else {
                cell_width
            };

            let fg_color = if is_inverted {
                if cell.bg == Color::Default {
                    *normal_background
                } else {
                    screen.resolve_color(cell.bg, palette)
                }
            } else if cell.hyperlink.is_some() && cell.fg == Color::Default {
                Rgb::new(100, 149, 237)
            } else if cell.fg == Color::Default {
                palette.foreground
            } else {
                screen.resolve_color(cell.fg, palette)
            };
            let fg_color = if cell.attrs.contains(CellAttrs::DIM) {
                Rgb::new(fg_color.r / 2, fg_color.g / 2, fg_color.b / 2)
            } else {
                fg_color
            };

            // Draw background (always draw for selected cells to show highlight)
            let needs_bg = cell.bg != Color::Default
                || is_inverted
                || is_selected
                || screen.modes.reverse_video;

            if needs_bg {
                let bg_color = if is_inverted {
                    // Inverted: use foreground color as background
                    if cell.fg == Color::Default {
                        palette.foreground
                    } else {
                        screen.resolve_color(cell.fg, palette)
                    }
                } else if cell.bg == Color::Default {
                    *normal_background
                } else {
                    screen.resolve_color(cell.bg, palette)
                };

                let (r, g, b) = bg_color.to_f64();
                cr.set_source_rgb(r, g, b);

                cr.rectangle(x, y, char_width, cell_height);
                cr.fill().ok();
            }

            // Draw character
            if foreground_visible && cell.text() != " " && !cell.attrs.contains(CellAttrs::HIDDEN) {
                let sprite_width = cell_width.round().max(1.0) as u32;
                let sprite_height = cell_height.round().max(1.0) as u32;
                if let Some(sprite) = cell
                    .single_char()
                    .and_then(|c| sprite_cache.get(c as u32, sprite_width, sprite_height))
                {
                    draw_sprite(cr, sprite, x, y, cell_width, cell_height, &fg_color);
                } else {
                    let (r, g, b) = fg_color.to_f64();
                    cr.set_source_rgb(r, g, b);

                    // Apply text attributes to font
                    let attrs = pango::AttrList::new();

                    if cell.attrs.contains(CellAttrs::BOLD) {
                        let attr = pango::AttrInt::new_weight(pango::Weight::Bold);
                        attrs.insert(attr);
                    }

                    if cell.attrs.contains(CellAttrs::ITALIC) {
                        let attr = pango::AttrInt::new_style(pango::Style::Italic);
                        attrs.insert(attr);
                    }

                    layout.set_attributes(Some(&attrs));
                    layout.set_text(cell.text());

                    cr.move_to(x, y);
                    pangocairo::functions::show_layout(cr, &layout);

                    // Reset attributes
                    layout.set_attributes(None::<&pango::AttrList>);
                }
            }

            if foreground_visible && !cell.attrs.contains(CellAttrs::HIDDEN) {
                draw_cell_decorations(
                    cr,
                    cell,
                    (x, y),
                    (char_width, cell_height),
                    &fg_color,
                    palette,
                    screen,
                );
            }
        }
    }

    // Images replace the cells beneath them and must therefore be composited
    // after the text grid but before the cursor and overlays.
    draw_terminal_images(cr, screen, cell_width, cell_height);

    // Draw cursor
    if cursor_visible(screen, blink_phase) {
        let cursor = &screen.cursor;
        let x = cursor.col as f64 * cell_width;
        let y = cursor.row as f64 * cell_height;

        let (r, g, b) = palette.cursor.to_f64();
        cr.set_source_rgb(r, g, b);

        match cursor.style {
            CursorStyle::Block => {
                cr.rectangle(x, y, cell_width, cell_height);
                cr.fill().ok();

                // Draw character under cursor with inverted color
                if let Some(cell) = screen.get_cell(cursor.row, cursor.col) {
                    if cell.text() != " " && !cell.attrs.contains(CellAttrs::HIDDEN) {
                        let (r, g, b) = theme.cursor.text_color.to_f64();
                        cr.set_source_rgb(r, g, b);
                        layout.set_text(cell.text());
                        cr.move_to(x, y);
                        pangocairo::functions::show_layout(cr, &layout);
                    }
                }
            }
            CursorStyle::Underline => {
                cr.rectangle(x, y + cell_height - 2.0, cell_width, 2.0);
                cr.fill().ok();
            }
            CursorStyle::Bar => {
                cr.rectangle(x, y, 2.0, cell_height);
                cr.fill().ok();
            }
        }
    }

    // Draw IM preedit (composition) text at the cursor position
    if preedit.active && !preedit.text.is_empty() && scroll_offset == 0 {
        let cursor = &screen.cursor;
        let x = cursor.col as f64 * cell_width;
        let y = cursor.row as f64 * cell_height;

        // Draw preedit background
        let preedit_width = preedit.text.chars().count() as f64 * cell_width;
        let (r, g, b) = palette.foreground.to_f64();
        cr.set_source_rgb(r, g, b);
        cr.rectangle(x, y, preedit_width, cell_height);
        cr.fill().ok();

        // Draw preedit text
        let (r, g, b) = palette.background.to_f64();
        cr.set_source_rgb(r, g, b);
        layout.set_text(&preedit.text);
        cr.move_to(x, y);
        pangocairo::functions::show_layout(cr, &layout);

        // Draw underline to indicate composition
        let (r, g, b) = palette.foreground.to_f64();
        cr.set_source_rgb(r, g, b);
        cr.rectangle(x, y + cell_height - 1.0, preedit_width, 1.0);
        cr.fill().ok();
    }

    // Draw scrollbar overlay when there is scrollback content
    let scrollback_len = screen.scrollback().len();
    if scrollback_len > 0 {
        let total_lines = scrollback_len + rows;
        let view_height = rows as f64 * cell_height;
        let view_width = cols as f64 * cell_width;

        let bar_width: f64 = 6.0;
        let bar_inset: f64 = 2.0;
        let bar_x = view_width - bar_width - bar_inset;
        let min_thumb_height: f64 = 20.0;

        let thumb_height = (rows as f64 / total_lines as f64 * view_height).max(min_thumb_height);

        let scrollable = view_height - thumb_height;
        let fraction = screen.scroll_offset as f64 / scrollback_len as f64;
        // fraction=0 (at bottom) → thumb at bottom, fraction=1 → thumb at top
        let thumb_y = (1.0 - fraction) * scrollable;

        let opacity = if screen.scroll_offset > 0 { 0.5 } else { 0.25 };
        let radius = bar_width / 2.0;

        // Draw rounded rect thumb
        cr.new_sub_path();
        cr.arc(
            bar_x + bar_width - radius,
            thumb_y + radius,
            radius,
            -std::f64::consts::FRAC_PI_2,
            0.0,
        );
        cr.arc(
            bar_x + bar_width - radius,
            thumb_y + thumb_height - radius,
            radius,
            0.0,
            std::f64::consts::FRAC_PI_2,
        );
        cr.arc(
            bar_x + radius,
            thumb_y + thumb_height - radius,
            radius,
            std::f64::consts::FRAC_PI_2,
            std::f64::consts::PI,
        );
        cr.arc(
            bar_x + radius,
            thumb_y + radius,
            radius,
            std::f64::consts::PI,
            3.0 * std::f64::consts::FRAC_PI_2,
        );
        cr.close_path();
        cr.set_source_rgba(0.5, 0.5, 0.5, opacity);
        cr.fill().ok();
    }
}

pub(crate) fn frontend_palette(theme: &Theme, background: Option<Rgb>) -> ColorPalette {
    let mut palette = theme.colors.clone();
    palette.cursor = theme.cursor.color;
    if let Some(background) = background {
        palette.background = background;
    }
    palette
}

pub(crate) fn parse_rgb(hex: &str) -> Option<Rgb> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    Some(Rgb::new(
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

fn draw_cell_decorations(
    cr: &cairo::Context,
    cell: &cterm_core::Cell,
    origin: (f64, f64),
    size: (f64, f64),
    foreground: &Rgb,
    palette: &cterm_core::color::ColorPalette,
    screen: &cterm_core::Screen,
) {
    let (x, y) = origin;
    let (width, height) = size;
    let has_hyperlink = cell.hyperlink.is_some();
    if cell.attrs.has_underline() || has_hyperlink {
        let color = if has_hyperlink {
            Rgb::new(100, 149, 237)
        } else if let Some(color) = cell.underline_color {
            screen.resolve_color(color, palette)
        } else {
            *foreground
        };
        let (red, green, blue) = color.to_f64();
        let underline_y = y + height - 2.0;
        cr.set_source_rgb(red, green, blue);
        cr.set_line_width(1.0);

        if cell.attrs.contains(CellAttrs::CURLY_UNDERLINE) {
            let mut current_x = x;
            let mut rising = true;
            cr.move_to(current_x, underline_y + 1.0);
            while current_x < x + width {
                current_x = (current_x + 2.0).min(x + width);
                cr.line_to(
                    current_x,
                    if rising {
                        underline_y - 1.0
                    } else {
                        underline_y + 1.0
                    },
                );
                rising = !rising;
            }
        } else {
            if cell.attrs.contains(CellAttrs::DOTTED_UNDERLINE) {
                cr.set_dash(&[1.0, 2.0], 0.0);
            } else if cell.attrs.contains(CellAttrs::DASHED_UNDERLINE) {
                cr.set_dash(&[4.0, 2.0], 0.0);
            }
            cr.move_to(x, underline_y);
            cr.line_to(x + width, underline_y);
        }
        cr.stroke().ok();
        cr.set_dash(&[], 0.0);

        if cell.attrs.contains(CellAttrs::DOUBLE_UNDERLINE) {
            cr.move_to(x, underline_y - 2.0);
            cr.line_to(x + width, underline_y - 2.0);
            cr.stroke().ok();
        }
    }

    let (red, green, blue) = foreground.to_f64();
    cr.set_source_rgb(red, green, blue);
    cr.set_line_width(1.0);
    if cell.attrs.contains(CellAttrs::STRIKETHROUGH) {
        cr.move_to(x, y + height / 2.0);
        cr.line_to(x + width, y + height / 2.0);
        cr.stroke().ok();
    }
    if cell.attrs.contains(CellAttrs::OVERLINE) {
        cr.move_to(x, y + 1.0);
        cr.line_to(x + width, y + 1.0);
        cr.stroke().ok();
    }
}

fn draw_sprite(
    cr: &cairo::Context,
    sprite: &Sprite,
    x: f64,
    y: f64,
    cell_width: f64,
    cell_height: f64,
    color: &Rgb,
) {
    let width = usize::from(sprite.width);
    let height = usize::from(sprite.height);
    let (red, green, blue) = color.to_f64();

    cr.save().ok();
    cr.translate(x, y);
    cr.scale(cell_width / width as f64, cell_height / height as f64);

    for (row_index, row) in sprite.bytes.chunks_exact(width).enumerate() {
        let mut column = 0;
        while column < width {
            let alpha = row[column];
            let start = column;
            while column < width && row[column] == alpha {
                column += 1;
            }
            if alpha == 0 {
                continue;
            }
            cr.set_source_rgba(red, green, blue, f64::from(alpha) / 255.0);
            cr.rectangle(start as f64, row_index as f64, (column - start) as f64, 1.0);
            cr.fill().ok();
        }
    }

    cr.restore().ok();
}

/// Draw the terminal's decoded inline images with Cairo.
fn draw_terminal_images(
    cr: &cairo::Context,
    screen: &cterm_core::Screen,
    cell_width: f64,
    cell_height: f64,
) {
    for image in screen.visible_images() {
        let Some(visible_row) = screen.image_visible_row(image) else {
            continue;
        };
        let Ok(width) = i32::try_from(image.pixel_width) else {
            log::warn!("Terminal image {} is too wide for Cairo", image.id);
            continue;
        };
        let Ok(height) = i32::try_from(image.pixel_height) else {
            log::warn!("Terminal image {} is too tall for Cairo", image.id);
            continue;
        };
        let expected_len = image
            .pixel_width
            .checked_mul(image.pixel_height)
            .and_then(|pixels| pixels.checked_mul(4));
        if expected_len != Some(image.data.len()) {
            log::warn!("Terminal image {} has invalid RGBA data", image.id);
            continue;
        }

        let Some(pixels) = cterm_ui::rgba_to_premultiplied_bgra(image.data.as_slice()) else {
            log::warn!("Terminal image {} has invalid RGBA data", image.id);
            continue;
        };
        let Ok(stride) = cairo::Format::ARgb32.stride_for_width(image.pixel_width as u32) else {
            log::warn!("Terminal image {} has an invalid Cairo stride", image.id);
            continue;
        };
        let Ok(surface) = cairo::ImageSurface::create_for_data(
            pixels,
            cairo::Format::ARgb32,
            width,
            height,
            stride,
        ) else {
            log::warn!(
                "Failed to create a Cairo surface for terminal image {}",
                image.id
            );
            continue;
        };

        let x = image.col as f64 * cell_width;
        let y = visible_row as f64 * cell_height;
        if cr.save().is_err() {
            continue;
        }
        cr.rectangle(x, y, image.pixel_width as f64, image.pixel_height as f64);
        cr.clip();
        if cr.set_source_surface(&surface, x, y).is_ok() {
            let _ = cr.paint();
        }
        let _ = cr.restore();
    }
}

/// Extract mouse-report modifier bits from a GTK modifier state.
fn gtk_state_to_mouse_mods(state: gdk::ModifierType) -> MouseModifiers {
    MouseModifiers {
        shift: state.contains(gdk::ModifierType::SHIFT_MASK),
        alt: state.contains(gdk::ModifierType::ALT_MASK),
        ctrl: state.contains(gdk::ModifierType::CONTROL_MASK),
    }
}

/// Whether an application has enabled any mouse tracking mode.
fn mouse_tracking_active(term: &Terminal) -> bool {
    term.screen().modes.mouse_mode != MouseMode::None
}

/// Encode a mouse event for the current tracking/encoding modes and, if it
/// produces a report, write it to the PTY. Returns true if the event was
/// consumed (a report was sent), false if mouse reporting is inactive for it.
fn report_mouse(
    term: &mut Terminal,
    event: MouseEvent,
    position: MousePosition,
    mods: MouseModifiers,
) -> bool {
    let mode = term.screen().modes.mouse_mode;
    let encoding = term.screen().modes.mouse_encoding;
    if let Some(seq) = encode_mouse_event(mode, encoding, event, position, mods) {
        let _ = term.write(&seq);
        true
    } else {
        false
    }
}

fn mouse_position_changed(
    encoding: MouseEncoding,
    previous: MousePosition,
    current: MousePosition,
) -> bool {
    if encoding == MouseEncoding::SgrPixels {
        (previous.pixel_x, previous.pixel_y) != (current.pixel_x, current.pixel_y)
    } else {
        (previous.col, previous.row) != (current.col, current.row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_dimensions_reject_missing_font_sentinels() {
        assert!(CellDimensions::checked(8.0, 18.0).is_some());
        assert!(CellDimensions::checked(f64::NAN, 18.0).is_none());
        assert!(CellDimensions::checked(8.0, f64::INFINITY).is_none());
        assert!(CellDimensions::checked(8.0, 192_185.0).is_none());
        assert!(CellDimensions::checked(0.0, 18.0).is_none());

        let fallback = CellDimensions::conservative_fallback(f64::NAN);
        assert_eq!(fallback.width, 9.0);
        assert_eq!(fallback.height, 18.0);
    }

    #[test]
    fn native_cursor_config_initializes_protocol_defaults() {
        let mut config = Config::default();
        config.appearance.cursor_style = cterm_app::config::CursorStyleConfig::Bar;
        config.appearance.cursor_blink = false;
        let mut terminal = Terminal::new(8, 2, ScreenConfig::default());

        configure_terminal_cursor(&mut terminal, &config);

        assert_eq!(terminal.screen().cursor.style, CursorStyle::Bar);
        assert!(!terminal.screen().cursor.blink.enabled());
        terminal.process(b"\x1b[?12h");
        assert!(terminal.screen().cursor.blink.enabled());
        terminal.process(b"\x1b[?12l\x1b[0 q");
        assert_eq!(terminal.screen().cursor.style, CursorStyle::Bar);
        assert!(!terminal.screen().cursor.blink.enabled());
    }

    #[test]
    fn soft_reset_preserves_native_cursor_defaults() {
        let mut screen = cterm_core::Screen::new(8, 2, ScreenConfig::default());
        screen.configure_cursor(CursorStyle::Bar, false);
        screen.cursor.restore_protocol_snapshot(
            Some(CursorStyle::Underline),
            Some(true),
            Some(true),
        );

        soft_reset_screen(&mut screen);

        assert_eq!(screen.cursor.style, CursorStyle::Bar);
        assert_eq!(screen.cursor.configured_style(), CursorStyle::Bar);
        assert!(!screen.cursor.blink.configured());
        assert!(!screen.cursor.blink.enabled());
    }
}
