//! What the command palette lists. Each provider is a plain function that adds items for
//! one kind of thing; [`PROVIDERS`] is the list the palette asks, in the order the items
//! show before anything is typed. A feature adds itself to the palette with one more
//! line there.

use std::rc::Rc;

use deltos_data::ObjectKind;
use deltos_settings::{KEYBIND_ACTIONS, PanelKind};
use gpui::{Action, App, Entity, SharedString, Window};

use crate::state::AppState;
use crate::ui::icons::Icon;
use crate::ui::settings::{Section, SettingsPanel};
use crate::ui::workspace::{PanelView, Workspace};

/// What an item is, for the prefix filters and the label beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Something to do: an app action with its shortcut.
    Action,
    /// An open panel or tab to go to.
    Panel,
    Connection,
    /// A table, view or collection of an open connection.
    Object,
    /// A page of Settings.
    Setting,
    /// A query saved by name.
    SavedQuery,
    /// A layout preset to apply.
    Layout,
}

impl Kind {
    /// The faint word on the right of an item that has no shortcut.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Action => "Command",
            Kind::Panel => "Tab",
            Kind::Connection => "Connection",
            Kind::Object => "Object",
            Kind::Setting => "Settings",
            Kind::SavedQuery => "Saved query",
            Kind::Layout => "Layout",
        }
    }
}

/// The part of the query that picks which kinds are listed: `>` commands, `@` objects,
/// `#` connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    All,
    Commands,
    Objects,
    Connections,
}

impl Scope {
    pub fn admits(self, kind: Kind) -> bool {
        match self {
            Scope::All => true,
            Scope::Commands => matches!(kind, Kind::Action | Kind::Setting | Kind::Layout),
            Scope::Objects => kind == Kind::Object,
            Scope::Connections => kind == Kind::Connection,
        }
    }

    pub fn prefix(self) -> &'static str {
        match self {
            Scope::All => "",
            Scope::Commands => ">",
            Scope::Objects => "@",
            Scope::Connections => "#",
        }
    }
}

/// Splits a typed query into its scope and the text to match.
pub fn parse_query(text: &str) -> (Scope, &str) {
    let trimmed = text.trim_start();
    let scope = match trimmed.chars().next() {
        Some('>') => Scope::Commands,
        Some('@') => Scope::Objects,
        Some('#') => Scope::Connections,
        _ => return (Scope::All, trimmed.trim_end()),
    };
    (scope, trimmed[1..].trim())
}

/// One entry of the palette.
#[derive(Clone)]
pub struct PaletteItem {
    /// Stable across runs, for the recently used list: `action:new-query`, `conn:prod`.
    pub id: String,
    pub title: SharedString,
    pub detail: Option<SharedString>,
    /// More text that finds the item when the title does not match, such as a table's
    /// schema. Matches here rank below title matches and are not highlighted.
    pub search: Option<String>,
    pub icon: Icon,
    pub kind: Kind,
    pub shortcut: Option<SharedString>,
    pub run: Rc<dyn Fn(&mut Window, &mut App)>,
}

impl PaletteItem {
    pub fn new(
        id: impl Into<String>,
        kind: Kind,
        icon: Icon,
        title: impl Into<SharedString>,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            detail: None,
            search: None,
            icon,
            kind,
            shortcut: None,
            run: Rc::new(run),
        }
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn search(mut self, text: impl Into<String>) -> Self {
        self.search = Some(text.into());
        self
    }

    pub fn shortcut(mut self, keys: Option<SharedString>) -> Self {
        self.shortcut = keys;
        self
    }
}

/// What providers read from, at the moment the palette opens.
pub struct Sources<'a> {
    pub state: &'a Entity<AppState>,
    pub workspace: &'a Entity<Workspace>,
    pub cx: &'a App,
}

impl Sources<'_> {
    pub fn app_state(&self) -> &AppState {
        self.state.read(self.cx)
    }

    pub fn ws(&self) -> &Workspace {
        self.workspace.read(self.cx)
    }

    /// The shortcut bound to a keyboard action of the settings, as people read it.
    pub fn shortcut(&self, action: &str) -> Option<SharedString> {
        let keys = self.app_state().settings.keybinds.get(action)?;
        let pretty = pretty_keys(keys);
        (!pretty.is_empty()).then(|| pretty.into())
    }
}

pub type Provider = fn(&Sources, &mut Vec<PaletteItem>);

/// Every source of items, in the order they list before anything is typed.
pub const PROVIDERS: &[Provider] = &[
    actions,
    panels,
    connections,
    objects,
    saved_queries,
    crate::ui::layouts::provider,
    settings_pages,
];

/// Every item from every provider.
pub fn collect(sources: &Sources) -> Vec<PaletteItem> {
    let mut items = Vec::new();
    for provider in PROVIDERS {
        provider(sources, &mut items);
    }
    items
}

/// `ctrl-shift-p` as `Ctrl+Shift+P`; a sequence such as `ctrl-k ctrl-s` keeps its space.
pub fn pretty_keys(keys: &str) -> String {
    keys.split_whitespace()
        .map(|stroke| {
            let mut parts: Vec<&str> = Vec::new();
            let mut rest = stroke;
            // Modifiers are the words before a dash; whatever is left is the key, which
            // may itself be "-" (as in ctrl--).
            while let Some(ix) = rest.find('-') {
                if ix == 0 {
                    break;
                }
                parts.push(&rest[..ix]);
                rest = &rest[ix + 1..];
            }
            parts.push(rest);
            parts
                .iter()
                .map(|p| match *p {
                    "ctrl" => "Ctrl".to_owned(),
                    "shift" => "Shift".to_owned(),
                    "alt" => "Alt".to_owned(),
                    "cmd" | "super" | "win" => "Super".to_owned(),
                    "fn" => "Fn".to_owned(),
                    "enter" => "Enter".to_owned(),
                    "escape" => "Esc".to_owned(),
                    "space" => "Space".to_owned(),
                    "tab" => "Tab".to_owned(),
                    "backspace" => "Backspace".to_owned(),
                    "delete" => "Delete".to_owned(),
                    "up" => "↑".to_owned(),
                    "down" => "↓".to_owned(),
                    "left" => "←".to_owned(),
                    "right" => "→".to_owned(),
                    "pageup" => "PgUp".to_owned(),
                    "pagedown" => "PgDn".to_owned(),
                    key => {
                        let mut c = key.chars();
                        match c.next() {
                            Some(first) => first.to_uppercase().chain(c).collect(),
                            None => String::new(),
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

type ActionCtor = fn() -> Box<dyn Action>;

/// The app actions the palette offers, by their name in the keyboard settings.
const ACTIONS: &[(&str, ActionCtor)] = &[
    ("new-query", || Box::new(crate::NewQuery)),
    ("close-tab", || Box::new(crate::CloseTab)),
    ("next-tab", || Box::new(crate::NextTab)),
    ("previous-tab", || Box::new(crate::PreviousTab)),
    ("refresh", || Box::new(crate::Refresh)),
    ("toggle-sidebar", || Box::new(crate::ToggleSidebar)),
    ("toggle-inspector", || Box::new(crate::ToggleInspector)),
    ("settings", || Box::new(crate::OpenSettings)),
    ("zoom-in", || Box::new(crate::ZoomIn)),
    ("zoom-out", || Box::new(crate::ZoomOut)),
    ("zoom-reset", || Box::new(crate::ZoomReset)),
    ("reset-layout", || Box::new(crate::ResetLayout)),
];

fn actions(s: &Sources, out: &mut Vec<PaletteItem>) {
    for (name, make) in ACTIONS {
        let title = KEYBIND_ACTIONS
            .iter()
            .find(|(a, _, _)| a == name)
            .map_or(*name, |(_, _, label)| *label);
        let make = *make;
        out.push(
            PaletteItem::new(
                format!("action:{name}"),
                Kind::Action,
                Icon::ChevronRight,
                title,
                move |window, cx| window.dispatch_action(make(), cx),
            )
            .shortcut(s.shortcut(name)),
        );
    }
    let ws = s.workspace.clone();
    out.push(PaletteItem::new(
        "action:toggle-history",
        Kind::Action,
        Icon::History,
        "Show or hide the history",
        move |_, cx| ws.update(cx, |w, cx| w.toggle(PanelKind::History, cx)),
    ));
    let ws = s.workspace.clone();
    out.push(PaletteItem::new(
        "action:connections",
        Kind::Action,
        Icon::Plug,
        "Connections",
        move |_, cx| ws.update(cx, |w, cx| w.open(PanelKind::Connections, true, cx)),
    ));
    let ws = s.workspace.clone();
    out.push(
        PaletteItem::new(
            "action:filter-sidebar",
            Kind::Action,
            Icon::Search,
            "Filter the sidebar",
            move |window, cx| ws.update(cx, |w, cx| w.focus_object_search(window, cx)),
        )
        .search("objects search"),
    );
}

/// The first line of a query, shortened, to tell query tabs apart.
fn first_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut out: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        out.push('…');
    }
    Some(out)
}

fn connection_name(state: &AppState, id: &str) -> String {
    state
        .session(id)
        .map(|s| s.name.clone())
        .or_else(|| state.saved_or_unsaved(id).map(|c| c.name.clone()))
        .unwrap_or_else(|| id.to_owned())
}

fn panels(s: &Sources, out: &mut Vec<PaletteItem>) {
    let ws = s.ws();
    let state = s.app_state();
    for panel in &ws.layout.panels {
        let Some(view) = ws.view(panel.id) else {
            continue;
        };
        let title = view.title(s.cx);
        let detail = match &panel.kind {
            PanelKind::Table { connection, .. } => Some(connection_name(state, connection)),
            PanelKind::Query { connection, text } => {
                let line = first_line(text).or_else(|| match view {
                    PanelView::Query(q) => first_line(&q.read(s.cx).text(s.cx)),
                    _ => None,
                });
                let conn = connection.as_deref().map(|c| connection_name(state, c));
                match (conn, line) {
                    (Some(c), Some(l)) => Some(format!("{c} · {l}")),
                    (c, l) => c.or(l),
                }
            }
            _ => None,
        };
        let (ws, id) = (s.workspace.clone(), panel.id);
        let mut item = PaletteItem::new(
            format!("panel:{}", id.0),
            Kind::Panel,
            view.icon(s.cx),
            title,
            move |_, cx| ws.update(cx, |w, cx| w.show(id, cx)),
        );
        if let Some(d) = detail {
            item = item.search(d.clone()).detail(d);
        }
        out.push(item);
    }
}

fn connections(s: &Sources, out: &mut Vec<PaletteItem>) {
    let state = s.app_state();
    let mut entries: Vec<(String, String, &'static str)> = state
        .settings
        .connections
        .iter()
        .map(|c| (c.id.clone(), c.name.clone(), c.kind.label()))
        .collect();
    // Connections opened from a link last until Deltos quits; they are listed as not
    // saved, so nobody takes one for a connection that will be there next time.
    entries.extend(
        state
            .unsaved
            .iter()
            .filter(|u| state.settings.connection(&u.id).is_none())
            .map(|u| (u.id.clone(), u.name.clone(), u.kind.label())),
    );
    if state.demo || state.session(crate::state::DEMO_ID).is_some() {
        entries.push((crate::state::DEMO_ID.into(), "Demo".into(), "Built-in"));
    }
    for (id, name, kind) in entries {
        let open = state.session(&id).is_some();
        let unsaved = state.settings.connection(&id).is_none() && id != crate::state::DEMO_ID;
        let (app, ws, conn) = (s.state.clone(), s.workspace.clone(), id.clone());
        let detail = connection_detail(kind, open, unsaved);
        out.push(
            PaletteItem::new(
                format!("conn:{id}"),
                Kind::Connection,
                Icon::Plug,
                name.clone(),
                move |_, cx| {
                    let connected = app.read(cx).session(&conn).is_some();
                    app.update(cx, |s, cx| s.connect(&conn, cx));
                    if connected && !ws.read(cx).is_open(&PanelKind::Objects) {
                        ws.update(cx, |w, cx| w.open(PanelKind::Objects, true, cx));
                    }
                },
            )
            .detail(detail),
        );
        if open {
            let (app, conn) = (s.state.clone(), id.clone());
            out.push(
                PaletteItem::new(
                    format!("disconnect:{id}"),
                    Kind::Connection,
                    Icon::Plug,
                    format!("Disconnect {name}"),
                    move |_, cx| app.update(cx, |s, cx| s.disconnect(&conn, cx)),
                )
                .detail(kind),
            );
        }
    }
}

/// What a connection's palette entry says under its name.
fn connection_detail(kind: &str, open: bool, unsaved: bool) -> String {
    let mut out = kind.to_owned();
    if unsaved {
        out.push_str(" · not saved");
    }
    if open {
        out.push_str(" · connected");
    }
    out
}

fn object_icon(kind: ObjectKind) -> Icon {
    match kind {
        ObjectKind::View | ObjectKind::MaterializedView => Icon::View,
        ObjectKind::Collection => Icon::Collection,
        _ => Icon::Table,
    }
}

fn objects(s: &Sources, out: &mut Vec<PaletteItem>) {
    let state = s.app_state();
    let several = state.sessions.len() > 1;
    for session in &state.sessions {
        for ns in &session.namespaces {
            let Some(objects) = &ns.objects else {
                continue;
            };
            let path = ns.namespace.path.join(".");
            for entry in objects.iter().filter(|o| o.object.kind.has_rows()) {
                let object = entry.object.clone();
                let detail = if several {
                    format!("{path} · {}", session.name)
                } else {
                    path.clone()
                };
                let (app, ws, conn) = (s.state.clone(), s.workspace.clone(), session.id.clone());
                let id = format!("object:{}:{path}:{}", session.id, object.name);
                let search = format!("{path}.{}", object.name);
                let icon = object_icon(object.kind);
                let title = object.name.clone();
                out.push(
                    PaletteItem::new(id, Kind::Object, icon, title, move |_, cx| {
                        app.update(cx, |s, cx| s.set_active(&conn, cx));
                        let kind = PanelKind::Table {
                            connection: conn.clone(),
                            object: object.clone(),
                        };
                        // Opened for good, as Enter in the sidebar's search did.
                        ws.update(cx, |w, cx| w.open(kind, true, cx));
                    })
                    .detail(detail)
                    .search(search),
                );
            }
        }
    }
}

/// The connection a saved query belongs to, by name, or that it is for all of them.
fn saved_query_detail(connection: Option<&str>, name: impl Fn(&str) -> String) -> String {
    match connection {
        Some(id) => name(id),
        None => "All connections".to_owned(),
    }
}

/// Saved queries; running one opens it in a new query tab, as the History panel does.
fn saved_queries(s: &Sources, out: &mut Vec<PaletteItem>) {
    let state = s.app_state();
    for snippet in crate::snippets::all_snippets(s.cx) {
        let detail = saved_query_detail(snippet.connection.as_deref(), |id| {
            connection_name(state, id)
        });
        let id = format!("snippet:{}", snippet.id);
        let title = snippet.name.clone();
        let app = s.state.clone();
        out.push(
            PaletteItem::new(id, Kind::SavedQuery, Icon::Code, title, move |_, cx| {
                crate::snippets::open(&app, &snippet, cx)
            })
            .search(format!("saved query {detail}"))
            .detail(detail),
        );
    }
}

fn settings_pages(s: &Sources, out: &mut Vec<PaletteItem>) {
    for section in Section::ALL {
        let ws = s.workspace.clone();
        out.push(
            PaletteItem::new(
                format!("settings:{}", section.label().to_lowercase()),
                Kind::Setting,
                section.icon(),
                format!("Settings: {}", section.label()),
                move |_, cx| {
                    show_settings(&ws, section, cx);
                },
            )
            .search("preferences"),
        );
    }
}

/// Opens Settings, or shows it, at `section`.
pub fn show_settings(
    workspace: &Entity<Workspace>,
    section: Section,
    cx: &mut App,
) -> Option<Entity<SettingsPanel>> {
    let settings = workspace.update(cx, |w, cx| {
        w.open(PanelKind::Settings, true, cx);
        w.layout
            .panels
            .iter()
            .find(|p| p.kind == PanelKind::Settings)
            .and_then(|p| w.view(p.id))
            .and_then(|v| match v {
                PanelView::Settings(e) => Some(e.clone()),
                _ => None,
            })
    })?;
    settings.update(cx, |p, cx| p.show_section(section, cx));
    Some(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_read_naturally() {
        assert_eq!(pretty_keys("ctrl-shift-p"), "Ctrl+Shift+P");
        assert_eq!(pretty_keys("ctrl-k"), "Ctrl+K");
        assert_eq!(pretty_keys("ctrl--"), "Ctrl+-");
        assert_eq!(pretty_keys("ctrl-="), "Ctrl+=");
        assert_eq!(pretty_keys("ctrl-,"), "Ctrl+,");
        assert_eq!(pretty_keys("ctrl-shift-tab"), "Ctrl+Shift+Tab");
        assert_eq!(pretty_keys("ctrl-k ctrl-s"), "Ctrl+K Ctrl+S");
        assert_eq!(pretty_keys("f5"), "F5");
        assert_eq!(pretty_keys("-"), "-");
        assert_eq!(pretty_keys(""), "");
        assert_eq!(pretty_keys("alt-up"), "Alt+↑");
    }

    #[test]
    fn prefixes_pick_the_scope() {
        assert_eq!(parse_query(">zoom"), (Scope::Commands, "zoom"));
        assert_eq!(parse_query("@ orders "), (Scope::Objects, "orders"));
        assert_eq!(parse_query("#prod"), (Scope::Connections, "prod"));
        assert_eq!(parse_query("  users"), (Scope::All, "users"));
        assert_eq!(parse_query(""), (Scope::All, ""));
        assert_eq!(parse_query("a>b"), (Scope::All, "a>b"));
        assert!(Scope::Commands.admits(Kind::Action));
        assert!(!Scope::Commands.admits(Kind::Object));
        assert!(Scope::Objects.admits(Kind::Object));
        assert!(Scope::All.admits(Kind::Panel));
    }

    #[test]
    fn query_tabs_show_their_first_line() {
        assert_eq!(
            first_line("\n  select 1;\nselect 2"),
            Some("select 1;".into())
        );
        assert_eq!(first_line("   \n"), None);
        let long = "x".repeat(80);
        assert_eq!(first_line(&long).map(|l| l.chars().count()), Some(61));
    }

    #[test]
    fn connection_entries_say_when_not_saved() {
        assert_eq!(connection_detail("PostgreSQL", false, false), "PostgreSQL");
        assert_eq!(
            connection_detail("PostgreSQL", true, false),
            "PostgreSQL · connected"
        );
        assert_eq!(
            connection_detail("SQLite", true, true),
            "SQLite · not saved · connected"
        );
    }

    #[test]
    fn saved_queries_name_their_connection() {
        let name = |id: &str| format!("name of {id}");
        assert_eq!(saved_query_detail(Some("prod"), name), "name of prod");
        assert_eq!(saved_query_detail(None, name), "All connections");
        assert_eq!(Kind::SavedQuery.label(), "Saved query");
        assert!(Scope::All.admits(Kind::SavedQuery));
        assert!(!Scope::Commands.admits(Kind::SavedQuery));
        assert!(!Scope::Connections.admits(Kind::SavedQuery));
    }

    #[test]
    fn every_palette_action_has_a_keyboard_entry() {
        for (name, _) in ACTIONS {
            assert!(
                KEYBIND_ACTIONS.iter().any(|(a, _, _)| a == name),
                "{name} is not in the keyboard settings"
            );
        }
    }
}
