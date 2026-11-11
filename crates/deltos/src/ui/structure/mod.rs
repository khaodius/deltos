//! The structure of a table, editable: columns, indexes and foreign keys. Edits stay in
//! the view until the person reviews the statements they make and applies them; the
//! statements come from [`deltos_drivers::ddl`]. The same view, empty, lays out a new
//! table in the "New table" dialog.

mod dialogs;

use std::sync::Arc;

use deltos_data::{Connection, ForeignKeyAction, ObjectInfo, ObjectKind, ObjectRef};
use deltos_drivers::ddl::{
    self, ColumnDef, Draft, Flavor, ForeignKeyDef, IndexDef, Plan, Staged, TableDef,
};
use gpui::{
    AnyElement, AnyView, App, AppContext, Context, ElementId, Entity, EventEmitter, Focusable,
    IntoElement, Render, SharedString, Subscription, WeakEntity, Window, div, prelude::*, px,
    uniform_list,
};

pub use dialogs::{confirm_drop, confirm_truncate, new_table};

use crate::safety::Environment;
use crate::state::AppState;
use crate::theme::{Theme, theme};
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{ButtonStyle, MenuItem, Tooltip, button, caption, empty_state, icon_button, menu};

/// One column, index or key as the view shows it: where it came from, what it is now,
/// and whether the person marked it to be dropped.
#[derive(Debug, Clone)]
struct Entry<T> {
    from: Option<usize>,
    def: T,
    dropped: bool,
}

impl<T: Clone> Entry<T> {
    fn all(items: &[T]) -> Vec<Entry<T>> {
        items
            .iter()
            .enumerate()
            .map(|(i, def)| Entry {
                from: Some(i),
                def: def.clone(),
                dropped: false,
            })
            .collect()
    }
}

fn staged<T: Clone>(entries: &[Entry<T>]) -> Vec<Staged<T>> {
    entries
        .iter()
        .filter(|e| !e.dropped)
        .map(|e| Staged {
            from: e.from,
            def: e.def.clone(),
        })
        .collect()
}

/// How an entry differs from where it started, for its marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    None,
    Added,
    Changed,
    Dropped,
}

fn change_of<T: PartialEq>(entry: &Entry<T>, original: &[T]) -> Change {
    match entry.from.and_then(|i| original.get(i)) {
        None => Change::Added,
        Some(_) if entry.dropped => Change::Dropped,
        Some(old) if *old != entry.def => Change::Changed,
        Some(_) => Change::None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Type,
    Default,
    Comment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Editing {
    row: usize,
    field: Field,
    /// The value when editing started, to put back on Escape.
    before: String,
}

pub enum StructureEvent {
    /// The table changed in the database; whoever shows its rows should load again.
    Applied,
}

pub struct StructureView {
    state: Entity<AppState>,
    connection: String,
    object: ObjectRef,
    /// Laying out a new table rather than showing an existing one.
    create: bool,
    info: Option<Result<ObjectInfo, String>>,
    flavor: Option<Flavor>,
    table: TableDef,
    columns: Vec<Entry<ColumnDef>>,
    indexes: Vec<Entry<IndexDef>>,
    keys: Vec<Entry<ForeignKeyDef>>,
    editing: Option<Editing>,
    editor: Entity<TextInput>,
    /// The row whose type menu is open.
    types_menu: Option<usize>,
    /// The dialog this view sits in, to return to from the dialogs it opens.
    host: Option<AnyView>,
    /// What the last apply did, until the next edit.
    notice: Option<String>,
    /// Why the staged changes can't be applied now, such as row changes of the table
    /// waiting to be committed. Set by the table that shows this view.
    blocked: Option<String>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<StructureEvent> for StructureView {}

impl StructureView {
    pub fn new(
        state: Entity<AppState>,
        connection: String,
        object: ObjectRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| TextInput::new("", cx));
        let mut subs = vec![cx.subscribe(&editor, |this, _, event, cx| this.on_editor(event, cx))];
        let focus = editor.read(cx).focus_handle(cx);
        subs.push(cx.on_blur(&focus, window, |this, _, cx| {
            if this.editing.take().is_some() {
                cx.notify();
            }
        }));
        let flavor = state
            .read(cx)
            .session(&connection)
            .and_then(|s| Flavor::of(s.kind, &s.conn.server_version()));
        Self {
            state,
            connection,
            object,
            create: false,
            info: None,
            flavor,
            table: TableDef::default(),
            columns: Vec::new(),
            indexes: Vec::new(),
            keys: Vec::new(),
            editing: None,
            editor,
            types_menu: None,
            host: None,
            notice: None,
            blocked: None,
            _subs: subs,
        }
    }

    /// An empty table in `namespace`, starting with a key column.
    fn new_table(
        state: Entity<AppState>,
        connection: String,
        namespace: Vec<String>,
        flavor: Flavor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let object = ObjectRef {
            namespace,
            name: String::new(),
            kind: ObjectKind::Table,
        };
        let mut this = Self::new(state, connection, object, window, cx);
        this.create = true;
        this.flavor = Some(flavor);
        this.table = TableDef::new(flavor, &this.object.namespace, "");
        let (ty, extra) = match flavor {
            Flavor::Postgres => ("bigserial", None),
            Flavor::Mysql => ("bigint unsigned", Some("AUTO_INCREMENT".to_string())),
            _ => ("INTEGER", None),
        };
        this.columns.push(Entry {
            from: None,
            def: ColumnDef {
                nullable: false,
                primary_key: Some(0),
                extra,
                ..ColumnDef::new("id", ty)
            },
            dropped: false,
        });
        this
    }

    /// Shows a newly read structure. Staged edits are dropped: they were made against
    /// what was there before.
    pub fn set_info(&mut self, info: Option<Result<ObjectInfo, String>>, cx: &mut Context<Self>) {
        if self.create {
            return;
        }
        self.flavor = self
            .state
            .read(cx)
            .session(&self.connection)
            .and_then(|s| Flavor::of(s.kind, &s.conn.server_version()));
        self.table = match (&info, self.flavor) {
            (Some(Ok(info)), Some(flavor)) => TableDef::from_info(flavor, &self.object, info),
            _ => TableDef::default(),
        };
        if let (Some(Ok(info)), None) = (&info, self.flavor) {
            // A database without DDL support here still shows what it has.
            self.table.columns = info
                .columns
                .iter()
                .map(|c| ColumnDef {
                    nullable: c.nullable.unwrap_or(true),
                    default: c.default.clone(),
                    comment: c.comment.clone(),
                    primary_key: c.primary_key,
                    ..ColumnDef::new(c.name.clone(), c.type_name.clone())
                })
                .collect();
        }
        self.info = info;
        self.discard(cx);
    }

    /// Holds the staged changes back with `reason`, or lets them through with `None`.
    pub fn set_blocked(&mut self, reason: Option<String>) {
        self.blocked = reason;
    }

    /// The connection's environment, for read-only and the confirmations.
    fn environment(&self, cx: &App) -> Option<Environment> {
        Environment::of(self.state.read(cx), &self.connection)
    }

    fn conn(&self, cx: &App) -> Option<Arc<dyn Connection>> {
        self.state
            .read(cx)
            .session(&self.connection)
            .map(|s| s.conn.clone())
    }

    /// Whether the person can change this table here.
    fn editable(&self, cx: &App) -> bool {
        if self.create {
            return true;
        }
        let ok = matches!(self.info, Some(Ok(_)));
        let table = match self.flavor {
            Some(Flavor::Mongo) => self.object.kind == ObjectKind::Collection,
            Some(_) => self.object.kind == ObjectKind::Table,
            None => false,
        };
        ok && table
            && self
                .conn(cx)
                .is_some_and(|c| c.capabilities().edit_rows && !crate::demo::is_demo(c.as_ref()))
    }

    fn edits_columns(&self) -> bool {
        self.flavor.is_some_and(Flavor::columns)
    }

    fn draft(&self) -> Draft {
        Draft {
            columns: staged(&self.columns),
            indexes: staged(&self.indexes),
            foreign_keys: staged(&self.keys),
        }
    }

    /// Staged edits, counting each added, changed or dropped entry once.
    fn changes(&self) -> usize {
        fn count<T: PartialEq>(entries: &[Entry<T>], original: &[T]) -> usize {
            entries
                .iter()
                .filter(|e| match change_of(e, original) {
                    Change::Added => !e.dropped,
                    Change::Changed | Change::Dropped => true,
                    Change::None => false,
                })
                .count()
        }
        count(&self.columns, &self.table.columns)
            + count(&self.indexes, &self.table.indexes)
            + count(&self.keys, &self.table.foreign_keys)
    }

    /// The table the "New table" dialog would create, named `name`.
    fn new_table_def(&self, name: &str) -> TableDef {
        let draft = self.draft();
        TableDef {
            name: name.trim().to_string(),
            columns: draft.columns.into_iter().map(|c| c.def).collect(),
            indexes: draft.indexes.into_iter().map(|c| c.def).collect(),
            foreign_keys: draft.foreign_keys.into_iter().map(|c| c.def).collect(),
            ..self.table.clone()
        }
    }

    fn plan(&self) -> Result<Plan, String> {
        let flavor = self
            .flavor
            .ok_or("This database's structure cannot be edited here.")?;
        ddl::plan(flavor, &self.table, &self.draft())
    }

    fn discard(&mut self, cx: &mut Context<Self>) {
        self.columns = Entry::all(&self.table.columns);
        self.indexes = Entry::all(&self.table.indexes);
        self.keys = Entry::all(&self.table.foreign_keys);
        self.editing = None;
        self.types_menu = None;
        cx.notify();
    }

    /// Column names a new index or key can use, in table order.
    fn column_names(&self) -> Vec<String> {
        self.columns
            .iter()
            .filter(|c| !c.dropped)
            .map(|c| c.def.name.clone())
            .filter(|n| !n.trim().is_empty())
            .collect()
    }

    // -----------------------------------------------------------------------------------
    // Editing cells.

    fn value(&self, row: usize, field: Field) -> String {
        let Some(c) = self.columns.get(row) else {
            return String::new();
        };
        match field {
            Field::Name => c.def.name.clone(),
            Field::Type => c.def.type_name.clone(),
            Field::Default => c.def.default.clone().unwrap_or_default(),
            Field::Comment => c.def.comment.clone().unwrap_or_default(),
        }
    }

    fn set_value(&mut self, row: usize, field: Field, text: &str) {
        let Some(c) = self.columns.get_mut(row) else {
            return;
        };
        let optional = |text: &str| (!text.is_empty()).then(|| text.to_string());
        match field {
            Field::Name => {
                let old = std::mem::replace(&mut c.def.name, text.to_string());
                // New indexes and keys follow a column they name.
                for index in self.indexes.iter_mut().filter(|i| i.from.is_none()) {
                    rename_in(&mut index.def.columns, &old, text);
                }
                for key in self.keys.iter_mut().filter(|k| k.from.is_none()) {
                    rename_in(&mut key.def.columns, &old, text);
                }
            }
            Field::Type => c.def.type_name = text.to_string(),
            Field::Default => c.def.default = optional(text),
            Field::Comment => c.def.comment = optional(text),
        }
    }

    fn start_edit(
        &mut self,
        row: usize,
        field: Field,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .editing
            .as_ref()
            .is_some_and(|e| e.row == row && e.field == field)
        {
            return;
        }
        self.notice = None;
        let before = self.value(row, field);
        self.editor.update(cx, |e, cx| {
            e.set_text(before.clone(), cx);
            e.select_everything(cx);
        });
        self.editing = Some(Editing { row, field, before });
        self.types_menu = None;
        window.focus(&self.editor.read(cx).focus_handle(cx));
        cx.notify();
    }

    fn on_editor(&mut self, event: &InputEvent, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.clone() else {
            return;
        };
        match event {
            InputEvent::Changed => {
                let text = self.editor.read(cx).text().to_owned();
                self.set_value(editing.row, editing.field, &text);
            }
            InputEvent::Submit => self.editing = None,
            InputEvent::Cancel => {
                self.set_value(editing.row, editing.field, &editing.before);
                self.editing = None;
            }
        }
        cx.notify();
    }

    fn add_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(flavor) = self.flavor else { return };
        let taken = self.column_names();
        let name = (1..)
            .map(|n| {
                if n == 1 {
                    "column".to_string()
                } else {
                    format!("column_{n}")
                }
            })
            .find(|n| !taken.contains(n))
            .unwrap_or_default();
        self.columns.push(Entry {
            from: None,
            def: ColumnDef::new(name, flavor.default_type()),
            dropped: false,
        });
        let row = self.columns.len() - 1;
        self.start_edit(row, Field::Name, window, cx);
    }

    /// Marks an entry to be dropped, or brings it back; an added one goes away at once.
    fn toggle_drop<T>(entries: &mut Vec<Entry<T>>, row: usize) {
        match entries.get_mut(row) {
            Some(e) if e.from.is_none() => {
                entries.remove(row);
            }
            Some(e) => e.dropped = !e.dropped,
            None => {}
        }
    }

    fn toggle_key(&mut self, row: usize, cx: &mut Context<Self>) {
        let next = self
            .columns
            .iter()
            .filter_map(|c| c.def.primary_key)
            .max()
            .map_or(0, |m| m + 1);
        if let Some(c) = self.columns.get_mut(row) {
            match c.def.primary_key.take() {
                Some(position) => {
                    for other in &mut self.columns {
                        if let Some(p) = other.def.primary_key.as_mut()
                            && *p > position
                        {
                            *p -= 1;
                        }
                    }
                }
                None => {
                    c.def.primary_key = Some(next);
                    c.def.nullable = false;
                }
            }
        }
        cx.notify();
    }

    fn stage_index(&mut self, index: IndexDef, cx: &mut Context<Self>) {
        self.indexes.push(Entry {
            from: None,
            def: index,
            dropped: false,
        });
        cx.notify();
    }

    fn stage_key(&mut self, key: ForeignKeyDef, cx: &mut Context<Self>) {
        self.keys.push(Entry {
            from: None,
            def: key,
            dropped: false,
        });
        cx.notify();
    }

    fn review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        if self.blocked.is_some() {
            cx.notify();
            return;
        }
        let Some(conn) = self.conn(cx) else { return };
        let environment = self.environment(cx);
        let plan = self.plan();
        let statements = plan.as_ref().map_or(0, |p| p.statements.len());
        let weak = cx.entity().downgrade();
        dialogs::review(
            dialogs::Review {
                title: format!("Change {}", self.object.name),
                subtitle: path_of(&self.object),
                connection: conn,
                namespace: self.object.namespace.clone(),
                plan,
                transactional: self.flavor.is_some_and(Flavor::transactional),
                confirm: None,
                environment,
                schema: true,
                action: "Apply",
                back: None,
                on_done: Box::new(move |cx| {
                    weak.update(cx, |this, cx| {
                        this.notice = Some(match statements {
                            1 => "Applied 1 statement".to_string(),
                            n => format!("Applied {n} statements"),
                        });
                        cx.emit(StructureEvent::Applied)
                    })
                    .ok();
                }),
            },
            window,
            cx,
        );
    }

    // -----------------------------------------------------------------------------------
    // Drawing.

    fn marker(change: Change, t: &Theme) -> gpui::Div {
        let color = match change {
            Change::None => None,
            Change::Added => Some(t.success),
            Change::Changed => Some(t.accent),
            Change::Dropped => Some(t.danger),
        };
        div()
            .flex_none()
            .w(px(3.))
            .h_full()
            .when_some(color, |el, c| el.bg(c))
    }

    fn column_row(
        &mut self,
        i: usize,
        editable: bool,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.columns.get(i) else {
            return div().into_any_element();
        };
        let change = change_of(entry, &self.table.columns);
        let dropped = entry.dropped;
        let can_edit = editable && !dropped && self.edits_columns();
        let original = entry.from.and_then(|f| self.table.columns.get(f));
        let comments = self.flavor.is_some_and(Flavor::comments);
        let def = entry.def.clone();
        let editing = self
            .editing
            .as_ref()
            .filter(|e| e.row == i)
            .map(|e| e.field);
        let changed = |field: Field| match original {
            Some(o) if !dropped => match field {
                Field::Name => o.name != def.name,
                Field::Type => o.type_name != def.type_name,
                Field::Default => o.default != def.default,
                Field::Comment => o.comment != def.comment,
            },
            _ => false,
        };
        let cell = |field: Field,
                    width: Option<f32>,
                    text: SharedString,
                    color: gpui::Hsla,
                    this: &Self,
                    cx: &mut Context<Self>| {
            let id = ElementId::NamedInteger(format!("structure-{field:?}").into(), i as u64);
            let el = div()
                .id(id)
                .h_full()
                .px(px(8.))
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap();
            let el = match width {
                Some(w) => el.w(px(w)).flex_none(),
                None => el.flex_1().min_w_0(),
            };
            if editing == Some(field) {
                return el
                    .child(
                        div()
                            .w_full()
                            .h(t.s(22.))
                            .px(px(4.))
                            .flex()
                            .items_center()
                            .rounded(t.radius_small)
                            .bg(t.input)
                            .border_1()
                            .border_color(t.accent)
                            .child(this.editor.clone()),
                    )
                    .into_any_element();
            }
            el.text_color(color)
                .when(changed(field), |el| {
                    el.font_weight(gpui::FontWeight::SEMIBOLD)
                })
                .child(div().overflow_hidden().text_ellipsis().child(text))
                .when(can_edit, |el| {
                    el.cursor_text().on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.start_edit(i, field, window, cx)
                        }),
                    )
                })
                .into_any_element()
        };

        let text_color = if dropped { t.text_faint } else { t.text };
        let name = cell(
            Field::Name,
            Some(200.),
            def.name.clone().into(),
            text_color,
            self,
            cx,
        );
        let ty = cell(
            Field::Type,
            Some(180.),
            def.type_name.clone().into(),
            if dropped {
                t.text_faint
            } else {
                t.syntax_keyword
            },
            self,
            cx,
        );
        let default = cell(
            Field::Default,
            Some(170.),
            def.default
                .clone()
                .unwrap_or_else(|| t.null_text.to_owned())
                .into(),
            if def.default.is_some() && !dropped {
                t.text
            } else {
                t.text_faint
            },
            self,
            cx,
        );
        let comment = comments.then(|| {
            cell(
                Field::Comment,
                None,
                def.comment.clone().unwrap_or_default().into(),
                if dropped { t.text_faint } else { t.text_muted },
                self,
                cx,
            )
        });

        let key = div()
            .id(ElementId::NamedInteger("structure-key".into(), i as u64))
            .w(px(22.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .when_some(def.primary_key, |el, _| {
                el.child(icon(Icon::Key).size(px(11.)).text_color(t.warning))
            })
            .when(self.create, |el| {
                el.cursor_pointer()
                    .hover(|s| s.bg(t.hover))
                    .tooltip({
                        let tip: &'static str = if def.primary_key.is_some() {
                            "Part of the primary key. Click to take it out"
                        } else {
                            "Click to add to the primary key"
                        };
                        move |_, cx| Tooltip::view(tip, cx)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_key(i, cx)))
            });

        let nullable = div()
            .id(ElementId::NamedInteger("structure-null".into(), i as u64))
            .w(px(80.))
            .flex_none()
            .h_full()
            .px(px(8.))
            .flex()
            .items_center()
            .text_color(if changed_nullable(original, &def) && !dropped {
                t.accent
            } else {
                t.text_muted
            })
            .child(if def.nullable { "YES" } else { "NO" })
            .when(can_edit && def.primary_key.is_none(), |el| {
                el.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(c) = this.columns.get_mut(i) {
                            c.def.nullable = !c.def.nullable;
                        }
                        cx.notify();
                    }))
            });

        let types = (can_edit && self.flavor.is_some_and(|f| !f.types().is_empty())).then(|| {
            let open = self.types_menu == Some(i);
            let mut picker = div()
                .id(ElementId::NamedInteger("structure-types".into(), i as u64))
                .flex_none()
                .size(t.s(18.))
                .rounded(t.radius_small)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .text_color(t.text_faint)
                .hover(|s| s.bg(t.hover).text_color(t.text))
                .child(icon(Icon::ChevronDown).size(px(10.)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.editing = None;
                    this.types_menu = Some(i);
                    cx.notify();
                }));
            if open && let Some(flavor) = self.flavor {
                let weak = cx.entity().downgrade();
                let items = flavor
                    .types()
                    .iter()
                    .map(|ty| {
                        let weak = weak.clone();
                        let checked = def.type_name.eq_ignore_ascii_case(ty);
                        let ty = ty.to_string();
                        MenuItem::new(ty.clone(), move |_, cx| {
                            weak.update(cx, |this, cx| {
                                if let Some(c) = this.columns.get_mut(i) {
                                    c.def.type_name = ty.clone();
                                }
                                cx.notify();
                            })
                            .ok();
                        })
                        .checked(checked)
                    })
                    .collect();
                picker = picker.child(menu(
                    "structure-types-menu",
                    items,
                    180.,
                    move |_, cx| {
                        weak.update(cx, |this, cx| {
                            this.types_menu = None;
                            cx.notify();
                        })
                        .ok();
                    },
                    t,
                ));
            }
            picker
        });

        let extra = def.extra.clone().filter(|e| !e.is_empty()).map(|e| {
            div()
                .flex_none()
                .pr(px(6.))
                .text_size(t.small_size)
                .text_color(t.text_faint)
                .child(e)
        });

        let action = editable.then(|| {
            icon_button(
                ElementId::NamedInteger("structure-drop".into(), i as u64),
                if dropped { Icon::Refresh } else { Icon::Trash },
                t,
            )
            .tooltip({
                let tip: &'static str = if dropped {
                    "Keep this column"
                } else {
                    "Drop this column"
                };
                move |_, cx| Tooltip::view(tip, cx)
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.editing = None;
                Self::toggle_drop(&mut this.columns, i);
                cx.notify();
            }))
        });

        div()
            .w_full()
            .h(t.row_height)
            .flex()
            .items_center()
            .when(t.zebra && i % 2 == 1, |el| el.bg(t.surface_alt))
            .border_b_1()
            .border_color(t.grid_line)
            .font_family(t.mono_font.clone())
            .text_size(t.mono_size)
            .when(dropped, |el| el.line_through())
            .child(Self::marker(change, t))
            .child(key)
            .child(name)
            .child(
                div()
                    .w(px(200.))
                    .flex_none()
                    .h_full()
                    .flex()
                    .items_center()
                    .child(div().flex_1().min_w_0().h_full().child(ty))
                    .children(extra)
                    .children(types),
            )
            .child(nullable)
            .child(default)
            .children(comment)
            .when(!comments, |el| el.child(div().flex_1()))
            .child(
                div()
                    .w(px(32.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .children(action),
            )
            .into_any_element()
    }

    fn header(&self, t: &Theme) -> gpui::Div {
        let label = |text: &'static str, width: Option<f32>| {
            let el = div().px(px(8.)).child(t.header_text(text));
            match width {
                Some(w) => el.w(px(w)).flex_none(),
                None => el.flex_1(),
            }
        };
        div()
            .flex_none()
            .h(t.row_height)
            .flex()
            .items_center()
            .bg(t.grid_header)
            .border_b_1()
            .border_color(t.border)
            .text_size(t.small_size)
            .font_weight(t.header_weight())
            .text_color(t.text_muted)
            .child(div().w(px(25.)).flex_none())
            .child(label("Column", Some(200.)))
            .child(label("Type", Some(200.)))
            .child(label("Nullable", Some(80.)))
            .child(label("Default", Some(170.)))
            .child(if self.flavor.is_some_and(Flavor::comments) {
                label("Comment", None)
            } else {
                div().flex_1()
            })
            .child(div().w(px(32.)).flex_none())
    }

    fn index_rows(&self, editable: bool, t: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.indexes
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let ix = &entry.def;
                let change = change_of(entry, &self.table.indexes);
                let droppable = editable && !ix.primary;
                let tag = if ix.primary {
                    Some(("PRIMARY", t.warning))
                } else if ix.unique {
                    Some(("UNIQUE", t.syntax_keyword))
                } else {
                    None
                };
                self.list_row(
                    ElementId::NamedInteger("structure-index".into(), i as u64),
                    change,
                    ix.name.clone(),
                    format!("({})", ix.columns.join(", ")),
                    tag,
                    droppable.then(|| {
                        cx.listener(
                            move |this: &mut Self,
                                  _: &gpui::ClickEvent,
                                  _: &mut Window,
                                  cx: &mut Context<Self>| {
                                Self::toggle_drop(&mut this.indexes, i);
                                cx.notify();
                            },
                        )
                    }),
                    t,
                )
            })
            .collect()
    }

    fn key_rows(&self, editable: bool, t: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let named = self.flavor.is_some_and(Flavor::named_foreign_keys);
        self.keys
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let fk = &entry.def;
                let change = change_of(entry, &self.table.foreign_keys);
                let mut detail = format!(
                    "({}) → {}({})",
                    fk.columns.join(", "),
                    fk.target.name,
                    fk.target_columns.join(", ")
                );
                for (word, action) in [("ON DELETE", fk.on_delete), ("ON UPDATE", fk.on_update)] {
                    if action != ForeignKeyAction::NoAction {
                        detail.push_str(&format!(" {word} {}", action.sql()));
                    }
                }
                let name = if named || entry.from.is_some() {
                    fk.name.clone()
                } else {
                    "(new)".to_string()
                };
                self.list_row(
                    ElementId::NamedInteger("structure-key-row".into(), i as u64),
                    change,
                    name,
                    detail,
                    None,
                    editable.then(|| {
                        cx.listener(
                            move |this: &mut Self,
                                  _: &gpui::ClickEvent,
                                  _: &mut Window,
                                  cx: &mut Context<Self>| {
                                Self::toggle_drop(&mut this.keys, i);
                                cx.notify();
                            },
                        )
                    }),
                    t,
                )
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn list_row(
        &self,
        id: ElementId,
        change: Change,
        name: String,
        detail: String,
        tag: Option<(&'static str, gpui::Hsla)>,
        on_drop: Option<impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static>,
        t: &Theme,
    ) -> AnyElement {
        let dropped = change == Change::Dropped;
        div()
            .h(t.s(24.))
            .flex()
            .items_center()
            .gap(px(10.))
            .font_family(t.mono_font.clone())
            .text_size(t.mono_size)
            .text_color(if dropped { t.text_faint } else { t.text })
            .when(dropped, |el| el.line_through())
            .child(Self::marker(change, t).h(t.s(16.)))
            .child(
                div()
                    .w(px(240.))
                    .flex_none()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(name),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(if dropped { t.text_faint } else { t.text_muted })
                    .child(detail),
            )
            .children(tag.map(|(tag, color)| div().flex_none().text_color(color).child(tag)))
            .children(on_drop.map(|f| {
                let mut id = id.clone();
                if let ElementId::NamedInteger(name, n) = &id {
                    id = ElementId::NamedInteger(format!("{name}-drop").into(), *n);
                }
                icon_button(id, if dropped { Icon::Refresh } else { Icon::Trash }, t)
                    .tooltip({
                        let tip: &'static str = if dropped { "Keep it" } else { "Drop it" };
                        move |_, cx| Tooltip::view(tip, cx)
                    })
                    .on_click(f)
            }))
            .into_any_element()
    }

    fn section(
        title: &'static str,
        add: Option<AnyElement>,
        rows: Vec<AnyElement>,
        empty: &'static str,
        t: &Theme,
    ) -> gpui::Div {
        let empty_row = rows.is_empty().then(|| {
            div()
                .text_size(t.small_size)
                .text_color(t.text_faint)
                .child(empty)
        });
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(t.s(22.))
                    .child(caption(title, t))
                    .child(div().flex_1())
                    .children(add),
            )
            .children(empty_row)
            .children(rows)
    }

    fn toolbar(&self, editable: bool, t: &Theme, cx: &mut Context<Self>) -> Option<gpui::Div> {
        if self.create {
            return None;
        }
        if !editable {
            // Say why nothing can be changed when it is the connection's choice.
            let env = self.environment(cx).filter(|e| e.read_only)?;
            return Some(
                div()
                    .flex_none()
                    .h(t.bar_height)
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .bg(t.base)
                    .border_b_1()
                    .border_color(t.border)
                    .text_size(t.small_size)
                    .text_color(t.text_muted)
                    .child(icon(Icon::Lock).size(px(12.)))
                    .child(format!(
                        "{} is read-only: its tables' structure can't be changed here.",
                        env.name
                    )),
            );
        }
        let changes = self.changes();
        let mut bar = div()
            .flex_none()
            .h(t.bar_height)
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .bg(t.base)
            .border_b_1()
            .border_color(t.border)
            .text_size(t.small_size)
            .text_color(t.text_muted);
        if self.edits_columns() {
            bar = bar.child(
                button("structure-add-column", "Add column", ButtonStyle::Ghost, t)
                    .on_click(cx.listener(|this, _, window, cx| this.add_column(window, cx))),
            );
        }
        bar = bar.child(div().flex_1());
        if changes > 0 {
            bar = bar
                .child(format!(
                    "{changes} {} staged",
                    if changes == 1 { "change" } else { "changes" }
                ))
                .child(
                    button("structure-discard", "Discard", ButtonStyle::Ghost, t)
                        .on_click(cx.listener(|this, _, _, cx| this.discard(cx))),
                );
            bar = match &self.blocked {
                Some(reason) => bar
                    .child(div().text_color(t.warning).child(reason.clone()))
                    .child(
                        button("structure-review", "Review SQL…", ButtonStyle::Primary, t)
                            .opacity(0.5)
                            .cursor_default(),
                    ),
                None => bar.child(
                    button("structure-review", "Review SQL…", ButtonStyle::Primary, t)
                        .on_click(cx.listener(|this, _, window, cx| this.review(window, cx))),
                ),
            };
        } else if let Some(notice) = &self.notice {
            bar = bar.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_color(t.success)
                    .child(icon(Icon::Check).size(px(12.)))
                    .child(notice.clone()),
            );
        } else {
            bar = bar.child("Click a cell to change it");
        }
        Some(bar)
    }

    fn add_button(id: &'static str, label: &'static str, t: &Theme) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(6.))
            .h(t.s(20.))
            .rounded(t.radius_small)
            .text_size(t.small_size)
            .text_color(t.text_muted)
            .cursor_pointer()
            .hover(|s| s.bg(t.hover).text_color(t.text))
            .child(icon(Icon::Plus).size(px(11.)))
            .child(label)
    }
}

fn changed_nullable(original: Option<&ColumnDef>, def: &ColumnDef) -> bool {
    original.is_some_and(|o| o.nullable != def.nullable)
}

fn rename_in(columns: &mut [String], old: &str, new: &str) {
    for c in columns.iter_mut().filter(|c| c.as_str() == old) {
        *c = new.to_string();
    }
}

/// `db.schema.table`, for dialog subtitles.
fn path_of(object: &ObjectRef) -> String {
    let mut parts = object.namespace.clone();
    parts.push(object.name.clone());
    parts.join(".")
}

impl Render for StructureView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        if !self.create {
            match &self.info {
                None => return empty_state("Loading structure…", None, &t).into_any_element(),
                Some(Err(e)) => {
                    return empty_state("Could not read the structure", Some(e.clone().into()), &t)
                        .into_any_element();
                }
                Some(Ok(_)) => {}
            }
        }
        let editable = self.editable(cx);
        let n = self.columns.len();
        let list = uniform_list(
            "structure-columns",
            n,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                let t = theme(cx).clone();
                range
                    .map(|i| this.column_row(i, editable, &t, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1();

        let weak: WeakEntity<Self> = cx.entity().downgrade();
        let add_index = (editable && self.flavor.is_some()).then(|| {
            let weak = weak.clone();
            Self::add_button("structure-add-index", "Add index", &t)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.editing = None;
                    let Some(flavor) = this.flavor else { return };
                    let weak = weak.clone();
                    dialogs::add_index(
                        flavor,
                        this.object.name.clone(),
                        this.column_names(),
                        Box::new(move |index, cx| {
                            weak.update(cx, |this, cx| this.stage_index(index, cx)).ok();
                        }),
                        this.host.clone(),
                        window,
                        cx,
                    );
                }))
                .into_any_element()
        });
        let add_key = (editable && self.flavor.is_some_and(Flavor::foreign_keys)).then(|| {
            let weak = weak.clone();
            Self::add_button("structure-add-key", "Add foreign key", &t)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.editing = None;
                    let (Some(flavor), Some(conn)) = (this.flavor, this.conn(cx)) else {
                        return;
                    };
                    let weak = weak.clone();
                    dialogs::add_key(
                        dialogs::KeyTarget {
                            flavor,
                            connection: conn,
                            namespace: this.object.namespace.clone(),
                            table: this.object.name.clone(),
                            columns: this.column_names(),
                        },
                        Box::new(move |key, cx| {
                            weak.update(cx, |this, cx| this.stage_key(key, cx)).ok();
                        }),
                        this.host.clone(),
                        window,
                        cx,
                    );
                }))
                .into_any_element()
        });
        let index_rows = self.index_rows(editable, &t, cx);
        let key_rows = self.key_rows(editable, &t, cx);
        let show_keys = self.flavor.is_none_or(Flavor::foreign_keys);
        let lower = div()
            .id("structure-lower")
            .flex_none()
            .max_h(px(260.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(10.))
            .border_t_1()
            .border_color(t.border)
            .child(Self::section(
                "Indexes",
                add_index,
                index_rows,
                "No indexes",
                &t,
            ))
            .when(show_keys, |el| {
                el.child(Self::section(
                    "Foreign keys",
                    add_key,
                    key_rows,
                    "No foreign keys",
                    &t,
                ))
            });
        let add_column = (self.create).then(|| {
            div()
                .flex_none()
                .px(px(6.))
                .py(px(4.))
                .border_b_1()
                .border_color(t.grid_line)
                .child(
                    Self::add_button("structure-new-column", "Add column", &t)
                        .on_click(cx.listener(|this, _, window, cx| this.add_column(window, cx))),
                )
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface)
            .children(self.toolbar(editable, &t, cx))
            .child(self.header(&t))
            .child(list)
            .children(add_column)
            .child(lower)
            .into_any_element()
    }
}
