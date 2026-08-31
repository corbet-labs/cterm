//! Quick Open overlay for rapidly searching and opening tab templates
//!
//! Shows a VS Code-style overlay at the top of the window for filtering
//! templates and open tabs with custom names.

use cterm_app::config::StickyTabConfig;
use cterm_app::{template_type_indicator, QuickOpenMatcher, TemplateMatch};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSColor, NSControlTextEditingDelegate, NSFont, NSStackView, NSTextField, NSTextFieldDelegate,
    NSUserInterfaceLayoutOrientation, NSView,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSInteger, NSObjectProtocol, NSPoint, NSRange, NSRect, NSSize,
    NSString,
};
use std::cell::{Cell, RefCell};

/// Height of the Quick Open overlay
pub const QUICK_OPEN_HEIGHT: f64 = 250.0;

/// Maximum number of results to display
const MAX_RESULTS: usize = 8;

/// An open tab that can be switched to via Quick Open
#[derive(Debug, Clone)]
pub struct OpenTabEntry {
    /// Display name (the custom title)
    pub name: String,
    /// Window pointer as usize for identification
    pub window_ptr: usize,
}

/// A filtered result in Quick Open - either a template or an open tab
#[derive(Debug, Clone)]
enum QuickOpenEntry {
    Template(Box<TemplateMatch>),
    OpenTab {
        entry: OpenTabEntry,
        score: i32,
        match_positions: Vec<usize>,
    },
}

impl QuickOpenEntry {
    fn name(&self) -> &str {
        match self {
            QuickOpenEntry::Template(m) => &m.template.name,
            QuickOpenEntry::OpenTab { entry, .. } => &entry.name,
        }
    }

    fn score(&self) -> i32 {
        match self {
            QuickOpenEntry::Template(m) => m.score,
            QuickOpenEntry::OpenTab { score, .. } => *score,
        }
    }
}

/// Ivars for the Quick Open overlay
pub struct QuickOpenOverlayIvars {
    /// Search text field
    search_field: RefCell<Option<Retained<NSTextField>>>,
    /// Results container stack view
    results_container: RefCell<Option<Retained<NSStackView>>>,
    /// Currently filtered entries (templates + open tabs)
    filtered: RefCell<Vec<QuickOpenEntry>>,
    /// All available templates
    templates: RefCell<Vec<StickyTabConfig>>,
    /// Currently open tabs with custom names
    open_tabs: RefCell<Vec<OpenTabEntry>>,
    /// Currently selected index
    selected_index: Cell<usize>,
    /// Callback when a template is selected
    on_select: RefCell<Option<Box<dyn Fn(StickyTabConfig)>>>,
    /// Callback when an open tab is selected (receives window pointer)
    on_switch_tab: RefCell<Option<Box<dyn Fn(usize)>>>,
    /// Result row views for highlighting
    result_rows: RefCell<Vec<Retained<NSView>>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "QuickOpenOverlay"]
    #[ivars = QuickOpenOverlayIvars]
    pub struct QuickOpenOverlay;

    unsafe impl NSObjectProtocol for QuickOpenOverlay {}

    // NSControlTextEditingDelegate is required by NSTextFieldDelegate
    unsafe impl NSControlTextEditingDelegate for QuickOpenOverlay {
        /// Intercept commands from the text field (Enter, Escape, arrow keys)
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn control_text_view_do_command_by_selector(
            &self,
            _control: &objc2_app_kit::NSControl,
            _text_view: &objc2_app_kit::NSTextView,
            command_selector: objc2::runtime::Sel,
        ) -> bool {
            let sel_name = command_selector.name().to_str().unwrap_or("");
            log::debug!("doCommandBySelector: {}", sel_name);

            match sel_name {
                "insertNewline:" => {
                    self.confirm_selection();
                    true // We handled it
                }
                "cancelOperation:" => {
                    self.hide();
                    true
                }
                "moveUp:" => {
                    self.select_previous();
                    true
                }
                "moveDown:" => {
                    self.select_next();
                    true
                }
                "insertTab:" => {
                    self.select_next();
                    true
                }
                "insertBacktab:" => {
                    self.select_previous();
                    true
                }
                _ => false, // Let the text field handle it
            }
        }

        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, _notification: &objc2_foundation::NSNotification) {
            self.update_filter();
        }

        #[unsafe(method(controlTextDidEndEditing:))]
        fn control_text_did_end_editing(&self, _notification: &objc2_foundation::NSNotification) {
            // Text field lost focus - dismiss the overlay
            self.hide();
        }
    }

    unsafe impl NSTextFieldDelegate for QuickOpenOverlay {}

    impl QuickOpenOverlay {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        /// Handle key events for navigation
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &objc2_app_kit::NSEvent) {
            let key_code = event.keyCode();

            match key_code {
                125 => self.select_next(),     // Down arrow
                126 => self.select_previous(), // Up arrow
                36 => self.confirm_selection(), // Enter/Return
                53 => self.hide(),             // Escape
                48 => self.select_next(),      // Tab
                _ => {
                    // Pass to super for normal text input
                    unsafe {
                        let _: () = msg_send![super(self), keyDown: event];
                    }
                }
            }
        }
    }
);

impl QuickOpenOverlay {
    /// Create a new Quick Open overlay
    pub fn new(
        mtm: MainThreadMarker,
        width: f64,
        templates: Vec<StickyTabConfig>,
    ) -> Retained<Self> {
        let frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(width, QUICK_OPEN_HEIGHT),
        );

        let this = mtm.alloc::<Self>();
        let this = this.set_ivars(QuickOpenOverlayIvars {
            search_field: RefCell::new(None),
            results_container: RefCell::new(None),
            filtered: RefCell::new(Vec::new()),
            templates: RefCell::new(templates),
            open_tabs: RefCell::new(Vec::new()),
            selected_index: Cell::new(0),
            on_select: RefCell::new(None),
            on_switch_tab: RefCell::new(None),
            result_rows: RefCell::new(Vec::new()),
        });

        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };

        // Set up appearance
        this.setWantsLayer(true);
        if let Some(layer) = this.layer() {
            unsafe {
                // Dark semi-transparent background
                let color = NSColor::colorWithSRGBRed_green_blue_alpha(0.12, 0.12, 0.12, 0.95);
                let cg_color: *mut std::ffi::c_void = msg_send![&*color, CGColor];
                let _: () = msg_send![&*layer, setBackgroundColor: cg_color];
                // Rounded corners
                let _: () = msg_send![&*layer, setCornerRadius: 8.0f64];
            }
        }

        // Create UI elements
        this.setup_ui(mtm, width);

        // Initially hidden
        this.setHidden(true);

        // Initialize with all templates
        this.update_filter();

        this
    }

    fn setup_ui(&self, mtm: MainThreadMarker, width: f64) {
        let padding = 12.0;
        let search_height = 28.0;
        let search_width = width - padding * 2.0;

        // Create search field
        let search_frame = NSRect::new(
            NSPoint::new(padding, padding),
            NSSize::new(search_width, search_height),
        );
        let search_field = unsafe { NSTextField::initWithFrame(mtm.alloc(), search_frame) };
        search_field
            .setPlaceholderString(Some(&NSString::from_str("Search templates and tabs...")));
        search_field.setBezeled(true);
        search_field.setEditable(true);
        search_field.setSelectable(true);

        // Style the search field
        unsafe {
            let font = NSFont::systemFontOfSize(14.0);
            search_field.setFont(Some(&font));
            search_field.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(self)));
        }

        unsafe {
            self.addSubview(&search_field);
        }
        *self.ivars().search_field.borrow_mut() = Some(search_field);

        // Create results container
        let results_y = padding + search_height + 8.0;
        let results_height = QUICK_OPEN_HEIGHT - results_y - padding;
        let results_frame = NSRect::new(
            NSPoint::new(padding, results_y),
            NSSize::new(search_width, results_height),
        );

        let results_container = unsafe {
            let stack = NSStackView::initWithFrame(mtm.alloc(), results_frame);
            stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
            stack.setAlignment(objc2_app_kit::NSLayoutAttribute::Leading);
            stack.setSpacing(2.0);
            // Use GravityAreas distribution to prevent rows from stretching to fill
            stack.setDistribution(objc2_app_kit::NSStackViewDistribution::GravityAreas);
            stack
        };

        unsafe {
            self.addSubview(&results_container);
        }
        *self.ivars().results_container.borrow_mut() = Some(results_container);
    }

    /// Show the overlay and focus the search field
    pub fn show(&self) {
        self.setHidden(false);

        // Clear search field and reset selection
        if let Some(ref search_field) = *self.ivars().search_field.borrow() {
            search_field.setStringValue(&NSString::from_str(""));
            unsafe {
                self.window()
                    .map(|w| w.makeFirstResponder(Some(search_field)));
            }
        }

        // Reset filter to show all templates
        self.ivars().selected_index.set(0);
        self.update_filter();
    }

    /// Hide the overlay
    pub fn hide(&self) {
        self.setHidden(true);

        // Return focus to the window's content view (terminal)
        unsafe {
            if let Some(window) = self.window() {
                if let Some(content) = window.contentView() {
                    window.makeFirstResponder(Some(&content));
                }
            }
        }
    }

    /// Check if overlay is visible
    pub fn is_visible(&self) -> bool {
        !self.is_hidden()
    }

    fn is_hidden(&self) -> bool {
        unsafe { msg_send![self, isHidden] }
    }

    /// Set the callback for template selection
    pub fn set_on_select<F>(&self, callback: F)
    where
        F: Fn(StickyTabConfig) + 'static,
    {
        *self.ivars().on_select.borrow_mut() = Some(Box::new(callback));
    }

    /// Update the filter based on current search text
    fn update_filter(&self) {
        let query = self
            .ivars()
            .search_field
            .borrow()
            .as_ref()
            .map(|f| f.stringValue().to_string())
            .unwrap_or_default();

        // Filter templates
        let templates = self.ivars().templates.borrow().clone();
        let matcher = QuickOpenMatcher::new(templates);
        let template_matches = matcher.filter(&query);

        // Filter open tabs using the same fuzzy matching logic
        let open_tabs = self.ivars().open_tabs.borrow().clone();
        let tab_matches = Self::filter_open_tabs(&open_tabs, &query);

        // Merge results: open tabs first (with score boost), then templates
        let mut entries: Vec<QuickOpenEntry> = Vec::new();
        for m in tab_matches {
            entries.push(m);
        }
        for m in template_matches {
            entries.push(QuickOpenEntry::Template(Box::new(m)));
        }

        // Sort by score descending, then name ascending
        entries.sort_by(|a, b| {
            b.score()
                .cmp(&a.score())
                .then_with(|| a.name().cmp(b.name()))
        });

        // Limit to MAX_RESULTS
        entries.truncate(MAX_RESULTS);

        *self.ivars().filtered.borrow_mut() = entries;

        // Reset selection to first item
        self.ivars().selected_index.set(0);

        // Rebuild result rows
        self.rebuild_results();
    }

    /// Filter open tabs by query string (same logic as QuickOpenMatcher)
    fn filter_open_tabs(tabs: &[OpenTabEntry], query: &str) -> Vec<QuickOpenEntry> {
        if query.is_empty() {
            return tabs
                .iter()
                .map(|t| QuickOpenEntry::OpenTab {
                    entry: t.clone(),
                    score: 50, // Slightly above 0 so open tabs show before unmatched templates
                    match_positions: Vec::new(),
                })
                .collect();
        }

        let query_lower = query.to_lowercase();
        tabs.iter()
            .filter_map(|tab| {
                let name_lower = tab.name.to_lowercase();

                // Exact match
                if name_lower == query_lower {
                    return Some(QuickOpenEntry::OpenTab {
                        entry: tab.clone(),
                        score: 1050, // Higher than template exact match
                        match_positions: (0..tab.name.len()).collect(),
                    });
                }

                // Prefix match
                if name_lower.starts_with(&query_lower) {
                    return Some(QuickOpenEntry::OpenTab {
                        entry: tab.clone(),
                        score: 550 + query_lower.len() as i32 * 10,
                        match_positions: (0..query_lower.len()).collect(),
                    });
                }

                // Substring match
                if let Some(pos) = name_lower.find(&query_lower) {
                    return Some(QuickOpenEntry::OpenTab {
                        entry: tab.clone(),
                        score: 250 + (100 - pos as i32).max(0),
                        match_positions: (pos..pos + query_lower.len()).collect(),
                    });
                }

                // Fuzzy match
                let mut match_positions = Vec::new();
                let mut name_chars = name_lower.char_indices().peekable();
                let mut last_match_pos: Option<usize> = None;
                let mut consecutive_bonus = 0i32;

                for query_char in query_lower.chars() {
                    let mut found = false;
                    for (pos, name_char) in name_chars.by_ref() {
                        if name_char == query_char {
                            if let Some(last) = last_match_pos {
                                if pos == last + 1 {
                                    consecutive_bonus += 20;
                                }
                            }
                            match_positions.push(pos);
                            last_match_pos = Some(pos);
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        return None;
                    }
                }

                Some(QuickOpenEntry::OpenTab {
                    entry: tab.clone(),
                    score: 150 + consecutive_bonus + match_positions.len() as i32 * 5,
                    match_positions,
                })
            })
            .collect()
    }

    /// Rebuild the results UI
    fn rebuild_results(&self) {
        let mtm = MainThreadMarker::from(self);

        // Clear existing rows
        if let Some(ref container) = *self.ivars().results_container.borrow() {
            // Remove all arranged subviews
            for view in self.ivars().result_rows.borrow().iter() {
                container.removeArrangedSubview(view);
                view.removeFromSuperview();
            }
        }
        self.ivars().result_rows.borrow_mut().clear();

        let filtered = self.ivars().filtered.borrow();
        let selected_idx = self.ivars().selected_index.get();

        if filtered.is_empty() {
            // Show "No results" message
            if let Some(ref container) = *self.ivars().results_container.borrow() {
                let label = self.create_result_row(mtm, "No results found", "", false);
                container.addArrangedSubview(&label);
                self.ivars().result_rows.borrow_mut().push(label);
            }
            return;
        }

        if let Some(ref container) = *self.ivars().results_container.borrow() {
            for (idx, entry) in filtered.iter().enumerate() {
                let (name, indicator) = match entry {
                    QuickOpenEntry::Template(m) => (
                        m.template.name.as_str(),
                        template_type_indicator(&m.template),
                    ),
                    QuickOpenEntry::OpenTab { entry, .. } => (entry.name.as_str(), "\u{25B6}"),
                };
                let row = self.create_result_row(mtm, name, indicator, idx == selected_idx);
                container.addArrangedSubview(&row);
                self.ivars().result_rows.borrow_mut().push(row);
            }
        }
    }

    /// Create a result row view
    fn create_result_row(
        &self,
        mtm: MainThreadMarker,
        name: &str,
        indicator: &str,
        selected: bool,
    ) -> Retained<NSView> {
        let container_width = self.frame().size.width - 24.0; // Account for padding
        let row_height = 24.0;
        let frame = NSRect::new(NSPoint::ZERO, NSSize::new(container_width, row_height));

        let row: Retained<NSView> = unsafe {
            let view = NSView::initWithFrame(mtm.alloc(), frame);
            view.setWantsLayer(true);
            view
        };

        // Set background color based on selection
        if let Some(layer) = row.layer() {
            unsafe {
                let color = if selected {
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.4, 0.8, 1.0)
                } else {
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.0)
                };
                let cg_color: *mut std::ffi::c_void = msg_send![&*color, CGColor];
                let _: () = msg_send![&*layer, setBackgroundColor: cg_color];
                let _: () = msg_send![&*layer, setCornerRadius: 4.0f64];
            }
        }

        // Create label for name
        let label_frame = NSRect::new(
            NSPoint::new(8.0, 2.0),
            NSSize::new(container_width - 40.0, row_height - 4.0),
        );
        let label = unsafe { NSTextField::initWithFrame(mtm.alloc(), label_frame) };
        label.setStringValue(&NSString::from_str(name));
        label.setBezeled(false);
        label.setDrawsBackground(false);
        label.setEditable(false);
        label.setSelectable(false);
        unsafe {
            let white = NSColor::whiteColor();
            label.setTextColor(Some(&white));
            let font = NSFont::systemFontOfSize(13.0);
            label.setFont(Some(&font));
            row.addSubview(&label);
        }

        // Create indicator label (right side)
        if !indicator.is_empty() {
            let indicator_frame = NSRect::new(
                NSPoint::new(container_width - 30.0, 2.0),
                NSSize::new(24.0, row_height - 4.0),
            );
            let indicator_label =
                unsafe { NSTextField::initWithFrame(mtm.alloc(), indicator_frame) };
            indicator_label.setStringValue(&NSString::from_str(indicator));
            indicator_label.setBezeled(false);
            indicator_label.setDrawsBackground(false);
            indicator_label.setEditable(false);
            indicator_label.setSelectable(false);
            unsafe {
                row.addSubview(&indicator_label);
            }
        }

        // Set fixed width constraint
        unsafe {
            let width_constraint: *mut AnyObject = msg_send![
                objc2::class!(NSLayoutConstraint),
                constraintWithItem: &*row,
                attribute: 7i64,  // NSLayoutAttributeWidth
                relatedBy: 0i64,  // NSLayoutRelationEqual
                toItem: std::ptr::null::<AnyObject>(),
                attribute: 0i64,  // NSLayoutAttributeNotAnAttribute
                multiplier: 1.0f64,
                constant: container_width
            ];
            let _: () = msg_send![width_constraint, setActive: true];
        }

        row
    }

    /// Select the next item in the list
    fn select_next(&self) {
        let filtered = self.ivars().filtered.borrow();
        if filtered.is_empty() {
            return;
        }

        let current = self.ivars().selected_index.get();
        let new_index = if current + 1 >= filtered.len() {
            0 // Wrap around
        } else {
            current + 1
        };
        drop(filtered);

        self.ivars().selected_index.set(new_index);
        self.update_selection_highlight();
    }

    /// Select the previous item in the list
    fn select_previous(&self) {
        let filtered = self.ivars().filtered.borrow();
        if filtered.is_empty() {
            return;
        }

        let current = self.ivars().selected_index.get();
        let new_index = if current == 0 {
            filtered.len() - 1 // Wrap around
        } else {
            current - 1
        };
        drop(filtered);

        self.ivars().selected_index.set(new_index);
        self.update_selection_highlight();
    }

    /// Update the visual highlight for selection
    fn update_selection_highlight(&self) {
        let selected_idx = self.ivars().selected_index.get();
        let rows = self.ivars().result_rows.borrow();

        for (idx, row) in rows.iter().enumerate() {
            if let Some(layer) = row.layer() {
                unsafe {
                    let color = if idx == selected_idx {
                        NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.4, 0.8, 1.0)
                    } else {
                        NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.0)
                    };
                    let cg_color: *mut std::ffi::c_void = msg_send![&*color, CGColor];
                    let _: () = msg_send![&*layer, setBackgroundColor: cg_color];
                }
            }
        }
    }

    /// Confirm the current selection
    fn confirm_selection(&self) {
        let filtered = self.ivars().filtered.borrow();
        let selected_idx = self.ivars().selected_index.get();

        if let Some(entry) = filtered.get(selected_idx) {
            match entry {
                QuickOpenEntry::Template(m) => {
                    let template = m.template.clone();
                    drop(filtered);
                    self.hide();
                    if let Some(ref callback) = *self.ivars().on_select.borrow() {
                        callback(template);
                    }
                }
                QuickOpenEntry::OpenTab { entry, .. } => {
                    let window_ptr = entry.window_ptr;
                    drop(filtered);
                    self.hide();
                    if let Some(ref callback) = *self.ivars().on_switch_tab.borrow() {
                        callback(window_ptr);
                    }
                }
            }
        }
    }

    /// Set the callback for switching to an open tab
    pub fn set_on_switch_tab<F>(&self, callback: F)
    where
        F: Fn(usize) + 'static,
    {
        *self.ivars().on_switch_tab.borrow_mut() = Some(Box::new(callback));
    }

    /// Update templates list
    pub fn set_templates(&self, templates: Vec<StickyTabConfig>) {
        *self.ivars().templates.borrow_mut() = templates;
        self.update_filter();
    }

    /// Update the list of open tabs with custom names
    pub fn set_open_tabs(&self, tabs: Vec<OpenTabEntry>) {
        *self.ivars().open_tabs.borrow_mut() = tabs;
        self.update_filter();
    }

    /// Update both templates and open tabs at once (avoids double filter)
    pub fn set_templates_and_tabs(&self, templates: Vec<StickyTabConfig>, tabs: Vec<OpenTabEntry>) {
        *self.ivars().templates.borrow_mut() = templates;
        *self.ivars().open_tabs.borrow_mut() = tabs;
        self.update_filter();
    }

    /// Update the overlay width when window resizes
    pub fn update_width(&self, width: f64) {
        let mut frame = self.frame();
        frame.size.width = width;
        self.setFrame(frame);

        // Update search field width
        let padding = 12.0;
        let search_width = width - padding * 2.0;

        if let Some(ref search_field) = *self.ivars().search_field.borrow() {
            let mut search_frame = search_field.frame();
            search_frame.size.width = search_width;
            search_field.setFrame(search_frame);
        }

        if let Some(ref results_container) = *self.ivars().results_container.borrow() {
            let mut results_frame = results_container.frame();
            results_frame.size.width = search_width;
            unsafe {
                let _: () = msg_send![&**results_container, setFrame: results_frame];
            }
        }

        // Rebuild results to update row widths
        self.rebuild_results();
    }

    // Helper methods
    fn frame(&self) -> NSRect {
        unsafe { msg_send![self, frame] }
    }

    #[allow(non_snake_case)]
    fn setFrame(&self, frame: NSRect) {
        unsafe {
            let _: () = msg_send![self, setFrame: frame];
        }
    }

    #[allow(non_snake_case)]
    fn setHidden(&self, hidden: bool) {
        unsafe {
            let _: () = msg_send![self, setHidden: hidden];
        }
    }

    #[allow(non_snake_case)]
    fn setWantsLayer(&self, wants: bool) {
        unsafe {
            let _: () = msg_send![self, setWantsLayer: wants];
        }
    }

    fn layer(&self) -> Option<Retained<AnyObject>> {
        unsafe { msg_send![self, layer] }
    }

    fn window(&self) -> Option<Retained<objc2_app_kit::NSWindow>> {
        unsafe { msg_send![self, window] }
    }
}
