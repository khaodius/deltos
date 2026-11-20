//! The Schema view of a collection: every field path found in a random sample of its
//! documents, nested as the documents nest, with the types each held and how often it
//! is there. It reads [`Connection::schema`], the same tree the graph view draws from.

use std::collections::HashSet;
use std::sync::Arc;

use deltos_data::{Connection, ObjectRef, ObjectSchema, SchemaField};
use gpui::{
    App, ClipboardItem, Context, Entity, FontWeight, IntoElement, Render, SharedString, Window,
    div, prelude::*, px, uniform_list,
};

use crate::net::Net;
use crate::state::AppState;
use crate::theme::{Theme, theme};
use crate::ui::icons::{Icon, icon};
use crate::ui::{Tooltip, empty_state, icon_button, segmented, thousands};

/// Sample sizes the person can pick.
const SAMPLES: [(&str, u32); 3] = [("100", 100), ("1,000", 1_000), ("10,000", 10_000)];

enum Load {
    Idle,
    Loading,
    Ready(Arc<ObjectSchema>),
    Failed(SharedString),
}

/// One line of the tree as shown.
struct Line {
    depth: usize,
    path: SharedString,
    name: SharedString,
    types: SharedString,
    share: f32,
    key: bool,
    children: bool,
    open: bool,
}

pub struct SchemaView {
    state: Entity<AppState>,
    connection: String,
    object: ObjectRef,
    sample: u32,
    load: Load,
    /// Paths whose children show.
    open: HashSet<String>,
    lines: Arc<[Line]>,
    generation: u64,
}

impl SchemaView {
    pub fn new(
        state: Entity<AppState>,
        connection: String,
        object: ObjectRef,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            state,
            connection,
            object,
            sample: 1_000,
            load: Load::Idle,
            open: HashSet::new(),
            lines: Arc::from(Vec::new()),
            generation: 0,
        };
        this.reload(cx);
        this
    }

    fn conn(&self, cx: &App) -> Option<Arc<dyn Connection>> {
        self.state
            .read(cx)
            .session(&self.connection)
            .map(|s| s.conn.clone())
    }

    /// Samples the collection again.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.conn(cx) else {
            self.load = Load::Failed("Not connected".into());
            cx.notify();
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        self.load = Load::Loading;
        cx.notify();
        let object = self.object.clone();
        let sample = self.sample;
        let job = cx
            .global::<Net>()
            .run(async move { conn.schema(&object, sample).await });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.load = match result {
                    Some(Ok(schema)) => {
                        // Top-level documents open; deeper levels wait for a click.
                        if this.open.is_empty() {
                            this.open = schema
                                .fields
                                .iter()
                                .filter(|f| !f.fields.is_empty())
                                .map(|f| f.path.clone())
                                .collect();
                        }
                        Load::Ready(Arc::new(schema))
                    }
                    Some(Err(e)) => Load::Failed(e.to_string().into()),
                    None => Load::Failed("The sample was cancelled.".into()),
                };
                this.rebuild();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn rebuild(&mut self) {
        let Load::Ready(schema) = &self.load else {
            self.lines = Arc::from(Vec::new());
            return;
        };
        let mut lines = Vec::new();
        for field in &schema.fields {
            push_lines(field, 0, &self.open, &mut lines);
        }
        self.lines = Arc::from(lines);
    }

    fn toggle(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.open.remove(path) {
            self.open.insert(path.to_owned());
        }
        self.rebuild();
        cx.notify();
    }

    /// A `$jsonSchema` validator that accepts what the sample held, for
    /// `collMod` or `createCollection`.
    fn copy_validator(&self, cx: &mut App) {
        if let Load::Ready(schema) = &self.load {
            let text = serde_json::to_string_pretty(&json_schema(schema)).unwrap_or_default();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn header(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let summary: SharedString = match &self.load {
            Load::Ready(s) if s.declared => format!("{} columns", s.fields.len()).into(),
            Load::Ready(s) => {
                let of = s
                    .total
                    .map(|n| format!(" of about {}", thousands(n)))
                    .unwrap_or_default();
                let cut = if s.truncated {
                    ". Some fields were left out to keep the tree small"
                } else {
                    ""
                };
                format!(
                    "{} fields in {} sampled documents{of}{cut}",
                    thousands(s.walk().count() as u64),
                    thousands(s.sampled)
                )
                .into()
            }
            Load::Loading => "Sampling…".into(),
            _ => "".into(),
        };
        let weak = cx.entity().downgrade();
        div()
            .flex_none()
            .h(t.bar_height)
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(t.small_size)
                    .text_color(t.text_muted)
                    .child(summary),
            )
            .child(
                div()
                    .text_size(t.small_size)
                    .text_color(t.text_faint)
                    .child("Sample"),
            )
            .child(segmented(
                "schema-sample",
                &SAMPLES,
                self.sample,
                move |n, _, cx| {
                    weak.update(cx, |this, cx| {
                        if this.sample != n {
                            this.sample = n;
                            this.reload(cx);
                        }
                    })
                    .ok();
                },
                t,
            ))
            .child(
                icon_button("schema-copy", Icon::Copy, t)
                    .tooltip(|_, cx| Tooltip::view("Copy as a $jsonSchema validator", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.copy_validator(cx))),
            )
            .child(
                icon_button("schema-refresh", Icon::Refresh, t)
                    .tooltip(|_, cx| Tooltip::view("Sample again", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
    }
}

fn push_lines(field: &SchemaField, depth: usize, open: &HashSet<String>, out: &mut Vec<Line>) {
    let mut types: Vec<String> = field
        .types
        .iter()
        .take(4)
        .map(|t| {
            let name = if t.name == "array" && !field.items.is_empty() {
                let items: Vec<&str> = field
                    .items
                    .iter()
                    .take(2)
                    .map(|i| i.name.as_str())
                    .collect();
                format!("array<{}>", items.join(" | "))
            } else {
                t.name.clone()
            };
            let total: u64 = field.types.iter().map(|t| t.count).sum();
            if field.types.len() > 1 && total > 0 {
                format!("{name} {}%", (t.count * 100 + total / 2) / total)
            } else {
                name
            }
        })
        .collect();
    if field.types.len() > 4 {
        types.push(format!("+{}", field.types.len() - 4));
    }
    out.push(Line {
        depth,
        path: field.path.clone().into(),
        name: field.name.clone().into(),
        types: types.join(", ").into(),
        share: field.share(),
        key: field.key,
        children: !field.fields.is_empty(),
        open: open.contains(&field.path),
    });
    if open.contains(&field.path) {
        for child in &field.fields {
            push_lines(child, depth + 1, open, out);
        }
    }
}

/// A `$jsonSchema` that requires the fields every sampled document had and allows the
/// types seen, nested for embedded documents.
pub fn json_schema(schema: &ObjectSchema) -> serde_json::Value {
    fn bson_type(name: &str) -> Option<&'static str> {
        Some(match name {
            "double" => "double",
            "string" => "string",
            "object" => "object",
            "array" => "array",
            "binData" | "uuid" => "binData",
            "objectId" => "objectId",
            "bool" => "bool",
            "date" => "date",
            "null" => "null",
            "regex" => "regex",
            "javascript" => "javascript",
            "int" => "int",
            "timestamp" => "timestamp",
            "long" => "long",
            "decimal" => "decimal",
            "minKey" => "minKey",
            "maxKey" => "maxKey",
            _ => return None,
        })
    }
    fn object(fields: &[SchemaField]) -> serde_json::Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for f in fields {
            let types: Vec<serde_json::Value> = f
                .types
                .iter()
                .filter_map(|t| bson_type(&t.name))
                .map(|t| serde_json::Value::String(t.to_owned()))
                .collect();
            let mut property = serde_json::Map::new();
            match types.len() {
                0 => {}
                1 => {
                    property.insert("bsonType".into(), types[0].clone());
                }
                _ => {
                    property.insert("bsonType".into(), serde_json::Value::Array(types));
                }
            }
            let only_objects = f
                .types
                .iter()
                .all(|t| t.name == "object" || t.name == "null");
            if !f.fields.is_empty()
                && only_objects
                && let serde_json::Value::Object(inner) = object(&f.fields)
            {
                property.extend(inner.into_iter().filter(|(k, _)| k != "bsonType"));
            }
            if f.of > 0 && f.present == f.of {
                required.push(serde_json::Value::String(f.name.clone()));
            }
            properties.insert(f.name.clone(), serde_json::Value::Object(property));
        }
        let mut out = serde_json::Map::new();
        out.insert("bsonType".into(), "object".into());
        if !required.is_empty() {
            out.insert("required".into(), serde_json::Value::Array(required));
        }
        out.insert("properties".into(), serde_json::Value::Object(properties));
        serde_json::Value::Object(out)
    }
    serde_json::json!({ "$jsonSchema": object(&schema.fields) })
}

impl Render for SchemaView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let header = self.header(&t, cx);
        let body = match &self.load {
            Load::Idle | Load::Loading => {
                empty_state("Sampling documents…", None, &t).into_any_element()
            }
            Load::Failed(e) => {
                empty_state("The schema could not be read", Some(e.clone()), &t).into_any_element()
            }
            Load::Ready(s) if s.fields.is_empty() => empty_state(
                "No fields",
                Some("The collection had no documents to sample.".into()),
                &t,
            )
            .into_any_element(),
            Load::Ready(_) => {
                let lines = self.lines.clone();
                let weak = cx.entity().downgrade();
                let rt = t.clone();
                uniform_list("schema-lines", lines.len(), move |range, _, _| {
                    range
                        .filter_map(|i| {
                            let line = lines.get(i)?;
                            let path = line.path.clone();
                            let weak = weak.clone();
                            Some(schema_line(line, &rt).on_click(move |_, _, cx| {
                                weak.update(cx, |this, cx| this.toggle(&path, cx)).ok();
                            }))
                        })
                        .collect()
                })
                .size_full()
                .into_any_element()
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface)
            .child(header)
            .child(
                div()
                    .flex_none()
                    .h(t.s(24.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .bg(t.grid_header)
                    .border_b_1()
                    .border_color(t.border)
                    .text_size(t.small_size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.text_muted)
                    .child(div().flex_1().child("Field"))
                    .child(div().w(t.s(220.)).child("Types"))
                    .child(div().w(t.s(140.)).child("Present")),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

fn schema_line(line: &Line, t: &Theme) -> gpui::Stateful<gpui::Div> {
    let pct = (line.share * 100.0).round() as u32;
    let bar_w = 80.0;
    div()
        .id(SharedString::from(format!("schema-{}", line.path)))
        .h(t.row_height)
        .w_full()
        .px(px(8.))
        .flex()
        .items_center()
        .gap(px(8.))
        .border_b_1()
        .border_color(t.grid_line)
        .hover(|s| s.bg(t.hover))
        .when(line.children, |el| el.cursor_pointer())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(4.))
                .pl(px(14.0 * line.depth as f32))
                .child(div().flex_none().w(px(12.)).text_color(t.text_faint).when(
                    line.children,
                    |el| {
                        el.child(
                            icon(if line.open {
                                Icon::ChevronDown
                            } else {
                                Icon::ChevronRight
                            })
                            .size(px(10.)),
                        )
                    },
                ))
                .when(line.key, |el| {
                    el.child(icon(Icon::Key).size(px(11.)).text_color(t.key))
                })
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_family(t.grid_font.clone())
                        .text_size(t.mono_size)
                        .text_color(t.text)
                        .child(line.name.clone()),
                ),
        )
        .child(
            div()
                .w(t.s(220.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .font_family(t.grid_font.clone())
                .text_size(t.small_size)
                .text_color(t.syntax_keyword)
                .child(line.types.clone()),
        )
        .child(
            div()
                .w(t.s(140.))
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                    div()
                        .w(px(bar_w))
                        .h(px(4.))
                        .rounded_full()
                        .bg(t.border)
                        .child(
                            div()
                                .h_full()
                                .w(px(bar_w * line.share))
                                .rounded_full()
                                .bg(if pct == 100 { t.success } else { t.accent }),
                        ),
                )
                .child(
                    div()
                        .text_size(t.small_size)
                        .text_color(t.text_muted)
                        .child(format!("{pct}%")),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_data::{SchemaType, ValueKind};

    fn field(path: &str, types: &[(&str, u64)], present: u64, of: u64) -> SchemaField {
        SchemaField {
            name: path.rsplit('.').next().unwrap_or(path).into(),
            path: path.into(),
            types: types
                .iter()
                .map(|(n, c)| SchemaType {
                    name: (*n).into(),
                    kind: ValueKind::Text,
                    count: *c,
                })
                .collect(),
            present,
            of,
            ..Default::default()
        }
    }

    #[test]
    fn writes_a_validator_from_the_sample() {
        let mut address = field("address", &[("object", 2)], 2, 2);
        address.fields = vec![field("address.city", &[("string", 2)], 2, 2)];
        let schema = ObjectSchema {
            sampled: 2,
            fields: vec![
                field("_id", &[("objectId", 2)], 2, 2),
                field("n", &[("int", 1), ("long", 1)], 1, 2),
                address,
            ],
            ..Default::default()
        };
        let v = json_schema(&schema);
        let root = &v["$jsonSchema"];
        assert_eq!(root["required"], serde_json::json!(["_id", "address"]));
        assert_eq!(
            root["properties"]["n"]["bsonType"],
            serde_json::json!(["int", "long"])
        );
        assert_eq!(
            root["properties"]["address"]["properties"]["city"]["bsonType"],
            "string"
        );
        assert_eq!(
            root["properties"]["address"]["required"],
            serde_json::json!(["city"])
        );
    }

    #[test]
    fn lists_open_fields_only() {
        let mut a = field("a", &[("object", 1)], 1, 1);
        a.fields = vec![field("a.b", &[("string", 1)], 1, 1)];
        let mut out = Vec::new();
        push_lines(&a, 0, &HashSet::new(), &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].children);
        let mut out = Vec::new();
        push_lines(&a, 0, &HashSet::from(["a".to_owned()]), &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].depth, 1);
        let mixed = field("m", &[("string", 3), ("null", 1)], 3, 4);
        let mut out = Vec::new();
        push_lines(&mixed, 0, &HashSet::new(), &mut out);
        assert_eq!(out[0].types.as_ref(), "string 75%, null 25%");
    }
}
