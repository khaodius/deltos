//! The saved panel layout: the dock trees of every window, what each panel shows, and
//! where the main window was.

use deltos_data::ObjectRef;
use deltos_dock::{Axis, Child, Layout, Node, PanelId, Rect, Stack};
use serde::{Deserialize, Serialize};

/// What a panel shows. Panels that refer to a connection name it by its saved id, so a
/// restored panel can reconnect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PanelKind {
    /// The saved connections and the form to add one.
    Connections,
    /// The tree of namespaces and objects of the active connection.
    Objects,
    /// The selected row's fields, one per line.
    Inspector,
    /// Statements that ran, with timings and errors.
    History,
    /// One table, view or collection: its rows and its structure.
    Table {
        connection: String,
        object: ObjectRef,
    },
    /// A query editor with its results.
    Query {
        #[serde(default)]
        connection: Option<String>,
        #[serde(default)]
        text: String,
    },
    Settings,
}

impl PanelKind {
    /// Whether the app keeps at most one of this kind open.
    pub fn is_singleton(&self) -> bool {
        matches!(
            self,
            PanelKind::Connections
                | PanelKind::Objects
                | PanelKind::Inspector
                | PanelKind::History
                | PanelKind::Settings
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedPanel {
    pub id: PanelId,
    #[serde(flatten)]
    pub kind: PanelKind,
}

/// The main window's last bounds in screen pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowPlacement {
    pub bounds: Rect,
    #[serde(default)]
    pub maximized: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedLayout {
    pub panels: Vec<SavedPanel>,
    pub dock: Layout,
    pub main_window: Option<WindowPlacement>,
}

impl Default for SavedLayout {
    fn default() -> Self {
        Self::default_for(None)
    }
}

/// The ids the default layout gives its panels.
pub const OBJECTS_PANEL: PanelId = PanelId(1);
pub const CONNECTIONS_PANEL: PanelId = PanelId(2);
pub const INSPECTOR_PANEL: PanelId = PanelId(3);

impl SavedLayout {
    /// The layout Deltos starts with: objects on the left, the connection list in the
    /// middle where tables open as tabs, the inspector on the right.
    pub fn default_for(_connection: Option<&str>) -> Self {
        let stack = |p: PanelId, size: f32| Child {
            size,
            node: Node::Stack(Stack::new(p)),
        };
        let dock = Layout::new(Node::Split {
            axis: Axis::Row,
            children: vec![
                stack(OBJECTS_PANEL, 0.19),
                stack(CONNECTIONS_PANEL, 0.60),
                stack(INSPECTOR_PANEL, 0.21),
            ],
        });
        Self {
            panels: vec![
                SavedPanel {
                    id: OBJECTS_PANEL,
                    kind: PanelKind::Objects,
                },
                SavedPanel {
                    id: CONNECTIONS_PANEL,
                    kind: PanelKind::Connections,
                },
                SavedPanel {
                    id: INSPECTOR_PANEL,
                    kind: PanelKind::Inspector,
                },
            ],
            dock,
            main_window: None,
        }
    }

    pub fn kind(&self, id: PanelId) -> Option<&PanelKind> {
        self.panels.iter().find(|p| p.id == id).map(|p| &p.kind)
    }

    /// A panel id not used yet.
    pub fn next_id(&self) -> PanelId {
        PanelId(
            self.panels
                .iter()
                .map(|p| p.id.0 + 1)
                .max()
                .unwrap_or(1)
                .max(16),
        )
    }

    /// Keeps the panel list and the dock in step: panels without a place are forgotten,
    /// places without a panel are dropped, and a singleton kind is kept once.
    pub fn sanitize(&mut self) {
        self.dock.normalize();
        let placed = self.dock.panels();
        let mut seen_ids = Vec::new();
        let mut seen_singletons: Vec<PanelKind> = Vec::new();
        self.panels.retain(|p| {
            if !placed.contains(&p.id) || seen_ids.contains(&p.id) {
                return false;
            }
            if p.kind.is_singleton() {
                if seen_singletons.contains(&p.kind) {
                    return false;
                }
                seen_singletons.push(p.kind.clone());
            }
            seen_ids.push(p.id);
            true
        });
        self.dock.retain(|id| seen_ids.contains(&id));
        if self.panels.is_empty() {
            let main_window = self.main_window;
            *self = Self::default();
            self.main_window = main_window;
        }
        if let Some(w) = &self.main_window {
            let b = w.bounds;
            if !(b.x.is_finite() && b.y.is_finite() && b.w >= 200.0 && b.h >= 150.0) {
                self.main_window = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_dock::{DropTarget, Zone};

    #[test]
    fn default_is_valid() {
        let l = SavedLayout::default();
        l.dock.validate().unwrap();
        assert_eq!(l.dock.panels().len(), 3);
        assert_eq!(l.kind(INSPECTOR_PANEL), Some(&PanelKind::Inspector));
    }

    #[test]
    fn sanitize_drops_strays() {
        let mut l = SavedLayout::default();
        // A panel with no place, and a place with no panel.
        l.panels.push(SavedPanel {
            id: PanelId(40),
            kind: PanelKind::History,
        });
        l.dock
            .insert(
                PanelId(41),
                DropTarget::Stack {
                    anchor: OBJECTS_PANEL,
                    zone: Zone::Center,
                },
            )
            .unwrap();
        // A second objects panel.
        l.panels.push(SavedPanel {
            id: PanelId(42),
            kind: PanelKind::Objects,
        });
        l.dock
            .insert(
                PanelId(42),
                DropTarget::Stack {
                    anchor: OBJECTS_PANEL,
                    zone: Zone::Center,
                },
            )
            .unwrap();
        l.sanitize();
        assert_eq!(l.panels.len(), 3);
        assert_eq!(l.dock.panels().len(), 3);
        assert!(l.next_id().0 >= 16);
    }

    #[test]
    fn empty_layout_resets() {
        let mut l = SavedLayout {
            panels: Vec::new(),
            dock: Layout::default(),
            main_window: None,
        };
        l.sanitize();
        assert_eq!(l, SavedLayout::default());
    }
}
