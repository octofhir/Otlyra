//! Persistent identity and invalidation for browser-owned UI.
//!
//! Widget constructors remain cheap descriptions. This arena is the state that
//! survives them: identity, geometry, measurement, and pending work. Browser
//! model values deliberately do not live here.

use std::any::TypeId;
use std::cell::RefCell;
use std::rc::Rc;

use otlyra_gfx::DisplayList;

use super::{Child, Cx, Described, Event, Rect, Size, Widget};

/// A persistent subtree used as a migration boundary for immediate widgets.
///
/// A newly constructed parent tree holds a cheap proxy. The actual child,
/// geometry, measurement, and display list remain here until its description
/// changes.
pub struct Retained<A> {
    state: Rc<RefCell<RetainedState<A>>>,
}

impl<A> Clone for Retained<A> {
    fn clone(&self) -> Self {
        Self {
            state: Rc::clone(&self.state),
        }
    }
}

struct RetainedState<A> {
    child: Child<A>,
    measurement: Option<Measurement>,
    rect: Option<Rect>,
    display_list: Option<DisplayList>,
    descriptions: Option<Vec<Described>>,
    builds: u64,
    semantics_builds: u64,
}

impl<A: 'static> Retained<A> {
    /// Start with `child` as a dirty subtree.
    pub fn new(child: Child<A>) -> Self {
        Self {
            state: Rc::new(RefCell::new(RetainedState {
                child,
                measurement: None,
                rect: None,
                display_list: None,
                descriptions: None,
                builds: 0,
                semantics_builds: 0,
            })),
        }
    }

    /// Replace the description, invalidating its layout and paint caches.
    pub fn replace(&self, child: Child<A>) {
        self.replace_with_dirty(child, UiDirty::ALL);
    }

    /// Replace the description while preserving clean semantics.
    pub fn replace_with_dirty(&self, child: Child<A>, dirty: UiDirty) {
        let mut state = self.state.borrow_mut();
        state.child = child;
        state.measurement = None;
        state.rect = None;
        state.display_list = None;
        if dirty.contains(UiDirty::SEMANTICS) {
            state.descriptions = None;
        }
    }

    /// A cheap widget proxy that can be put into each short-lived parent tree.
    pub fn widget(&self) -> Child<A> {
        Box::new(RetainedProxy {
            retained: self.clone(),
        })
    }

    /// Number of display lists actually rebuilt behind this boundary.
    pub fn builds(&self) -> u64 {
        self.state.borrow().builds
    }

    /// Number of semantic descriptions built behind this boundary.
    pub fn semantics_builds(&self) -> u64 {
        self.state.borrow().semantics_builds
    }
}

struct RetainedProxy<A> {
    retained: Retained<A>,
}

impl<A: 'static> Widget<A> for RetainedProxy<A> {
    fn measure(&mut self, available: Size, cx: &mut Cx) -> Size {
        let mut state = self.retained.state.borrow_mut();
        if let Some(measurement) = state.measurement
            && measurement.available == available
        {
            return measurement.measured;
        }
        let measured = state.child.measure(available, cx);
        state.measurement = Some(Measurement {
            available,
            measured,
        });
        measured
    }

    fn place(&mut self, rect: Rect, cx: &mut Cx) {
        let mut state = self.retained.state.borrow_mut();
        if state.rect != Some(rect) {
            state.child.place(rect, cx);
            state.rect = Some(rect);
            state.display_list = None;
        }
    }

    fn draw(&mut self, cx: &mut Cx, list: &mut DisplayList) {
        let mut state = self.retained.state.borrow_mut();
        if state.display_list.is_none() {
            let mut built = DisplayList::new();
            state.child.draw(cx, &mut built);
            state.display_list = Some(built);
            state.builds += 1;
        }
        list.append(state.display_list.as_ref().expect("just built"));
    }

    fn event(&mut self, event: &Event, cx: &mut Cx) -> Option<A> {
        self.retained.state.borrow_mut().child.event(event, cx)
    }

    fn describe(&self, out: &mut Vec<Described>) {
        let mut state = self.retained.state.borrow_mut();
        if state.descriptions.is_none() {
            let mut descriptions = Vec::new();
            state.child.describe(&mut descriptions);
            state.descriptions = Some(descriptions);
            state.semantics_builds += 1;
        }
        out.extend(
            state
                .descriptions
                .as_ref()
                .expect("just built")
                .iter()
                .cloned(),
        );
    }

    fn label_text(&self) -> Option<String> {
        self.retained.state.borrow().child.label_text()
    }

    fn flex(&self) -> f64 {
        self.retained.state.borrow().child.flex()
    }
}

/// Stable handle to one live render node.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct UiNodeId {
    slot: u32,
    generation: u32,
}

/// Identity supplied for a child that may move among its siblings.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct WidgetKey(u64);

impl WidgetKey {
    /// A key from an application-owned stable integer, such as a tab id.
    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }
}

/// Runtime identity of a widget description type.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct WidgetType {
    id: TypeId,
    name: &'static str,
}

impl WidgetType {
    /// The identity of description type `T`.
    pub fn of<T: 'static>() -> Self {
        Self {
            id: TypeId::of::<T>(),
            name: std::any::type_name::<T>(),
        }
    }

    /// A diagnostic name for tree inspection.
    pub const fn name(self) -> &'static str {
        self.name
    }
}

/// Work invalidated on a persistent node.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct UiDirty(u8);

impl UiDirty {
    /// Description or child identity changed.
    pub const RECONCILE: Self = Self(1 << 0);
    /// Measurement or geometry changed.
    pub const LAYOUT: Self = Self(1 << 1);
    /// Display-list contents changed.
    pub const PAINT: Self = Self(1 << 2);
    /// Accessible role, value, label, or bounds changed.
    pub const SEMANTICS: Self = Self(1 << 3);
    /// Only a retained layer property changed.
    pub const COMPOSITE: Self = Self(1 << 4);
    /// Every kind of work, used for a newly mounted node.
    pub const ALL: Self = Self(
        Self::RECONCILE.0 | Self::LAYOUT.0 | Self::PAINT.0 | Self::SEMANTICS.0 | Self::COMPOSITE.0,
    );

    /// Whether all flags in `other` are set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no work is pending.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Combine invalidations.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

/// One child description presented to reconciliation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NodeSpec {
    widget_type: WidgetType,
    key: Option<WidgetKey>,
    changed: UiDirty,
}

impl NodeSpec {
    /// An unkeyed child whose identity is its structural position.
    pub fn new<T: 'static>() -> Self {
        Self {
            widget_type: WidgetType::of::<T>(),
            key: None,
            changed: UiDirty::default(),
        }
    }

    /// Preserve identity when this child moves.
    pub const fn keyed(mut self, key: WidgetKey) -> Self {
        self.key = Some(key);
        self
    }

    /// Declare the narrow work implied by changed properties.
    pub const fn changed(mut self, dirty: UiDirty) -> Self {
        self.changed = dirty;
        self
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
struct Measurement {
    available: Size,
    measured: Size,
}

#[derive(Debug)]
struct RenderNode {
    widget_type: WidgetType,
    key: Option<WidgetKey>,
    parent: Option<UiNodeId>,
    children: Vec<UiNodeId>,
    rect: Rect,
    dirty: UiDirty,
    measurement: Option<Measurement>,
}

#[derive(Debug, Default)]
struct Slot {
    generation: u32,
    node: Option<RenderNode>,
}

/// Persistent render-node storage for one UI surface.
#[derive(Debug, Default)]
pub struct RenderArena {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

impl RenderArena {
    /// Create an empty surface runtime.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount one root or child. New nodes require every pass.
    pub fn mount(
        &mut self,
        parent: Option<UiNodeId>,
        widget_type: WidgetType,
        key: Option<WidgetKey>,
    ) -> UiNodeId {
        assert!(
            parent.is_none() || parent.is_some_and(|id| self.node(id).is_some()),
            "parent must be a live node"
        );
        let node = RenderNode {
            widget_type,
            key,
            parent,
            children: Vec::new(),
            rect: Rect::ZERO,
            dirty: UiDirty::ALL,
            measurement: None,
        };
        let slot = if let Some(slot) = self.free.pop() {
            self.slots[slot as usize].node = Some(node);
            slot
        } else {
            let slot = u32::try_from(self.slots.len()).expect("render arena exceeded u32");
            self.slots.push(Slot {
                generation: 0,
                node: Some(node),
            });
            slot
        };
        let id = UiNodeId {
            slot,
            generation: self.slots[slot as usize].generation,
        };
        if let Some(parent) = parent {
            self.node_mut(parent)
                .expect("parent was checked")
                .children
                .push(id);
        }
        id
    }

    /// Reconcile direct children by type plus key or structural position.
    pub fn reconcile_children(
        &mut self,
        parent: UiNodeId,
        specs: impl IntoIterator<Item = NodeSpec>,
    ) -> Vec<UiNodeId> {
        let old = std::mem::take(
            &mut self
                .node_mut(parent)
                .expect("reconciliation parent must be live")
                .children,
        );
        let old_order = old.clone();
        let mut unused = vec![true; old.len()];
        let mut next = Vec::new();

        for (position, spec) in specs.into_iter().enumerate() {
            let matched = match spec.key {
                Some(key) => old.iter().enumerate().find_map(|(index, id)| {
                    (unused[index]
                        && self.node(*id).is_some_and(|node| {
                            node.key == Some(key) && node.widget_type == spec.widget_type
                        }))
                    .then_some((index, *id))
                }),
                None => old.get(position).and_then(|id| {
                    self.node(*id)
                        .is_some_and(|node| {
                            node.key.is_none() && node.widget_type == spec.widget_type
                        })
                        .then_some((position, *id))
                }),
            };
            let id = if let Some((index, id)) = matched {
                unused[index] = false;
                self.invalidate(id, spec.changed);
                id
            } else {
                self.mount(None, spec.widget_type, spec.key)
            };
            self.node_mut(id).expect("new child is live").parent = Some(parent);
            next.push(id);
        }

        for (index, id) in old.into_iter().enumerate() {
            if unused[index] {
                self.remove_subtree(id);
            }
        }
        self.node_mut(parent)
            .expect("parent must remain live")
            .children
            .clone_from(&next);
        if next != old_order {
            self.invalidate(parent, UiDirty::RECONCILE.union(UiDirty::LAYOUT));
        }
        next
    }

    /// Mark narrow work, propagating layout invalidation to ancestors.
    pub fn invalidate(&mut self, id: UiNodeId, dirty: UiDirty) {
        if dirty.is_empty() {
            return;
        }
        let propagates_layout = dirty.contains(UiDirty::LAYOUT);
        let mut current = Some(id);
        let mut first = true;
        while let Some(node_id) = current {
            let Some(node) = self.node_mut(node_id) else {
                return;
            };
            let applied = if first { dirty } else { UiDirty::LAYOUT };
            node.dirty.insert(applied);
            if applied.contains(UiDirty::LAYOUT) {
                node.measurement = None;
            }
            current = propagates_layout.then_some(node.parent).flatten();
            first = false;
        }
    }

    /// Dirty work currently attached to a live node.
    pub fn dirty(&self, id: UiNodeId) -> Option<UiDirty> {
        self.node(id).map(|node| node.dirty)
    }

    /// Mark selected work complete.
    pub fn clear_dirty(&mut self, id: UiNodeId, dirty: UiDirty) {
        if let Some(node) = self.node_mut(id) {
            node.dirty.remove(dirty);
        }
    }

    /// Store the one geometry used by paint, hit testing, and semantics.
    pub fn set_rect(&mut self, id: UiNodeId, rect: Rect) {
        let node = self.node_mut(id).expect("placed node must be live");
        if node.rect != rect {
            node.rect = rect;
            node.dirty.insert(UiDirty::PAINT.union(UiDirty::SEMANTICS));
        }
    }

    /// Last placed geometry.
    pub fn rect(&self, id: UiNodeId) -> Option<Rect> {
        self.node(id).map(|node| node.rect)
    }

    /// Cache one measurement for exact constraints.
    pub fn cache_measurement(&mut self, id: UiNodeId, available: Size, measured: Size) {
        let node = self.node_mut(id).expect("measured node must be live");
        node.measurement = Some(Measurement {
            available,
            measured,
        });
        node.dirty.remove(UiDirty::LAYOUT);
    }

    /// Return the cached result only when constraints and layout inputs match.
    pub fn measurement(&self, id: UiNodeId, available: Size) -> Option<Size> {
        let node = self.node(id)?;
        (!node.dirty.contains(UiDirty::LAYOUT))
            .then_some(node.measurement)
            .flatten()
            .filter(|measurement| measurement.available == available)
            .map(|measurement| measurement.measured)
    }

    /// Direct children in paint and hit-test order.
    pub fn children(&self, id: UiNodeId) -> Option<&[UiNodeId]> {
        self.node(id).map(|node| node.children.as_slice())
    }

    /// Whether this id still names its original live node.
    pub fn contains(&self, id: UiNodeId) -> bool {
        self.node(id).is_some()
    }

    fn node(&self, id: UiNodeId) -> Option<&RenderNode> {
        self.slots
            .get(id.slot as usize)
            .filter(|slot| slot.generation == id.generation)?
            .node
            .as_ref()
    }

    fn node_mut(&mut self, id: UiNodeId) -> Option<&mut RenderNode> {
        self.slots
            .get_mut(id.slot as usize)
            .filter(|slot| slot.generation == id.generation)?
            .node
            .as_mut()
    }

    fn remove_subtree(&mut self, id: UiNodeId) {
        let Some(node) = self.node_mut(id) else {
            return;
        };
        let children = std::mem::take(&mut node.children);
        for child in children {
            self.remove_subtree(child);
        }
        let slot = &mut self.slots[id.slot as usize];
        slot.node = None;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(id.slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    use otlyra_gfx::DisplayItem;
    use otlyra_text::TextEngine;

    struct Row;
    struct Label;
    struct Button;

    struct Counted {
        measures: Rc<Cell<u64>>,
        places: Rc<Cell<u64>>,
        draws: Rc<Cell<u64>>,
    }

    impl Widget<()> for Counted {
        fn measure(&mut self, _available: Size, _cx: &mut Cx) -> Size {
            self.measures.set(self.measures.get() + 1);
            Size::new(80.0, 20.0)
        }

        fn place(&mut self, _rect: Rect, _cx: &mut Cx) {
            self.places.set(self.places.get() + 1);
        }

        fn draw(&mut self, _cx: &mut Cx, list: &mut DisplayList) {
            self.draws.set(self.draws.get() + 1);
            list.push(DisplayItem::PopLayer);
        }
    }

    fn root(arena: &mut RenderArena) -> UiNodeId {
        arena.mount(None, WidgetType::of::<Row>(), None)
    }

    #[test]
    fn keyed_children_keep_identity_when_moved() {
        let mut arena = RenderArena::new();
        let root = root(&mut arena);
        let one = WidgetKey::from_u64(1);
        let two = WidgetKey::from_u64(2);
        let before = arena.reconcile_children(
            root,
            [
                NodeSpec::new::<Button>().keyed(one),
                NodeSpec::new::<Button>().keyed(two),
            ],
        );
        arena.clear_dirty(root, UiDirty::ALL);
        let after = arena.reconcile_children(
            root,
            [
                NodeSpec::new::<Button>().keyed(two),
                NodeSpec::new::<Button>().keyed(one),
            ],
        );
        assert_eq!(after, [before[1], before[0]]);
        assert!(
            arena.dirty(root).unwrap().contains(UiDirty::LAYOUT),
            "moving children changes their parent's layout"
        );
    }

    #[test]
    fn changed_type_replaces_a_keyed_node_and_invalidates_the_old_handle() {
        let mut arena = RenderArena::new();
        let root = root(&mut arena);
        let key = WidgetKey::from_u64(7);
        let old = arena.reconcile_children(root, [NodeSpec::new::<Label>().keyed(key)])[0];
        let new = arena.reconcile_children(root, [NodeSpec::new::<Button>().keyed(key)])[0];
        assert_ne!(new, old);
        assert!(!arena.contains(old));
        assert!(arena.contains(new));
    }

    #[test]
    fn layout_invalidation_clears_measurement_and_reaches_the_root() {
        let mut arena = RenderArena::new();
        let root = root(&mut arena);
        let child = arena.reconcile_children(root, [NodeSpec::new::<Label>()])[0];
        let available = Size::new(200.0, 40.0);
        arena.cache_measurement(child, available, Size::new(80.0, 20.0));
        arena.clear_dirty(root, UiDirty::ALL);
        arena.invalidate(child, UiDirty::LAYOUT);
        assert_eq!(arena.measurement(child, available), None);
        assert!(arena.dirty(root).unwrap().contains(UiDirty::LAYOUT));
    }

    #[test]
    fn paint_change_keeps_measurement_and_parent_layout_clean() {
        let mut arena = RenderArena::new();
        let root = root(&mut arena);
        let child = arena.reconcile_children(root, [NodeSpec::new::<Label>()])[0];
        let available = Size::new(200.0, 40.0);
        let measured = Size::new(80.0, 20.0);
        arena.cache_measurement(child, available, measured);
        arena.clear_dirty(root, UiDirty::ALL);
        arena.clear_dirty(child, UiDirty::ALL);
        arena.invalidate(child, UiDirty::PAINT);
        assert_eq!(arena.measurement(child, available), Some(measured));
        assert!(!arena.dirty(root).unwrap().contains(UiDirty::LAYOUT));
        assert!(arena.dirty(child).unwrap().contains(UiDirty::PAINT));
    }

    #[test]
    fn stale_id_does_not_resurrect_when_its_slot_is_reused() {
        let mut arena = RenderArena::new();
        let root = root(&mut arena);
        let old = arena.reconcile_children(root, [NodeSpec::new::<Label>()])[0];
        arena.reconcile_children(root, std::iter::empty());
        let new = arena.reconcile_children(root, [NodeSpec::new::<Label>()])[0];
        assert_ne!(old, new);
        assert!(!arena.contains(old));
        assert!(arena.contains(new));
    }

    #[test]
    fn a_fresh_proxy_reuses_measurement_geometry_and_paint() {
        let measures = Rc::new(Cell::new(0));
        let places = Rc::new(Cell::new(0));
        let draws = Rc::new(Cell::new(0));
        let retained = Retained::new(Box::new(Counted {
            measures: Rc::clone(&measures),
            places: Rc::clone(&places),
            draws: Rc::clone(&draws),
        }));
        let available = Size::new(200.0, 40.0);
        let rect = Rect::new(0.0, 0.0, 200.0, 40.0);
        let mut text = TextEngine::new();
        let mut cx = Cx::new(&mut text);

        for _ in 0..2 {
            let mut proxy = retained.widget();
            assert_eq!(proxy.measure(available, &mut cx), Size::new(80.0, 20.0));
            proxy.place(rect, &mut cx);
            proxy.draw(&mut cx, &mut DisplayList::new());
        }

        assert_eq!(measures.get(), 1);
        assert_eq!(places.get(), 1);
        assert_eq!(draws.get(), 1);
        assert_eq!(retained.builds(), 1);
    }
}
