//! Native pane wrappers and session-to-layout ownership for macOS.

use std::cell::Cell;
use std::collections::BTreeMap;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly};
use objc2_app_kit::{NSBezierPath, NSColor, NSEvent, NSView};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize};

use cterm_ui::{
    PaneBranch, PaneDirection, PaneId, PaneLayout, PaneLayoutError, PaneRect, PaneTree,
    PositionedPane, SplitDirection, SplitRatio, SplitRequest,
};

use crate::terminal_view::TerminalView;

const PANE_BORDER_WIDTH: f64 = 1.0;
const DIVIDER_HIT_SLOP: f64 = 5.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneDivider {
    path: Vec<PaneBranch>,
    direction: SplitDirection,
    bounds: PaneRect,
}

impl PaneDivider {
    fn ratio_at(&self, x: f64, y: f64) -> SplitRatio {
        let (position, origin, extent) = match self.direction {
            SplitDirection::Horizontal => (x, f64::from(self.bounds.x), self.bounds.width),
            SplitDirection::Vertical => (y, f64::from(self.bounds.y), self.bounds.height),
        };
        if extent == 0 {
            return SplitRatio::HALF;
        }
        let offset = (position - origin).clamp(0.0, f64::from(extent));
        let basis_points = (offset * 10_000.0 / f64::from(extent)).round().clamp(
            f64::from(SplitRatio::MIN.basis_points()),
            f64::from(SplitRatio::MAX.basis_points()),
        ) as u16;
        SplitRatio::from_basis_points(basis_points)
            .expect("clamped divider ratio is within model bounds")
    }
}

fn split_extent(total: u32, ratio: SplitRatio) -> u32 {
    match total {
        0 => 0,
        1 => 1,
        _ => ((u64::from(total) * u64::from(ratio.basis_points()) / 10_000) as u32)
            .clamp(1, total - 1),
    }
}

fn split_rects(
    bounds: PaneRect,
    direction: SplitDirection,
    ratio: SplitRatio,
) -> (PaneRect, PaneRect) {
    match direction {
        SplitDirection::Horizontal => {
            let first = split_extent(bounds.width, ratio);
            (
                PaneRect::new(bounds.x, bounds.y, first, bounds.height),
                PaneRect::new(
                    bounds.x.saturating_add(first),
                    bounds.y,
                    bounds.width - first,
                    bounds.height,
                ),
            )
        }
        SplitDirection::Vertical => {
            let first = split_extent(bounds.height, ratio);
            (
                PaneRect::new(bounds.x, bounds.y, bounds.width, first),
                PaneRect::new(
                    bounds.x,
                    bounds.y.saturating_add(first),
                    bounds.width,
                    bounds.height - first,
                ),
            )
        }
    }
}

fn divider_at(
    tree: &PaneTree,
    bounds: PaneRect,
    x: f64,
    y: f64,
    path: &mut Vec<PaneBranch>,
    best: &mut Option<(f64, PaneDivider)>,
) {
    let PaneTree::Split {
        direction,
        first_ratio,
        first,
        second,
    } = tree
    else {
        return;
    };
    let (first_bounds, second_bounds) = split_rects(bounds, *direction, *first_ratio);
    let (distance, within_span) = match direction {
        SplitDirection::Horizontal => (
            (x - f64::from(second_bounds.x)).abs(),
            y >= f64::from(bounds.y) && y <= f64::from(bounds.y.saturating_add(bounds.height)),
        ),
        SplitDirection::Vertical => (
            (y - f64::from(second_bounds.y)).abs(),
            x >= f64::from(bounds.x) && x <= f64::from(bounds.x.saturating_add(bounds.width)),
        ),
    };
    if within_span && distance <= DIVIDER_HIT_SLOP {
        let candidate = PaneDivider {
            path: path.clone(),
            direction: *direction,
            bounds,
        };
        let replace = best.as_ref().is_none_or(|(best_distance, best_divider)| {
            distance < *best_distance
                || (distance == *best_distance && candidate.path.len() > best_divider.path.len())
        });
        if replace {
            *best = Some((distance, candidate));
        }
    }

    path.push(PaneBranch::First);
    divider_at(first, first_bounds, x, y, path, best);
    path.pop();
    path.push(PaneBranch::Second);
    divider_at(second, second_bounds, x, y, path, best);
    path.pop();
}

/// A flipped host keeps [`PaneRect`] coordinates top-left based on macOS.
define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CtermPaneHostView"]
    #[ivars = ()]
    pub struct PaneHostView;

    unsafe impl NSObjectProtocol for PaneHostView {}

    impl PaneHostView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                let _: bool = unsafe {
                    msg_send![&*window, beginPaneDividerDragAt: event.locationInWindow()]
                };
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                let _: bool =
                    unsafe { msg_send![&*window, dragPaneDividerTo: event.locationInWindow()] };
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if let Some(window) = self.window() {
                let _: bool = unsafe { msg_send![&*window, endPaneDividerDrag] };
            }
        }
    }
);

impl PaneHostView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

pub struct PaneFrameViewIvars {
    terminal: Retained<TerminalView>,
    active: Cell<bool>,
    border_color: [f64; 3],
    focus_color: [f64; 3],
}

/// Draws an inactive divider or active focus ring around a terminal view.
define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CtermPaneFrameView"]
    #[ivars = PaneFrameViewIvars]
    pub struct PaneFrameView;

    unsafe impl NSObjectProtocol for PaneFrameView {}

    impl PaneFrameView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            let rgb = if self.ivars().active.get() {
                self.ivars().focus_color
            } else {
                self.ivars().border_color
            };
            let color = NSColor::colorWithSRGBRed_green_blue_alpha(rgb[0], rgb[1], rgb[2], 1.0);
            color.setFill();
            NSBezierPath::fillRect(self.bounds());
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.layout_terminal();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                let began: bool = unsafe {
                    msg_send![&*window, beginPaneDividerDragAt: event.locationInWindow()]
                };
                if began {
                    return;
                }
                let _: () = unsafe { msg_send![&*window, focusPaneForView: &*self.ivars().terminal] };
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                let _: bool =
                    unsafe { msg_send![&*window, dragPaneDividerTo: event.locationInWindow()] };
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if let Some(window) = self.window() {
                let _: bool = unsafe { msg_send![&*window, endPaneDividerDrag] };
            }
        }
    }
);

impl PaneFrameView {
    pub fn new(
        mtm: MainThreadMarker,
        terminal: Retained<TerminalView>,
        theme: &cterm_ui::Theme,
    ) -> Retained<Self> {
        let border = theme.ui.border;
        let focus = theme.cursor.color;
        let this = mtm.alloc::<Self>();
        let this = this.set_ivars(PaneFrameViewIvars {
            terminal,
            active: Cell::new(false),
            border_color: rgb_components(border),
            focus_color: rgb_components(focus),
        });
        let this: Retained<Self> = unsafe {
            msg_send![
                super(this),
                initWithFrame: NSRect::new(NSPoint::ZERO, NSSize::new(1.0, 1.0))
            ]
        };
        this.addSubview(&this.ivars().terminal);
        this.layout_terminal();
        this
    }

    pub fn terminal(&self) -> Retained<TerminalView> {
        self.ivars().terminal.clone()
    }

    pub fn set_active(&self, active: bool) {
        if self.ivars().active.replace(active) != active {
            self.setNeedsDisplay(true);
        }
    }

    fn layout_terminal(&self) {
        let bounds = self.bounds();
        let inset_x = PANE_BORDER_WIDTH.min(bounds.size.width / 2.0);
        let inset_y = PANE_BORDER_WIDTH.min(bounds.size.height / 2.0);
        let frame = NSRect::new(
            NSPoint::new(inset_x, inset_y),
            NSSize::new(
                (bounds.size.width - inset_x * 2.0).max(0.0),
                (bounds.size.height - inset_y * 2.0).max(0.0),
            ),
        );
        let _: () = unsafe { msg_send![&*self.ivars().terminal, setFrame: frame] };
    }
}

fn rgb_components(color: cterm_core::Rgb) -> [f64; 3] {
    [
        f64::from(color.r) / 255.0,
        f64::from(color.g) / 255.0,
        f64::from(color.b) / 255.0,
    ]
}

/// Keeps native pane/session objects in lock-step with the reusable layout.
pub(crate) struct PaneRegistry<T> {
    layout: PaneLayout,
    entries: BTreeMap<PaneId, T>,
}

impl<T> Default for PaneRegistry<T> {
    fn default() -> Self {
        Self {
            layout: PaneLayout::new(),
            entries: BTreeMap::new(),
        }
    }
}

impl<T> PaneRegistry<T> {
    pub fn from_layout(layout: PaneLayout) -> Self {
        Self {
            layout,
            entries: BTreeMap::new(),
        }
    }

    pub fn layout(&self) -> &PaneLayout {
        &self.layout
    }

    pub fn layout_mut(&mut self) -> &mut PaneLayout {
        &mut self.layout
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn active_id(&self) -> PaneId {
        self.layout.active()
    }

    pub fn active(&self) -> Option<&T> {
        self.entries.get(&self.active_id())
    }

    pub fn get(&self, id: PaneId) -> Option<&T> {
        self.entries.get(&id)
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.entries.values()
    }

    pub fn insert_initial(&mut self, entry: T) -> Result<PaneId, T> {
        let id = self.layout.active();
        if self.entries.contains_key(&id) {
            return Err(entry);
        }
        self.entries.insert(id, entry);
        Ok(id)
    }

    pub fn insert_restored(&mut self, id: PaneId, entry: T) -> Result<(), T> {
        if !self.layout.contains(id) || self.entries.contains_key(&id) {
            return Err(entry);
        }
        self.entries.insert(id, entry);
        Ok(())
    }

    pub fn split(
        &mut self,
        target: PaneId,
        request: SplitRequest,
        entry: T,
    ) -> Result<PaneId, (PaneLayoutError, T)> {
        match self.layout.split(target, request) {
            Ok(id) => {
                let replaced = self.entries.insert(id, entry);
                debug_assert!(replaced.is_none(), "new pane IDs cannot already be present");
                Ok(id)
            }
            Err(error) => Err((error, entry)),
        }
    }

    pub fn close(&mut self, target: PaneId) -> Result<T, PaneLayoutError> {
        self.layout.close(target)?;
        Ok(self
            .entries
            .remove(&target)
            .expect("layout and native pane entries stay synchronized"))
    }

    pub fn set_active(&mut self, id: PaneId) -> Result<(), PaneLayoutError> {
        self.layout.set_active(id)
    }

    pub fn focus_direction(
        &mut self,
        direction: PaneDirection,
        bounds: PaneRect,
    ) -> Option<PaneId> {
        self.layout.focus_direction(direction, bounds)
    }

    pub fn positions(&self, bounds: PaneRect) -> Vec<PositionedPane> {
        self.layout.layout(bounds)
    }

    pub fn divider_at(&self, bounds: PaneRect, x: f64, y: f64) -> Option<PaneDivider> {
        if self.layout.zoomed().is_some() {
            return None;
        }
        let mut best = None;
        divider_at(
            &self.layout.tree(),
            bounds,
            x,
            y,
            &mut Vec::new(),
            &mut best,
        );
        best.map(|(_, divider)| divider)
    }

    pub fn drag_divider(
        &mut self,
        divider: &PaneDivider,
        x: f64,
        y: f64,
    ) -> Result<bool, PaneLayoutError> {
        self.layout
            .set_split_ratio(&divider.path, divider.ratio_at(x, y))
    }

    pub fn id_matching(&self, mut matches: impl FnMut(&T) -> bool) -> Option<PaneId> {
        self.entries
            .iter()
            .find_map(|(id, entry)| matches(entry).then_some(*id))
    }

    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        std::mem::take(&mut self.entries).into_values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cterm_ui::{SplitDirection, SplitPlacement, SplitRatio};

    fn split_request(direction: SplitDirection) -> SplitRequest {
        SplitRequest {
            direction,
            placement: SplitPlacement::Second,
            ratio: SplitRatio::HALF,
        }
    }

    #[test]
    fn initial_native_entry_uses_the_layout_root() {
        let mut panes = PaneRegistry::default();
        let root = panes.insert_initial("root").unwrap();
        assert_eq!(root, panes.active_id());
        assert_eq!(panes.active(), Some(&"root"));
        assert_eq!(panes.insert_initial("duplicate"), Err("duplicate"));
    }

    #[test]
    fn failed_split_returns_ownership_without_changing_registry() {
        let mut panes = PaneRegistry::default();
        let root = panes.insert_initial("root").unwrap();
        let unknown = panes
            .layout_mut()
            .split_active(SplitRequest::default())
            .unwrap();
        panes.layout_mut().close(unknown).unwrap();

        let result = panes.split(unknown, SplitRequest::default(), "orphan");
        assert!(
            matches!(result, Err((PaneLayoutError::UnknownPane(id), "orphan")) if id == unknown)
        );
        assert_eq!(panes.len(), 1);
        assert_eq!(panes.get(root), Some(&"root"));
    }

    #[test]
    fn close_returns_exact_entry_and_tracks_replacement_focus() {
        let mut panes = PaneRegistry::default();
        let root = panes.insert_initial("root").unwrap();
        let second = panes
            .split(root, split_request(SplitDirection::Horizontal), "second")
            .unwrap();
        assert_eq!(panes.active_id(), second);
        assert_eq!(panes.close(second).unwrap(), "second");
        assert_eq!(panes.active_id(), root);
        assert_eq!(panes.active(), Some(&"root"));
    }

    #[test]
    fn focus_resize_and_zoom_keep_native_mapping_stable() {
        let bounds = PaneRect::new(0, 0, 120, 60);
        let mut panes = PaneRegistry::default();
        let left = panes.insert_initial(10).unwrap();
        let right = panes
            .split(left, split_request(SplitDirection::Horizontal), 20)
            .unwrap();

        panes.set_active(left).unwrap();
        assert_eq!(
            panes.focus_direction(PaneDirection::Right, bounds),
            Some(right)
        );
        assert!(panes
            .layout_mut()
            .adjust_active_size(PaneDirection::Left, 10, bounds));
        assert!(panes.layout_mut().toggle_zoom());
        assert_eq!(panes.positions(bounds).len(), 1);
        assert_eq!(panes.get(left), Some(&10));
        assert_eq!(panes.get(right), Some(&20));
    }

    #[test]
    fn matching_and_drain_are_deterministic() {
        let mut panes = PaneRegistry::default();
        let first = panes.insert_initial(10).unwrap();
        let second = panes
            .split(first, split_request(SplitDirection::Vertical), 20)
            .unwrap();
        assert_eq!(panes.id_matching(|value| *value == 20), Some(second));
        assert_eq!(panes.drain().collect::<Vec<_>>(), vec![10, 20]);
        assert!(panes.is_empty());
    }

    #[test]
    fn restored_entries_must_match_the_serialized_layout() {
        let mut layout = PaneLayout::new();
        let first = layout.active();
        let second = layout.split_active(SplitRequest::default()).unwrap();
        let mut panes = PaneRegistry::from_layout(layout);
        assert_eq!(panes.insert_restored(second, "second"), Ok(()));
        assert_eq!(panes.insert_restored(first, "first"), Ok(()));
        assert_eq!(panes.insert_restored(first, "duplicate"), Err("duplicate"));
        assert_eq!(panes.active(), Some(&"second"));
        assert_eq!(
            panes.values().copied().collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn divider_hit_testing_prefers_the_nested_split_at_intersections() {
        let mut panes = PaneRegistry::default();
        let first = panes.insert_initial("first").unwrap();
        let second = panes
            .split(first, split_request(SplitDirection::Horizontal), "second")
            .unwrap();
        panes
            .split(second, split_request(SplitDirection::Vertical), "third")
            .unwrap();

        let root = panes.divider_at(PaneRect::new(0, 0, 100, 100), 50.0, 10.0);
        assert_eq!(root.unwrap().path, Vec::<PaneBranch>::new());
        let nested = panes.divider_at(PaneRect::new(0, 0, 100, 100), 50.0, 50.0);
        assert_eq!(nested.unwrap().path, vec![PaneBranch::Second]);
    }

    #[test]
    fn divider_drag_persists_a_bounded_ratio_and_disables_when_zoomed() {
        let bounds = PaneRect::new(0, 0, 200, 100);
        let mut panes = PaneRegistry::default();
        let first = panes.insert_initial("first").unwrap();
        panes
            .split(first, split_request(SplitDirection::Horizontal), "second")
            .unwrap();
        let divider = panes.divider_at(bounds, 100.0, 50.0).unwrap();

        assert!(panes.drag_divider(&divider, 180.0, 50.0).unwrap());
        assert_eq!(panes.positions(bounds)[0].rect.width, 180);
        assert!(panes.drag_divider(&divider, -100.0, 50.0).unwrap());
        assert_eq!(panes.positions(bounds)[0].rect.width, 10);

        panes.layout_mut().toggle_zoom();
        assert!(panes.divider_at(bounds, 10.0, 50.0).is_none());
    }
}
