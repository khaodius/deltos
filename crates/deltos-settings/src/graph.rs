//! What the graph view remembers: where it looked on each connection, and the panel its
//! Back button returns to.

use deltos_dock::PanelId;
use deltos_graph::SavedView;
use serde::{Deserialize, Serialize};

/// Connections whose last view is kept.
pub const GRAPH_VIEWS_KEPT: usize = 32;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphMemory {
    /// The panel the graph was opened from, which Back shows again.
    pub return_to: Option<PanelId>,
    /// The last view of each connection, most recent first. An entry this version cannot
    /// read is skipped rather than costing the whole file.
    #[serde(deserialize_with = "lenient")]
    pub views: Vec<GraphPlace>,
}

/// One connection's last view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphPlace {
    pub connection: String,
    pub view: SavedView,
}

impl GraphMemory {
    pub fn view(&self, connection: &str) -> Option<&SavedView> {
        self.views
            .iter()
            .find(|p| p.connection == connection)
            .map(|p| &p.view)
    }

    /// Keeps `view` as the last one of `connection`.
    pub fn remember(&mut self, connection: &str, view: SavedView) {
        self.views.retain(|p| p.connection != connection);
        self.views.insert(
            0,
            GraphPlace {
                connection: connection.to_owned(),
                view,
            },
        );
        self.views.truncate(GRAPH_VIEWS_KEPT);
    }

    pub(crate) fn sanitize(&mut self) {
        let mut seen: Vec<String> = Vec::new();
        self.views.retain(|p| {
            let keep = !p.connection.is_empty()
                && !seen.contains(&p.connection)
                && p.view.zoom.is_finite()
                && p.view.dx.is_finite()
                && p.view.dy.is_finite();
            seen.push(p.connection.clone());
            keep
        });
        self.views.truncate(GRAPH_VIEWS_KEPT);
    }
}

fn lenient<'de, D>(deserializer: D) -> Result<Vec<GraphPlace>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Place(GraphPlace),
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
                Entry::Place(p) => Some(p),
                Entry::Unreadable(_) => None,
            })
            .collect(),
        List::Unreadable(_) => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_graph::{Anchor, Focus};

    fn view(zoom: f32) -> SavedView {
        SavedView {
            anchor: Anchor::Origin,
            dx: 1.0,
            dy: 2.0,
            zoom,
            selected: Some(Focus::Overview),
        }
    }

    #[test]
    fn remembers_one_view_per_connection_newest_first() {
        let mut m = GraphMemory::default();
        m.remember("a", view(0.5));
        m.remember("b", view(0.6));
        m.remember("a", view(0.7));
        assert_eq!(m.views.len(), 2);
        assert_eq!(m.views[0].connection, "a");
        assert_eq!(m.view("a").unwrap().zoom, 0.7);
        for i in 0..100 {
            m.remember(&format!("c{i}"), view(1.0));
        }
        assert_eq!(m.views.len(), GRAPH_VIEWS_KEPT);
    }

    #[test]
    fn unreadable_entries_are_skipped() {
        let text = r#"
return_to = 40

[[views]]
connection = "a"
view = { anchor = { type = "origin" }, dx = 1.0, dy = 2.0, zoom = 0.5 }

[[views]]
connection = "b"
view = { anchor = { type = "wormhole" }, dx = 1.0, dy = 2.0, zoom = 0.5 }
"#;
        let m: GraphMemory = toml::from_str(text).unwrap();
        assert_eq!(m.return_to, Some(PanelId(40)));
        assert_eq!(m.views.len(), 1);
        assert_eq!(m.views[0].connection, "a");
        let mut bad: GraphMemory = toml::from_str("views = 3").unwrap();
        assert!(bad.views.is_empty());
        bad.views.push(GraphPlace {
            connection: "x".into(),
            view: view(f32::NAN),
        });
        bad.sanitize();
        assert!(bad.views.is_empty());
    }
}
