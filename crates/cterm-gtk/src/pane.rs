//! GTK pane composition backed by the frontend-neutral pane model.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Orientation, Paned, Widget};

use cterm_ui::{
    PaneBranch, PaneDirection, PaneId, PaneLayout, PaneLayoutError, PaneRect, PaneTree, SplitRatio,
    SplitRequest,
};

const LAYOUT_UNITS: u32 = 10_000;
type DividerRatioUpdates = Rc<RefCell<Vec<(Vec<PaneBranch>, SplitRatio)>>>;

/// A pane layout and the resources owned by each leaf.
///
/// Keeping resources in the same structure as the layout makes session cleanup
/// deterministic: closing a pane returns exactly the resource which must be
/// stopped, while closing a tab drains every remaining resource.
pub(crate) struct PaneSet<T> {
    layout: PaneLayout,
    entries: BTreeMap<PaneId, T>,
    pending_ratios: DividerRatioUpdates,
}

impl<T> PaneSet<T> {
    pub(crate) fn new(entry: T) -> Self {
        let layout = PaneLayout::new();
        let entries = BTreeMap::from([(layout.active(), entry)]);
        Self {
            layout,
            entries,
            pending_ratios: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub(crate) fn active_id(&self) -> PaneId {
        self.layout.active()
    }

    pub(crate) fn from_layout(
        layout: PaneLayout,
        entries: impl IntoIterator<Item = (PaneId, T)>,
    ) -> Result<Self, PaneLayoutError> {
        let entries = entries.into_iter().collect::<BTreeMap<_, _>>();
        let mut layout_ids = layout.pane_ids();
        layout_ids.sort_unstable();
        if layout_ids != entries.keys().copied().collect::<Vec<_>>() {
            return Err(PaneLayoutError::UnknownPane(layout.active()));
        }
        Ok(Self {
            layout,
            entries,
            pending_ratios: Rc::new(RefCell::new(Vec::new())),
        })
    }

    pub(crate) fn layout(&self) -> &PaneLayout {
        &self.layout
    }

    pub(crate) fn pane_ids(&self) -> Vec<PaneId> {
        self.layout.pane_ids()
    }

    pub(crate) fn flush_divider_ratios(&mut self) {
        self.apply_pending_ratios();
    }

    pub(crate) fn active(&self) -> &T {
        self.entries
            .get(&self.active_id())
            .expect("pane resources mirror the pane layout")
    }

    pub(crate) fn get(&self, id: PaneId) -> Option<&T> {
        self.entries.get(&id)
    }

    pub(crate) fn get_mut(&mut self, id: PaneId) -> Option<&mut T> {
        self.entries.get_mut(&id)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (PaneId, &T)> {
        self.entries.iter().map(|(id, entry)| (*id, entry))
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn split(
        &mut self,
        target: PaneId,
        request: SplitRequest,
        entry: T,
    ) -> Result<PaneId, PaneLayoutError> {
        self.apply_pending_ratios();
        let id = self.layout.split(target, request)?;
        let replaced = self.entries.insert(id, entry);
        debug_assert!(replaced.is_none(), "pane IDs are never reused");
        self.assert_consistent();
        Ok(id)
    }

    pub(crate) fn close(&mut self, id: PaneId) -> Result<T, PaneLayoutError> {
        self.apply_pending_ratios();
        self.layout.close(id)?;
        let entry = self
            .entries
            .remove(&id)
            .expect("pane resources mirror the pane layout");
        self.assert_consistent();
        Ok(entry)
    }

    pub(crate) fn set_active(&mut self, id: PaneId) -> Result<(), PaneLayoutError> {
        self.apply_pending_ratios();
        self.layout.set_active(id)
    }

    pub(crate) fn focus(&mut self, direction: PaneDirection) -> Option<PaneId> {
        self.apply_pending_ratios();
        self.layout.focus_direction(direction, model_bounds())
    }

    pub(crate) fn resize(&mut self, direction: PaneDirection, amount: u32) -> bool {
        self.apply_pending_ratios();
        self.layout
            .adjust_active_size(direction, amount, model_bounds())
    }

    pub(crate) fn toggle_zoom(&mut self) -> bool {
        self.apply_pending_ratios();
        self.layout.toggle_zoom()
    }

    pub(crate) fn is_zoomed(&self) -> bool {
        self.layout.zoomed().is_some()
    }

    pub(crate) fn update_styles<F>(&self, widget_for: F)
    where
        F: Fn(&T) -> Widget,
    {
        for (id, entry) in self.iter() {
            let widget = widget_for(entry);
            if id == self.active_id() {
                widget.add_css_class("pane-active");
            } else {
                widget.remove_css_class("pane-active");
            }
        }
    }

    pub(crate) fn rebuild<F>(&self, container: &GtkBox, widget_for: F)
    where
        F: Fn(&T) -> Widget,
    {
        if let Some(child) = container.first_child() {
            container.remove(&child);
            drop(child);
        }

        self.update_styles(&widget_for);

        let tree = match self.layout.zoomed() {
            Some(id) => PaneTree::Pane(id),
            None => self.layout.tree(),
        };
        let root = build_tree(&tree, &self.entries, &widget_for, &self.pending_ratios, &[]);
        container.append(&root);
        // Every rebuilt Paned captures the ratios now stored in the model.
        self.pending_ratios.borrow_mut().clear();
    }

    fn apply_pending_ratios(&mut self) {
        let updates = self.pending_ratios.borrow().clone();
        for (path, ratio) in updates {
            if let Err(error) = self.layout.set_split_ratio(&path, ratio) {
                log::debug!("Ignoring stale GTK pane divider update: {error}");
            }
        }
    }

    fn assert_consistent(&self) {
        let mut layout_ids = self.layout.pane_ids();
        layout_ids.sort_unstable();
        debug_assert_eq!(layout_ids, self.entries.keys().copied().collect::<Vec<_>>());
    }
}

fn model_bounds() -> PaneRect {
    PaneRect::new(0, 0, LAYOUT_UNITS, LAYOUT_UNITS)
}

fn build_tree<T, F>(
    tree: &PaneTree,
    entries: &BTreeMap<PaneId, T>,
    widget_for: &F,
    pending_ratios: &DividerRatioUpdates,
    path: &[PaneBranch],
) -> Widget
where
    F: Fn(&T) -> Widget,
{
    match tree {
        PaneTree::Pane(id) => widget_for(
            entries
                .get(id)
                .expect("pane resources mirror the pane layout"),
        ),
        PaneTree::Split {
            direction,
            first_ratio,
            first,
            second,
        } => {
            let orientation = match direction {
                cterm_ui::SplitDirection::Horizontal => Orientation::Horizontal,
                cterm_ui::SplitDirection::Vertical => Orientation::Vertical,
            };
            let paned = Paned::new(orientation);
            paned.set_hexpand(true);
            paned.set_vexpand(true);
            paned.set_wide_handle(true);
            paned.set_resize_start_child(true);
            paned.set_resize_end_child(true);
            paned.set_shrink_start_child(true);
            paned.set_shrink_end_child(true);

            let mut first_path = path.to_vec();
            first_path.push(PaneBranch::First);
            let mut second_path = path.to_vec();
            second_path.push(PaneBranch::Second);
            let first_child = build_tree(first, entries, widget_for, pending_ratios, &first_path);
            let second_child =
                build_tree(second, entries, widget_for, pending_ratios, &second_path);
            paned.set_start_child(Some(&first_child));
            paned.set_end_child(Some(&second_child));

            let initial_basis_points = first_ratio.basis_points();
            let maximum_path = path.to_vec();
            let pending_for_maximum = Rc::clone(pending_ratios);
            paned.connect_max_position_notify(move |paned| {
                let basis_points = pending_for_maximum
                    .borrow()
                    .iter()
                    .rev()
                    .find(|(path, _)| path == &maximum_path)
                    .map(|(_, ratio)| ratio.basis_points())
                    .unwrap_or(initial_basis_points);
                let position =
                    paned.max_position().saturating_mul(i32::from(basis_points)) / 10_000;
                if paned.position() != position {
                    paned.set_position(position);
                }
            });

            let split_path = path.to_vec();
            let pending = Rc::clone(pending_ratios);
            paned.connect_position_notify(move |paned| {
                let maximum = paned.max_position();
                if maximum <= 0 {
                    return;
                }
                let basis_points = (i64::from(paned.position()) * 10_000 + i64::from(maximum) / 2)
                    / i64::from(maximum);
                let basis_points = basis_points.clamp(500, 9_500) as u16;
                let ratio = SplitRatio::from_basis_points(basis_points)
                    .expect("the GTK divider ratio is clamped");
                let mut pending = pending.borrow_mut();
                if let Some((_, pending_ratio)) =
                    pending.iter_mut().find(|(path, _)| path == &split_path)
                {
                    *pending_ratio = ratio;
                } else {
                    pending.push((split_path.clone(), ratio));
                }
            });

            paned.upcast()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cterm_ui::{SplitDirection, SplitPlacement, SplitRatio};

    fn split(direction: SplitDirection) -> SplitRequest {
        SplitRequest {
            direction,
            placement: SplitPlacement::Second,
            ratio: SplitRatio::HALF,
        }
    }

    #[test]
    fn split_focus_resize_zoom_and_close_preserve_resources() {
        let mut panes = PaneSet::new("first");
        let first = panes.active_id();
        let right = panes
            .split(first, split(SplitDirection::Horizontal), "right")
            .unwrap();
        let bottom_right = panes
            .split(right, split(SplitDirection::Vertical), "bottom-right")
            .unwrap();

        assert_eq!(panes.len(), 3);
        assert_eq!(panes.active(), &"bottom-right");
        assert_eq!(panes.focus(PaneDirection::Up), Some(right));
        assert_eq!(panes.active(), &"right");
        assert!(panes.resize(PaneDirection::Left, 600));
        assert!(panes.toggle_zoom());
        assert!(!panes.toggle_zoom());

        assert_eq!(panes.close(right).unwrap(), "right");
        assert_eq!(panes.len(), 2);
        assert!(panes.get(right).is_none());
        assert!(panes.get(first).is_some());
        assert!(panes.get(bottom_right).is_some());
    }

    #[test]
    fn final_resource_cannot_be_closed() {
        let mut panes = PaneSet::new(7_u8);
        assert_eq!(
            panes.close(panes.active_id()),
            Err(PaneLayoutError::LastPane)
        );
        assert_eq!(panes.len(), 1);
        assert_eq!(panes.active(), &7);
    }

    #[test]
    fn restored_layout_requires_exactly_one_resource_per_leaf() {
        let mut layout = PaneLayout::new();
        let first = layout.active();
        let second = layout
            .split(first, split(SplitDirection::Horizontal))
            .unwrap();

        let panes = PaneSet::from_layout(layout.clone(), [(first, "a"), (second, "b")]).unwrap();
        assert_eq!(panes.layout(), &layout);
        assert_eq!(panes.active(), &"b");
        assert!(PaneSet::from_layout(layout, [(first, "a")]).is_err());
    }

    #[test]
    fn queued_divider_ratio_is_committed_before_the_next_model_action() {
        let mut panes = PaneSet::new("first");
        let first = panes.active_id();
        panes
            .split(first, split(SplitDirection::Horizontal), "second")
            .unwrap();
        panes
            .pending_ratios
            .borrow_mut()
            .push((Vec::new(), SplitRatio::from_basis_points(7_000).unwrap()));

        panes.toggle_zoom();
        panes.toggle_zoom();
        let PaneTree::Split { first_ratio, .. } = panes.layout().tree() else {
            panic!("split layout expected");
        };
        assert_eq!(first_ratio.basis_points(), 7_000);
    }
}
