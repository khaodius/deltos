//! The graph view: every namespace and object of the active connection on one plane, with
//! lines for the relations between them.
//!
//! It is a panel like any other, so it docks, tears off and rejoins. It opens on the spot
//! the person was looking at in the regular view (a namespace, a table, or one of its
//! columns), and they zoom out from there and click through the database. Back returns to
//! the panel it was opened from. Where it looked is saved per connection, pinned to the
//! object at its middle, so it comes back to the same place after a restart.
//!
//! The model (layout, camera, relations) is in `deltos-graph`. This file draws it and
//! feeds it: it lists namespaces that are not listed yet, describes the objects that come
//! into view (a few at a time, never the whole database at once), and turns what each
//! description says into cards and lines.
//!
//! Drawing is one canvas. Only cells in view are visited, text is left out once it would be
//! too small to read, and font sizes are rounded to half pixels so that the text system's
//! cache serves most lines while the zoom changes.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::{Duration, Instant};

use deltos_data::{ObjectInfo, ObjectKind, ObjectRef};
use deltos_graph::{
    Camera, Detail, FOOTER_H, Focus, HEADER_H, Hit, Layout, Listing, MAX_ROWS, NODE_W, NodeHit,
    ROW_H, Rect, Relation, RelationSource, SavedView, node_height,
};
use gpui::{
    App, BorderStyle, Bounds, Context, DispatchPhase, Entity, FocusHandle, Focusable, FontWeight,
    Hsla, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathBuilder, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, SharedString, Subscription,
    TextRun, WeakEntity, Window, canvas, div, fill, point, prelude::*, px, quad, size,
};

use crate::net::Net;
use crate::state::{AppState, StateEvent};
use crate::theme::{Theme, theme};
use crate::ui::icons::{Icon, icon};
use crate::ui::workspace::Workspace;
use crate::ui::{ButtonStyle, Tooltip, button, compact_count, empty_state, icon_button};

/// Descriptions running at once.
const DESCRIBE_IN_FLIGHT: usize = 4;
/// Namespace listings running at once.
const LIST_IN_FLIGHT: usize = 3;
/// Cards in view, below which they are described even when zoomed far out.
const DESCRIBE_WHEN_FEW: usize = 60;
/// Descriptions kept per connection. Past this the oldest are dropped and described
/// again if they come back into view.
const DESCRIBED_KEPT: usize = 4_000;
/// How long a flight to a new spot takes at normal motion speed.
const FLIGHT: Duration = Duration::from_millis(280);
/// How long after the camera stops its place is saved.
const SAVE_AFTER: Duration = Duration::from_millis(700);
/// How far the pointer moves with the button down before a press becomes a pan.
const DRAG_SLOP: f32 = 4.0;
/// Lines drawn per frame, at most.
const MAX_EDGES: usize = 8_000;
/// The smallest text drawn, in pixels.
const MIN_TEXT: f32 = 5.5;

/// What a card shows of one column.
struct CardColumn {
    name: SharedString,
    ty: SharedString,
    key: bool,
    link: bool,
}

/// What a card shows of a described object.
struct Card {
    columns: Vec<CardColumn>,
    total: usize,
}

impl Card {
    fn new(info: &ObjectInfo, links: &HashSet<String>) -> Card {
        let columns = info
            .columns
            .iter()
            .take(MAX_ROWS)
            .map(|c| CardColumn {
                name: c.name.clone().into(),
                ty: c.type_name.clone().into(),
                key: c.primary_key.is_some(),
                link: links.contains(&c.name),
            })
            .collect();
        Card {
            columns,
            total: info.columns.len(),
        }
    }

    fn height(&self) -> f32 {
        node_height(Some(self.total))
    }
}

/// A line between two cards, by slot, and the rows it starts and ends at.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Edge {
    from: usize,
    from_row: Option<usize>,
    to: usize,
    to_row: Option<usize>,
    inferred: bool,
}

/// What the canvas reads while it paints. The panel changes it between frames.
#[derive(Default)]
struct Model {
    layout: Rc<Layout>,
    /// By slot.
    cards: Vec<Option<Rc<Card>>>,
    edges: Vec<Edge>,
    camera: Camera,
    viewport: Option<Bounds<Pixels>>,
    /// The selected card, and the selected row on it.
    selected: Option<(usize, Option<usize>)>,
}

impl Model {
    fn height(&self, slot: usize) -> f32 {
        self.cards
            .get(slot)
            .and_then(Option::as_ref)
            .map_or(node_height(None), |c| c.height())
    }

    fn size(&self) -> (f32, f32) {
        self.viewport.map_or((1000.0, 700.0), |b| {
            (b.size.width.into(), b.size.height.into())
        })
    }

    fn hit(&self, local: (f32, f32)) -> Hit {
        let world = self.camera.to_world(local, self.size());
        self.layout.hit(
            world,
            |s| self.height(s),
            |s| self.cards.get(s).and_then(Option::as_ref).map(|c| c.total),
        )
    }
}

enum Described {
    Loading,
    Ready { card: Rc<Card>, names: Vec<String> },
    Failed,
}

/// Where the camera should go once the plane holds what it needs.
#[derive(Debug, Clone)]
enum Pending {
    Focus(Focus),
    Saved(SavedView),
}

struct Flight {
    from: Camera,
    to: Camera,
    start: Instant,
    duration: Duration,
}

struct Pan {
    start: Point<Pixels>,
    last: Point<Pixels>,
    moved: bool,
    clicks: usize,
}

pub struct GraphPanel {
    state: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    model: Rc<RefCell<Model>>,
    /// The connection shown, and a hash of its listings, to tell when to lay out again.
    connection: Option<String>,
    fingerprint: u64,
    source: Option<Box<dyn RelationSource>>,
    described: HashMap<ObjectRef, Described>,
    /// Oldest first, for dropping descriptions past the limit.
    described_order: VecDeque<ObjectRef>,
    relations: Vec<Relation>,
    relation_set: HashSet<Relation>,
    queue: VecDeque<ObjectRef>,
    in_flight: usize,
    pending: Option<Pending>,
    /// The selected object or column, by name, so it survives a new layout.
    selected: Option<Focus>,
    flight: Option<Flight>,
    flight_generation: u64,
    pan: Rc<RefCell<Option<Pan>>>,
    save_generation: u64,
    /// Takes the keyboard on the next frame, after being shown from elsewhere.
    wants_focus: bool,
    _subs: Vec<Subscription>,
}

impl GraphPanel {
    pub fn new(
        state: Entity<AppState>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs = vec![cx.subscribe(&state, |this, _, e, cx| match e {
            StateEvent::Sessions | StateEvent::ActiveSession => this.sync(cx),
            StateEvent::DataChanged {
                connection,
                structure: true,
            } if Some(connection) == this.connection.as_ref() => {
                this.forget_descriptions(cx);
            }
            StateEvent::Settings => cx.notify(),
            _ => {}
        })];
        let mut this = Self {
            state,
            workspace,
            focus: cx.focus_handle(),
            model: Rc::default(),
            connection: None,
            fingerprint: 0,
            source: None,
            described: HashMap::new(),
            described_order: VecDeque::new(),
            relations: Vec::new(),
            relation_set: HashSet::new(),
            queue: VecDeque::new(),
            in_flight: 0,
            pending: None,
            selected: None,
            flight: None,
            flight_generation: 0,
            pan: Rc::default(),
            save_generation: 0,
            wants_focus: true,
            _subs: subs,
        };
        this.sync(cx);
        this
    }

    /// Shows `focus` on `connection`: the place the person was in the regular view.
    pub fn show(&mut self, connection: &str, focus: Focus, cx: &mut Context<Self>) {
        if self.connection.as_deref() != Some(connection) {
            self.state.update(cx, |s, cx| s.set_active(connection, cx));
            self.sync(cx);
        }
        if matches!(focus, Focus::Object { .. } | Focus::Column { .. }) {
            self.selected = Some(focus.clone());
        }
        self.pending = Some(Pending::Focus(focus));
        self.wants_focus = true;
        // Fly from where the camera is, when the graph is already on screen.
        let animate = self.model.borrow().viewport.is_some();
        self.resolve_pending(animate, cx);
        cx.notify();
    }

    /// The connection shown.
    pub fn connection_id(&self) -> Option<&str> {
        self.connection.as_deref()
    }

    /// Saves where the camera is now, without waiting.
    pub fn save_now(&mut self, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        // A view that has not landed yet keeps the saved one.
        if self.pending.is_some() {
            return;
        }
        let view = {
            let m = self.model.borrow();
            if m.layout.is_empty() {
                return;
            }
            m.layout.save(&m.camera, self.selected.clone())
        };
        self.state.update(cx, |s, cx| {
            s.update_settings_quietly(cx, |st| st.graph.remember(&connection, view))
        });
    }

    // ---- following the connection ---------------------------------------------------

    /// Follows the active connection and its listings: switches connection, lists what
    /// is not listed, and lays out again when the listings changed.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let active = self.state.read(cx).active.clone();
        if active != self.connection {
            self.save_now(cx);
            self.connection = active.clone();
            self.fingerprint = 0;
            self.described.clear();
            self.described_order.clear();
            self.relations.clear();
            self.relation_set.clear();
            self.queue.clear();
            self.selected = None;
            self.flight = None;
            let s = self.state.read(cx);
            self.source = s
                .active_session()
                .map(|session| deltos_graph::for_kind(session.kind));
            let saved = active
                .as_deref()
                .and_then(|c| s.settings.graph.view(c))
                .cloned();
            self.selected = saved.as_ref().and_then(|v| v.selected.clone());
            self.pending = Some(saved.map_or(Pending::Focus(Focus::Overview), Pending::Saved));
            let viewport = self.model.borrow().viewport;
            *self.model.borrow_mut() = Model {
                viewport,
                ..Model::default()
            };
        }
        let Some(id) = self.connection.clone() else {
            cx.notify();
            return;
        };
        if self.source.is_none() {
            self.source = self
                .state
                .read(cx)
                .session(&id)
                .map(|session| deltos_graph::for_kind(session.kind));
        }
        // List the namespaces nobody has listed yet, a few at a time.
        let (unlisted, listing) = {
            let s = self.state.read(cx);
            match s.session(&id) {
                Some(session) => (
                    session
                        .namespaces
                        .iter()
                        .filter(|n| n.objects.is_none() && !n.loading && n.error.is_none())
                        .map(|n| n.namespace.path.clone())
                        .collect::<Vec<_>>(),
                    session.namespaces.iter().filter(|n| n.loading).count(),
                ),
                None => (Vec::new(), 0),
            }
        };
        for path in unlisted
            .into_iter()
            .take(LIST_IN_FLIGHT.saturating_sub(listing))
        {
            self.state
                .update(cx, |s, cx| s.load_objects(&id, &path, cx));
        }
        let fingerprint = self.listing_fingerprint(&id, cx);
        if fingerprint != self.fingerprint {
            self.fingerprint = fingerprint;
            self.relayout(cx);
        }
        cx.notify();
    }

    fn listing_fingerprint(&self, id: &str, cx: &App) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        if let Some(session) = self.state.read(cx).session(id) {
            for n in &session.namespaces {
                n.namespace.path.hash(&mut h);
                match &n.objects {
                    Some(list) => {
                        list.len().hash(&mut h);
                        for o in list {
                            o.object.hash(&mut h);
                        }
                    }
                    None => usize::MAX.hash(&mut h),
                }
            }
        }
        // Never 0, which means "not laid out".
        h.finish() | 1
    }

    fn relayout(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.connection.clone() else {
            return;
        };
        let layout = {
            let s = self.state.read(cx);
            let Some(session) = s.session(&id) else {
                return;
            };
            Layout::build(session.namespaces.iter().map(|n| Listing {
                path: &n.namespace.path,
                objects: n.objects.as_deref(),
            }))
        };
        let cards = layout
            .slots
            .iter()
            .map(|s| match self.described.get(&s.object) {
                Some(Described::Ready { card, .. }) => Some(card.clone()),
                _ => None,
            })
            .collect();
        {
            let mut m = self.model.borrow_mut();
            m.layout = Rc::new(layout);
            m.cards = cards;
        }
        self.rebuild_edges();
        self.resolve_selection();
        if self.pending.is_some() {
            self.resolve_pending(false, cx);
        }
    }

    fn forget_descriptions(&mut self, cx: &mut Context<Self>) {
        self.described.clear();
        self.described_order.clear();
        self.relations.clear();
        self.relation_set.clear();
        self.queue.clear();
        {
            let mut m = self.model.borrow_mut();
            let n = m.layout.slots.len();
            m.cards = vec![None; n];
            m.edges.clear();
        }
        cx.notify();
    }

    // ---- the camera -----------------------------------------------------------------

    /// Moves the camera to the pending focus or saved view once the plane can place it.
    fn resolve_pending(&mut self, animate: bool, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.clone() else {
            return;
        };
        let target = {
            let m = self.model.borrow();
            if m.viewport.is_none() || m.layout.is_empty() {
                return;
            }
            let size = m.size();
            match &pending {
                Pending::Focus(focus) => {
                    let row = focus.column().and_then(|c| {
                        let slot = m.layout.slot(focus.object()?)?;
                        self.row_of(&m.layout.slots[slot].object, c)
                    });
                    m.layout.camera_for(focus, size, row)
                }
                Pending::Saved(view) => m.layout.restore(view),
            }
        };
        let Some(target) = target else {
            // Not placed yet. Give up only once every namespace is listed.
            if self.all_listed(cx) {
                self.pending = None;
                if matches!(pending, Pending::Saved(_)) {
                    self.pending = Some(Pending::Focus(Focus::Overview));
                    self.resolve_pending(animate, cx);
                }
            }
            return;
        };
        // A focus on a column waits for the column's row to be known, and the overview
        // for every namespace to be on the plane.
        let waits = match &pending {
            Pending::Focus(Focus::Column { object, .. }) => !matches!(
                self.described.get(object),
                Some(Described::Ready { .. } | Described::Failed)
            ),
            Pending::Focus(Focus::Overview) => !self.all_listed(cx),
            _ => false,
        };
        if !waits {
            self.pending = None;
        }
        self.fly_to(target, animate, cx);
        if let Pending::Focus(f) = &pending
            && let Some(object) = f.object()
            && !self.described.contains_key(object)
        {
            self.queue.push_front(object.clone());
            self.pump(cx);
        }
    }

    fn all_listed(&self, cx: &App) -> bool {
        self.connection
            .as_deref()
            .and_then(|id| self.state.read(cx).session(id))
            .is_some_and(|s| {
                !s.loading
                    && s.namespaces
                        .iter()
                        .all(|n| n.objects.is_some() || n.error.is_some())
            })
    }

    fn fly_to(&mut self, to: Camera, animate: bool, cx: &mut Context<Self>) {
        let (motion_on, speed) = {
            let m = &theme(cx).motion_style;
            (m.on, m.speed)
        };
        let from = self.model.borrow().camera;
        self.flight_generation += 1;
        if !animate || !motion_on || from.same_as(&to) {
            self.flight = None;
            self.model.borrow_mut().camera = to;
            self.schedule_save(cx);
            cx.notify();
            return;
        }
        let duration = FLIGHT.div_f32(speed.max(0.25));
        self.flight = Some(Flight {
            from,
            to,
            start: Instant::now(),
            duration,
        });
        let generation = self.flight_generation;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(8))
                    .await;
                let more = this
                    .update(cx, |this, cx| {
                        if this.flight_generation != generation {
                            return false;
                        }
                        let Some(f) = &this.flight else {
                            return false;
                        };
                        let t = f.start.elapsed().as_secs_f32() / f.duration.as_secs_f32();
                        let eased = ease_in_out(t.min(1.0));
                        let camera = f.from.lerp(&f.to, eased);
                        this.model.borrow_mut().camera = camera;
                        cx.notify();
                        if t >= 1.0 {
                            this.flight = None;
                            this.schedule_save(cx);
                            return false;
                        }
                        true
                    })
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
        })
        .detach();
    }

    /// Stops a flight where it is, for a pan or zoom by hand.
    fn take_camera(&mut self) {
        self.flight = None;
        self.flight_generation += 1;
        // A view still waiting to land would pull the camera away from the person.
        self.pending = None;
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save_generation += 1;
        let generation = self.save_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            this.update(cx, |this, cx| {
                if this.save_generation == generation {
                    this.save_now(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn zoom_by(&mut self, factor: f32, at: Option<(f32, f32)>, cx: &mut Context<Self>) {
        self.take_camera();
        {
            let mut m = self.model.borrow_mut();
            let size = m.size();
            let at = at.unwrap_or((size.0 * 0.5, size.1 * 0.5));
            m.camera.zoom_at(at, factor, size);
        }
        self.schedule_save(cx);
        cx.notify();
    }

    fn pan_by(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
        self.take_camera();
        self.model.borrow_mut().camera.pan(dx, dy);
        self.schedule_save(cx);
        cx.notify();
    }

    fn fit_all(&mut self, cx: &mut Context<Self>) {
        let target = {
            let m = self.model.borrow();
            m.layout.camera_for(&Focus::Overview, m.size(), None)
        };
        if let Some(target) = target {
            self.take_camera();
            self.fly_to(target, true, cx);
        }
    }

    fn fly_to_focus(&mut self, focus: &Focus, cx: &mut Context<Self>) {
        let target = {
            let m = self.model.borrow();
            let row = focus.column().and_then(|c| self.row_of(focus.object()?, c));
            m.layout.camera_for(focus, m.size(), row)
        };
        if let Some(target) = target {
            self.take_camera();
            self.fly_to(target, true, cx);
        }
    }

    // ---- describing what comes into view --------------------------------------------

    /// Queues the descriptions of cards the last frame drew without one.
    fn want(&mut self, slots: Vec<usize>, cx: &mut Context<Self>) {
        let objects: Vec<ObjectRef> = {
            let m = self.model.borrow();
            slots
                .into_iter()
                .filter_map(|s| m.layout.slots.get(s).map(|s| s.object.clone()))
                .collect()
        };
        let mut added = false;
        for o in objects {
            if !self.described.contains_key(&o) && !self.queue.contains(&o) {
                self.queue.push_back(o);
                added = true;
            }
        }
        // Only what is in view now: a long pan should not leave a backlog behind it.
        while self.queue.len() > 200 {
            self.queue.pop_front();
        }
        if added {
            self.pump(cx);
        }
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.connection.clone() else {
            return;
        };
        let Some(conn) = self.state.read(cx).session(&id).map(|s| s.conn.clone()) else {
            return;
        };
        while self.in_flight < DESCRIBE_IN_FLIGHT {
            let Some(object) = self.queue.pop_front() else {
                break;
            };
            if self.described.contains_key(&object) {
                continue;
            }
            self.described.insert(object.clone(), Described::Loading);
            self.in_flight += 1;
            let (conn, o) = (conn.clone(), object.clone());
            let job = cx
                .global::<Net>()
                .run(async move { conn.describe(&o).await });
            let id = id.clone();
            cx.spawn(async move |this, cx| {
                let result = job.await;
                this.update(cx, |this, cx| {
                    this.in_flight = this.in_flight.saturating_sub(1);
                    if this.connection.as_deref() == Some(id.as_str()) {
                        match result {
                            Some(Ok(info)) => this.described_one(object, &info, cx),
                            _ => {
                                this.described.insert(object, Described::Failed);
                            }
                        }
                    }
                    this.pump(cx);
                    if matches!(this.pending, Some(Pending::Focus(Focus::Column { .. }))) {
                        this.resolve_pending(true, cx);
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }

    fn described_one(&mut self, object: ObjectRef, info: &ObjectInfo, _cx: &mut Context<Self>) {
        let new = {
            let m = self.model.borrow();
            match &self.source {
                Some(source) => source.relations(&object, info, m.layout.as_ref()),
                None => Vec::new(),
            }
        };
        let links: HashSet<String> = new
            .iter()
            .filter(|r| r.from == object)
            .flat_map(|r| r.from_columns.iter().cloned())
            .collect();
        let card = Rc::new(Card::new(info, &links));
        let names = info.columns.iter().map(|c| c.name.clone()).collect();
        self.described.insert(
            object.clone(),
            Described::Ready {
                card: card.clone(),
                names,
            },
        );
        self.described_order.push_back(object.clone());
        let mut changed_edges = false;
        for r in new {
            if self.relation_set.insert(r.clone()) {
                self.relations.push(r);
                changed_edges = true;
            }
        }
        {
            let mut m = self.model.borrow_mut();
            if let Some(slot) = m.layout.slot(&object)
                && let Some(c) = m.cards.get_mut(slot)
            {
                *c = Some(card);
            }
        }
        // Rows of lines that end at this card are known now.
        self.rebuild_edges();
        let _ = changed_edges;
        self.resolve_selection();
        while self.described_order.len() > DESCRIBED_KEPT {
            if let Some(old) = self.described_order.pop_front() {
                self.described.remove(&old);
                let mut m = self.model.borrow_mut();
                if let Some(slot) = m.layout.slot(&old)
                    && let Some(c) = m.cards.get_mut(slot)
                {
                    *c = None;
                }
            }
        }
    }

    /// The row of `column` on the card of `object`, when it is described and listed.
    fn row_of(&self, object: &ObjectRef, column: &str) -> Option<usize> {
        match self.described.get(object)? {
            Described::Ready { names, .. } => names
                .iter()
                .position(|n| n == column)
                .filter(|i| *i < MAX_ROWS),
            _ => None,
        }
    }

    fn rebuild_edges(&mut self) {
        let edges = {
            let m = self.model.borrow();
            let layout = &m.layout;
            let mut edges = Vec::with_capacity(self.relations.len());
            for r in &self.relations {
                let (Some(from), Some(to)) = (layout.slot(&r.from), layout.slot(&r.to)) else {
                    continue;
                };
                let row = |object: &ObjectRef, columns: &[String]| {
                    columns.first().and_then(|c| self.row_of(object, c))
                };
                edges.push(Edge {
                    from,
                    from_row: row(&r.from, &r.from_columns),
                    to,
                    to_row: row(&r.to, &r.to_columns),
                    inferred: r.inferred,
                });
            }
            edges
        };
        self.model.borrow_mut().edges = edges;
    }

    fn resolve_selection(&mut self) {
        let selected = self.selected.as_ref().and_then(|f| {
            let m = self.model.borrow();
            let slot = m.layout.slot(f.object()?)?;
            Some((slot, f.column().and_then(|c| self.row_of(f.object()?, c))))
        });
        self.model.borrow_mut().selected = selected;
    }

    // ---- pointer --------------------------------------------------------------------

    fn local(&self, p: Point<Pixels>) -> (f32, f32) {
        let origin = self
            .model
            .borrow()
            .viewport
            .map_or(point(px(0.), px(0.)), |b| b.origin);
        ((p.x - origin.x).into(), (p.y - origin.y).into())
    }

    fn press(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        *self.pan.borrow_mut() = Some(Pan {
            start: e.position,
            last: e.position,
            moved: false,
            clicks: e.click_count,
        });
        cx.notify();
    }

    fn drag_to(&mut self, p: Point<Pixels>, cx: &mut Context<Self>) {
        let delta = {
            let mut pan = self.pan.borrow_mut();
            let Some(pan) = pan.as_mut() else {
                return;
            };
            if !pan.moved {
                let d = p - pan.start;
                if f32::from(d.x).hypot(f32::from(d.y)) < DRAG_SLOP {
                    return;
                }
                pan.moved = true;
            }
            let d = p - pan.last;
            pan.last = p;
            (f32::from(d.x), f32::from(d.y))
        };
        self.pan_by(delta.0, delta.1, cx);
    }

    fn release(&mut self, p: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(pan) = self.pan.borrow_mut().take() else {
            return;
        };
        if !pan.moved {
            self.click(self.local(p), pan.clicks, cx);
        }
        cx.notify();
    }

    fn click(&mut self, local: (f32, f32), clicks: usize, cx: &mut Context<Self>) {
        let (hit, detail) = {
            let m = self.model.borrow();
            (m.hit(local), m.camera.detail())
        };
        match hit {
            Hit::Nothing => self.select(None, cx),
            Hit::Region(ri) => {
                self.select(None, cx);
                if detail <= Detail::Names || clicks >= 2 {
                    let path = self.model.borrow().layout.regions[ri].path.clone();
                    self.fly_to_focus(&Focus::Namespace { path }, cx);
                }
            }
            Hit::Node(slot, part) => {
                let object = self.model.borrow().layout.slots[slot].object.clone();
                if clicks >= 2 {
                    self.open_in_regular_view(&object, cx);
                    return;
                }
                match part {
                    NodeHit::Column(row) => self.click_column(object, row, cx),
                    NodeHit::Header | NodeHit::More => {
                        let focus = Focus::Object { object };
                        self.select(Some(focus.clone()), cx);
                        if detail <= Detail::Names || part == NodeHit::More {
                            self.fly_to_focus(&focus, cx);
                        }
                    }
                }
            }
        }
    }

    /// Selects a column. A column that refers to another object flies there.
    fn click_column(&mut self, object: ObjectRef, row: usize, cx: &mut Context<Self>) {
        let Some(Described::Ready { names, .. }) = self.described.get(&object) else {
            self.select(Some(Focus::Object { object }), cx);
            return;
        };
        let Some(column) = names.get(row).cloned() else {
            return;
        };
        let target = self
            .relations
            .iter()
            .find(|r| r.from == object && r.from_columns.first() == Some(&column))
            .map(|r| Focus::Column {
                object: r.to.clone(),
                column: r.to_columns.first().cloned().unwrap_or_default(),
            });
        let here = Focus::Column { object, column };
        let already = self.selected.as_ref() == Some(&here);
        match target {
            // A second click on a linked column follows it.
            Some(target) if already => {
                self.select(Some(target.clone()), cx);
                self.fly_to_focus(&target, cx);
            }
            _ => self.select(Some(here), cx),
        }
    }

    fn select(&mut self, focus: Option<Focus>, cx: &mut Context<Self>) {
        self.selected = focus;
        self.resolve_selection();
        self.schedule_save(cx);
        cx.notify();
    }

    fn open_in_regular_view(&mut self, object: &ObjectRef, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        self.save_now(cx);
        let kind = deltos_settings::PanelKind::Table {
            connection,
            object: object.clone(),
        };
        self.workspace
            .update(cx, |w, cx| w.open(kind, true, cx))
            .ok();
    }

    fn scroll(&mut self, e: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let at = self.local(e.position);
        match e.delta {
            // A mouse wheel zooms, as in every map.
            ScrollDelta::Lines(d) => {
                let steps = if d.y != 0.0 { d.y } else { d.x };
                self.zoom_by(1.18f32.powf(steps.clamp(-6.0, 6.0)), Some(at), cx);
            }
            // A touchpad pans, and zooms with Ctrl held (which is also how pinches arrive).
            ScrollDelta::Pixels(d) => {
                let (dx, dy) = (f32::from(d.x), f32::from(d.y));
                if e.modifiers.control || e.modifiers.platform {
                    self.zoom_by((dy / 160.0).clamp(-1.0, 1.0).exp(), Some(at), cx);
                } else {
                    self.pan_by(dx, dy, cx);
                }
            }
        }
    }

    fn key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let k = &e.keystroke;
        if k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
            return false;
        }
        let step = 80.0;
        match k.key.as_str() {
            "escape" | "backspace" => self.back(window, cx),
            "=" | "+" => self.zoom_by(1.3, None, cx),
            "-" => self.zoom_by(1.0 / 1.3, None, cx),
            "0" => self.fit_all(cx),
            "left" => self.pan_by(step, 0.0, cx),
            "right" => self.pan_by(-step, 0.0, cx),
            "up" => self.pan_by(0.0, step, cx),
            "down" => self.pan_by(0.0, -step, cx),
            "enter" => {
                if let Some(o) = self.selected.as_ref().and_then(Focus::object).cloned() {
                    self.open_in_regular_view(&o, cx);
                }
            }
            "f" => {
                if let Some(f) = self.selected.clone() {
                    self.fly_to_focus(&f, cx);
                }
            }
            _ => return false,
        }
        true
    }

    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_now(cx);
        let ws = self.workspace.clone();
        // Not inside this panel's own update: going back may close it. The keyboard
        // goes back with the person, so the shortcuts keep working there.
        window.defer(cx, move |window, cx| {
            let Some(Some(panel)) = ws.update(cx, |w, cx| w.graph_back(cx)).ok() else {
                return;
            };
            let focus = ws
                .upgrade()
                .and_then(|w| w.read(cx).view(panel).and_then(|v| v.focus_handle(cx)));
            if let Some(f) = focus {
                window.focus(&f);
            }
        });
    }

    // ---- drawing --------------------------------------------------------------------

    fn header(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let s = self.state.read(cx);
        let back_to = self
            .workspace
            .upgrade()
            .and_then(|w| w.read(cx).graph_return_title(cx));
        let session = self.connection.as_deref().and_then(|id| s.session(id));
        let mut crumbs: Vec<(SharedString, Option<Focus>)> = Vec::new();
        if let Some(session) = session {
            crumbs.push((session.name.clone().into(), Some(Focus::Overview)));
        }
        if let Some(f) = &self.selected {
            if let Some(ns) = f.namespace() {
                crumbs.push((
                    ns.join(".").into(),
                    Some(Focus::Namespace { path: ns.to_vec() }),
                ));
            }
            if let Some(o) = f.object() {
                crumbs.push((
                    o.name.clone().into(),
                    Some(Focus::Object { object: o.clone() }),
                ));
            }
            if let Some(c) = f.column() {
                crumbs.push((c.to_owned().into(), Some(f.clone())));
            }
        }
        let zoom = self.model.borrow().camera.zoom;
        let weak = cx.entity().downgrade();
        let mut trail = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(4.))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(t.small_size);
        let last = crumbs.len().saturating_sub(1);
        for (i, (label, focus)) in crumbs.into_iter().enumerate() {
            if i > 0 {
                trail = trail.child(
                    icon(Icon::ChevronRight)
                        .size(px(10.))
                        .text_color(t.text_faint),
                );
            }
            let w = weak.clone();
            trail = trail.child(
                div()
                    .id(("graph-crumb", i))
                    .px(px(4.))
                    .rounded(t.radius_small)
                    .cursor_pointer()
                    .text_color(if i == last { t.text } else { t.text_muted })
                    .hover(|s| s.bg(t.hover))
                    .child(label)
                    .on_click(move |_, _, cx| {
                        if let Some(f) = focus.clone() {
                            w.update(cx, |p, cx| p.fly_to_focus(&f, cx)).ok();
                        }
                    }),
            );
        }
        let w = weak.clone();
        let back = button(
            "graph-back",
            back_to.map_or_else(|| "Back".to_owned(), |t| format!("Back to {t}")),
            ButtonStyle::Ghost,
            t,
        )
        .h(t.s(22.))
        .px(px(6.))
        .text_size(t.small_size)
        .tooltip(|_, cx| Tooltip::view("Back to the regular view (Esc)", cx))
        .on_click(move |_, window, cx| {
            w.update(cx, |p, cx| p.back(window, cx)).ok();
        });
        let (w1, w2, w3) = (weak.clone(), weak.clone(), weak);
        div()
            .flex_none()
            .h(t.bar_height)
            .px(px(6.))
            .flex()
            .items_center()
            .gap(px(6.))
            .bg(t.base)
            .border_b_1()
            .border_color(t.border)
            .child(back)
            .child(div().w(px(1.)).h(px(14.)).bg(t.border))
            .child(trail)
            .child(
                div()
                    .flex_none()
                    .w(px(40.))
                    .text_size(t.small_size)
                    .text_color(t.text_faint)
                    .text_right()
                    .child(format!("{:.0}%", zoom * 100.0)),
            )
            .child(
                icon_button("graph-zoom-out", Icon::Minimize, t)
                    .tooltip(|_, cx| Tooltip::view("Zoom out (-)", cx))
                    .on_click(move |_, _, cx| {
                        w1.update(cx, |p, cx| p.zoom_by(1.0 / 1.3, None, cx)).ok();
                    }),
            )
            .child(
                icon_button("graph-zoom-in", Icon::Plus, t)
                    .tooltip(|_, cx| Tooltip::view("Zoom in (+)", cx))
                    .on_click(move |_, _, cx| {
                        w2.update(cx, |p, cx| p.zoom_by(1.3, None, cx)).ok();
                    }),
            )
            .child(
                icon_button("graph-fit", Icon::Maximize, t)
                    .tooltip(|_, cx| Tooltip::view("Show everything (0)", cx))
                    .on_click(move |_, _, cx| {
                        w3.update(cx, |p, cx| p.fit_all(cx)).ok();
                    }),
            )
    }

    fn canvas(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.clone();
        let paint_model = self.model.clone();
        let pan = self.pan.clone();
        let weak = cx.entity().downgrade();
        let weak_paint = weak.clone();
        let t = t.clone();
        canvas(
            move |bounds, _, cx| {
                let resized = {
                    let mut m = model.borrow_mut();
                    let old = m.viewport.replace(bounds);
                    old.map(|o| o.size) != Some(bounds.size)
                };
                if resized {
                    let w = weak.clone();
                    cx.defer(move |cx| {
                        w.update(cx, |p, cx| {
                            p.resolve_pending(false, cx);
                            cx.notify();
                        })
                        .ok();
                    });
                }
            },
            move |bounds, (), window, cx| {
                let missing = paint(&paint_model.borrow(), bounds, &t, window, cx);
                if !missing.is_empty() {
                    let w = weak_paint.clone();
                    cx.defer(move |cx| {
                        w.update(cx, |p, cx| p.want(missing, cx)).ok();
                    });
                }
                // While a press lasts, follow the pointer past the panel and the window.
                if pan.borrow().is_some() {
                    let w = weak_paint.clone();
                    window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                        if phase != DispatchPhase::Bubble {
                            return;
                        }
                        let p = e.position;
                        let held = e.pressed_button == Some(MouseButton::Left);
                        w.update(cx, |g, cx| {
                            if held {
                                g.drag_to(p, cx);
                            } else {
                                g.release(p, cx);
                            }
                        })
                        .ok();
                    });
                    let w = weak_paint.clone();
                    window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                        if phase != DispatchPhase::Capture || e.button != MouseButton::Left {
                            return;
                        }
                        let p = e.position;
                        w.update(cx, |g, cx| g.release(p, cx)).ok();
                    });
                }
            },
        )
        .size_full()
    }
}

fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Rounded to half pixels, so the same text at nearly the same zoom shares one layout.
fn text_size(world: f32, zoom: f32) -> f32 {
    (world * zoom * 2.0).round() / 2.0
}

struct Painter<'a> {
    origin: Point<Pixels>,
    camera: Camera,
    viewport: (f32, f32),
    t: &'a Theme,
}

impl Painter<'_> {
    fn point(&self, world: (f32, f32)) -> Point<Pixels> {
        let (x, y) = self.camera.to_screen(world, self.viewport);
        point(self.origin.x + px(x), self.origin.y + px(y))
    }

    fn bounds(&self, r: &Rect) -> Bounds<Pixels> {
        let s = self.camera.rect_to_screen(r, self.viewport);
        Bounds {
            origin: point(self.origin.x + px(s.x), self.origin.y + px(s.y)),
            size: size(px(s.w), px(s.h)),
        }
    }

    fn text(
        &self,
        text: SharedString,
        font_size: f32,
        weight: FontWeight,
        color: Hsla,
        at: Point<Pixels>,
        max_width: f32,
        window: &mut Window,
        cx: &mut App,
    ) -> f32 {
        if font_size < MIN_TEXT || max_width <= 0.0 || text.is_empty() {
            return 0.0;
        }
        let mut font = gpui::font(self.t.ui_font.clone());
        font.weight = weight;
        let run = TextRun {
            len: text.len(),
            font,
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(text, px(font_size), &[run], None);
        let line_height = px(font_size * 1.3);
        let clip = Bounds {
            origin: point(at.x, at.y - px(2.)),
            size: size(px(max_width), line_height + px(4.)),
        };
        window.with_content_mask(Some(gpui::ContentMask { bounds: clip }), |window| {
            let _ = line.paint(at, line_height, window, cx);
        });
        f32::from(line.width).min(max_width)
    }
}

/// Draws the plane. Returns the cards in view that have no description yet, when the
/// zoom shows enough of them to be worth describing.
fn paint(
    m: &Model,
    bounds: Bounds<Pixels>,
    t: &Theme,
    window: &mut Window,
    cx: &mut App,
) -> Vec<usize> {
    let viewport: (f32, f32) = (bounds.size.width.into(), bounds.size.height.into());
    let p = Painter {
        origin: bounds.origin,
        camera: m.camera,
        viewport,
        t,
    };
    let zoom = m.camera.zoom;
    let detail = m.camera.detail();
    let view = m.camera.visible(viewport);
    let layout = &m.layout;
    let mut missing = Vec::new();
    window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
        window.paint_quad(fill(bounds, t.base));
        paint_dots(&p, &view, window);

        // Namespaces.
        let region_radius = px((14.0 * zoom).clamp(2.0, 14.0));
        for r in layout.regions.iter().filter(|r| r.rect.intersects(&view)) {
            window.paint_quad(quad(
                p.bounds(&r.rect),
                region_radius,
                t.sidebar.opacity(0.6),
                px(1.),
                t.border,
                BorderStyle::Solid,
            ));
        }

        // Lines, under the cards.
        let selected = m.selected.map(|(s, _)| s);
        let mut drawn = 0;
        for e in &m.edges {
            if drawn >= MAX_EDGES {
                break;
            }
            let (a, b) = (&layout.slots[e.from], &layout.slots[e.to]);
            let ya = layout.column_y(e.from, e.from_row);
            let yb = layout.column_y(e.to, e.to_row);
            // Leave from the side facing the target; a line between cards stacked in one
            // column, or from a card to itself, loops out to the right.
            let (xa, xb, da, db) = if b.x > a.x + NODE_W {
                (a.x + NODE_W, b.x, 1.0, -1.0)
            } else if b.x + NODE_W < a.x {
                (a.x, b.x + NODE_W, -1.0, 1.0)
            } else {
                (a.x + NODE_W, b.x + NODE_W, 1.0, 1.0)
            };
            let reach = ((xb - xa).abs() * 0.45).max(60.0);
            let lo = (xa.min(xb) - reach, ya.min(yb));
            let hi = (xa.max(xb) + reach, ya.max(yb));
            if !Rect::new(lo.0, lo.1, hi.0 - lo.0, (hi.1 - lo.1).max(1.0)).intersects(&view) {
                continue;
            }
            let lit = selected.is_some_and(|s| s == e.from || s == e.to);
            let width = if lit { 2.0 } else { 1.25 };
            let mut path = PathBuilder::stroke(px(width));
            if e.inferred {
                path = path.dash_array(&[px(5.), px(4.)]);
            }
            path.move_to(p.point((xa, ya)));
            path.cubic_bezier_to(
                p.point((xb, yb)),
                p.point((xa + da * reach, ya)),
                p.point((xb + db * reach, yb)),
            );
            if let Ok(path) = path.build() {
                let color = if lit {
                    t.accent
                } else if selected.is_some() {
                    t.text_faint.opacity(0.25)
                } else {
                    t.text_faint.opacity(0.6)
                };
                window.paint_path(path, color);
            }
            // A dot where the line arrives.
            let end = p.point((xb, yb));
            let r = (3.0 * zoom).clamp(1.5, 3.5);
            window.paint_quad(
                fill(
                    Bounds {
                        origin: point(end.x - px(r), end.y - px(r)),
                        size: size(px(r * 2.0), px(r * 2.0)),
                    },
                    if lit { t.accent } else { t.text_faint },
                )
                .corner_radii(px(r)),
            );
            drawn += 1;
        }

        // Cards.
        let visible = layout.visible(&view);
        let describe = detail >= Detail::Columns || visible.len() <= DESCRIBE_WHEN_FEW;
        let radius = px((6.0 * zoom).clamp(1.0, 6.0));
        // Names stay readable while the cards are wide enough to hold a few letters.
        let name_size = text_size(13.0, zoom).max(10.0);
        let row_size = text_size(11.5, zoom);
        for &slot in &visible {
            let s = &layout.slots[slot];
            let card = m.cards.get(slot).and_then(Option::as_ref);
            if card.is_none() && describe {
                missing.push(slot);
            }
            let height = card.map_or(node_height(None), |c| c.height());
            let rect = s.rect(height);
            if !rect.intersects(&view) {
                continue;
            }
            let b = p.bounds(&rect);
            let is_selected = selected == Some(slot);
            let stripe = match s.object.kind {
                ObjectKind::View | ObjectKind::MaterializedView => t.syntax_string,
                ObjectKind::Collection => t.syntax_number,
                _ => t.accent,
            };
            window.paint_quad(quad(
                b,
                radius,
                t.surface,
                px(if is_selected { 2. } else { 1. }),
                if is_selected { t.accent } else { t.border },
                BorderStyle::Solid,
            ));
            if detail == Detail::Outline {
                // A colored bar for the kind, readable as a pattern from far out.
                let bar = Rect::new(rect.x, rect.y, NODE_W, HEADER_H);
                window.paint_quad(fill(p.bounds(&bar), stripe.opacity(0.35)).corner_radii(radius));
                continue;
            }
            let header = p.bounds(&Rect::new(rect.x, rect.y, NODE_W, HEADER_H));
            window.paint_quad(fill(header, t.grid_header).corner_radii(gpui::Corners {
                top_left: radius,
                top_right: radius,
                bottom_left: px(0.),
                bottom_right: px(0.),
            }));
            window.paint_quad(fill(
                Bounds {
                    origin: header.origin,
                    size: size(px((3.0 * zoom).max(1.0)), header.size.height),
                },
                stripe,
            ));
            let pad = 10.0 * zoom;
            let text_y = |world_y: f32, size: f32| {
                let y = p.point((0.0, world_y)).y;
                y - px(size * 0.65)
            };
            let count = s.rows.map(compact_count);
            let count_w = match &count {
                Some(c) if detail >= Detail::Columns => {
                    let w = c.len() as f32 * row_size * 0.62;
                    p.text(
                        c.clone().into(),
                        row_size,
                        FontWeight::NORMAL,
                        t.text_faint,
                        point(
                            header.right() - px(pad + w),
                            text_y(rect.y + HEADER_H * 0.5, row_size),
                        ),
                        w + 2.0,
                        window,
                        cx,
                    ) + 6.0 * zoom
                }
                _ => 0.0,
            };
            p.text(
                s.object.name.clone().into(),
                name_size,
                FontWeight::SEMIBOLD,
                t.text,
                point(
                    header.left() + px(pad),
                    text_y(rect.y + HEADER_H * 0.5, name_size),
                ),
                f32::from(header.size.width) - pad * 2.0 - count_w,
                window,
                cx,
            );
            if detail < Detail::Columns {
                // Too small for column names: a bar per column, as long as its name, so
                // the card still reads as the table it is.
                if let Some(card) = card {
                    for (i, c) in card.columns.iter().enumerate() {
                        let y = rect.y + HEADER_H + i as f32 * ROW_H + ROW_H * 0.3;
                        let w = (c.name.len() as f32 * 7.0).clamp(16.0, NODE_W - 30.0);
                        window.paint_quad(fill(
                            p.bounds(&Rect::new(rect.x + 19.0, y, w, ROW_H * 0.4)),
                            if c.key { t.text_muted } else { t.hover },
                        ));
                    }
                }
                continue;
            }
            match card {
                None => {
                    // Not described yet: quiet bars where the columns will be.
                    for i in 0..3 {
                        let y = rect.y + HEADER_H + i as f32 * ROW_H + ROW_H * 0.3;
                        let w = [0.62, 0.45, 0.54][i];
                        window.paint_quad(
                            fill(
                                p.bounds(&Rect::new(
                                    rect.x + 10.0,
                                    y,
                                    (NODE_W - 20.0) * w,
                                    ROW_H * 0.4,
                                )),
                                t.hover,
                            )
                            .corner_radii(px(3.0 * zoom)),
                        );
                    }
                }
                Some(card) => {
                    let selected_row = m.selected.filter(|(s, _)| *s == slot).and_then(|(_, r)| r);
                    for (i, c) in card.columns.iter().enumerate() {
                        let y = rect.y + HEADER_H + i as f32 * ROW_H;
                        if selected_row == Some(i) {
                            window.paint_quad(fill(
                                p.bounds(&Rect::new(rect.x + 1.0, y, NODE_W - 2.0, ROW_H)),
                                t.accent_soft,
                            ));
                        }
                        let mid = y + ROW_H * 0.5;
                        let mut x = rect.x + 10.0;
                        if c.key || c.link {
                            let r = 2.5;
                            window.paint_quad(
                                fill(
                                    p.bounds(&Rect::new(x, mid - r, r * 2.0, r * 2.0)),
                                    if c.key { t.key } else { t.syntax_keyword },
                                )
                                .corner_radii(px(r * zoom)),
                            );
                        }
                        x += 9.0;
                        let type_w = if detail >= Detail::Types {
                            let w = (c.ty.len() as f32 * row_size * 0.6).min(NODE_W * 0.4 * zoom);
                            let right = p.point((rect.x + NODE_W - 10.0, 0.0)).x;
                            p.text(
                                c.ty.clone(),
                                row_size,
                                FontWeight::NORMAL,
                                t.text_faint,
                                point(right - px(w), text_y(mid, row_size)),
                                w + 2.0,
                                window,
                                cx,
                            );
                            w / zoom + 8.0
                        } else {
                            0.0
                        };
                        p.text(
                            c.name.clone(),
                            row_size,
                            FontWeight::NORMAL,
                            if c.key { t.text } else { t.text_muted },
                            point(p.point((x, 0.0)).x, text_y(mid, row_size)),
                            (rect.x + NODE_W - 10.0 - type_w - x) * zoom,
                            window,
                            cx,
                        );
                    }
                    if card.total > MAX_ROWS {
                        let y = rect.y + HEADER_H + MAX_ROWS as f32 * ROW_H + FOOTER_H * 0.5;
                        p.text(
                            format!("{} more", card.total - MAX_ROWS).into(),
                            row_size,
                            FontWeight::NORMAL,
                            t.text_faint,
                            point(p.point((rect.x + 19.0, 0.0)).x, text_y(y, row_size)),
                            (NODE_W - 30.0) * zoom,
                            window,
                            cx,
                        );
                    }
                }
            }
        }

        // Namespace names last, at a size that stays readable from far out.
        let label = (22.0 * zoom).clamp(11.0, 20.0);
        for r in layout.regions.iter().filter(|r| r.rect.intersects(&view)) {
            let b = p.bounds(&r.rect);
            let at = point(b.left() + px(14.0), b.top() + px((18.0 * zoom).max(6.0)));
            let w = p.text(
                r.label.clone().into(),
                label,
                FontWeight::SEMIBOLD,
                t.text_muted,
                at,
                f32::from(b.size.width).max(240.0),
                window,
                cx,
            );
            let note = if !r.listed {
                "Loading…".to_owned()
            } else {
                match r.len {
                    0 => "empty".to_owned(),
                    1 => "1 object".to_owned(),
                    n => format!("{n} objects"),
                }
            };
            p.text(
                note.into(),
                (label * 0.72).max(10.0),
                FontWeight::NORMAL,
                t.text_faint,
                point(at.x + px(w + 10.0), at.y + px(label * 0.2)),
                200.0,
                window,
                cx,
            );
        }
    });
    missing
}

/// A dot grid behind everything, its spacing doubling as the camera pulls back so it
/// always reads as the same texture.
fn paint_dots(p: &Painter, view: &Rect, window: &mut Window) {
    let zoom = p.camera.zoom;
    let mut spacing = 40.0f32;
    while spacing * zoom < 26.0 {
        spacing *= 2.0;
    }
    let x0 = (view.x / spacing).floor() as i64;
    let y0 = (view.y / spacing).floor() as i64;
    let nx = (view.w / spacing).ceil() as i64 + 1;
    let ny = (view.h / spacing).ceil() as i64 + 1;
    if nx * ny > 8_000 {
        return;
    }
    let color = p.t.border.opacity(0.7);
    for j in 0..ny {
        for i in 0..nx {
            let at = p.point(((x0 + i) as f32 * spacing, (y0 + j) as f32 * spacing));
            window.paint_quad(fill(
                Bounds {
                    origin: point(at.x - px(0.75), at.y - px(0.75)),
                    size: size(px(1.5), px(1.5)),
                },
                color,
            ));
        }
    }
}

impl Focusable for GraphPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for GraphPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.wants_focus) {
            window.focus(&self.focus);
        }
        let t = theme(cx).clone();
        let base = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.base)
            .key_context("GraphPanel")
            .track_focus(&self.focus);
        let shown = self
            .connection
            .as_deref()
            .and_then(|id| self.state.read(cx).session(id))
            .map(|session| {
                (
                    session.error.clone(),
                    session.namespaces.is_empty() && !session.loading,
                )
            });
        let Some((error, empty)) = shown else {
            let state = self.state.clone();
            return base
                .child(self.header(&t, cx))
                .child(
                    empty_state(
                        "No connection",
                        Some("Open one to see its tables and how they relate.".into()),
                        &t,
                    )
                    .child(
                        button(
                            "graph-connections",
                            "Connections",
                            ButtonStyle::Secondary,
                            &t,
                        )
                        .mt(px(8.))
                        .on_click(move |_, _, cx| {
                            state.update(cx, |s, cx| {
                                s.open(deltos_settings::PanelKind::Connections, true, cx)
                            })
                        }),
                    ),
                )
                .into_any_element();
        };
        let body = div()
            .id("graph-canvas")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .cursor_grab()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, window, cx| this.press(e, window, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, e: &MouseUpEvent, _, cx| this.release(e.position, cx)),
            )
            .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| this.scroll(e, cx)))
            .child(self.canvas(&t, cx))
            .when_some(error, |el, e| {
                el.child(
                    div()
                        .absolute()
                        .bottom(px(10.))
                        .left(px(10.))
                        .text_size(t.small_size)
                        .text_color(t.danger)
                        .child(e),
                )
            })
            .when(empty, |el| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(empty_state("Nothing to show", None, &t)),
                )
            });
        base.on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
            if this.key(e, window, cx) {
                cx.stop_propagation();
            }
        }))
        .child(self.header(&t, cx))
        .child(body)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_and_text_sizes() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-6);
        assert_eq!(ease_in_out(2.0), 1.0);
        assert_eq!(text_size(13.0, 1.0), 13.0);
        assert_eq!(text_size(13.0, 0.51), 6.5);
        assert_eq!(text_size(13.0, 0.53), 7.0);
    }
}
