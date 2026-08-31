//! Tab bar implementation for macOS
//!
//! Native macOS tab bar using NSStackView or NSSegmentedControl.

use std::cell::RefCell;
use std::collections::HashMap;

use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSButton, NSColor, NSEvent, NSMenu, NSMenuItem, NSStackView, NSStackViewGravity,
    NSUserInterfaceLayoutOrientation,
};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use cterm_ui::traits::TabInfo;

/// Tab entry with button and metadata
struct TabEntry {
    id: u64,
    title: String,
    button: Retained<NSButton>,
    active: bool,
    has_bell: bool,
    color: Option<String>,
}

/// Tab bar state
pub struct TabBarIvars {
    tabs: RefCell<Vec<TabEntry>>,
    tab_buttons: RefCell<HashMap<u64, Retained<NSButton>>>,
    active_tab: RefCell<Option<u64>>,
    on_tab_click: RefCell<Option<Box<dyn Fn(u64)>>>,
    on_tab_close: RefCell<Option<Box<dyn Fn(u64)>>>,
    on_new_tab: RefCell<Option<Box<dyn Fn()>>>,
    on_tab_rename: RefCell<Option<Box<dyn Fn(u64)>>>,
    on_tab_set_color: RefCell<Option<Box<dyn Fn(u64)>>>,
    /// Tab ID for context menu actions
    context_menu_tab_id: RefCell<Option<u64>>,
}

define_class!(
    #[unsafe(super(NSStackView))]
    #[thread_kind = MainThreadOnly]
    #[name = "TabBar"]
    #[ivars = TabBarIvars]
    pub struct TabBar;

    unsafe impl NSObjectProtocol for TabBar {}

    impl TabBar {
        /// Handle right-click on the tab bar
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            let location = unsafe { self.convertPoint_fromView(event.locationInWindow(), None) };
            if let Some(tab_id) = self.tab_at_point(location) {
                self.show_context_menu(tab_id, location);
            }
        }

        /// Context menu action: Rename tab
        #[unsafe(method(contextMenuRenameTab:))]
        fn context_menu_rename_tab(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.handle_context_menu_rename();
        }

        /// Context menu action: Set tab color
        #[unsafe(method(contextMenuSetTabColor:))]
        fn context_menu_set_tab_color(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            self.handle_context_menu_set_color();
        }
    }
);

impl TabBar {
    /// Create a new tab bar
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let frame = NSRect::new(NSPoint::ZERO, NSSize::new(800.0, 28.0));

        let this = mtm.alloc::<Self>();
        let this = this.set_ivars(TabBarIvars {
            tabs: RefCell::new(Vec::new()),
            tab_buttons: RefCell::new(HashMap::new()),
            active_tab: RefCell::new(None),
            on_tab_click: RefCell::new(None),
            on_tab_close: RefCell::new(None),
            on_new_tab: RefCell::new(None),
            on_tab_rename: RefCell::new(None),
            on_tab_set_color: RefCell::new(None),
            context_menu_tab_id: RefCell::new(None),
        });

        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };

        // Configure stack view
        this.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        this.setSpacing(4.0);
        this.setEdgeInsets(objc2_foundation::NSEdgeInsets {
            top: 4.0,
            left: 8.0,
            bottom: 4.0,
            right: 8.0,
        });

        // Set distribution
        unsafe {
            let _: () = msg_send![&*this, setDistribution: 0i64]; // NSStackViewDistributionFill = 0
        }

        // Enable layer backing and set background color for visibility
        this.setWantsLayer(true);
        if let Some(layer) = this.layer() {
            // Light gray background
            unsafe {
                let color = NSColor::colorWithSRGBRed_green_blue_alpha(0.9, 0.9, 0.9, 1.0);
                let cg_color = color.CGColor();
                layer.setBackgroundColor(Some(&cg_color));
            }
        }

        // Add "new tab" button
        let new_tab_button = unsafe {
            NSButton::buttonWithTitle_target_action(&NSString::from_str("+"), None, None, mtm)
        };
        unsafe {
            let _: () = msg_send![&*new_tab_button, setBezelStyle: 1i64]; // NSBezelStyleRounded
        }
        this.addView_inGravity(&new_tab_button, NSStackViewGravity::Trailing);

        log::debug!("TabBar created");
        this
    }

    /// Add a new tab
    pub fn add_tab(&self, id: u64, title: &str) {
        let mtm = MainThreadMarker::from(self);

        // Create tab button with proper styling
        let button = unsafe {
            NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, mtm)
        };

        // Style the button
        unsafe {
            let _: () = msg_send![&*button, setBezelStyle: 1i64]; // NSBezelStyleRounded
            let _: () = msg_send![&*button, setButtonType: 0i64]; // NSButtonTypeMomentaryLight
        }

        // Store in our list
        self.ivars().tabs.borrow_mut().push(TabEntry {
            id,
            title: title.to_string(),
            button: button.clone(),
            active: false,
            has_bell: false,
            color: None,
        });

        self.ivars()
            .tab_buttons
            .borrow_mut()
            .insert(id, button.clone());

        // Add to stack view (before the + button)
        let count = self.views().len();
        log::info!("Adding tab {} - views before: {}", id, count);

        // Simply add to leading gravity
        self.addView_inGravity(&button, NSStackViewGravity::Leading);

        let count_after = self.views().len();
        log::info!(
            "Tab {} added - views after: {}, hidden: {}",
            id,
            count_after,
            self.isHidden()
        );
    }

    /// Remove a tab
    pub fn remove_tab(&self, id: u64) {
        let mut tabs = self.ivars().tabs.borrow_mut();
        if let Some(pos) = tabs.iter().position(|t| t.id == id) {
            let entry = tabs.remove(pos);
            self.removeView(&entry.button);
        }
        self.ivars().tab_buttons.borrow_mut().remove(&id);
    }

    /// Set active tab
    pub fn set_active(&self, id: u64) {
        *self.ivars().active_tab.borrow_mut() = Some(id);

        // Update button states
        for tab in self.ivars().tabs.borrow_mut().iter_mut() {
            tab.active = tab.id == id;
            // Update button appearance based on active state
            // NSButton doesn't have a built-in "selected" state,
            // so we'd need to use bezel style or color changes
        }
    }

    /// Set tab title
    pub fn set_title(&self, id: u64, title: &str) {
        if let Some(button) = self.ivars().tab_buttons.borrow().get(&id) {
            button.setTitle(&NSString::from_str(title));
        }

        for tab in self.ivars().tabs.borrow_mut().iter_mut() {
            if tab.id == id {
                tab.title = title.to_string();
                break;
            }
        }
    }

    /// Set bell indicator
    pub fn set_bell(&self, id: u64, has_bell: bool) {
        for tab in self.ivars().tabs.borrow_mut().iter_mut() {
            if tab.id == id {
                tab.has_bell = has_bell;
                // Update button title to show bell
                let title = if has_bell {
                    format!("🔔 {}", tab.title)
                } else {
                    tab.title.clone()
                };
                tab.button.setTitle(&NSString::from_str(&title));
                break;
            }
        }
    }

    /// Clear bell indicator
    pub fn clear_bell(&self, id: u64) {
        self.set_bell(id, false);
    }

    /// Set tab color
    pub fn set_color(&self, id: u64, color: Option<&str>) {
        for tab in self.ivars().tabs.borrow_mut().iter_mut() {
            if tab.id == id {
                tab.color = color.map(|s| s.to_string());

                // Apply color to button background using layer
                tab.button.setWantsLayer(true);
                if let Some(layer) = tab.button.layer() {
                    if let Some(hex) = color {
                        // Parse hex color (e.g., "#E95420" or "E95420")
                        let hex = hex.trim_start_matches('#');
                        if hex.len() == 6 {
                            if let (Ok(r), Ok(g), Ok(b)) = (
                                u8::from_str_radix(&hex[0..2], 16),
                                u8::from_str_radix(&hex[2..4], 16),
                                u8::from_str_radix(&hex[4..6], 16),
                            ) {
                                unsafe {
                                    let color = NSColor::colorWithSRGBRed_green_blue_alpha(
                                        r as f64 / 255.0,
                                        g as f64 / 255.0,
                                        b as f64 / 255.0,
                                        1.0,
                                    );
                                    let cg_color = color.CGColor();
                                    layer.setBackgroundColor(Some(&cg_color));
                                    layer.setCornerRadius(4.0);
                                }
                            }
                        }
                    } else {
                        // Clear the background color
                        layer.setBackgroundColor(None);
                    }
                }
                break;
            }
        }
    }

    /// Get active tab ID
    pub fn active_tab(&self) -> Option<u64> {
        *self.ivars().active_tab.borrow()
    }

    /// Get all tab IDs
    pub fn tab_ids(&self) -> Vec<u64> {
        self.ivars().tabs.borrow().iter().map(|t| t.id).collect()
    }

    /// Set callback for tab click
    pub fn set_on_click<F: Fn(u64) + 'static>(&self, callback: F) {
        *self.ivars().on_tab_click.borrow_mut() = Some(Box::new(callback));
    }

    /// Set callback for tab close
    pub fn set_on_close<F: Fn(u64) + 'static>(&self, callback: F) {
        *self.ivars().on_tab_close.borrow_mut() = Some(Box::new(callback));
    }

    /// Set callback for new tab
    pub fn set_on_new_tab<F: Fn() + 'static>(&self, callback: F) {
        *self.ivars().on_new_tab.borrow_mut() = Some(Box::new(callback));
    }

    /// Set callback for tab rename request
    pub fn set_on_rename<F: Fn(u64) + 'static>(&self, callback: F) {
        *self.ivars().on_tab_rename.borrow_mut() = Some(Box::new(callback));
    }

    /// Set callback for tab set color request
    pub fn set_on_set_color<F: Fn(u64) + 'static>(&self, callback: F) {
        *self.ivars().on_tab_set_color.borrow_mut() = Some(Box::new(callback));
    }

    /// Find which tab contains the given point (in tab bar coordinates)
    fn tab_at_point(&self, point: NSPoint) -> Option<u64> {
        for tab in self.ivars().tabs.borrow().iter() {
            let frame = tab.button.frame();
            if point.x >= frame.origin.x
                && point.x <= frame.origin.x + frame.size.width
                && point.y >= frame.origin.y
                && point.y <= frame.origin.y + frame.size.height
            {
                return Some(tab.id);
            }
        }
        None
    }

    /// Show context menu for a tab
    pub fn show_context_menu(&self, tab_id: u64, location: NSPoint) {
        let mtm = MainThreadMarker::from(self);

        // Store the tab ID for the menu action
        *self.ivars().context_menu_tab_id.borrow_mut() = Some(tab_id);

        // Create context menu
        let menu = NSMenu::new(mtm);

        // Rename item
        let rename_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &NSString::from_str("Rename Tab..."),
                Some(sel!(contextMenuRenameTab:)),
                &NSString::from_str(""),
            )
        };
        unsafe { rename_item.setTarget(Some(self)) };
        menu.addItem(&rename_item);

        // Set Color item
        let color_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &NSString::from_str("Set Tab Color..."),
                Some(sel!(contextMenuSetTabColor:)),
                &NSString::from_str(""),
            )
        };
        unsafe { color_item.setTarget(Some(self)) };
        menu.addItem(&color_item);

        // Show the menu
        menu.popUpMenuPositioningItem_atLocation_inView(None, location, Some(self));
    }

    /// Handle rename tab from context menu
    fn handle_context_menu_rename(&self) {
        if let Some(tab_id) = *self.ivars().context_menu_tab_id.borrow() {
            if let Some(ref callback) = *self.ivars().on_tab_rename.borrow() {
                callback(tab_id);
            }
        }
    }

    /// Handle set tab color from context menu
    fn handle_context_menu_set_color(&self) {
        if let Some(tab_id) = *self.ivars().context_menu_tab_id.borrow() {
            if let Some(ref callback) = *self.ivars().on_tab_set_color.borrow() {
                callback(tab_id);
            }
        }
    }

    /// Update visibility based on tab count
    pub fn update_visibility(&self) {
        let count = self.ivars().tabs.borrow().len();
        // Only show tab bar if more than one tab
        self.setHidden(count <= 1);
    }
}

impl cterm_ui::traits::TabBar for TabBar {
    fn add_tab(&mut self, info: TabInfo) {
        TabBar::add_tab(self, info.id, &info.title);
        if let Some(ref color) = info.color {
            TabBar::set_color(self, info.id, Some(color));
        }
        if info.active {
            TabBar::set_active(self, info.id);
        }
    }

    fn remove_tab(&mut self, id: u64) {
        TabBar::remove_tab(self, id);
    }

    fn update_tab(&mut self, info: TabInfo) {
        TabBar::set_title(self, info.id, &info.title);
        TabBar::set_color(self, info.id, info.color.as_deref());
        if info.has_unread {
            TabBar::set_bell(self, info.id, true);
        }
        if info.active {
            TabBar::set_active(self, info.id);
        }
    }

    fn set_active(&mut self, id: u64) {
        TabBar::set_active(self, id);
    }

    fn active_tab(&self) -> Option<u64> {
        TabBar::active_tab(self)
    }

    fn tab_ids(&self) -> Vec<u64> {
        TabBar::tab_ids(self)
    }

    fn reorder(&mut self, _from: usize, _to: usize) {
        // TODO: Implement tab reordering
    }
}
