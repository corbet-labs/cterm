//! Frontend-neutral pane tree and layout operations.
//!
//! A pane layout is a binary tree. Leaves carry stable [`PaneId`] values and
//! interior nodes split their rectangle between two children. The tree owns no
//! terminal sessions or platform widgets, so native frontends can use it as a
//! deterministic layout model without depending on one another.

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use std::cmp::Reverse;
use std::collections::HashSet;
use std::fmt;

const RATIO_SCALE: u16 = 10_000;
const MIN_RATIO_BASIS_POINTS: u16 = 500;
const MAX_RATIO_BASIS_POINTS: u16 = RATIO_SCALE - MIN_RATIO_BASIS_POINTS;

/// A stable identifier for a pane within a [`PaneLayout`].
///
/// IDs are allocated monotonically and are not reused after a pane is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaneId(u64);

impl PaneId {
    /// Return the numeric representation of this ID.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PaneId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The axis along which a node divides its rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SplitDirection {
    /// Divide width into left and right children.
    Horizontal,
    /// Divide height into top and bottom children.
    Vertical,
}

/// A spatial direction used for focus and resize operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PaneDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Which side of a split receives a newly created pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SplitPlacement {
    /// Left for a horizontal split, top for a vertical split.
    First,
    /// Right for a horizontal split, bottom for a vertical split.
    Second,
}

/// Error returned when constructing an out-of-range split ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "split ratio {basis_points} basis points is outside the supported range {MIN_RATIO_BASIS_POINTS}..={MAX_RATIO_BASIS_POINTS}"
)]
pub struct InvalidSplitRatio {
    basis_points: u16,
}

impl InvalidSplitRatio {
    /// Return the rejected ratio in basis points.
    pub const fn basis_points(self) -> u16 {
        self.basis_points
    }
}

/// The share assigned to one side of a split, represented in basis points.
///
/// Ratios are bounded to 5% through 95%. Integer storage makes layout and
/// persisted state deterministic and avoids invalid floating-point values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SplitRatio(u16);

impl SplitRatio {
    /// The smallest supported share (5%).
    pub const MIN: Self = Self(MIN_RATIO_BASIS_POINTS);

    /// The largest supported share (95%).
    pub const MAX: Self = Self(MAX_RATIO_BASIS_POINTS);

    /// An even split.
    pub const HALF: Self = Self(RATIO_SCALE / 2);

    /// Construct a ratio from an integer percentage.
    pub fn from_percent(percent: u8) -> Result<Self, InvalidSplitRatio> {
        Self::from_basis_points(u16::from(percent) * 100)
    }

    /// Construct a ratio from basis points, where 10,000 is 100%.
    pub const fn from_basis_points(basis_points: u16) -> Result<Self, InvalidSplitRatio> {
        if basis_points < MIN_RATIO_BASIS_POINTS || basis_points > MAX_RATIO_BASIS_POINTS {
            Err(InvalidSplitRatio { basis_points })
        } else {
            Ok(Self(basis_points))
        }
    }

    /// Return this ratio in basis points, where 10,000 is 100%.
    pub const fn basis_points(self) -> u16 {
        self.0
    }

    fn complement(self) -> Self {
        Self(RATIO_SCALE - self.0)
    }

    fn clamped_from_fraction(first: u32, total: u32) -> Self {
        if total == 0 {
            return Self::HALF;
        }

        let scaled =
            (u64::from(first) * u64::from(RATIO_SCALE) + u64::from(total) / 2) / u64::from(total);
        let basis_points = u16::try_from(scaled)
            .unwrap_or(RATIO_SCALE)
            .clamp(MIN_RATIO_BASIS_POINTS, MAX_RATIO_BASIS_POINTS);
        Self(basis_points)
    }

    fn first_extent(self, total: u32) -> u32 {
        match total {
            0 => 0,
            1 => 1,
            _ => {
                let extent = (u64::from(total) * u64::from(self.0) / u64::from(RATIO_SCALE)) as u32;
                extent.clamp(1, total - 1)
            }
        }
    }
}

impl Default for SplitRatio {
    fn default() -> Self {
        Self::HALF
    }
}

impl Serialize for SplitRatio {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u16(self.0)
    }
}

impl<'de> Deserialize<'de> for SplitRatio {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let basis_points = u16::deserialize(deserializer)?;
        Self::from_basis_points(basis_points).map_err(D::Error::custom)
    }
}

/// Parameters for splitting an existing pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitRequest {
    /// The axis along which the pane is split.
    pub direction: SplitDirection,
    /// The side on which the new pane is inserted.
    pub placement: SplitPlacement,
    /// The share of the existing pane's rectangle assigned to the new pane.
    pub ratio: SplitRatio,
}

impl Default for SplitRequest {
    fn default() -> Self {
        Self {
            direction: SplitDirection::Horizontal,
            placement: SplitPlacement::Second,
            ratio: SplitRatio::HALF,
        }
    }
}

/// An integer rectangle in frontend-defined units, such as cells or pixels.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PaneRect {
    /// Construct a rectangle.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn x_start(self) -> u64 {
        u64::from(self.x)
    }

    fn x_end(self) -> u64 {
        self.x_start() + u64::from(self.width)
    }

    fn y_start(self) -> u64 {
        u64::from(self.y)
    }

    fn y_end(self) -> u64 {
        self.y_start() + u64::from(self.height)
    }

    fn split(self, direction: SplitDirection, ratio: SplitRatio) -> (Self, Self) {
        match direction {
            SplitDirection::Horizontal => {
                let first_width = ratio.first_extent(self.width);
                let second_width = self.width - first_width;
                (
                    Self::new(self.x, self.y, first_width, self.height),
                    Self::new(
                        self.x.saturating_add(first_width),
                        self.y,
                        second_width,
                        self.height,
                    ),
                )
            }
            SplitDirection::Vertical => {
                let first_height = ratio.first_extent(self.height);
                let second_height = self.height - first_height;
                (
                    Self::new(self.x, self.y, self.width, first_height),
                    Self::new(
                        self.x,
                        self.y.saturating_add(first_height),
                        self.width,
                        second_height,
                    ),
                )
            }
        }
    }
}

/// A pane and its computed frontend rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionedPane {
    /// Stable pane ID.
    pub id: PaneId,
    /// Preorder index within the unzoomed pane tree.
    pub index: usize,
    /// Rectangle assigned to this pane.
    pub rect: PaneRect,
    /// Whether this pane currently has input focus.
    pub is_active: bool,
    /// Whether this pane is the zoom target.
    pub is_zoomed: bool,
}

/// Pane-tree operation failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PaneLayoutError {
    #[error("pane {0} does not exist")]
    UnknownPane(PaneId),
    #[error("the final pane cannot be closed")]
    LastPane,
    #[error("pane ID space is exhausted")]
    IdExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Branch {
    First,
    Second,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum PaneNode {
    Pane(PaneId),
    Split {
        direction: SplitDirection,
        first_ratio: SplitRatio,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

impl PaneNode {
    fn contains(&self, id: PaneId) -> bool {
        match self {
            Self::Pane(candidate) => *candidate == id,
            Self::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    fn first_pane(&self) -> PaneId {
        match self {
            Self::Pane(id) => *id,
            Self::Split { first, .. } => first.first_pane(),
        }
    }

    fn pane_count(&self) -> usize {
        match self {
            Self::Pane(_) => 1,
            Self::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    fn collect_ids(&self, ids: &mut Vec<PaneId>) {
        match self {
            Self::Pane(id) => ids.push(*id),
            Self::Split { first, second, .. } => {
                first.collect_ids(ids);
                second.collect_ids(ids);
            }
        }
    }

    fn split_pane(&mut self, target: PaneId, new_pane: PaneId, request: SplitRequest) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                let existing = Self::Pane(*id);
                let inserted = Self::Pane(new_pane);
                let (first, second, first_ratio) = match request.placement {
                    SplitPlacement::First => (inserted, existing, request.ratio),
                    SplitPlacement::Second => (existing, inserted, request.ratio.complement()),
                };
                *self = Self::Split {
                    direction: request.direction,
                    first_ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.split_pane(target, new_pane, request)
                    || second.split_pane(target, new_pane, request)
            }
        }
    }

    fn remove(self, target: PaneId) -> (Option<Self>, Option<PaneId>) {
        match self {
            Self::Pane(id) if id == target => (None, None),
            Self::Pane(id) => (Some(Self::Pane(id)), None),
            Self::Split {
                direction,
                first_ratio,
                first,
                second,
            } => {
                let (new_first, replacement) = first.remove(target);
                if new_first.is_none() {
                    let replacement = second.first_pane();
                    return (Some(*second), Some(replacement));
                }
                if replacement.is_some() {
                    return (
                        Some(Self::Split {
                            direction,
                            first_ratio,
                            first: Box::new(new_first.expect("checked above")),
                            second,
                        }),
                        replacement,
                    );
                }

                let (new_second, replacement) = second.remove(target);
                if new_second.is_none() {
                    let first = new_first.expect("the first subtree still exists");
                    let replacement = first.first_pane();
                    return (Some(first), Some(replacement));
                }

                (
                    Some(Self::Split {
                        direction,
                        first_ratio,
                        first: Box::new(new_first.expect("the first subtree still exists")),
                        second: Box::new(new_second.expect("checked above")),
                    }),
                    replacement,
                )
            }
        }
    }

    fn collect_layout(
        &self,
        rect: PaneRect,
        active: PaneId,
        zoomed: Option<PaneId>,
        panes: &mut Vec<PositionedPane>,
    ) {
        match self {
            Self::Pane(id) => panes.push(PositionedPane {
                id: *id,
                index: panes.len(),
                rect,
                is_active: *id == active,
                is_zoomed: Some(*id) == zoomed,
            }),
            Self::Split {
                direction,
                first_ratio,
                first,
                second,
            } => {
                let (first_rect, second_rect) = rect.split(*direction, *first_ratio);
                first.collect_layout(first_rect, active, zoomed, panes);
                second.collect_layout(second_rect, active, zoomed, panes);
            }
        }
    }

    fn path_to(&self, target: PaneId, path: &mut Vec<Branch>) -> bool {
        match self {
            Self::Pane(id) => *id == target,
            Self::Split { first, second, .. } => {
                path.push(Branch::First);
                if first.path_to(target, path) {
                    return true;
                }
                path.pop();

                path.push(Branch::Second);
                if second.path_to(target, path) {
                    return true;
                }
                path.pop();
                false
            }
        }
    }

    fn node_at_path<'a>(&'a self, path: &[Branch]) -> &'a Self {
        let mut node = self;
        for branch in path {
            node = match (node, branch) {
                (Self::Split { first, .. }, Branch::First) => first,
                (Self::Split { second, .. }, Branch::Second) => second,
                (Self::Pane(_), _) => unreachable!("pane paths only descend through splits"),
            };
        }
        node
    }

    fn node_at_path_mut<'a>(&'a mut self, path: &[Branch]) -> &'a mut Self {
        let mut node = self;
        for branch in path {
            node = match (node, branch) {
                (Self::Split { first, .. }, Branch::First) => first,
                (Self::Split { second, .. }, Branch::Second) => second,
                (Self::Pane(_), _) => unreachable!("pane paths only descend through splits"),
            };
        }
        node
    }

    fn rect_at_path(&self, bounds: PaneRect, path: &[Branch]) -> PaneRect {
        let mut node = self;
        let mut rect = bounds;
        for branch in path {
            match node {
                Self::Split {
                    direction,
                    first_ratio,
                    first,
                    second,
                } => {
                    let (first_rect, second_rect) = rect.split(*direction, *first_ratio);
                    match branch {
                        Branch::First => {
                            node = first;
                            rect = first_rect;
                        }
                        Branch::Second => {
                            node = second;
                            rect = second_rect;
                        }
                    }
                }
                Self::Pane(_) => unreachable!("pane paths only descend through splits"),
            }
        }
        rect
    }
}

#[derive(Deserialize)]
struct PaneLayoutRepr {
    root: PaneNode,
    active: PaneId,
    zoomed: Option<PaneId>,
    next_id: u64,
}

/// A serializable pane split tree with active-pane and zoom state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneLayout {
    root: PaneNode,
    active: PaneId,
    zoomed: Option<PaneId>,
    next_id: u64,
}

impl Default for PaneLayout {
    fn default() -> Self {
        Self::new()
    }
}

impl<'de> Deserialize<'de> for PaneLayout {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_repr(PaneLayoutRepr::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl PaneLayout {
    /// Create a layout containing one active pane.
    pub fn new() -> Self {
        let first = PaneId(1);
        Self {
            root: PaneNode::Pane(first),
            active: first,
            zoomed: None,
            next_id: 2,
        }
    }

    /// Return the active pane.
    pub const fn active(&self) -> PaneId {
        self.active
    }

    /// Return the zoomed pane, if any.
    pub const fn zoomed(&self) -> Option<PaneId> {
        self.zoomed
    }

    /// Return the number of panes in this layout.
    pub fn len(&self) -> usize {
        self.root.pane_count()
    }

    /// Pane layouts always contain at least one pane.
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Return whether the layout contains a pane.
    pub fn contains(&self, id: PaneId) -> bool {
        self.root.contains(id)
    }

    /// Return pane IDs in deterministic preorder.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::with_capacity(self.len());
        self.root.collect_ids(&mut ids);
        ids
    }

    /// Focus an existing pane.
    ///
    /// Focusing a different pane exits zoom mode so the new pane is visible.
    pub fn set_active(&mut self, id: PaneId) -> Result<(), PaneLayoutError> {
        if !self.contains(id) {
            return Err(PaneLayoutError::UnknownPane(id));
        }
        if id != self.active {
            self.zoomed = None;
        }
        self.active = id;
        Ok(())
    }

    /// Split a pane and return the stable ID allocated to the new pane.
    ///
    /// The new pane becomes active and structural changes exit zoom mode.
    pub fn split(
        &mut self,
        target: PaneId,
        request: SplitRequest,
    ) -> Result<PaneId, PaneLayoutError> {
        if !self.contains(target) {
            return Err(PaneLayoutError::UnknownPane(target));
        }
        let following_id = self
            .next_id
            .checked_add(1)
            .ok_or(PaneLayoutError::IdExhausted)?;
        let new_pane = PaneId(self.next_id);
        let changed = self.root.split_pane(target, new_pane, request);
        debug_assert!(changed, "pane existence was checked before splitting");
        self.next_id = following_id;
        self.active = new_pane;
        self.zoomed = None;
        Ok(new_pane)
    }

    /// Split the active pane and return the stable ID allocated to the new pane.
    pub fn split_active(&mut self, request: SplitRequest) -> Result<PaneId, PaneLayoutError> {
        self.split(self.active, request)
    }

    /// Close a pane, collapsing its parent split into the surviving subtree.
    ///
    /// When the active pane closes, focus moves to the nearest surviving sibling
    /// subtree. Pane IDs are never reused.
    pub fn close(&mut self, target: PaneId) -> Result<(), PaneLayoutError> {
        if !self.contains(target) {
            return Err(PaneLayoutError::UnknownPane(target));
        }
        if self.len() == 1 {
            return Err(PaneLayoutError::LastPane);
        }

        let root = std::mem::replace(&mut self.root, PaneNode::Pane(self.active));
        let (root, replacement) = root.remove(target);
        self.root = root.expect("closing one of several panes cannot empty the tree");
        if target == self.active {
            self.active = replacement.expect("a removed pane has a surviving sibling");
        }
        self.zoomed = None;
        Ok(())
    }

    /// Close the active pane.
    pub fn close_active(&mut self) -> Result<(), PaneLayoutError> {
        self.close(self.active)
    }

    /// Zoom a pane to occupy the full layout rectangle.
    pub fn zoom(&mut self, target: PaneId) -> Result<(), PaneLayoutError> {
        if !self.contains(target) {
            return Err(PaneLayoutError::UnknownPane(target));
        }
        self.active = target;
        self.zoomed = Some(target);
        Ok(())
    }

    /// Exit zoom mode.
    pub fn unzoom(&mut self) {
        self.zoomed = None;
    }

    /// Toggle zoom mode for the active pane and return whether it is now zoomed.
    pub fn toggle_zoom(&mut self) -> bool {
        if self.zoomed.is_some() {
            self.zoomed = None;
            false
        } else {
            self.zoomed = Some(self.active);
            true
        }
    }

    /// Compute pane rectangles, respecting the current zoom state.
    pub fn layout(&self, bounds: PaneRect) -> Vec<PositionedPane> {
        if let Some(id) = self.zoomed {
            return vec![PositionedPane {
                id,
                index: self
                    .pane_ids()
                    .iter()
                    .position(|candidate| *candidate == id)
                    .expect("zoomed pane is part of the tree"),
                rect: bounds,
                is_active: true,
                is_zoomed: true,
            }];
        }
        self.layout_unzoomed(bounds)
    }

    /// Compute every pane rectangle while ignoring zoom for geometry.
    pub fn layout_unzoomed(&self, bounds: PaneRect) -> Vec<PositionedPane> {
        let mut panes = Vec::with_capacity(self.len());
        self.root
            .collect_layout(bounds, self.active, self.zoomed, &mut panes);
        panes
    }

    /// Return the nearest pane in a spatial direction.
    ///
    /// Candidates must overlap the active pane on the perpendicular axis. The
    /// nearest edge wins, followed by the largest edge overlap and preorder.
    pub fn neighbor(&self, direction: PaneDirection, bounds: PaneRect) -> Option<PaneId> {
        let panes = self.layout_unzoomed(bounds);
        let active = panes.iter().find(|pane| pane.id == self.active)?;
        panes
            .iter()
            .filter(|candidate| candidate.id != active.id)
            .filter_map(|candidate| {
                neighbor_score(active.rect, candidate.rect, direction).map(
                    |(gap, overlap, center_distance)| {
                        (
                            (gap, overlap, center_distance, candidate.index),
                            candidate.id,
                        )
                    },
                )
            })
            .min_by_key(|(score, _)| *score)
            .map(|(_, id)| id)
    }

    /// Focus the nearest pane in a direction.
    ///
    /// A successful move exits zoom mode. If no pane lies in that direction,
    /// focus and zoom state are unchanged.
    pub fn focus_direction(
        &mut self,
        direction: PaneDirection,
        bounds: PaneRect,
    ) -> Option<PaneId> {
        let neighbor = self.neighbor(direction, bounds)?;
        self.active = neighbor;
        self.zoomed = None;
        Some(neighbor)
    }

    /// Move the nearest divider on the requested axis by `amount` units.
    ///
    /// Left and up move the divider toward smaller coordinates; right and down
    /// move it toward larger coordinates. This matches directional terminal
    /// resizing: the active pane grows when the divider moves outward and
    /// shrinks when it moves inward. Returns `false` if no matching split exists
    /// or the ratio is already at its bound.
    pub fn adjust_active_size(
        &mut self,
        direction: PaneDirection,
        amount: u32,
        bounds: PaneRect,
    ) -> bool {
        if amount == 0 {
            return false;
        }

        let mut path = Vec::new();
        let found = self.root.path_to(self.active, &mut path);
        debug_assert!(found, "the active pane is part of the tree");
        let required_split = match direction {
            PaneDirection::Left | PaneDirection::Right => SplitDirection::Horizontal,
            PaneDirection::Up | PaneDirection::Down => SplitDirection::Vertical,
        };
        let Some(split_depth) = (0..path.len()).rev().find(|depth| {
            matches!(
                self.root.node_at_path(&path[..*depth]),
                PaneNode::Split { direction, .. } if *direction == required_split
            )
        }) else {
            return false;
        };

        let split_rect = self.root.rect_at_path(bounds, &path[..split_depth]);
        let extent = match required_split {
            SplitDirection::Horizontal => split_rect.width,
            SplitDirection::Vertical => split_rect.height,
        };
        if extent < 2 {
            return false;
        }

        let PaneNode::Split { first_ratio, .. } = self.root.node_at_path_mut(&path[..split_depth])
        else {
            unreachable!("the selected path points to a split");
        };
        let old_ratio = *first_ratio;
        let old_extent = old_ratio.first_extent(extent);
        let signed_amount = i64::from(amount);
        let target_extent = match direction {
            PaneDirection::Left | PaneDirection::Up => i64::from(old_extent) - signed_amount,
            PaneDirection::Right | PaneDirection::Down => i64::from(old_extent) + signed_amount,
        }
        .clamp(0, i64::from(extent)) as u32;
        let new_ratio = SplitRatio::clamped_from_fraction(target_extent, extent);
        if new_ratio.first_extent(extent) == old_extent {
            return false;
        }

        *first_ratio = new_ratio;
        self.zoomed = None;
        true
    }

    fn from_repr(repr: PaneLayoutRepr) -> Result<Self, String> {
        let mut ids = Vec::new();
        repr.root.collect_ids(&mut ids);
        let mut unique = HashSet::with_capacity(ids.len());
        if ids.iter().any(|id| id.0 == 0 || !unique.insert(*id)) {
            return Err("pane IDs must be non-zero and unique".into());
        }
        if !unique.contains(&repr.active) {
            return Err("the active pane must exist in the pane tree".into());
        }
        if repr.zoomed.is_some_and(|zoomed| zoomed != repr.active) {
            return Err("the zoomed pane must be the active pane".into());
        }
        let highest_id = ids
            .iter()
            .map(|id| id.0)
            .max()
            .expect("a pane tree always has a leaf");
        if repr.next_id <= highest_id {
            return Err("the next pane ID must be greater than every existing pane ID".into());
        }

        Ok(Self {
            root: repr.root,
            active: repr.active,
            zoomed: repr.zoomed,
            next_id: repr.next_id,
        })
    }
}

fn neighbor_score(
    active: PaneRect,
    candidate: PaneRect,
    direction: PaneDirection,
) -> Option<(u64, Reverse<u64>, u64)> {
    let (gap, overlap, center_distance) = match direction {
        PaneDirection::Left if candidate.x_end() <= active.x_start() => (
            active.x_start() - candidate.x_end(),
            interval_overlap(
                active.y_start(),
                active.y_end(),
                candidate.y_start(),
                candidate.y_end(),
            ),
            doubled_center_distance(
                active.y_start(),
                active.y_end(),
                candidate.y_start(),
                candidate.y_end(),
            ),
        ),
        PaneDirection::Right if candidate.x_start() >= active.x_end() => (
            candidate.x_start() - active.x_end(),
            interval_overlap(
                active.y_start(),
                active.y_end(),
                candidate.y_start(),
                candidate.y_end(),
            ),
            doubled_center_distance(
                active.y_start(),
                active.y_end(),
                candidate.y_start(),
                candidate.y_end(),
            ),
        ),
        PaneDirection::Up if candidate.y_end() <= active.y_start() => (
            active.y_start() - candidate.y_end(),
            interval_overlap(
                active.x_start(),
                active.x_end(),
                candidate.x_start(),
                candidate.x_end(),
            ),
            doubled_center_distance(
                active.x_start(),
                active.x_end(),
                candidate.x_start(),
                candidate.x_end(),
            ),
        ),
        PaneDirection::Down if candidate.y_start() >= active.y_end() => (
            candidate.y_start() - active.y_end(),
            interval_overlap(
                active.x_start(),
                active.x_end(),
                candidate.x_start(),
                candidate.x_end(),
            ),
            doubled_center_distance(
                active.x_start(),
                active.x_end(),
                candidate.x_start(),
                candidate.x_end(),
            ),
        ),
        _ => return None,
    };

    (overlap > 0).then_some((gap, Reverse(overlap), center_distance))
}

fn interval_overlap(first_start: u64, first_end: u64, second_start: u64, second_end: u64) -> u64 {
    first_end
        .min(second_end)
        .saturating_sub(first_start.max(second_start))
}

fn doubled_center_distance(
    first_start: u64,
    first_end: u64,
    second_start: u64,
    second_end: u64,
) -> u64 {
    (first_start + first_end).abs_diff(second_start + second_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(direction: SplitDirection, placement: SplitPlacement, percent: u8) -> SplitRequest {
        SplitRequest {
            direction,
            placement,
            ratio: SplitRatio::from_percent(percent).unwrap(),
        }
    }

    fn rect_of(layout: &PaneLayout, bounds: PaneRect, id: PaneId) -> PaneRect {
        layout
            .layout_unzoomed(bounds)
            .into_iter()
            .find(|pane| pane.id == id)
            .unwrap()
            .rect
    }

    #[test]
    fn starts_with_one_active_pane() {
        let layout = PaneLayout::new();
        assert_eq!(layout.len(), 1);
        assert!(!layout.is_empty());
        assert_eq!(layout.active().get(), 1);
        assert_eq!(layout.pane_ids(), vec![layout.active()]);
        assert_eq!(layout.zoomed(), None);
    }

    #[test]
    fn split_ratio_rejects_values_outside_safe_bounds() {
        assert_eq!(SplitRatio::from_percent(4).unwrap_err().basis_points(), 400);
        assert_eq!(SplitRatio::from_percent(5).unwrap(), SplitRatio::MIN);
        assert_eq!(SplitRatio::from_percent(50).unwrap(), SplitRatio::HALF);
        assert_eq!(SplitRatio::from_percent(95).unwrap(), SplitRatio::MAX);
        assert!(SplitRatio::from_percent(96).is_err());
        assert!(SplitRatio::from_basis_points(499).is_err());
        assert!(SplitRatio::from_basis_points(9_501).is_err());
    }

    #[test]
    fn placement_and_new_pane_ratio_are_unambiguous() {
        let bounds = PaneRect::new(0, 0, 100, 40);
        let mut second_layout = PaneLayout::new();
        let original = second_layout.active();
        let inserted = second_layout
            .split_active(request(
                SplitDirection::Horizontal,
                SplitPlacement::Second,
                30,
            ))
            .unwrap();
        assert_eq!(rect_of(&second_layout, bounds, original).width, 70);
        assert_eq!(rect_of(&second_layout, bounds, inserted).width, 30);

        let mut first_layout = PaneLayout::new();
        let original = first_layout.active();
        let inserted = first_layout
            .split_active(request(SplitDirection::Vertical, SplitPlacement::First, 25))
            .unwrap();
        assert_eq!(rect_of(&first_layout, bounds, inserted).height, 10);
        assert_eq!(rect_of(&first_layout, bounds, original).height, 30);
        assert_eq!(first_layout.pane_ids(), vec![inserted, original]);
    }

    #[test]
    fn nested_layout_is_deterministic_and_covers_odd_rectangles() {
        let bounds = PaneRect::new(7, 11, 101, 51);
        let mut layout = PaneLayout::new();
        let first = layout.active();
        let second = layout
            .split_active(request(
                SplitDirection::Horizontal,
                SplitPlacement::Second,
                50,
            ))
            .unwrap();
        let third = layout
            .split_active(request(
                SplitDirection::Vertical,
                SplitPlacement::Second,
                40,
            ))
            .unwrap();

        assert_eq!(
            rect_of(&layout, bounds, first),
            PaneRect::new(7, 11, 50, 51)
        );
        assert_eq!(
            rect_of(&layout, bounds, second),
            PaneRect::new(57, 11, 51, 30)
        );
        assert_eq!(
            rect_of(&layout, bounds, third),
            PaneRect::new(57, 41, 51, 21)
        );

        let area: u64 = layout
            .layout(bounds)
            .iter()
            .map(|pane| u64::from(pane.rect.width) * u64::from(pane.rect.height))
            .sum();
        assert_eq!(area, u64::from(bounds.width) * u64::from(bounds.height));
    }

    #[test]
    fn stable_ids_are_monotonic_and_never_reused() {
        let mut layout = PaneLayout::new();
        let first = layout.active();
        let second = layout.split_active(SplitRequest::default()).unwrap();
        let third = layout.split_active(SplitRequest::default()).unwrap();
        layout.close(third).unwrap();
        layout.set_active(first).unwrap();
        let fourth = layout.split_active(SplitRequest::default()).unwrap();

        assert_eq!(second.get(), 2);
        assert_eq!(third.get(), 3);
        assert_eq!(fourth.get(), 4);
        assert!(!layout.contains(third));
    }

    #[test]
    fn closing_collapses_splits_and_prefers_the_nearest_sibling() {
        let mut layout = PaneLayout::new();
        let first = layout.active();
        let second = layout.split_active(SplitRequest::default()).unwrap();
        let _third = layout
            .split_active(request(
                SplitDirection::Vertical,
                SplitPlacement::Second,
                50,
            ))
            .unwrap();

        layout.close_active().unwrap();
        assert_eq!(layout.active(), second);
        assert_eq!(layout.pane_ids(), vec![first, second]);
        layout.close_active().unwrap();
        assert_eq!(layout.active(), first);
        assert_eq!(layout.pane_ids(), vec![first]);
    }

    #[test]
    fn closing_inactive_pane_preserves_focus_and_last_close_is_atomic() {
        let mut layout = PaneLayout::new();
        let first = layout.active();
        let second = layout.split_active(SplitRequest::default()).unwrap();
        layout.set_active(first).unwrap();
        layout.close(second).unwrap();
        assert_eq!(layout.active(), first);

        let before = layout.clone();
        assert_eq!(layout.close_active(), Err(PaneLayoutError::LastPane));
        assert_eq!(layout, before);
    }

    #[test]
    fn unknown_pane_operations_do_not_mutate_layout() {
        let mut layout = PaneLayout::new();
        let unknown = PaneId(99);
        let before = layout.clone();
        assert_eq!(
            layout.set_active(unknown),
            Err(PaneLayoutError::UnknownPane(unknown))
        );
        assert_eq!(
            layout.split(unknown, SplitRequest::default()),
            Err(PaneLayoutError::UnknownPane(unknown))
        );
        assert_eq!(
            layout.close(unknown),
            Err(PaneLayoutError::UnknownPane(unknown))
        );
        assert_eq!(
            layout.zoom(unknown),
            Err(PaneLayoutError::UnknownPane(unknown))
        );
        assert_eq!(layout, before);
    }

    #[test]
    fn directional_focus_uses_edge_overlap_and_exits_zoom() {
        let bounds = PaneRect::new(0, 0, 100, 100);
        let mut layout = PaneLayout::new();
        let left = layout.active();
        let right = layout
            .split_active(request(
                SplitDirection::Horizontal,
                SplitPlacement::Second,
                50,
            ))
            .unwrap();
        layout.set_active(left).unwrap();
        let lower_left = layout
            .split_active(request(
                SplitDirection::Vertical,
                SplitPlacement::Second,
                30,
            ))
            .unwrap();

        layout.zoom(right).unwrap();
        assert_eq!(layout.neighbor(PaneDirection::Left, bounds), Some(left));
        assert_eq!(
            layout.focus_direction(PaneDirection::Left, bounds),
            Some(left)
        );
        assert_eq!(layout.zoomed(), None);
        assert_eq!(
            layout.focus_direction(PaneDirection::Down, bounds),
            Some(lower_left)
        );
        assert_eq!(layout.focus_direction(PaneDirection::Down, bounds), None);
        assert_eq!(layout.active(), lower_left);
    }

    #[test]
    fn directional_resize_moves_nearest_matching_divider() {
        let bounds = PaneRect::new(0, 0, 100, 100);
        let mut layout = PaneLayout::new();
        let left = layout.active();
        let right = layout.split_active(SplitRequest::default()).unwrap();

        layout.set_active(left).unwrap();
        assert!(layout.adjust_active_size(PaneDirection::Right, 10, bounds));
        assert_eq!(rect_of(&layout, bounds, left).width, 60);
        assert!(layout.adjust_active_size(PaneDirection::Left, 20, bounds));
        assert_eq!(rect_of(&layout, bounds, left).width, 40);

        layout.set_active(right).unwrap();
        assert!(layout.adjust_active_size(PaneDirection::Left, 10, bounds));
        assert_eq!(rect_of(&layout, bounds, right).width, 70);
        assert!(layout.adjust_active_size(PaneDirection::Right, 10, bounds));
        assert_eq!(rect_of(&layout, bounds, right).width, 60);
        assert!(!layout.adjust_active_size(PaneDirection::Down, 10, bounds));
        assert!(!layout.adjust_active_size(PaneDirection::Right, 0, bounds));
    }

    #[test]
    fn resize_clamps_to_ratio_bounds_and_handles_tiny_geometry() {
        let bounds = PaneRect::new(0, 0, 100, 1);
        let mut layout = PaneLayout::new();
        let first = layout.active();
        layout.split_active(SplitRequest::default()).unwrap();
        layout.set_active(first).unwrap();

        assert!(layout.adjust_active_size(PaneDirection::Right, u32::MAX, bounds));
        assert_eq!(rect_of(&layout, bounds, first).width, 95);
        assert!(!layout.adjust_active_size(PaneDirection::Right, 1, bounds));
        assert!(layout.adjust_active_size(PaneDirection::Left, u32::MAX, bounds));
        assert_eq!(rect_of(&layout, bounds, first).width, 5);

        let tiny = PaneRect::new(u32::MAX, u32::MAX, 1, 1);
        assert!(!layout.adjust_active_size(PaneDirection::Right, 1, tiny));
        let panes = layout.layout_unzoomed(tiny);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].rect.width + panes[1].rect.width, 1);
    }

    #[test]
    fn resize_chooses_the_nearest_nested_split_on_the_axis() {
        let bounds = PaneRect::new(0, 0, 200, 80);
        let mut layout = PaneLayout::new();
        let outer_left = layout.active();
        let outer_right = layout.split_active(SplitRequest::default()).unwrap();
        let inner_right = layout.split_active(SplitRequest::default()).unwrap();

        assert!(layout.adjust_active_size(PaneDirection::Left, 10, bounds));
        assert_eq!(rect_of(&layout, bounds, outer_left).width, 100);
        assert_eq!(rect_of(&layout, bounds, outer_right).width, 40);
        assert_eq!(rect_of(&layout, bounds, inner_right).width, 60);
    }

    #[test]
    fn zoom_preserves_tree_and_structural_changes_unzoom() {
        let bounds = PaneRect::new(3, 4, 120, 60);
        let mut layout = PaneLayout::new();
        let first = layout.active();
        let second = layout.split_active(SplitRequest::default()).unwrap();

        layout.zoom(first).unwrap();
        assert_eq!(layout.layout(bounds).len(), 1);
        assert_eq!(layout.layout(bounds)[0].rect, bounds);
        assert_eq!(layout.layout_unzoomed(bounds).len(), 2);
        assert!(!layout.toggle_zoom());
        assert_eq!(layout.zoomed(), None);
        assert!(layout.toggle_zoom());
        assert_eq!(layout.zoomed(), Some(first));

        layout.zoom(second).unwrap();
        layout.split_active(SplitRequest::default()).unwrap();
        assert_eq!(layout.zoomed(), None);
        assert_eq!(layout.len(), 3);
    }

    #[test]
    fn representation_validation_protects_deserialized_invariants() {
        let duplicate = PaneLayoutRepr {
            root: PaneNode::Split {
                direction: SplitDirection::Horizontal,
                first_ratio: SplitRatio::HALF,
                first: Box::new(PaneNode::Pane(PaneId(1))),
                second: Box::new(PaneNode::Pane(PaneId(1))),
            },
            active: PaneId(1),
            zoomed: None,
            next_id: 2,
        };
        assert!(PaneLayout::from_repr(duplicate).is_err());

        let missing_active = PaneLayoutRepr {
            root: PaneNode::Pane(PaneId(1)),
            active: PaneId(2),
            zoomed: None,
            next_id: 3,
        };
        assert!(PaneLayout::from_repr(missing_active).is_err());

        let mismatched_zoom = PaneLayoutRepr {
            root: PaneNode::Split {
                direction: SplitDirection::Vertical,
                first_ratio: SplitRatio::HALF,
                first: Box::new(PaneNode::Pane(PaneId(1))),
                second: Box::new(PaneNode::Pane(PaneId(2))),
            },
            active: PaneId(1),
            zoomed: Some(PaneId(2)),
            next_id: 3,
        };
        assert!(PaneLayout::from_repr(mismatched_zoom).is_err());

        let reused_next_id = PaneLayoutRepr {
            root: PaneNode::Pane(PaneId(4)),
            active: PaneId(4),
            zoomed: None,
            next_id: 4,
        };
        assert!(PaneLayout::from_repr(reused_next_id).is_err());

        let layout = PaneLayout::new();
        let valid = PaneLayoutRepr {
            root: layout.root.clone(),
            active: layout.active,
            zoomed: layout.zoomed,
            next_id: layout.next_id,
        };
        assert_eq!(PaneLayout::from_repr(valid).unwrap(), layout);
    }
}
