//! The workspace: every open panel, the dock layout of every window, and the moves
//! between them.
//!
//! Panels are entities created once and kept here by [`PanelId`]. A window draws its
//! dock tree with a [`DockView`], which renders whichever panel entities its tree holds,
//! so a panel moved to another window is the same entity with its scroll position,
//! selection and loaded rows intact.
//!
//! Moving panels:
//!
//! - **Drag a tab or a panel header** (left button, past a few pixels of slop). The stack
//!   under the pointer shows where it would land: a tab position in the strip, the
//!   middle to join as a tab, or an edge to split. A strip along the outside of the
//!   window docks beside everything.
//! - **Drag it past the window's edge** and it opens in a window of its own under the
//!   pointer at once; on X11 the drag carries on as a move of that new window.
//! - **Move a panel window over the main window** and the main window shows where it
//!   would dock; when the window stops there, it rejoins. Its Dock button and closing it
//!   also put its panels back where they came from. Wayland does not tell clients where
//!   their windows are, so there only the button and closing dock.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use deltos_dock::{DropTarget, Edge, Node, PanelId, Rect, TreeId, WindowId, Zone};
use deltos_settings::{PanelKind, SavedLayout, SavedPanel};
use gpui::{
    AnyView, App, AppContext, Bounds, Context, Entity, Pixels, Point, SharedString, Styled,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions, point, px,
    size,
};

use crate::state::{AppState, StateEvent};
use crate::ui::connections::ConnectionsPanel;
use crate::ui::history::HistoryPanel;
use crate::ui::icons::Icon;
use crate::ui::inspector::InspectorPanel;
use crate::ui::panel_window::PanelWindow;
use crate::ui::query::QueryPanel;
use crate::ui::settings::SettingsPanel;
use crate::ui::sidebar::ObjectsPanel;
use crate::ui::table::TablePanel;

/// How far the pointer moves with the button down before a press becomes a drag.
const DRAG_SLOP: f32 = 5.0;
/// How far past the window's edge a dragged panel pops out.
const POP_OUT_MARGIN: f32 = 10.0;
/// How long a panel window must rest over the main window before it docks.
const DOCK_DWELL: Duration = Duration::from_millis(450);

/// A panel entity, by what it is, so the dock can ask for its title and icon.
#[derive(Clone)]
pub enum PanelView {
    Connections(Entity<ConnectionsPanel>),
    Objects(Entity<ObjectsPanel>),
    Inspector(Entity<InspectorPanel>),
    History(Entity<HistoryPanel>),
    Table(Entity<TablePanel>),
    Query(Entity<QueryPanel>),
    Settings(Entity<SettingsPanel>),
}

impl PanelView {
    pub fn view(&self) -> AnyView {
        let full = gpui::StyleRefinement::default().size_full();
        match self {
            PanelView::Connections(v) => AnyView::from(v.clone()).cached(full),
            PanelView::Objects(v) => AnyView::from(v.clone()).cached(full),
            PanelView::Inspector(v) => AnyView::from(v.clone()).cached(full),
            PanelView::History(v) => AnyView::from(v.clone()).cached(full),
            PanelView::Table(v) => AnyView::from(v.clone()).cached(full),
            PanelView::Query(v) => AnyView::from(v.clone()).cached(full),
            PanelView::Settings(v) => AnyView::from(v.clone()).cached(full),
        }
    }

    pub fn title(&self, cx: &App) -> SharedString {
        match self {
            PanelView::Connections(_) => "Connections".into(),
            PanelView::Objects(_) => "Objects".into(),
            PanelView::Inspector(_) => "Inspector".into(),
            PanelView::History(_) => "History".into(),
            PanelView::Table(v) => v.read(cx).title(),
            PanelView::Query(v) => v.read(cx).title(),
            PanelView::Settings(_) => "Settings".into(),
        }
    }

    pub fn icon(&self, cx: &App) -> Icon {
        match self {
            PanelView::Connections(_) => Icon::Plug,
            PanelView::Objects(_) => Icon::Database,
            PanelView::Inspector(_) => Icon::SidebarRight,
            PanelView::History(_) => Icon::History,
            PanelView::Table(v) => v.read(cx).icon(),
            PanelView::Query(_) => Icon::Code,
            PanelView::Settings(_) => Icon::Settings,
        }
    }

    /// Tool panels draw a quiet header when alone in their stack instead of a tab.
    pub fn is_tool(&self) -> bool {
        matches!(
            self,
            PanelView::Objects(_) | PanelView::Inspector(_) | PanelView::History(_)
        )
    }

    /// Panels that open as documents: tables, queries, settings, the connection list.
    pub fn is_document(&self) -> bool {
        !self.is_tool()
    }

    /// The background behind the panel, so a stack's frame matches it.
    pub fn is_sidebar(&self) -> bool {
        self.is_tool()
    }
}

/// A tab or header being dragged.
#[derive(Debug, Clone)]
pub struct Drag {
    pub panel: PanelId,
    /// The tree of the window the press happened in; its window receives the moves.
    pub source: TreeId,
    pub origin: Point<Pixels>,
    pub cursor: Point<Pixels>,
    pub active: bool,
    pub hover: Option<Hover>,
    /// Smoothed pointer speed in pixels per move, and when the pointer last moved, for
    /// the ghost's stretch.
    pub velocity: (f32, f32),
    pub moved_at: Instant,
}

/// Where a drop would land, and the area to highlight in that window.
#[derive(Debug, Clone, PartialEq)]
pub struct Hover {
    pub tree: TreeId,
    pub target: DropTarget,
    pub highlight: Bounds<Pixels>,
    /// A tab insertion shows as a caret rather than an area.
    pub caret: bool,
}

/// A split divider being dragged.
#[derive(Debug, Clone)]
pub struct SplitResize {
    pub tree: TreeId,
    pub path: Vec<usize>,
    pub handle: usize,
    pub row: bool,
    pub start: Point<Pixels>,
    pub length: f32,
    pub start_sizes: Vec<f32>,
}

/// A panel window being moved by the system, watched to offer docking.
#[derive(Debug, Clone)]
struct WindowMove {
    window: WindowId,
    /// The pointer's position inside the moving window when the move began.
    grab: Point<Pixels>,
    start: Point<Pixels>,
    generation: u64,
}

/// Geometry of one window's dock, recorded as it paints, for hit testing drags.
#[derive(Debug, Default, Clone)]
pub struct TreeZones {
    pub root: Option<Bounds<Pixels>>,
    pub stacks: Vec<StackZone>,
    pub tabs: HashMap<PanelId, Bounds<Pixels>>,
    pub splits: HashMap<Vec<usize>, Bounds<Pixels>>,
}

#[derive(Debug, Clone)]
pub struct StackZone {
    pub anchor: PanelId,
    pub panels: Vec<PanelId>,
    pub strip: Bounds<Pixels>,
    pub content: Bounds<Pixels>,
}

pub type Zones = Rc<RefCell<HashMap<TreeId, TreeZones>>>;

pub struct Workspace {
    pub state: Entity<AppState>,
    pub layout: SavedLayout,
    views: HashMap<PanelId, PanelView>,
    pub drag: Option<Drag>,
    pub resize: Option<SplitResize>,
    pub zones: Zones,
    floating: HashMap<WindowId, WindowHandle<PanelWindow>>,
    /// The main window's bounds on screen.
    pub main_bounds: Option<Bounds<Pixels>>,
    window_move: Option<WindowMove>,
    /// A panel window held over the main window, and where it would dock.
    pub redock: Option<(WindowId, Hover)>,
    /// Where each popped-out panel came from, so docking puts it back there.
    homes: HashMap<PanelId, DropTarget>,
    /// The panel that last landed in a stack, and a count that changes with every
    /// landing, so its settle animation plays once per landing.
    pub landed: Option<(PanelId, u64)>,
    /// The table tab that the next table opened from the sidebar replaces, as in a
    /// file browser's preview. Opening it for good, or editing it, keeps it.
    preview: Option<PanelId>,
    /// The document panel shown last, which new documents open beside.
    last_document: Option<PanelId>,
    generation: u64,
    _subs: Vec<Subscription>,
}

/// Whether window positions on screen are known: not on Wayland.
pub fn screen_positions_known() -> bool {
    !(cfg!(target_os = "linux")
        && std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()))
}

impl Workspace {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut layout = state.read(cx).settings.layout.clone();
        layout.sanitize();
        let subs = vec![cx.subscribe(&state, |this, _, event, cx| {
            if let StateEvent::Open { kind, pin } = event {
                this.open(kind.clone(), *pin, cx);
            }
        })];
        let mut this = Self {
            state,
            layout,
            views: HashMap::new(),
            drag: None,
            resize: None,
            zones: Rc::default(),
            floating: HashMap::new(),
            main_bounds: None,
            window_move: None,
            redock: None,
            homes: HashMap::new(),
            landed: None,
            preview: None,
            last_document: None,
            generation: 0,
            _subs: subs,
        };
        let panels = this.layout.panels.clone();
        for p in panels {
            let view = this.create_view(p.id, &p.kind, cx);
            this.views.insert(p.id, view);
        }
        this
    }

    pub fn view(&self, panel: PanelId) -> Option<&PanelView> {
        self.views.get(&panel)
    }

    fn create_view(&mut self, id: PanelId, kind: &PanelKind, cx: &mut Context<Self>) -> PanelView {
        let state = self.state.clone();
        match kind {
            PanelKind::Connections => {
                PanelView::Connections(cx.new(|cx| ConnectionsPanel::new(state, cx)))
            }
            PanelKind::Objects => PanelView::Objects(cx.new(|cx| ObjectsPanel::new(state, cx))),
            PanelKind::Inspector => {
                PanelView::Inspector(cx.new(|cx| InspectorPanel::new(state, cx)))
            }
            PanelKind::History => PanelView::History(cx.new(|cx| HistoryPanel::new(state, cx))),
            PanelKind::Table { connection, object } => {
                let (connection, object) = (connection.clone(), object.clone());
                PanelView::Table(cx.new(|cx| TablePanel::new(state, id, connection, object, cx)))
            }
            PanelKind::Query { connection, text } => {
                let (connection, text) = (connection.clone(), text.clone());
                PanelView::Query(cx.new(|cx| QueryPanel::new(state, id, connection, text, cx)))
            }
            PanelKind::Settings => PanelView::Settings(cx.new(|cx| SettingsPanel::new(state, cx))),
        }
    }

    /// Writes the layout into the settings file (after the usual short pause).
    pub fn save_layout(&mut self, cx: &mut Context<Self>) {
        // Query panels keep their text across restarts.
        for p in &mut self.layout.panels {
            if let (PanelKind::Query { text, .. }, Some(PanelView::Query(v))) =
                (&mut p.kind, self.views.get(&p.id))
            {
                *text = v.read(cx).text(cx);
            }
        }
        let layout = self.layout.clone();
        self.state.update(cx, |s, cx| {
            s.update_settings_quietly(cx, |st| st.layout = layout)
        });
        cx.notify();
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        debug_assert!(self.layout.dock.validate().is_ok());
        self.close_empty_windows(cx);
        self.save_layout(cx);
    }

    // ---- opening and closing --------------------------------------------------------

    /// Opens a panel showing `kind`, or shows the one that already does.
    pub fn open(&mut self, kind: PanelKind, pin: bool, cx: &mut Context<Self>) {
        if let Some(existing) = self
            .layout
            .panels
            .iter()
            .find(|p| p.kind == kind)
            .map(|p| p.id)
        {
            if pin && self.preview == Some(existing) {
                self.preview = None;
            }
            self.show(existing, cx);
            return;
        }
        let id = self.layout.next_id();
        let target = match &kind {
            PanelKind::Objects => self.tool_target(Edge::Left),
            PanelKind::Inspector => self.tool_target(Edge::Right),
            PanelKind::History => match self.document_anchor() {
                Some(anchor) => DropTarget::Stack {
                    anchor,
                    zone: Zone::Edge(Edge::Bottom),
                },
                None => self.tool_target(Edge::Bottom),
            },
            _ => self.document_target(),
        };
        // A table from the sidebar takes the place of the preview tab.
        let replacing = match (&kind, self.preview) {
            (PanelKind::Table { .. }, Some(p)) if self.layout.dock.contains(p) => Some(p),
            _ => None,
        };
        let target = match replacing.and_then(|p| self.layout.dock.find(p)) {
            Some(loc) => DropTarget::Stack {
                anchor: replacing.unwrap_or(id),
                zone: Zone::Tab(loc.index + 1),
            },
            None => target,
        };
        let view = self.create_view(id, &kind, cx);
        let is_table = matches!(kind, PanelKind::Table { .. });
        self.views.insert(id, view);
        self.layout.panels.push(SavedPanel { id, kind });
        if self.layout.dock.insert(id, target).is_err() {
            let _ = self.layout.dock.insert(
                id,
                DropTarget::Root {
                    tree: TreeId::Main,
                    zone: Zone::Center,
                },
            );
        }
        self.settle_tool_size(id);
        if let Some(old) = replacing {
            self.close(old, cx);
        }
        self.preview = (is_table && !pin).then_some(id);
        self.show(id, cx);
        self.changed(cx);
    }

    /// A tool panel placed alone beside other panels in the main window gets a tool's
    /// width (or height, under the documents) instead of half of what it was dropped
    /// next to. Its neighbors give up the space in proportion to their sizes.
    fn settle_tool_size(&mut self, panel: PanelId) {
        if !self.views.get(&panel).is_some_and(PanelView::is_tool) {
            return;
        }
        let Some(loc) = self.layout.dock.find(panel) else {
            return;
        };
        if loc.tree != TreeId::Main
            || self
                .layout
                .dock
                .stack_of(panel)
                .is_none_or(|s| s.panels.len() != 1)
        {
            return;
        }
        let Some((&index, parent)) = loc.path.split_last() else {
            return;
        };
        let Some(Node::Split { axis, children }) = self.layout.dock.main.node(parent) else {
            return;
        };
        let row = *axis == deltos_dock::Axis::Row;
        let main = self
            .main_bounds
            .map_or(size(px(1320.), px(820.)), |b| b.size);
        let (want, length) = if row {
            (280.0, f32::from(main.width))
        } else {
            (220.0, f32::from(main.height))
        };
        let fraction = (want / length.max(1.0)).clamp(0.1, 0.45);
        let rest: f32 = children
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .map(|(_, c)| c.size)
            .sum();
        if rest <= 0.0 {
            return;
        }
        let sizes: Vec<f32> = children
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == index {
                    fraction
                } else {
                    c.size / rest * (1.0 - fraction)
                }
            })
            .collect();
        let parent = parent.to_vec();
        self.layout.dock.set_sizes(TreeId::Main, &parent, &sizes);
    }

    /// Where a tool panel opens when it is not in the layout: beside everything on
    /// `edge` of the main window.
    fn tool_target(&self, edge: Edge) -> DropTarget {
        DropTarget::Root {
            tree: TreeId::Main,
            zone: Zone::Edge(edge),
        }
    }

    /// A panel in the stack documents open into.
    fn document_anchor(&self) -> Option<PanelId> {
        let is_doc = |p: &PanelId| self.views.get(p).is_some_and(PanelView::is_document);
        self.last_document
            .filter(|p| self.layout.dock.contains(*p) && is_doc(p))
            .or_else(|| self.layout.dock.main.panels().into_iter().find(is_doc))
            .or_else(|| self.layout.dock.panels().into_iter().find(is_doc))
    }

    fn document_target(&self) -> DropTarget {
        match self.document_anchor() {
            Some(anchor) => DropTarget::Stack {
                anchor,
                zone: Zone::Center,
            },
            None => match self.layout.dock.main.panels().first() {
                // Beside the tools rather than inside one of them.
                Some(&anchor) => DropTarget::Stack {
                    anchor,
                    zone: Zone::Edge(Edge::Right),
                },
                None => DropTarget::Root {
                    tree: TreeId::Main,
                    zone: Zone::Center,
                },
            },
        }
    }

    /// Brings `panel` to the front of its stack and its window.
    pub fn show(&mut self, panel: PanelId, cx: &mut Context<Self>) {
        self.layout.dock.activate(panel);
        if self.views.get(&panel).is_some_and(PanelView::is_document) {
            self.last_document = Some(panel);
        }
        if let Some(TreeId::Floating(w)) = self.layout.dock.find(panel).map(|l| l.tree)
            && let Some(handle) = self.floating.get(&w)
        {
            handle
                .update(cx, |_, window, _| window.activate_window())
                .ok();
        }
        if let Some(PanelView::Table(t)) = self.views.get(&panel) {
            let t = t.clone();
            t.update(cx, |t, cx| t.on_shown(cx));
        }
        cx.notify();
    }

    /// Keeps a preview tab open for good.
    pub fn pin(&mut self, panel: PanelId, cx: &mut Context<Self>) {
        if self.preview == Some(panel) {
            self.preview = None;
            cx.notify();
        }
    }

    pub fn is_preview(&self, panel: PanelId) -> bool {
        self.preview == Some(panel)
    }

    pub fn close(&mut self, panel: PanelId, cx: &mut Context<Self>) {
        self.layout.dock.remove(panel);
        self.layout.panels.retain(|p| p.id != panel);
        self.views.remove(&panel);
        if self.preview == Some(panel) {
            self.preview = None;
        }
        self.changed(cx);
    }

    /// Opens a tool panel, or closes it when it is open.
    pub fn toggle(&mut self, kind: PanelKind, cx: &mut Context<Self>) {
        match self
            .layout
            .panels
            .iter()
            .find(|p| p.kind == kind)
            .map(|p| p.id)
        {
            Some(id) => self.close(id, cx),
            None => self.open(kind, true, cx),
        }
    }

    pub fn is_open(&self, kind: &PanelKind) -> bool {
        self.layout.panels.iter().any(|p| p.kind == *kind)
    }

    /// The panel showing in the stack the person used last, for keyboard actions.
    pub fn active_document(&self) -> Option<PanelId> {
        let anchor = self.document_anchor()?;
        self.layout.dock.stack_of(anchor)?.active_panel()
    }

    /// Reloads the table in front, for the refresh shortcut.
    pub fn refresh_active(&mut self, cx: &mut Context<Self>) {
        if let Some(PanelView::Table(t)) = self.active_document().and_then(|p| self.views.get(&p)) {
            t.clone().update(cx, |t, cx| t.refresh(cx));
        }
    }

    /// Shows the object sidebar and puts the cursor in its search field.
    pub fn focus_object_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_open(&PanelKind::Objects) {
            self.open(PanelKind::Objects, true, cx);
        }
        let objects = self.views.values().find_map(|v| match v {
            PanelView::Objects(o) => Some(o.clone()),
            _ => None,
        });
        if let Some(o) = objects {
            o.update(cx, |o, cx| o.focus_search(window, cx));
        }
    }

    /// `DELTOS_DEMO`: opens the demo connection and a screen to look at. `settings` opens
    /// Settings, `connections` the connection list, `wide` the 300 column table, `query`
    /// a query tab that runs at once, `popout` the big table with the inspector in its own
    /// window; anything else opens the 50 million row table.
    pub fn start_demo(&mut self, screen: &str, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| s.open_demo(cx));
        let object =
            |namespace: &str, name: &str, kind: deltos_data::ObjectKind| deltos_data::ObjectRef {
                namespace: vec!["demo".into(), namespace.into()],
                name: name.into(),
                kind,
            };
        let connection = crate::state::DEMO_ID.to_owned();
        let kind = match screen {
            "settings" => PanelKind::Settings,
            "connections" => PanelKind::Connections,
            "query" => PanelKind::Query {
                connection: Some(connection),
                text: "SELECT * FROM orders LIMIT 100000;\n\n-- Ctrl+Enter runs the statement at the cursor.\nSELECT COUNT(*) FROM users;".into(),
            },
            "wide" => PanelKind::Table {
                connection,
                object: object("public", "events_wide", deltos_data::ObjectKind::Table),
            },
            _ => PanelKind::Table {
                connection,
                object: object("public", "orders", deltos_data::ObjectKind::Table),
            },
        };
        let run = matches!(kind, PanelKind::Query { .. });
        self.open(kind, true, cx);
        if screen == "popout" {
            // A torn-off inspector, to see a floating panel window.
            self.pop_out(deltos_settings::INSPECTOR_PANEL, None, false, cx);
        }
        if run {
            let query = self.views.values().find_map(|v| match v {
                PanelView::Query(q) => Some(q.clone()),
                _ => None,
            });
            if let Some(q) = query {
                q.update(cx, |q, cx| q.run_current(cx));
            }
        }
    }

    pub fn cycle_tab(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(active) = self.active_document() else {
            return;
        };
        let Some(stack) = self.layout.dock.stack_of(active) else {
            return;
        };
        let n = stack.panels.len() as isize;
        let next = stack.panels[((stack.active as isize + step).rem_euclid(n)) as usize];
        self.show(next, cx);
    }

    pub fn reset_layout(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<WindowId> = self.floating.keys().copied().collect();
        for w in ids {
            if let Some(h) = self.floating.remove(&w) {
                h.update(cx, |_, window, _| window.remove_window()).ok();
            }
        }
        let main_window = self.layout.main_window;
        self.layout = SavedLayout::default();
        self.layout.main_window = main_window;
        self.views.clear();
        let panels = self.layout.panels.clone();
        for p in panels {
            let view = self.create_view(p.id, &p.kind, cx);
            self.views.insert(p.id, view);
        }
        self.preview = None;
        self.changed(cx);
    }

    // ---- dragging tabs --------------------------------------------------------------

    /// A press on a tab or header: shows the panel, and may become a drag.
    pub fn press(
        &mut self,
        panel: PanelId,
        source: TreeId,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.drag = Some(Drag {
            panel,
            source,
            origin: at,
            cursor: at,
            active: false,
            hover: None,
            velocity: (0.0, 0.0),
            moved_at: Instant::now(),
        });
        if self
            .layout
            .dock
            .stack_of(panel)
            .and_then(|s| s.active_panel())
            != Some(panel)
        {
            self.show(panel, cx);
        } else if self.views.get(&panel).is_some_and(PanelView::is_document) {
            self.last_document = Some(panel);
        }
    }

    /// The pointer moved in the window of `tree`.
    pub fn pointer_moved(
        &mut self,
        tree: TreeId,
        e: &gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(r) = self.resize.clone() {
            if r.tree != tree {
                return;
            }
            if e.pressed_button != Some(gpui::MouseButton::Left) {
                self.end_resize(cx);
                return;
            }
            let d = if r.row {
                e.position.x - r.start.x
            } else {
                e.position.y - r.start.y
            };
            let delta = f32::from(d) / r.length.max(1.0);
            let mut sizes = r.start_sizes.clone();
            let pair = sizes[r.handle] + sizes[r.handle + 1];
            let min = deltos_dock::MIN_FRACTION.min(pair / 2.0);
            let a = (sizes[r.handle] + delta).clamp(min, pair - min);
            sizes[r.handle] = a;
            sizes[r.handle + 1] = pair - a;
            self.layout.dock.set_sizes(r.tree, &r.path, &sizes);
            cx.notify();
            return;
        }
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        if drag.source != tree {
            return;
        }
        if e.pressed_button != Some(gpui::MouseButton::Left) {
            self.drag = None;
            cx.notify();
            return;
        }
        let step = e.position - drag.cursor;
        drag.velocity = (
            drag.velocity.0 * 0.55 + f32::from(step.x) * 0.45,
            drag.velocity.1 * 0.55 + f32::from(step.y) * 0.45,
        );
        drag.moved_at = Instant::now();
        drag.cursor = e.position;
        if !drag.active {
            let d = e.position - drag.origin;
            if f32::from(d.x).hypot(f32::from(d.y)) < DRAG_SLOP {
                return;
            }
            drag.active = true;
        }
        let drag = drag.clone();
        let size = window.viewport_size();
        let p = e.position;
        let m = px(POP_OUT_MARGIN);
        let outside = p.x < -m || p.y < -m || p.x > size.width + m || p.y > size.height + m;
        let hover = if outside {
            // Over another window of ours, the drop goes there.
            self.screen_point(tree, p, window)
                .and_then(|screen| self.hover_on_screen(screen, drag.panel, Some(tree)))
        } else {
            self.hover_at(tree, p, drag.panel)
        };
        if outside && hover.is_none() {
            let alone_in_window = matches!(tree, TreeId::Floating(_))
                && self
                    .layout
                    .dock
                    .tree(tree)
                    .is_some_and(|t| t.panels().len() == 1);
            if alone_in_window {
                // Pulling the only panel out of its window moves the window.
                self.drag = None;
                if let TreeId::Floating(w) = tree {
                    self.begin_window_move(w, drag.origin, window);
                }
                window.start_window_move();
            } else {
                let screen = self.screen_point(tree, p, window);
                self.drag = None;
                self.pop_out(drag.panel, screen, true, cx);
            }
            cx.notify();
            return;
        }
        if let Some(d) = self.drag.as_mut() {
            d.hover = hover;
        }
        cx.notify();
    }

    /// The button came up in the window of `tree`. Returns whether a drag or a resize
    /// ended, so the release does nothing else.
    pub fn pointer_released(&mut self, tree: TreeId, cx: &mut Context<Self>) -> bool {
        if self.resize.as_ref().is_some_and(|r| r.tree == tree) {
            self.end_resize(cx);
            return true;
        }
        let Some(drag) = self.drag.take() else {
            return false;
        };
        if drag.source != tree {
            self.drag = Some(drag);
            return false;
        }
        cx.notify();
        if !drag.active {
            return false;
        }
        if let Some(h) = drag.hover {
            self.pin(drag.panel, cx);
            if self.layout.dock.move_panel(drag.panel, h.target).is_ok() {
                self.land(drag.panel);
                self.show(drag.panel, cx);
                self.changed(cx);
            }
        }
        true
    }

    /// Marks `panel` as just landed, so its stack plays the settle animation.
    fn land(&mut self, panel: PanelId) {
        self.generation += 1;
        self.landed = Some((panel, self.generation));
    }

    /// The point `p` of the window of `tree`, on the screen.
    fn screen_point(
        &self,
        tree: TreeId,
        p: Point<Pixels>,
        window: &Window,
    ) -> Option<Point<Pixels>> {
        if !screen_positions_known() {
            return None;
        }
        let _ = tree;
        Some(window.bounds().origin + p)
    }

    /// The drop target under a point on the screen, in whichever of our windows is there,
    /// other than `except`.
    fn hover_on_screen(
        &self,
        screen: Point<Pixels>,
        panel: PanelId,
        except: Option<TreeId>,
    ) -> Option<Hover> {
        let main = self.main_bounds?;
        if except != Some(TreeId::Main) && main.contains(&screen) {
            let local = screen - main.origin;
            return self.hover_at(TreeId::Main, local, panel);
        }
        None
    }

    /// The drop target at `p` in the window of `tree`.
    pub fn hover_at(&self, tree: TreeId, p: Point<Pixels>, panel: PanelId) -> Option<Hover> {
        let zones = self.zones.borrow();
        let z = zones.get(&tree)?;
        let root = z.root?;
        if !root.contains(&p) {
            return None;
        }
        let has_panels = !z.stacks.is_empty();
        // A thin band along the outside of the window docks beside everything.
        let band = px(12.);
        if has_panels {
            let edge = if p.x < root.left() + band {
                Some(Edge::Left)
            } else if p.x > root.right() - band {
                Some(Edge::Right)
            } else if p.y > root.bottom() - band {
                Some(Edge::Bottom)
            } else {
                None
            };
            if let Some(edge) = edge {
                return Some(Hover {
                    tree,
                    target: DropTarget::Root {
                        tree,
                        zone: Zone::Edge(edge),
                    },
                    highlight: edge_part(root, edge, 0.25),
                    caret: false,
                });
            }
        } else {
            return Some(Hover {
                tree,
                target: DropTarget::Root {
                    tree,
                    zone: Zone::Center,
                },
                highlight: root,
                caret: false,
            });
        }
        for s in &z.stacks {
            if s.strip.contains(&p) {
                // Between the tabs: the slot after every tab whose middle is left of p.
                let mut index = 0;
                let mut caret_x = s.strip.left();
                for (i, tab) in s.panels.iter().enumerate() {
                    if let Some(b) = z.tabs.get(tab) {
                        if p.x > b.center().x {
                            index = i + 1;
                            caret_x = b.right();
                        } else {
                            if i == index {
                                caret_x = b.left();
                            }
                            break;
                        }
                    }
                }
                let alone = s.panels.len() == 1 && s.panels[0] == panel;
                if alone {
                    return None;
                }
                return Some(Hover {
                    tree,
                    target: DropTarget::Stack {
                        anchor: s.anchor,
                        zone: Zone::Tab(index),
                    },
                    highlight: Bounds {
                        origin: point(caret_x - px(1.), s.strip.top() + px(4.)),
                        size: size(px(2.), s.strip.size.height - px(8.)),
                    },
                    caret: true,
                });
            }
            if s.content.contains(&p) {
                let c = s.content;
                let fx = f32::from(p.x - c.left()) / f32::from(c.size.width).max(1.0);
                let fy = f32::from(p.y - c.top()) / f32::from(c.size.height).max(1.0);
                let edges = [
                    (fx, Edge::Left),
                    (1.0 - fx, Edge::Right),
                    (fy, Edge::Top),
                    (1.0 - fy, Edge::Bottom),
                ];
                let (d, edge) = edges
                    .into_iter()
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .unwrap_or((1.0, Edge::Left));
                let alone = s.panels.len() == 1 && s.panels[0] == panel;
                if d < 0.25 && !alone {
                    return Some(Hover {
                        tree,
                        target: DropTarget::Stack {
                            anchor: s.anchor,
                            zone: Zone::Edge(edge),
                        },
                        highlight: edge_part(c, edge, 0.5),
                        caret: false,
                    });
                }
                if s.panels.contains(&panel) {
                    return None;
                }
                return Some(Hover {
                    tree,
                    target: DropTarget::Stack {
                        anchor: s.anchor,
                        zone: Zone::Center,
                    },
                    highlight: c,
                    caret: false,
                });
            }
        }
        None
    }

    pub fn begin_resize(&mut self, resize: SplitResize, cx: &mut Context<Self>) {
        self.resize = Some(resize);
        cx.notify();
    }

    fn end_resize(&mut self, cx: &mut Context<Self>) {
        if self.resize.take().is_some() {
            self.changed(cx);
        }
    }

    // ---- panel windows ----------------------------------------------------------------

    /// Moves `panel` into a window of its own under the screen point `at` (or near the
    /// main window when positions are unknown). With `keep_moving`, the new window
    /// follows the pointer as the press carries on.
    pub fn pop_out(
        &mut self,
        panel: PanelId,
        at: Option<Point<Pixels>>,
        keep_moving: bool,
        cx: &mut Context<Self>,
    ) {
        let home = self.home_of(panel);
        let (w, h) = match self.views.get(&panel) {
            Some(v) if v.is_tool() => (340., 560.),
            _ => (880., 560.),
        };
        let grab = point(px(w / 2.), px(14.));
        let origin = match (at, self.main_bounds) {
            (Some(p), _) => p - grab,
            (None, Some(main)) => main.origin + point(px(80.), px(80.)),
            (None, None) => point(px(120.), px(120.)),
        };
        let rect = Rect {
            x: f32::from(origin.x),
            y: f32::from(origin.y),
            w,
            h,
        };
        let Ok(id) = self.layout.dock.detach(panel, Some(rect)) else {
            return;
        };
        // The new window plays its own entrance; a landing still pending would play a
        // second one there.
        self.landed = None;
        if let Some(home) = home {
            self.homes.insert(panel, home);
        }
        if self.floating.contains_key(&id) {
            self.changed(cx);
            return;
        }
        self.open_window(id, keep_moving.then_some(grab), cx);
        self.generation += 1;
        self.window_move = Some(WindowMove {
            window: id,
            grab,
            start: origin,
            generation: self.generation,
        });
        self.changed(cx);
    }

    /// Where `panel` sits now, as a drop target that would put it back.
    fn home_of(&self, panel: PanelId) -> Option<DropTarget> {
        let loc = self.layout.dock.find(panel)?;
        if loc.tree != TreeId::Main {
            return None;
        }
        let stack = self.layout.dock.stack_of(panel)?;
        if let Some(&other) = stack.panels.iter().find(|p| **p != panel) {
            return Some(DropTarget::Stack {
                anchor: other,
                zone: Zone::Center,
            });
        }
        // Alone in its stack: beside the neighbor in its split.
        let (index, parent) = loc.path.split_last()?;
        let Some(Node::Split { axis, children }) = self.layout.dock.main.node(parent) else {
            return None;
        };
        let (neighbor, edge) = if *index > 0 {
            (
                index - 1,
                if *axis == deltos_dock::Axis::Row {
                    Edge::Right
                } else {
                    Edge::Bottom
                },
            )
        } else {
            (
                index + 1,
                if *axis == deltos_dock::Axis::Row {
                    Edge::Left
                } else {
                    Edge::Top
                },
            )
        };
        let mut panels = Vec::new();
        children.get(neighbor)?.node.panels(&mut panels);
        let anchor = if edge == Edge::Right || edge == Edge::Bottom {
            *panels.last()?
        } else {
            *panels.first()?
        };
        Some(DropTarget::Stack {
            anchor,
            zone: Zone::Edge(edge),
        })
    }

    /// Opens the window for panel window `id` at its saved bounds. GPUI draws a new
    /// window as it opens, and drawing reads this workspace, so the window opens just
    /// after the current update. If it cannot open, its panels go back to the main
    /// window. With `keep_moving`, the press that tore the panel off goes on
    /// moving the new window, with the pointer at that point of it.
    fn open_window(
        &mut self,
        id: WindowId,
        keep_moving: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.layout.dock.window(id).map(|w| w.bounds) else {
            return;
        };
        let settings = &self.state.read(cx).settings;
        let own_bar = settings.appearance.custom_title_bar;
        let decorations = crate::decorations(settings);
        let bounds = saved.map_or_else(
            || Bounds::centered(None, size(px(880.), px(560.)), cx),
            |r| Bounds {
                origin: point(px(r.x), px(r.y)),
                size: size(px(r.w.max(240.)), px(r.h.max(200.))),
            },
        );
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Deltos".into()),
                appears_transparent: own_bar,
                ..Default::default()
            }),
            window_decorations: Some(decorations),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(240.), px(160.))),
            app_id: Some("deltos".into()),
            focus: true,
            show: true,
            ..Default::default()
        };
        let workspace = cx.entity();
        let weak = workspace.downgrade();
        let state = self.state.clone();
        cx.defer(move |cx| {
            let opened = cx.open_window(options, move |window, cx| {
                cx.new(|cx| PanelWindow::new(workspace, state, id, window, cx))
            });
            weak.update(cx, |this, cx| match opened {
                Ok(handle) => {
                    tracing::debug!("panel window {} opened", id.0);
                    this.floating.insert(id, handle);
                    if let Some(grab) = keep_moving {
                        // Once the window is shown, the press still down moves it, so
                        // tearing off and placing is one motion: the window stays under
                        // the pointer as it leaves.
                        cx.spawn(async move |_, cx| {
                            // The window may take a few frames to be mapped and listed.
                            for attempt in 0..12 {
                                cx.background_executor()
                                    .timer(Duration::from_millis(16))
                                    .await;
                                let started = handle
                                    .update(cx, |_, window, _| {
                                        crate::ui::motion::follow_pointer(
                                            window,
                                            grab,
                                            attempt == 11,
                                        )
                                    })
                                    .unwrap_or(true);
                                if started {
                                    break;
                                }
                            }
                        })
                        .detach();
                    }
                }
                Err(e) => {
                    tracing::error!("could not open a panel window: {e}");
                    let _ = this.layout.dock.merge_window(
                        id,
                        DropTarget::Root {
                            tree: TreeId::Main,
                            zone: Zone::Edge(Edge::Right),
                        },
                    );
                    this.changed(cx);
                }
            })
            .ok();
        });
    }

    /// Opens the panel windows saved from the last run.
    pub fn restore_windows(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<WindowId> = self.layout.dock.floating.iter().map(|f| f.id).collect();
        for id in ids {
            if !self.floating.contains_key(&id) {
                self.open_window(id, None, cx);
            }
        }
    }

    /// Closes panel windows whose trees became empty.
    fn close_empty_windows(&mut self, cx: &mut Context<Self>) {
        let gone: Vec<WindowId> = self
            .floating
            .keys()
            .copied()
            .filter(|w| self.layout.dock.window(*w).is_none())
            .collect();
        for w in gone {
            if let Some(h) = self.floating.remove(&w) {
                h.update(cx, |_, window, _| window.remove_window()).ok();
            }
        }
    }

    /// Puts every panel of window `id` back into the main window, each where it came
    /// from when that place still exists. The window itself is closed by the caller or
    /// by [`Workspace::close_empty_windows`].
    pub fn dock_window(&mut self, id: WindowId, cx: &mut Context<Self>) {
        let Some(tree) = self.layout.dock.window(id).map(|f| f.tree.clone()) else {
            return;
        };
        for panel in tree.panels() {
            let target = self
                .homes
                .remove(&panel)
                .filter(|t| match t {
                    DropTarget::Stack { anchor, .. } => self
                        .layout
                        .dock
                        .find(*anchor)
                        .is_some_and(|l| l.tree == TreeId::Main),
                    DropTarget::Root { .. } => true,
                })
                .unwrap_or_else(|| self.document_target());
            let target = match target {
                // The document stack may itself be in this window.
                DropTarget::Stack { anchor, .. }
                    if self.layout.dock.find(anchor).map(|l| l.tree) != Some(TreeId::Main) =>
                {
                    DropTarget::Root {
                        tree: TreeId::Main,
                        zone: Zone::Edge(Edge::Right),
                    }
                }
                t => t,
            };
            if self.layout.dock.move_panel(panel, target).is_ok() {
                self.settle_tool_size(panel);
            }
        }
        self.redock = None;
        self.changed(cx);
    }

    /// Called as panel window `id` closes: its panels dock back.
    pub fn window_closing(&mut self, id: WindowId, cx: &mut Context<Self>) {
        self.floating.remove(&id);
        self.dock_window(id, cx);
    }

    /// Called when the person presses on a panel window's header, before the system
    /// moves the window. `grab` is the press inside the window.
    pub fn begin_window_move(&mut self, id: WindowId, grab: Point<Pixels>, window: &Window) {
        self.generation += 1;
        self.window_move = Some(WindowMove {
            window: id,
            grab,
            start: window.bounds().origin,
            generation: self.generation,
        });
    }

    /// A panel window moved or resized. Records its bounds, and while it is being moved
    /// over the main window, shows and then performs the dock.
    pub fn window_bounds_changed(
        &mut self,
        id: WindowId,
        bounds: Bounds<Pixels>,
        pointer: Point<Pixels>,
        opened: Instant,
        cx: &mut Context<Self>,
    ) {
        self.layout.dock.set_window_bounds(
            id,
            Rect {
                x: f32::from(bounds.origin.x),
                y: f32::from(bounds.origin.y),
                w: f32::from(bounds.size.width),
                h: f32::from(bounds.size.height),
            },
        );
        let layout = self.layout.clone();
        self.state.update(cx, |s, cx| {
            s.update_settings_quietly(cx, |st| st.layout = layout)
        });
        if !screen_positions_known() {
            return;
        }
        // A move the app did not see begin (Windows hands header presses to the system):
        // the pointer's last position in the window is where it was grabbed.
        let fresh = self.window_move.as_ref().is_none_or(|m| m.window != id);
        if fresh {
            if opened.elapsed() < Duration::from_millis(700) {
                return;
            }
            self.generation += 1;
            self.window_move = Some(WindowMove {
                window: id,
                grab: pointer,
                start: bounds.origin,
                generation: self.generation,
            });
        }
        let Some(m) = self.window_move.clone() else {
            return;
        };
        let moved = bounds.origin - m.start;
        if f32::from(moved.x).abs() + f32::from(moved.y).abs() < 16.0 {
            return;
        }
        let screen = bounds.origin + m.grab;
        let hover = self
            .layout
            .dock
            .window(id)
            .and_then(|f| f.tree.panels().first().copied())
            .and_then(|panel| self.hover_on_screen(screen, panel, Some(TreeId::Floating(id))));
        let changed = self.redock.as_ref().map(|r| &r.1) != hover.as_ref();
        self.redock = hover.map(|h| (id, h));
        if changed {
            cx.notify();
        }
        self.generation += 1;
        let generation = self.generation;
        if let Some(m) = self.window_move.as_mut() {
            m.generation = generation;
        }
        if self.redock.is_some() {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(DOCK_DWELL).await;
                this.update(cx, |this, cx| {
                    let still = this
                        .window_move
                        .as_ref()
                        .is_some_and(|m| m.generation == generation);
                    if !still {
                        return;
                    }
                    if let Some((w, hover)) = this.redock.take() {
                        this.window_move = None;
                        this.merge_window(w, hover.target, cx);
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    fn merge_window(&mut self, id: WindowId, target: DropTarget, cx: &mut Context<Self>) {
        let panels = self
            .layout
            .dock
            .window(id)
            .map(|f| f.tree.panels())
            .unwrap_or_default();
        if self.layout.dock.merge_window(id, target).is_ok() {
            for p in &panels {
                self.homes.remove(p);
            }
            if let Some(first) = panels.first() {
                self.land(*first);
                self.show(*first, cx);
            }
            if let Some(main) = cx
                .windows()
                .into_iter()
                .find(|w| w.downcast::<crate::Root>().is_some())
            {
                main.update(cx, |_, window, _| window.activate_window())
                    .ok();
            }
        }
        self.changed(cx);
    }

    /// Closes the panel windows without docking their panels, for quitting: they open
    /// again next time.
    pub fn close_windows_for_quit(&mut self, cx: &mut App) {
        for (_, h) in self.floating.drain() {
            h.update(cx, |_, window, _| window.remove_window()).ok();
        }
    }

    /// The title for panel window `id`: its active panel.
    pub fn window_title(&self, id: WindowId, cx: &App) -> SharedString {
        let tree = self.layout.dock.window(id).map(|f| &f.tree);
        let active = tree.and_then(|t| match &t.root {
            Some(Node::Stack(s)) => s.active_panel(),
            _ => t.panels().first().copied(),
        });
        active
            .and_then(|p| self.views.get(&p))
            .map_or_else(|| "Deltos".into(), |v| v.title(cx))
    }
}

/// The part of `b` toward `edge`, `share` of its size.
fn edge_part(b: Bounds<Pixels>, edge: Edge, share: f32) -> Bounds<Pixels> {
    let (w, h) = (b.size.width, b.size.height);
    match edge {
        Edge::Left => Bounds {
            origin: b.origin,
            size: size(w * share, h),
        },
        Edge::Right => Bounds {
            origin: point(b.right() - w * share, b.top()),
            size: size(w * share, h),
        },
        Edge::Top => Bounds {
            origin: b.origin,
            size: size(w, h * share),
        },
        Edge::Bottom => Bounds {
            origin: point(b.left(), b.bottom() - h * share),
            size: size(w, h * share),
        },
    }
}
