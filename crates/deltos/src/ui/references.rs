//! Following foreign keys in the inspector. From a table's row, a key leads to the row it
//! refers to, which the inspector then shows under a trail of the rows passed through:
//! Back returns, and a key of the row shown leads further. Each row is fetched in the
//! background with one browse of the referenced table, filtered on the key, limit 1.

use std::sync::Arc;

use deltos_data::{BrowseRequest, Column, Filter, FilterOp, ForeignKey, Value};
use deltos_settings::PanelKind;
use gpui::{
    AnyElement, App, Entity, Focusable, FontWeight, Global, IntoElement, SharedString, Task,
    Window, div, prelude::*, px,
};

use crate::net::Net;
use crate::state::{AppState, Inspected};
use crate::theme::{Theme, theme};
use crate::ui::icons::{Icon, icon};
use crate::ui::inspector::full_text;
use crate::ui::text_input::TextInput;
use crate::ui::{ButtonStyle, Tooltip, button, empty_state, icon_button};

/// Rows kept in a trail; following further drops the oldest.
const MAX_STEPS: usize = 32;

/// One foreign key followed from one row.
#[derive(Debug, Clone, PartialEq)]
pub struct Hop {
    pub connection: String,
    /// The key, as the table it starts from declares it.
    pub key: ForeignKey,
    /// The row's values in the key's columns, in key order.
    pub values: Vec<Value>,
}

impl Hop {
    /// The conditions that find the rows the key leads to.
    pub fn filters(&self) -> Vec<Filter> {
        self.key
            .target_columns
            .iter()
            .zip(&self.values)
            .map(|(c, v)| Filter::new(c.clone(), FilterOp::Eq, v.clone()))
            .collect()
    }

    /// The key's target and values in words, such as `id = 42`.
    pub fn condition(&self) -> String {
        let value = |v: &Value| match v {
            Value::Text(s) => format!("'{}'", s.chars().take(40).collect::<String>()),
            v => v.preview(40),
        };
        match (self.key.target_columns.as_slice(), self.values.as_slice()) {
            ([c], [v]) => format!("{c} = {}", value(v)),
            (columns, values) => format!(
                "({}) = ({})",
                columns.join(", "),
                values.iter().map(value).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

/// Where each of `names` is among `columns`; `None` when one is missing.
pub fn positions(columns: &[Column], names: &[String]) -> Option<Vec<usize>> {
    if names.is_empty() {
        return None;
    }
    names
        .iter()
        .map(|n| columns.iter().position(|c| &c.name == n))
        .collect()
}

/// The values at `positions` in a row, or `None` when one is NULL or missing: a key with a
/// NULL in it points nowhere.
pub fn values_at(row: &[Value], positions: &[usize]) -> Option<Vec<Value>> {
    positions
        .iter()
        .map(|&i| row.get(i).filter(|v| !v.is_null()).cloned())
        .collect()
}

/// A shortcut from the settings as a person reads it: `ctrl-enter` becomes `Ctrl+Enter`.
pub fn shortcut_label(keys: &str) -> String {
    let first = keys.split_whitespace().next().unwrap_or_default();
    first
        .split('-')
        .filter(|p| !p.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_uppercase().chain(chars).collect::<String>()
            })
        })
        .collect::<Vec<_>>()
        .join("+")
}

enum Found {
    Loading,
    Row {
        columns: Arc<[Column]>,
        values: Vec<Value>,
        /// The row's own keys whose columns are all there, with their positions.
        keys: Vec<(ForeignKey, Vec<usize>)>,
    },
    Missing,
    Failed(String),
}

struct Step {
    hop: Hop,
    found: Found,
}

/// The rows passed through, from the inspected row the first key was followed from.
#[derive(Default)]
pub struct Trail {
    /// The inspected row it started from: its grid's columns and its index.
    origin: Option<(Arc<[Column]>, u64)>,
    steps: Vec<Step>,
    /// Bumped whenever the steps change, so a fetch that lands late is dropped.
    generation: u64,
    _fetch: Option<Task<()>>,
}

impl Global for Trail {}

impl Trail {
    fn starts_at(&self, row: &Inspected) -> bool {
        self.origin
            .as_ref()
            .is_some_and(|(columns, index)| Arc::ptr_eq(columns, &row.columns) && *index == row.row)
    }
}

fn update(cx: &mut App, f: impl FnOnce(&mut Trail)) {
    cx.default_global::<Trail>();
    cx.update_global::<Trail, _>(|trail, _| f(trail));
}

/// Follows `hop` from the inspected row `from`, starting a new trail, and shows the
/// inspector.
pub fn follow(state: &Entity<AppState>, from: &Inspected, hop: Hop, cx: &mut App) {
    let origin = (from.columns.clone(), from.row);
    update(cx, |t| {
        t.origin = Some(origin);
        t.steps.clear();
    });
    push(state, hop, cx);
    state.update(cx, |s, cx| s.open(PanelKind::Inspector, false, cx));
}

/// Follows `hop` from the row the trail shows now.
fn push(state: &Entity<AppState>, hop: Hop, cx: &mut App) {
    update(cx, |t| {
        if t.steps.len() >= MAX_STEPS {
            t.steps.remove(0);
        }
        t.steps.push(Step {
            hop,
            found: Found::Loading,
        });
        t.generation += 1;
    });
    fetch(state, cx);
}

/// Keeps the first `n` steps; none goes back to the inspected row.
fn truncate(n: usize, cx: &mut App) {
    update(cx, |t| {
        if n < t.steps.len() {
            t.steps.truncate(n);
            t.generation += 1;
            t._fetch = None;
        }
    });
}

/// Loads the last step's row.
fn fetch(state: &Entity<AppState>, cx: &mut App) {
    let Some((hop, generation)) = cx
        .try_global::<Trail>()
        .and_then(|t| Some((t.steps.last()?.hop.clone(), t.generation)))
    else {
        return;
    };
    let land = move |found: Found, cx: &mut App| {
        update(cx, |t| {
            if t.generation == generation
                && let Some(step) = t.steps.last_mut()
            {
                step.found = found;
            }
        });
    };
    let Some(conn) = state
        .read(cx)
        .session(&hop.connection)
        .map(|s| s.conn.clone())
    else {
        land(Found::Failed("The connection is closed.".into()), cx);
        return;
    };
    let request = BrowseRequest {
        limit: 1,
        filters: hop.filters(),
        ..BrowseRequest::default()
    };
    let target = hop.key.target.clone();
    let job = cx.global::<Net>().run(async move {
        let page = conn.browse(&target, &request).await?;
        // The row's own keys, to follow further. A row whose structure cannot be read
        // still shows, without them.
        let keys = match conn.describe(&target).await {
            Ok(info) => info.foreign_keys,
            Err(e) => {
                tracing::debug!("could not read the keys of {}: {e}", target.name);
                Vec::new()
            }
        };
        Ok::<_, deltos_data::Error>((page, keys))
    });
    let task = cx.spawn(async move |cx| {
        let found = match job.await {
            None => Found::Failed("Cancelled.".into()),
            Some(Err(e)) => Found::Failed(e.to_string()),
            Some(Ok((page, keys))) => match page.rows.into_iter().next() {
                None => Found::Missing,
                Some(row) => Found::Row {
                    keys: keys
                        .into_iter()
                        .filter_map(|k| {
                            let at = positions(&page.columns, &k.columns)?;
                            (k.columns.len() == k.target_columns.len()).then_some((k, at))
                        })
                        .collect(),
                    columns: page.columns,
                    values: row.into_vec(),
                },
            },
        };
        cx.update(|cx| land(found, cx)).ok();
    });
    update(cx, |t| t._fetch = Some(task));
}

/// The inspector's content while it follows keys from `row`, or `None` when it shows the
/// row itself.
pub fn view(
    state: &Entity<AppState>,
    row: &Inspected,
    filter: &Entity<TextInput>,
    window: &Window,
    cx: &App,
) -> Option<AnyElement> {
    let trail = cx.try_global::<Trail>()?;
    if !trail.starts_at(row) {
        return None;
    }
    let step = trail.steps.last()?;
    let t = theme(cx);
    let n = trail.steps.len();

    // Back, and the rows passed through: each one before the last goes back to it.
    let mut crumbs = div()
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap(px(2.))
        .overflow_hidden()
        .text_size(t.small_size);
    let names = std::iter::once(row.title.as_str())
        .chain(trail.steps.iter().map(|s| s.hop.key.target.name.as_str()));
    for (i, name) in names.enumerate() {
        if i > 0 {
            crumbs = crumbs.child(
                icon(Icon::ChevronRight)
                    .flex_none()
                    .size(px(10.))
                    .text_color(t.text_faint),
            );
        }
        let crumb = div()
            .id(("crumb", i))
            .flex_none()
            .px(px(3.))
            .rounded(t.radius_small)
            .child(name.to_owned());
        crumbs = crumbs.child(if i == n {
            crumb.font_weight(FontWeight::SEMIBOLD).text_color(t.text)
        } else {
            crumb
                .text_color(t.text_muted)
                .cursor_pointer()
                .hover(|s| s.bg(t.hover).text_color(t.text))
                .on_click(move |_, _, cx| truncate(i, cx))
        });
    }
    let header = div()
        .flex_none()
        .px(px(6.))
        .pt(px(6.))
        .pb(px(4.))
        .flex()
        .items_center()
        .gap(px(4.))
        .child(
            icon_button("trail-back", Icon::ChevronLeft, t)
                .tooltip(|_, cx| Tooltip::view("Back", cx))
                .on_click(move |_, _, cx| truncate(n - 1, cx)),
        )
        .child(crumbs);
    let open = {
        let state = state.clone();
        let hop = step.hop.clone();
        button("trail-open", "Open table", ButtonStyle::Ghost, t)
            .h(t.s(22.))
            .px(px(6.))
            .text_size(t.small_size)
            .tooltip(|_, cx| Tooltip::view("Open the table filtered to this row", cx))
            .on_click(move |_, _, cx| {
                let filters: Vec<(String, Value)> = hop
                    .key
                    .target_columns
                    .iter()
                    .cloned()
                    .zip(hop.values.iter().cloned())
                    .collect();
                crate::ui::table::open_filtered(
                    &state,
                    &hop.connection,
                    &hop.key.target,
                    &filters,
                    cx,
                );
            })
    };
    let subtitle = div()
        .flex_none()
        .px(px(10.))
        .pb(px(6.))
        .flex()
        .items_center()
        .gap(px(6.))
        .text_size(t.small_size)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(t.text_faint)
                .child(step.hop.condition()),
        )
        .when(matches!(step.found, Found::Row { .. }), |el| el.child(open));

    let body = match &step.found {
        Found::Loading => empty_state("Loading…", None, t).into_any_element(),
        Found::Missing => empty_state(
            "No row found",
            Some(
                format!(
                    "{} has no row where {}.",
                    step.hop.key.target.name,
                    step.hop.condition()
                )
                .into(),
            ),
            t,
        )
        .into_any_element(),
        Found::Failed(e) => div()
            .p(px(12.))
            .text_size(t.small_size)
            .text_color(t.danger)
            .child(e.clone())
            .into_any_element(),
        Found::Row {
            columns,
            values,
            keys,
        } => {
            let focused = filter.focus_handle(cx).is_focused(window);
            let needle = filter.read(cx).text().trim().to_lowercase();
            let mut fields = div().flex().flex_col().px(px(10.)).pb(px(12.)).gap(px(10.));
            for (c, column) in columns.iter().enumerate() {
                if !needle.is_empty() && !column.name.to_lowercase().contains(&needle) {
                    continue;
                }
                let value = values.get(c).cloned().unwrap_or_default();
                // The first key of this column whose values are all there leads further.
                let next = keys
                    .iter()
                    .filter(|(_, at)| at.contains(&c))
                    .find_map(|(key, at)| {
                        Some(Hop {
                            connection: step.hop.connection.clone(),
                            key: key.clone(),
                            values: values_at(values, at)?,
                        })
                    });
                fields = fields.child(field(c, column, &value, next, state, t));
            }
            // Only the inspected row's own fields edit, through its table's staged
            // changes; a row reached by a key is shown as it is in the database.
            let read_only = div()
                .flex_none()
                .px(px(10.))
                .pb(px(6.))
                .flex()
                .items_start()
                .gap(px(6.))
                .text_size(t.small_size)
                .text_color(t.text_faint)
                .child(icon(Icon::Lock).size(px(11.)).flex_none())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child("Read-only here. Open the table to edit this row."),
                );
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(read_only)
                .child(
                    div().flex_none().px(px(8.)).pb(px(8.)).child(
                        div()
                            .h(t.s(24.))
                            .px(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .rounded(t.radius_small)
                            .bg(t.input)
                            .border_1()
                            .border_color(if focused { t.accent } else { t.border })
                            .text_size(t.small_size)
                            .child(icon(Icon::Search).size(px(12.)).text_color(t.text_faint))
                            .child(filter.clone()),
                    ),
                )
                .child(
                    div()
                        .id("trail-fields")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(fields),
                )
                .into_any_element()
        }
    };
    Some(
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.sidebar)
            .child(header)
            .child(subtitle)
            .child(body)
            .into_any_element(),
    )
}

/// One field of a followed row, as the inspector draws fields, with an arrow on a column
/// of a key that leads further.
fn field(
    index: usize,
    column: &Column,
    value: &Value,
    next: Option<Hop>,
    state: &Entity<AppState>,
    t: &Theme,
) -> impl IntoElement {
    let null = value.is_null();
    let text: SharedString = if null {
        t.null_text.into()
    } else {
        full_text(value, column.kind).into()
    };
    let arrow = next.map(|hop| {
        let state = state.clone();
        let label = format!("Open the {} row", hop.key.target.name);
        icon_button(("trail-follow", index), Icon::ChevronRight, t)
            .size(t.s(16.))
            .tooltip(move |_, cx| Tooltip::view(label.clone(), cx))
            .on_click(move |_, _, cx| push(&state, hop.clone(), cx))
    });
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(t.small_size)
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.text_muted)
                        .child(column.name.clone()),
                )
                .when(column.primary_key.is_some(), |el| {
                    el.child(icon(Icon::Key).size(px(10.)).text_color(t.warning))
                })
                .children(arrow)
                .child(div().flex_1())
                .child(
                    div()
                        .text_color(t.text_faint)
                        .child(column.type_name.clone()),
                ),
        )
        .child(
            div()
                .px(px(8.))
                .py(px(5.))
                .rounded(t.radius_small)
                .bg(t.input)
                .border_1()
                .border_color(t.border)
                .font_family(t.mono_font.clone())
                .text_size(t.mono_size)
                .text_color(if null { t.text_faint } else { t.text })
                .whitespace_normal()
                .child(text),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_data::{ObjectKind, ObjectRef, ValueKind};

    fn column(name: &str) -> Column {
        Column::new(name, "int", ValueKind::Int)
    }

    fn hop(target_columns: &[&str], values: Vec<Value>) -> Hop {
        Hop {
            connection: "c".into(),
            key: ForeignKey {
                name: "fk".into(),
                columns: target_columns.iter().map(|c| format!("x_{c}")).collect(),
                target: ObjectRef {
                    namespace: vec!["public".into()],
                    name: "region".into(),
                    kind: ObjectKind::Table,
                },
                target_columns: target_columns.iter().map(|c| c.to_string()).collect(),
                on_delete: Default::default(),
                on_update: Default::default(),
            },
            values,
        }
    }

    #[test]
    fn keys_find_their_columns_and_values() {
        let columns = [column("id"), column("country"), column("code")];
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            positions(&columns, &names(&["code", "country"])),
            Some(vec![2, 1])
        );
        assert_eq!(positions(&columns, &names(&["code", "nope"])), None);
        assert_eq!(positions(&columns, &[]), None);

        let row = [Value::Int(1), Value::text("DE"), Value::Null];
        assert_eq!(
            values_at(&row, &[1, 0]),
            Some(vec![Value::text("DE"), Value::Int(1)])
        );
        assert_eq!(values_at(&row, &[1, 2]), None);
        assert_eq!(values_at(&row, &[7]), None);
    }

    #[test]
    fn hops_filter_on_the_target_columns() {
        let one = hop(&["id"], vec![Value::Int(42)]);
        assert_eq!(one.condition(), "id = 42");
        assert_eq!(
            one.filters(),
            [Filter::new("id", FilterOp::Eq, Value::Int(42))]
        );
        let two = hop(&["country", "code"], vec![Value::text("DE"), Value::Int(7)]);
        assert_eq!(two.condition(), "(country, code) = ('DE', 7)");
        assert_eq!(two.filters().len(), 2);
        assert_eq!(two.filters()[1].column, "code");
    }

    #[test]
    fn shortcuts_read_naturally() {
        assert_eq!(shortcut_label("ctrl-enter"), "Ctrl+Enter");
        assert_eq!(shortcut_label("cmd-shift-k f2"), "Cmd+Shift+K");
        assert_eq!(shortcut_label(""), "");
    }
}
