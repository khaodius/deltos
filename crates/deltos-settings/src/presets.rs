//! Named arrangements of the main window's panels, and what is remembered of the last
//! session.
//!
//! A preset holds where the tool panels sit and one slot where the documents go (tables,
//! queries, the connection list, Settings). It holds no data: applying one keeps every
//! open document, gathered into that slot, and opens or closes tool panels to match.

use deltos_dock::{Axis, Child, Layout, Node, PanelId, Stack};
use serde::{Deserialize, Serialize};

use crate::layout::{PanelKind, SavedLayout, SavedPanel};

/// What a slot of a preset holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresetPanel {
    /// Every open document, as tabs.
    Documents,
    Objects,
    Inspector,
    History,
}

impl PresetPanel {
    /// The tool panel this slot opens; `None` for the documents.
    pub fn kind(self) -> Option<PanelKind> {
        match self {
            PresetPanel::Documents => None,
            PresetPanel::Objects => Some(PanelKind::Objects),
            PresetPanel::Inspector => Some(PanelKind::Inspector),
            PresetPanel::History => Some(PanelKind::History),
        }
    }

    fn of(kind: &PanelKind) -> Self {
        match kind {
            PanelKind::Objects => PresetPanel::Objects,
            PanelKind::Inspector => PresetPanel::Inspector,
            PanelKind::History => PresetPanel::History,
            _ => PresetPanel::Documents,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresetChild {
    pub size: f32,
    pub node: PresetNode,
}

/// A preset's tree: splits as in the dock, stacks of slots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PresetNode {
    Split {
        axis: Axis,
        children: Vec<PresetChild>,
    },
    Stack {
        panels: Vec<PresetPanel>,
        #[serde(default)]
        active: usize,
    },
}

impl PresetNode {
    fn stack(panel: PresetPanel) -> Self {
        PresetNode::Stack {
            panels: vec![panel],
            active: 0,
        }
    }

    fn split(axis: Axis, children: Vec<(f32, PresetNode)>) -> Self {
        PresetNode::Split {
            axis,
            children: children
                .into_iter()
                .map(|(size, node)| PresetChild { size, node })
                .collect(),
        }
    }

    fn has_documents(&self) -> bool {
        match self {
            PresetNode::Stack { panels, .. } => panels.contains(&PresetPanel::Documents),
            PresetNode::Split { children, .. } => children.iter().any(|c| c.node.has_documents()),
        }
    }

    /// The slots in reading order.
    pub fn panels(&self, out: &mut Vec<PresetPanel>) {
        match self {
            PresetNode::Stack { panels, .. } => out.extend_from_slice(panels),
            PresetNode::Split { children, .. } => children.iter().for_each(|c| c.node.panels(out)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutPreset {
    pub name: String,
    pub root: PresetNode,
    /// Brings a query tab to the front, opening one when none is open.
    #[serde(default)]
    pub query: bool,
}

/// Names of the presets that come with the app; they cannot be renamed or deleted.
pub const BUILT_IN_PRESETS: [&str; 3] = ["Default", "Focus", "Query"];

impl LayoutPreset {
    /// The presets that come with the app: the default arrangement, the documents alone,
    /// and a query beside the object sidebar.
    pub fn built_in() -> Vec<LayoutPreset> {
        vec![
            LayoutPreset {
                name: BUILT_IN_PRESETS[0].into(),
                root: PresetNode::split(
                    Axis::Row,
                    vec![
                        (0.19, PresetNode::stack(PresetPanel::Objects)),
                        (0.60, PresetNode::stack(PresetPanel::Documents)),
                        (0.21, PresetNode::stack(PresetPanel::Inspector)),
                    ],
                ),
                query: false,
            },
            LayoutPreset {
                name: BUILT_IN_PRESETS[1].into(),
                root: PresetNode::stack(PresetPanel::Documents),
                query: false,
            },
            LayoutPreset {
                name: BUILT_IN_PRESETS[2].into(),
                root: PresetNode::split(
                    Axis::Row,
                    vec![
                        (0.2, PresetNode::stack(PresetPanel::Objects)),
                        (0.8, PresetNode::stack(PresetPanel::Documents)),
                    ],
                ),
                query: true,
            },
        ]
    }

    /// The arrangement of `layout`'s main window. Documents count once, in the stack of
    /// `primary` (the document in front) or else the first stack that has one; a layout
    /// with no document in the main window gets a documents slot on the right.
    pub fn capture(
        name: impl Into<String>,
        layout: &SavedLayout,
        primary: Option<PanelId>,
    ) -> Self {
        let primary = primary
            .filter(|p| layout.kind(*p).is_some_and(PanelKind::is_document))
            .filter(|p| layout.dock.main.contains(*p))
            .or_else(|| {
                layout
                    .dock
                    .main
                    .panels()
                    .into_iter()
                    .find(|p| layout.kind(*p).is_some_and(PanelKind::is_document))
            });
        let root = layout
            .dock
            .main
            .root
            .as_ref()
            .and_then(|n| capture_node(n, layout, primary))
            .map(|n| tidy(n).unwrap_or_else(|| PresetNode::stack(PresetPanel::Documents)))
            .unwrap_or_else(|| PresetNode::stack(PresetPanel::Documents));
        let root = if root.has_documents() {
            root
        } else {
            PresetNode::split(
                Axis::Row,
                vec![
                    (0.4, root),
                    (0.6, PresetNode::stack(PresetPanel::Documents)),
                ],
            )
        };
        Self {
            name: name.into(),
            root,
            query: false,
        }
    }

    /// `current` rearranged by this preset. Tool panels go where the preset puts them,
    /// keeping their ids when they are open; tool panels the preset leaves out close.
    /// Every document stays open, in its order, in the documents slot, with `front` (or
    /// the one in front of the main window) showing; with none open, the connection list
    /// fills the slot. Panel windows are folded back in. The main window's placement is
    /// kept.
    pub fn arrange(&self, current: &SavedLayout, front: Option<PanelId>) -> SavedLayout {
        let order = current.dock.panels();
        let mut documents: Vec<SavedPanel> = order
            .iter()
            .filter_map(|id| current.panels.iter().find(|p| p.id == *id))
            .filter(|p| p.kind.is_document())
            .cloned()
            .collect();
        let mut next = current.next_id().0;
        let fresh = |next: &mut u64| {
            let id = PanelId(*next);
            *next += 1;
            id
        };
        if documents.is_empty() {
            documents.push(SavedPanel {
                id: fresh(&mut next),
                kind: PanelKind::Connections,
            });
        }
        let front = front
            .filter(|f| documents.iter().any(|d| d.id == *f))
            .or_else(|| {
                current
                    .dock
                    .main
                    .root
                    .as_ref()
                    .and_then(|r| front_document(r, current))
            });
        let mut panels = documents.clone();
        let mut tools: Vec<(PresetPanel, PanelId)> = Vec::new();
        let mut wanted = Vec::new();
        self.root.panels(&mut wanted);
        for slot in wanted {
            let Some(kind) = slot.kind() else {
                continue;
            };
            if tools.iter().any(|(s, _)| *s == slot) {
                continue;
            }
            let id = current
                .panels
                .iter()
                .find(|p| p.kind == kind)
                .map_or_else(|| fresh(&mut next), |p| p.id);
            tools.push((slot, id));
            panels.push(SavedPanel { id, kind });
        }
        let documents: Vec<PanelId> = documents.iter().map(|d| d.id).collect();
        let root = build_node(&self.root, &documents, front, &tools, &mut false);
        let mut out = SavedLayout {
            panels,
            dock: Layout::new(root),
            main_window: current.main_window,
        };
        out.sanitize();
        out
    }
}

/// The document showing in the first stack of `node` that shows one.
fn front_document(node: &Node, layout: &SavedLayout) -> Option<PanelId> {
    match node {
        Node::Stack(s) => s
            .active_panel()
            .filter(|p| layout.kind(*p).is_some_and(PanelKind::is_document)),
        Node::Split { children, .. } => children
            .iter()
            .find_map(|c| front_document(&c.node, layout)),
    }
}

fn capture_node(node: &Node, layout: &SavedLayout, primary: Option<PanelId>) -> Option<PresetNode> {
    match node {
        Node::Stack(s) => {
            let mut panels = Vec::new();
            let mut active = 0;
            for (i, id) in s.panels.iter().enumerate() {
                let Some(kind) = layout.kind(*id) else {
                    continue;
                };
                let slot = PresetPanel::of(kind);
                let keep = match slot {
                    PresetPanel::Documents => Some(*id) == primary,
                    _ => !panels.contains(&slot),
                };
                // The primary document's stack takes the slot; its other documents and
                // documents elsewhere go with it.
                let slot = if slot == PresetPanel::Documents && !keep {
                    continue;
                } else {
                    slot
                };
                if i == s.active {
                    active = panels.len();
                }
                panels.push(slot);
            }
            if panels.is_empty() {
                return None;
            }
            let active = active.min(panels.len() - 1);
            Some(PresetNode::Stack { panels, active })
        }
        Node::Split { axis, children } => {
            let children: Vec<PresetChild> = children
                .iter()
                .filter_map(|c| {
                    capture_node(&c.node, layout, primary)
                        .map(|node| PresetChild { size: c.size, node })
                })
                .collect();
            if children.is_empty() {
                return None;
            }
            Some(PresetNode::Split {
                axis: *axis,
                children,
            })
        }
    }
}

/// Folds splits left with one child into it, and keeps sizes summing to one.
fn tidy(node: PresetNode) -> Option<PresetNode> {
    match node {
        PresetNode::Stack { .. } => Some(node),
        PresetNode::Split { axis, children } => {
            let mut kept: Vec<PresetChild> = children
                .into_iter()
                .filter_map(|c| {
                    tidy(c.node).map(|node| PresetChild {
                        size: if c.size.is_finite() && c.size > 0.0 {
                            c.size
                        } else {
                            1.0
                        },
                        node,
                    })
                })
                .collect();
            match kept.len() {
                0 => None,
                1 => kept.pop().map(|c| c.node),
                _ => {
                    let total: f32 = kept.iter().map(|c| c.size).sum();
                    for c in &mut kept {
                        c.size /= total;
                    }
                    Some(PresetNode::Split {
                        axis,
                        children: kept,
                    })
                }
            }
        }
    }
}

/// The dock node for a preset node. Only the first documents slot gets the documents;
/// `placed` says whether that happened.
fn build_node(
    node: &PresetNode,
    documents: &[PanelId],
    front: Option<PanelId>,
    tools: &[(PresetPanel, PanelId)],
    placed: &mut bool,
) -> Node {
    match node {
        PresetNode::Split { axis, children } => Node::Split {
            axis: *axis,
            children: children
                .iter()
                .map(|c| Child {
                    size: c.size,
                    node: build_node(&c.node, documents, front, tools, placed),
                })
                .collect(),
        },
        PresetNode::Stack { panels, active } => {
            let mut ids = Vec::new();
            let mut front_ix = None;
            for (i, slot) in panels.iter().enumerate() {
                if *slot == PresetPanel::Documents {
                    if *placed {
                        continue;
                    }
                    *placed = true;
                    for d in documents {
                        if Some(*d) == front || (front.is_none() && i == *active) {
                            front_ix.get_or_insert(ids.len());
                        }
                        ids.push(*d);
                    }
                } else if let Some((_, id)) = tools.iter().find(|(s, _)| s == slot) {
                    if i == *active {
                        front_ix.get_or_insert(ids.len());
                    }
                    ids.push(*id);
                }
            }
            // An empty stack is dropped when the layout is normalized.
            Node::Stack(Stack {
                active: front_ix.unwrap_or(0),
                panels: ids,
            })
        }
    }
}

/// `name` trimmed, with a number added if a preset already has it.
pub fn unique_name(name: &str, taken: &[&str]) -> String {
    let base = name.trim();
    let base = if base.is_empty() { "Layout" } else { base };
    let clash = |n: &str| taken.iter().any(|t| t.eq_ignore_ascii_case(n));
    if !clash(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|n| !clash(n))
        .unwrap_or_else(|| base.to_owned())
}

/// Reads the presets of the settings file, skipping any that do not parse, so a preset
/// written by a newer version (with a panel this one does not know) leaves the rest of
/// the file loading.
pub(crate) fn lenient<'de, D>(deserializer: D) -> Result<Vec<LayoutPreset>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Preset(LayoutPreset),
        Unreadable(serde::de::IgnoredAny),
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum List {
        Entries(Vec<Entry>),
        Unreadable(serde::de::IgnoredAny),
    }
    Ok(match List::deserialize(deserializer)? {
        List::Entries(entries) => entries
            .into_iter()
            .filter_map(|e| match e {
                Entry::Preset(p) => Some(p),
                Entry::Unreadable(_) => None,
            })
            .collect(),
        List::Unreadable(_) => Vec::new(),
    })
}

/// How many presets of the user's are kept.
pub const PRESET_LIMIT: usize = 50;

/// Drops presets without a usable name, with a built-in or repeated name, or with no
/// documents slot (which a hand-edited file could give), and repairs their sizes.
pub(crate) fn sanitize(presets: &mut Vec<LayoutPreset>) {
    let mut seen: Vec<String> = BUILT_IN_PRESETS.iter().map(|n| n.to_lowercase()).collect();
    presets.retain_mut(|p| {
        p.name = p.name.trim().to_owned();
        let key = p.name.to_lowercase();
        if p.name.is_empty() || seen.contains(&key) || !p.root.has_documents() {
            return false;
        }
        match tidy(p.root.clone()) {
            Some(root) => p.root = root,
            None => return false,
        }
        seen.push(key);
        true
    });
    presets.truncate(PRESET_LIMIT);
}

/// What is remembered of the last session, besides the layout itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LastSession {
    /// Opens the last session's tabs and connections again at start.
    pub restore: bool,
    /// The saved connections that were open, by id.
    pub connections: Vec<String>,
    /// The one that was active.
    pub active: Option<String>,
}

impl Default for LastSession {
    fn default() -> Self {
        Self {
            restore: true,
            connections: Vec::new(),
            active: None,
        }
    }
}

impl SavedLayout {
    /// This layout's arrangement with no tables or queries open, for a start that does
    /// not restore the last session.
    pub fn without_documents(&self) -> SavedLayout {
        let preset = LayoutPreset::capture("", self, None);
        let mut base = self.clone();
        base.panels
            .retain(|p| !matches!(p.kind, PanelKind::Table { .. } | PanelKind::Query { .. }));
        let keep: Vec<PanelId> = base.panels.iter().map(|p| p.id).collect();
        base.dock.retain(|id| keep.contains(&id));
        preset.arrange(&base, None)
    }

    /// This layout with tables only of the connections `saved` says are kept, as if the
    /// others had been closed, and queries of any other connection left with their text
    /// but no connection. A connection opened from a link is never saved and never opens
    /// again by itself, so its tabs would come back attached to nothing.
    pub fn keeping_connections(&self, saved: impl Fn(&str) -> bool) -> SavedLayout {
        let mut out = self.clone();
        for p in &mut out.panels {
            if let PanelKind::Query { connection, .. } = &mut p.kind
                && connection.as_deref().is_some_and(|c| !saved(c))
            {
                *connection = None;
            }
        }
        out.panels.retain(|p| match &p.kind {
            PanelKind::Table { connection, .. } => saved(connection),
            _ => true,
        });
        let keep: Vec<PanelId> = out.panels.iter().map(|p| p.id).collect();
        out.dock.retain(|id| keep.contains(&id));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{CONNECTIONS_PANEL, INSPECTOR_PANEL, OBJECTS_PANEL};
    use deltos_data::{ObjectKind, ObjectRef};
    use deltos_dock::{DropTarget, Edge, Rect, TreeId, Zone};

    fn table(name: &str) -> PanelKind {
        PanelKind::Table {
            connection: "c".into(),
            object: ObjectRef {
                namespace: vec!["public".into()],
                name: name.into(),
                kind: ObjectKind::Table,
            },
        }
    }

    fn add(l: &mut SavedLayout, id: u64, kind: PanelKind, target: DropTarget) -> PanelId {
        let id = PanelId(id);
        l.panels.push(SavedPanel { id, kind });
        l.dock.insert(id, target).unwrap();
        id
    }

    fn beside(anchor: PanelId, zone: Zone) -> DropTarget {
        DropTarget::Stack { anchor, zone }
    }

    /// The default layout with two tables beside the connection list and history under
    /// them.
    fn sample() -> (SavedLayout, PanelId, PanelId) {
        let mut l = SavedLayout::default();
        let a = add(
            &mut l,
            20,
            table("a"),
            beside(CONNECTIONS_PANEL, Zone::Center),
        );
        let b = add(
            &mut l,
            21,
            table("b"),
            beside(CONNECTIONS_PANEL, Zone::Center),
        );
        add(
            &mut l,
            22,
            PanelKind::History,
            beside(a, Zone::Edge(Edge::Bottom)),
        );
        l.dock.activate(a);
        (l, a, b)
    }

    fn kinds(l: &SavedLayout) -> Vec<PanelKind> {
        l.dock
            .panels()
            .into_iter()
            .filter_map(|p| l.kind(p).cloned())
            .collect()
    }

    #[test]
    fn capture_keeps_tools_and_one_document_slot() {
        let (l, a, _) = sample();
        let p = LayoutPreset::capture("Mine", &l, Some(a));
        let mut slots = Vec::new();
        p.root.panels(&mut slots);
        assert_eq!(
            slots,
            [
                PresetPanel::Objects,
                PresetPanel::Documents,
                PresetPanel::History,
                PresetPanel::Inspector
            ]
        );
        // It survives the settings file.
        let text = toml::to_string(&p).unwrap();
        assert_eq!(toml::from_str::<LayoutPreset>(&text).unwrap(), p);
    }

    #[test]
    fn arrange_keeps_documents_and_ids() {
        let (l, a, b) = sample();
        let focus = &LayoutPreset::built_in()[1];
        let out = focus.arrange(&l, Some(b));
        out.dock.validate().unwrap();
        // Only documents, in their order, with b in front.
        assert_eq!(out.dock.panels(), [CONNECTIONS_PANEL, a, b]);
        assert_eq!(out.dock.stack_of(a).and_then(|s| s.active_panel()), Some(b));
        assert_eq!(out.panels.len(), 3);

        // Back to the default: the tools come back, the open ones with their ids.
        let default = &LayoutPreset::built_in()[0];
        let back = default.arrange(&l, None);
        back.dock.validate().unwrap();
        assert!(back.kind(OBJECTS_PANEL) == Some(&PanelKind::Objects));
        assert!(back.kind(INSPECTOR_PANEL) == Some(&PanelKind::Inspector));
        assert!(!kinds(&back).contains(&PanelKind::History));
        // The table that was in front stays in front.
        assert_eq!(
            back.dock.stack_of(a).and_then(|s| s.active_panel()),
            Some(a)
        );
        assert_eq!(back.dock.main.panels().len(), 5);
    }

    #[test]
    fn arrange_opens_missing_tools_with_new_ids() {
        let mut l = SavedLayout::default();
        l.dock.remove(OBJECTS_PANEL);
        l.sanitize();
        let out = LayoutPreset::built_in()[0].arrange(&l, None);
        out.dock.validate().unwrap();
        let objects = out
            .panels
            .iter()
            .find(|p| p.kind == PanelKind::Objects)
            .map(|p| p.id)
            .unwrap();
        assert!(objects.0 >= 16, "a fresh id, not one in use");
        assert_eq!(out.dock.main.panels().len(), 3);
    }

    #[test]
    fn arrange_with_no_documents_shows_the_connection_list() {
        let mut l = SavedLayout::default();
        l.dock.remove(CONNECTIONS_PANEL);
        l.sanitize();
        let out = LayoutPreset::built_in()[1].arrange(&l, None);
        assert_eq!(kinds(&out), [PanelKind::Connections]);
    }

    #[test]
    fn arrange_folds_panel_windows_back() {
        let (mut l, a, b) = sample();
        let w = l
            .dock
            .detach(
                b,
                Some(Rect {
                    x: 0.,
                    y: 0.,
                    w: 400.,
                    h: 300.,
                }),
            )
            .unwrap();
        assert!(l.dock.window(w).is_some());
        let out = LayoutPreset::built_in()[0].arrange(&l, Some(a));
        assert!(out.dock.floating.is_empty());
        assert!(out.dock.main.contains(b));
        assert_eq!(out.dock.find(b).map(|f| f.tree), Some(TreeId::Main));
    }

    #[test]
    fn captured_presets_round_trip_through_arrange() {
        let (l, a, _) = sample();
        let p = LayoutPreset::capture("Mine", &l, Some(a));
        let out = p.arrange(&l, Some(a));
        out.dock.validate().unwrap();
        assert_eq!(out.dock.main.panels().len(), l.dock.main.panels().len());
        assert!(kinds(&out).contains(&PanelKind::History));
    }

    #[test]
    fn tabs_of_connections_not_kept_are_not_restored() {
        let (mut l, a, b) = sample();
        if let Some(p) = l.panels.iter_mut().find(|p| p.id == b) {
            p.kind = PanelKind::Table {
                connection: "link:1".into(),
                object: ObjectRef {
                    namespace: vec!["public".into()],
                    name: "b".into(),
                    kind: ObjectKind::Table,
                },
            };
        }
        let q = add(
            &mut l,
            30,
            PanelKind::Query {
                connection: Some("link:1".into()),
                text: "select 1".into(),
            },
            beside(a, Zone::Center),
        );
        let out = l.keeping_connections(|id| id == "c");
        assert!(out.dock.validate().is_ok());
        assert!(out.kind(a).is_some() && out.dock.contains(a));
        assert!(out.kind(b).is_none() && !out.dock.contains(b));
        assert_eq!(
            out.kind(q),
            Some(&PanelKind::Query {
                connection: None,
                text: "select 1".into(),
            })
        );
        // Nothing changes when every connection is kept.
        assert_eq!(l.keeping_connections(|_| true), l);
    }

    #[test]
    fn a_layout_without_documents_gets_a_slot() {
        let mut l = SavedLayout::default();
        l.dock.remove(CONNECTIONS_PANEL);
        l.sanitize();
        let p = LayoutPreset::capture("Tools", &l, None);
        assert!(p.root.has_documents());
    }

    #[test]
    fn starting_without_the_session_keeps_the_arrangement() {
        let (l, _, _) = sample();
        let out = l.without_documents();
        out.dock.validate().unwrap();
        let k = kinds(&out);
        assert!(k.contains(&PanelKind::History));
        assert!(k.contains(&PanelKind::Connections));
        assert!(!k.iter().any(|k| matches!(k, PanelKind::Table { .. })));
    }

    #[test]
    fn names_stay_unique() {
        let taken = ["Default", "Focus", "Mine", "Mine 2"];
        assert_eq!(unique_name("  Wide ", &taken), "Wide");
        assert_eq!(unique_name("mine", &taken), "mine 3");
        assert_eq!(unique_name("default", &taken), "default 2");
        assert_eq!(unique_name("   ", &taken), "Layout");
    }

    #[test]
    fn hand_edited_presets_are_repaired() {
        let mut list = vec![
            LayoutPreset::built_in()[0].clone(),
            LayoutPreset::capture(" Mine ", &SavedLayout::default(), None),
            LayoutPreset::capture("mine", &SavedLayout::default(), None),
            LayoutPreset {
                name: "No documents".into(),
                root: PresetNode::stack(PresetPanel::Objects),
                query: false,
            },
            LayoutPreset {
                name: "Odd sizes".into(),
                root: PresetNode::split(
                    Axis::Row,
                    vec![
                        (f32::NAN, PresetNode::stack(PresetPanel::Objects)),
                        (3.0, PresetNode::stack(PresetPanel::Documents)),
                    ],
                ),
                query: false,
            },
        ];
        sanitize(&mut list);
        let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Mine", "Odd sizes"]);
        let PresetNode::Split { children, .. } = &list[1].root else {
            panic!("still a split");
        };
        let total: f32 = children.iter().map(|c| c.size).sum();
        assert!((total - 1.0).abs() < 1e-4);
    }

    #[test]
    fn unreadable_presets_are_skipped_not_the_file() {
        // A preset from a newer version, with a slot this one does not know, beside one
        // it can read.
        let text = r#"
[[connections]]
id = "shop"
name = "Shop"
kind = "sqlite"

[[layout_presets]]
name = "Newer"
[layout_presets.root]
type = "stack"
panels = ["documents", "someday"]

[[layout_presets]]
name = "Plain"
[layout_presets.root]
type = "stack"
panels = ["documents"]
"#;
        let loaded = crate::Settings::from_toml(text).unwrap();
        assert_eq!(loaded.connections.len(), 1);
        let names: Vec<&str> = loaded
            .layout_presets
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["Plain"]);
        // Not a list at all.
        let odd = crate::Settings::from_toml("layout_presets = 3").unwrap();
        assert!(odd.layout_presets.is_empty());
        // Files from before presets load as before.
        let old = crate::Settings::from_toml("[editor]\n").unwrap();
        assert!(old.layout_presets.is_empty());
        assert!(old.session.restore);
        // And saved presets come back.
        let mut s = crate::Settings::default();
        s.layout_presets
            .push(LayoutPreset::capture("Mine", &SavedLayout::default(), None));
        let back = crate::Settings::from_toml(&s.to_toml().unwrap()).unwrap();
        assert_eq!(back.layout_presets, s.layout_presets);
    }

    #[test]
    fn session_defaults_to_restoring() {
        let s: LastSession = toml::from_str("").unwrap();
        assert!(s.restore);
        assert!(s.connections.is_empty());
    }
}
