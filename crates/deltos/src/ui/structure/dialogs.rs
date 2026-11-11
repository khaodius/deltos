//! The dialogs around the structure view: reviewing the statements before they run,
//! adding an index or a foreign key, laying out a new table, and dropping or emptying
//! one behind a typed confirmation. They show through the transfer host, one at a time;
//! a dialog opened from another can lead back to it.

use std::sync::Arc;

use deltos_data::{Connection, ExecuteOptions, ForeignKeyAction, ObjectKind, ObjectRef};
use deltos_drivers::ddl::{self, Flavor, ForeignKeyDef, IndexDef, Plan};
use futures::StreamExt;
use gpui::{
    AnyElement, AnyView, App, AppContext, ClipboardItem, Context, ElementId, Entity, FocusHandle,
    Focusable, IntoElement, Render, SharedString, Subscription, Window, div, prelude::*, px,
};

use super::{StructureView, path_of};
use crate::net::Net;
use crate::safety::Environment;
use crate::state::AppState;
use crate::theme::{Theme, theme};
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::transfer::{close_dialog, dialog_frame, note, open_dialog, option_row, warning};
use crate::ui::{ButtonStyle, MenuItem, button, field, menu, switch};
use deltos_settings::PanelKind;

/// Leaves a dialog: back to the one it came from, or closed.
fn leave(back: &Option<AnyView>, window: &mut Window, cx: &mut App) {
    match back {
        Some(view) => open_dialog(view.clone(), window, cx),
        None => close_dialog(cx),
    }
}

fn disabled(el: gpui::Stateful<gpui::Div>, off: bool) -> gpui::Stateful<gpui::Div> {
    if off {
        el.opacity(0.5).cursor_default()
    } else {
        el
    }
}

/// Column names to pick in order, each showing its place in the pick.
fn picker(
    id: &'static str,
    names: &[String],
    chosen: &[String],
    on_toggle: impl Fn(String, &mut Window, &mut App) + 'static,
    t: &Theme,
) -> gpui::Div {
    let on_toggle = std::rc::Rc::new(on_toggle);
    let mut row = div().flex().flex_wrap().gap(px(4.));
    for (i, name) in names.iter().enumerate() {
        let place = chosen.iter().position(|c| c == name);
        let toggle = on_toggle.clone();
        let name = name.clone();
        row = row.child(
            div()
                .id(ElementId::NamedInteger(id.into(), i as u64))
                .h(t.s(22.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(5.))
                .rounded(t.radius_small)
                .border_1()
                .font_family(t.mono_font.clone())
                .text_size(t.mono_size)
                .cursor_pointer()
                .map(|el| match place {
                    Some(_) => el
                        .bg(t.accent_soft)
                        .border_color(t.accent)
                        .text_color(t.text),
                    None => el
                        .bg(t.input)
                        .border_color(t.border_strong)
                        .text_color(t.text_muted)
                        .hover(|s| s.text_color(t.text)),
                })
                .children(place.map(|p| {
                    div()
                        .text_size(t.small_size)
                        .text_color(t.accent)
                        .child((p + 1).to_string())
                }))
                .child(name.clone())
                .on_click(move |_, window, cx| toggle(name.clone(), window, cx)),
        );
    }
    row
}

fn toggle_in(list: &mut Vec<String>, name: String) {
    match list.iter().position(|c| *c == name) {
        Some(i) => {
            list.remove(i);
        }
        None => list.push(name),
    }
}

// ---------------------------------------------------------------------------------------
// Reviewing statements.

pub(super) struct Review {
    pub title: String,
    pub subtitle: String,
    pub connection: Arc<dyn Connection>,
    pub namespace: Vec<String>,
    pub plan: Result<Plan, String>,
    pub transactional: bool,
    /// A name the person must type before the statements run.
    pub confirm: Option<String>,
    /// The connection's environment: staging and production show it, in its color, by
    /// the button that runs the statements.
    pub environment: Option<Environment>,
    /// Run through [`Connection::apply_schema`]; otherwise each statement runs as typed
    /// SQL would, with the database's own rules (cascades and all).
    pub schema: bool,
    pub action: &'static str,
    pub back: Option<AnyView>,
    pub on_done: Box<dyn FnOnce(&mut App)>,
}

struct ReviewDialog {
    review: Review,
    on_done: Option<Box<dyn FnOnce(&mut App)>>,
    confirm: Option<Entity<TextInput>>,
    running: bool,
    error: Option<String>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

pub(super) fn review(mut review: Review, window: &mut Window, cx: &mut App) {
    let on_done = std::mem::replace(&mut review.on_done, Box::new(|_| {}));
    let view = cx.new(|cx| {
        let mut subs = Vec::new();
        let confirm = review.confirm.as_ref().map(|name| {
            let input = cx.new(|cx| TextInput::new(name.clone(), cx));
            subs.push(
                cx.subscribe(&input, |this: &mut ReviewDialog, _, e, cx| match e {
                    InputEvent::Submit => this.run(cx),
                    InputEvent::Cancel => close_dialog(cx),
                    InputEvent::Changed => cx.notify(),
                }),
            );
            input
        });
        ReviewDialog {
            review,
            on_done: Some(on_done),
            confirm,
            running: false,
            error: None,
            focus: cx.focus_handle(),
            _subs: subs,
        }
    });
    let focus = match &view.read(cx).confirm {
        Some(input) => input.read(cx).focus_handle(cx),
        None => view.read(cx).focus.clone(),
    };
    open_dialog(view.into(), window, cx);
    window.focus(&focus);
}

impl ReviewDialog {
    fn ready(&self, cx: &App) -> bool {
        let plan_ok = self.review.plan.as_ref().is_ok_and(|p| !p.is_empty());
        let confirmed = match (&self.confirm, &self.review.confirm) {
            (Some(input), Some(name)) => input.read(cx).text() == name,
            _ => true,
        };
        plan_ok && confirmed && !self.running
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        if !self.ready(cx) {
            return;
        }
        let Ok(plan) = &self.review.plan else { return };
        let statements = plan.statements.clone();
        let conn = self.review.connection.clone();
        let namespace = self.review.namespace.clone();
        let schema = self.review.schema;
        self.running = true;
        self.error = None;
        let job = cx.global::<Net>().run(async move {
            if schema {
                return conn
                    .apply_schema(&namespace, &statements)
                    .await
                    .map_err(|e| e.to_string());
            }
            let options = ExecuteOptions {
                namespace: Some(namespace),
                ..ExecuteOptions::default()
            };
            for statement in &statements {
                let mut stream = conn
                    .execute(statement, &options)
                    .await
                    .map_err(|e| e.to_string())?;
                while let Some(batch) = stream.batches.next().await {
                    batch.map_err(|e| e.to_string())?;
                }
            }
            Ok(())
        });
        let on_done = self.on_done.take();
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|| Err("cancelled".into()));
            match result {
                Ok(()) => {
                    cx.update(|cx| {
                        close_dialog(cx);
                        if let Some(f) = on_done {
                            f(cx);
                        }
                    })
                    .ok();
                }
                Err(e) => {
                    this.update(cx, |this, cx| {
                        this.running = false;
                        this.error = Some(e);
                        this.on_done = on_done;
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn script(&self) -> String {
        self.review
            .plan
            .as_ref()
            .map(Plan::script)
            .unwrap_or_default()
    }
}

/// Statements in a code block, one line per line of SQL.
fn code_block(plan: &Plan, t: &Theme) -> gpui::Stateful<gpui::Div> {
    let mut block = div()
        .id("review-sql")
        .max_h(px(320.))
        .overflow_y_scroll()
        .p(px(10.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .rounded(t.radius_small)
        .bg(t.base)
        .border_1()
        .border_color(t.border)
        .font_family(t.mono_font.clone())
        .text_size(t.mono_size)
        .text_color(t.text);
    for statement in &plan.statements {
        let mut lines = div().flex().flex_col();
        let mut iter = statement.lines().peekable();
        while let Some(line) = iter.next() {
            let text = if iter.peek().is_none() {
                format!("{line};")
            } else {
                line.to_string()
            };
            lines = lines.child(div().whitespace_normal().child(text));
        }
        block = block.child(lines);
    }
    block
}

impl Render for ReviewDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let mut body = div().flex().flex_col().gap(px(10.));
        match &self.review.plan {
            Err(e) => {
                body = body.child(note(format!("Nothing can run yet: {e}"), true, &t));
            }
            Ok(plan) if plan.is_empty() => {
                body = body.child(note("There is nothing to change.", false, &t));
            }
            Ok(plan) => {
                body = body.child(code_block(plan, &t));
                for line in &plan.notes {
                    body = body.child(note(line.clone(), false, &t));
                }
                if self.review.transactional && plan.statements.len() > 1 {
                    body = body.child(note(
                        "The statements run in one transaction: if one fails, nothing changes.",
                        false,
                        &t,
                    ));
                }
            }
        }
        let sensitive = self
            .review
            .environment
            .as_ref()
            .filter(|e| e.is_sensitive());
        if let Some(env) = sensitive {
            body = body.child(option_row(
                "Runs on",
                crate::ui::environment::named(env, &t),
                &t,
            ));
        }
        if let (Some(input), Some(name)) = (&self.confirm, &self.review.confirm) {
            body = body
                .child(warning(
                    format!("This cannot be undone. Type {name} to confirm."),
                    &t,
                ))
                .child(field(input, &t, window, cx));
        }
        if let Some(e) = &self.error {
            body = body.child(note(e.clone(), true, &t));
        }
        let ready = self.ready(cx);
        let mut buttons: Vec<AnyElement> = Vec::new();
        if let Some(back) = self.review.back.clone() {
            buttons.push(
                disabled(
                    button("review-back", "Back", ButtonStyle::Ghost, &t),
                    self.running,
                )
                .when(!self.running, |el| {
                    el.on_click(move |_, window, cx| open_dialog(back.clone(), window, cx))
                })
                .into_any_element(),
            );
        }
        buttons.push(div().flex_1().into_any_element());
        buttons.push(
            disabled(
                button("review-cancel", "Cancel", ButtonStyle::Ghost, &t),
                self.running,
            )
            .when(!self.running, |el| el.on_click(|_, _, cx| close_dialog(cx)))
            .into_any_element(),
        );
        if self.review.plan.as_ref().is_ok_and(|p| !p.is_empty()) {
            let script = self.script();
            buttons.push(
                button("review-copy", "Copy SQL", ButtonStyle::Secondary, &t)
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(script.clone()))
                    })
                    .into_any_element(),
            );
        }
        let label = if self.running {
            "Running…"
        } else {
            self.review.action
        };
        let style = if self.review.confirm.is_some() || sensitive.is_some() {
            ButtonStyle::Danger
        } else {
            ButtonStyle::Primary
        };
        buttons.push(
            disabled(button("review-run", label, style, &t), !ready)
                .when(ready, |el| {
                    el.on_click(cx.listener(|this, _, _, cx| this.run(cx)))
                })
                .into_any_element(),
        );
        div().track_focus(&self.focus).child(
            dialog_frame(
                self.review.title.clone(),
                Some(self.review.subtitle.clone().into()),
                body,
                buttons,
                &t,
            )
            .w(px(680.)),
        )
    }
}

// ---------------------------------------------------------------------------------------
// Adding an index.

struct IndexDialog {
    flavor: Flavor,
    table: String,
    columns: Vec<String>,
    chosen: Vec<String>,
    unique: bool,
    name: Entity<TextInput>,
    /// The name the dialog last filled in; once the person types another, it stays.
    auto_name: String,
    error: Option<String>,
    on_add: Option<Box<dyn FnOnce(IndexDef, &mut App)>>,
    back: Option<AnyView>,
    _subs: Vec<Subscription>,
}

pub(super) fn add_index(
    flavor: Flavor,
    table: String,
    columns: Vec<String>,
    on_add: Box<dyn FnOnce(IndexDef, &mut App)>,
    back: Option<AnyView>,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| {
        let name = cx.new(|cx| TextInput::new("Index name", cx));
        let subs = vec![cx.subscribe_in(
            &name,
            window,
            |this: &mut IndexDialog, _, e, window, cx| match e {
                InputEvent::Submit => this.add(window, cx),
                InputEvent::Cancel => leave(&this.back, window, cx),
                InputEvent::Changed => {
                    this.error = None;
                    cx.notify();
                }
            },
        )];
        IndexDialog {
            flavor,
            table,
            columns,
            chosen: Vec::new(),
            unique: false,
            name,
            auto_name: String::new(),
            error: None,
            on_add: Some(on_add),
            back,
            _subs: subs,
        }
    });
    let focus = view.read(cx).name.read(cx).focus_handle(cx);
    open_dialog(view.into(), window, cx);
    window.focus(&focus);
}

impl IndexDialog {
    /// Fills in a name from the columns while the person has not typed their own.
    fn rename(&mut self, cx: &mut Context<Self>) {
        let current = self.name.read(cx).text().to_owned();
        if current.is_empty() || current == self.auto_name {
            let name = if self.chosen.is_empty() {
                String::new()
            } else {
                ddl::index_name(self.flavor, &self.table, &self.chosen, self.unique)
            };
            self.auto_name = name.clone();
            self.name.update(cx, |n, cx| n.set_text(name, cx));
        }
        cx.notify();
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim().to_owned();
        if self.chosen.is_empty() {
            self.error = Some("Pick the columns the index covers.".into());
        } else if name.is_empty() {
            self.error = Some("The index needs a name.".into());
        } else {
            if let Some(f) = self.on_add.take() {
                f(
                    IndexDef {
                        name,
                        columns: self.chosen.clone(),
                        unique: self.unique,
                        ..Default::default()
                    },
                    cx,
                );
            }
            leave(&self.back, window, cx);
            return;
        }
        cx.notify();
    }
}

impl Render for IndexDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let weak = cx.entity().downgrade();
        let columns = picker(
            "index-column",
            &self.columns,
            &self.chosen,
            move |name, _, cx| {
                weak.update(cx, |this, cx| {
                    toggle_in(&mut this.chosen, name);
                    this.error = None;
                    this.rename(cx);
                })
                .ok();
            },
            &t,
        );
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(option_row("Columns", columns, &t))
            .child(note(
                "Click columns in the order the index should use them.",
                false,
                &t,
            ))
            .child(option_row(
                "Unique",
                switch("index-unique", self.unique, &t).on_click(cx.listener(|this, _, _, cx| {
                    this.unique = !this.unique;
                    this.rename(cx);
                })),
                &t,
            ))
            .child(option_row(
                "Name",
                field(&self.name, &t, window, cx).flex_1(),
                &t,
            ));
        if let Some(e) = &self.error {
            body = body.child(note(e.clone(), true, &t));
        }
        let back = self.back.clone();
        let buttons = vec![
            button("index-cancel", "Cancel", ButtonStyle::Ghost, &t)
                .on_click(move |_, window, cx| leave(&back, window, cx))
                .into_any_element(),
            button("index-add", "Add index", ButtonStyle::Primary, &t)
                .on_click(cx.listener(|this, _, window, cx| this.add(window, cx)))
                .into_any_element(),
        ];
        dialog_frame(
            "Add an index",
            Some(
                format!(
                    "On {}. It is made along with the other changes.",
                    self.table
                )
                .into(),
            ),
            body,
            buttons,
            &t,
        )
    }
}

// ---------------------------------------------------------------------------------------
// Adding a foreign key.

pub(super) struct KeyTarget {
    pub flavor: Flavor,
    pub connection: Arc<dyn Connection>,
    pub namespace: Vec<String>,
    pub table: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyMenu {
    Table,
    OnDelete,
    OnUpdate,
}

struct KeyDialog {
    target: KeyTarget,
    chosen: Vec<String>,
    tables: Option<Result<Vec<String>, String>>,
    referenced: Option<String>,
    /// The referenced table's columns, and which of them form its primary key.
    referenced_columns: Option<Result<Vec<(String, bool)>, String>>,
    referenced_chosen: Vec<String>,
    on_delete: ForeignKeyAction,
    on_update: ForeignKeyAction,
    name: Entity<TextInput>,
    auto_name: String,
    menu: Option<KeyMenu>,
    error: Option<String>,
    on_add: Option<Box<dyn FnOnce(ForeignKeyDef, &mut App)>>,
    back: Option<AnyView>,
    _subs: Vec<Subscription>,
}

pub(super) fn add_key(
    target: KeyTarget,
    on_add: Box<dyn FnOnce(ForeignKeyDef, &mut App)>,
    back: Option<AnyView>,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| {
        let name = cx.new(|cx| TextInput::new("Constraint name", cx));
        let subs = vec![cx.subscribe_in(
            &name,
            window,
            |this: &mut KeyDialog, _, e, window, cx| match e {
                InputEvent::Submit => this.add(window, cx),
                InputEvent::Cancel => leave(&this.back, window, cx),
                InputEvent::Changed => {
                    this.error = None;
                    cx.notify();
                }
            },
        )];
        let (conn, namespace) = (target.connection.clone(), target.namespace.clone());
        let job = cx.global::<Net>().run(async move {
            conn.objects(&namespace)
                .await
                .map(|objects| {
                    objects
                        .into_iter()
                        .filter(|o| o.object.kind == ObjectKind::Table)
                        .map(|o| o.object.name)
                        .collect::<Vec<_>>()
                })
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let tables = job.await.unwrap_or_else(|| Err("cancelled".into()));
            this.update(cx, |this, cx| {
                this.tables = Some(tables);
                cx.notify();
            })
            .ok();
        })
        .detach();
        KeyDialog {
            target,
            chosen: Vec::new(),
            tables: None,
            referenced: None,
            referenced_columns: None,
            referenced_chosen: Vec::new(),
            on_delete: ForeignKeyAction::NoAction,
            on_update: ForeignKeyAction::NoAction,
            name,
            auto_name: String::new(),
            menu: None,
            error: None,
            on_add: Some(on_add),
            back,
            _subs: subs,
        }
    });
    open_dialog(view.into(), window, cx);
}

impl KeyDialog {
    fn rename(&mut self, cx: &mut Context<Self>) {
        let current = self.name.read(cx).text().to_owned();
        if current.is_empty() || current == self.auto_name {
            let name = if self.chosen.is_empty() {
                String::new()
            } else {
                ddl::foreign_key_name(&self.target.table, &self.chosen)
            };
            self.auto_name = name.clone();
            self.name.update(cx, |n, cx| n.set_text(name, cx));
        }
        cx.notify();
    }

    fn pick_table(&mut self, name: String, cx: &mut Context<Self>) {
        self.referenced = Some(name.clone());
        self.referenced_columns = None;
        self.referenced_chosen.clear();
        let conn = self.target.connection.clone();
        let object = ObjectRef {
            namespace: self.target.namespace.clone(),
            name,
            kind: ObjectKind::Table,
        };
        let job = cx.global::<Net>().run(async move {
            conn.describe(&object)
                .await
                .map(|info| {
                    let mut key: Vec<(u16, String)> = info
                        .columns
                        .iter()
                        .filter_map(|c| c.primary_key.map(|p| (p, c.name.clone())))
                        .collect();
                    key.sort();
                    let columns = info
                        .columns
                        .into_iter()
                        .map(|c| {
                            let pk = c.primary_key.is_some();
                            (c.name, pk)
                        })
                        .collect::<Vec<_>>();
                    (columns, key.into_iter().map(|(_, n)| n).collect::<Vec<_>>())
                })
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|| Err("cancelled".into()));
            this.update(cx, |this, cx| {
                match result {
                    Ok((columns, key)) => {
                        // The primary key is what a foreign key refers to most often.
                        this.referenced_chosen = key;
                        this.referenced_columns = Some(Ok(columns));
                    }
                    Err(e) => this.referenced_columns = Some(Err(e)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim().to_owned();
        let named = self.target.flavor.named_foreign_keys();
        let error = if self.chosen.is_empty() {
            Some("Pick the columns that refer to the other table.")
        } else if self.referenced.is_none() {
            Some("Pick the table the key refers to.")
        } else if self.referenced_chosen.len() != self.chosen.len() {
            Some("Pick as many referenced columns as columns, in the same order.")
        } else if named && name.is_empty() {
            Some("The key needs a name.")
        } else {
            None
        };
        if let Some(e) = error {
            self.error = Some(e.into());
            cx.notify();
            return;
        }
        if let (Some(f), Some(table)) = (self.on_add.take(), self.referenced.clone()) {
            f(
                ForeignKeyDef {
                    name: if named { name } else { String::new() },
                    columns: self.chosen.clone(),
                    target: ObjectRef {
                        namespace: self.target.namespace.clone(),
                        name: table,
                        kind: ObjectKind::Table,
                    },
                    target_columns: self.referenced_chosen.clone(),
                    on_delete: self.on_delete,
                    on_update: self.on_update,
                },
                cx,
            );
        }
        leave(&self.back, window, cx);
    }

    fn dropdown(
        &self,
        id: &'static str,
        which: KeyMenu,
        label: String,
        items: Vec<MenuItem>,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let mut el = div()
            .id(id)
            .h(t.s(26.))
            .min_w(px(180.))
            .px(px(8.))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(6.))
            .rounded(t.radius_small)
            .bg(t.input)
            .border_1()
            .border_color(t.border_strong)
            .cursor_pointer()
            .child(div().overflow_hidden().whitespace_nowrap().child(label))
            .child(
                icon(Icon::ChevronDown)
                    .size(px(11.))
                    .text_color(t.text_faint),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.menu = Some(which);
                cx.notify();
            }));
        if self.menu == Some(which) {
            let weak = cx.entity().downgrade();
            el = el.child(menu(
                ElementId::Name(format!("{id}-menu").into()),
                items,
                200.,
                move |_, cx| {
                    weak.update(cx, |this, cx| {
                        this.menu = None;
                        cx.notify();
                    })
                    .ok();
                },
                t,
            ));
        }
        el
    }

    fn action_items(
        &self,
        current: ForeignKeyAction,
        set: fn(&mut KeyDialog, ForeignKeyAction),
        cx: &mut Context<Self>,
    ) -> Vec<MenuItem> {
        ForeignKeyAction::ALL
            .into_iter()
            .map(|action| {
                let weak = cx.entity().downgrade();
                MenuItem::new(action.sql(), move |_, cx| {
                    weak.update(cx, |this, cx| {
                        set(this, action);
                        cx.notify();
                    })
                    .ok();
                })
                .checked(action == current)
            })
            .collect()
    }
}

impl Render for KeyDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let weak = cx.entity().downgrade();
        let columns = picker(
            "key-column",
            &self.target.columns,
            &self.chosen,
            move |name, _, cx| {
                weak.update(cx, |this, cx| {
                    toggle_in(&mut this.chosen, name);
                    this.error = None;
                    this.rename(cx);
                })
                .ok();
            },
            &t,
        );
        let table_items: Vec<MenuItem> = match &self.tables {
            Some(Ok(tables)) => tables
                .iter()
                .map(|name| {
                    let weak = cx.entity().downgrade();
                    let pick = name.clone();
                    MenuItem::new(name.clone(), move |_, cx| {
                        weak.update(cx, |this, cx| this.pick_table(pick.clone(), cx))
                            .ok();
                    })
                    .checked(self.referenced.as_deref() == Some(name.as_str()))
                })
                .collect(),
            Some(Err(e)) => vec![MenuItem::new(e.clone(), |_, _| {}).enabled(false)],
            None => vec![MenuItem::new("Loading…", |_, _| {}).enabled(false)],
        };
        let table_label = self
            .referenced
            .clone()
            .unwrap_or_else(|| "Choose a table".into());
        let table = self.dropdown(
            "key-table",
            KeyMenu::Table,
            table_label,
            table_items,
            &t,
            cx,
        );
        let referenced: AnyElement = match &self.referenced_columns {
            None if self.referenced.is_some() => {
                note("Reading its columns…", false, &t).into_any_element()
            }
            None => note("Pick a table first.", false, &t).into_any_element(),
            Some(Err(e)) => note(e.clone(), true, &t).into_any_element(),
            Some(Ok(columns)) => {
                let names: Vec<String> = columns.iter().map(|(n, _)| n.clone()).collect();
                let weak = cx.entity().downgrade();
                picker(
                    "key-referenced",
                    &names,
                    &self.referenced_chosen,
                    move |name, _, cx| {
                        weak.update(cx, |this, cx| {
                            toggle_in(&mut this.referenced_chosen, name);
                            this.error = None;
                            cx.notify();
                        })
                        .ok();
                    },
                    &t,
                )
                .into_any_element()
            }
        };
        let delete_items = self.action_items(self.on_delete, |this, a| this.on_delete = a, cx);
        let update_items = self.action_items(self.on_update, |this, a| this.on_update = a, cx);
        let on_delete = self.dropdown(
            "key-on-delete",
            KeyMenu::OnDelete,
            self.on_delete.sql().into(),
            delete_items,
            &t,
            cx,
        );
        let on_update = self.dropdown(
            "key-on-update",
            KeyMenu::OnUpdate,
            self.on_update.sql().into(),
            update_items,
            &t,
            cx,
        );
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(option_row("Columns", columns, &t))
            .child(option_row("References", table, &t))
            .child(option_row("Its columns", referenced, &t))
            .child(note(
                "Pick the columns on both sides in the same order.",
                false,
                &t,
            ))
            .child(option_row("On delete", on_delete, &t))
            .child(option_row("On update", on_update, &t));
        if self.target.flavor.named_foreign_keys() {
            body = body.child(option_row(
                "Name",
                field(&self.name, &t, window, cx).flex_1(),
                &t,
            ));
        }
        if let Some(e) = &self.error {
            body = body.child(note(e.clone(), true, &t));
        }
        let back = self.back.clone();
        let buttons = vec![
            button("key-cancel", "Cancel", ButtonStyle::Ghost, &t)
                .on_click(move |_, window, cx| leave(&back, window, cx))
                .into_any_element(),
            button("key-add", "Add foreign key", ButtonStyle::Primary, &t)
                .on_click(cx.listener(|this, _, window, cx| this.add(window, cx)))
                .into_any_element(),
        ];
        dialog_frame(
            "Add a foreign key",
            Some(
                format!(
                    "From {}. It is made along with the other changes.",
                    self.target.table
                )
                .into(),
            ),
            body,
            buttons,
            &t,
        )
    }
}

// ---------------------------------------------------------------------------------------
// A new table.

struct NewTableDialog {
    state: Entity<AppState>,
    connection: String,
    conn: Arc<dyn Connection>,
    namespace: Vec<String>,
    flavor: Flavor,
    name: Entity<TextInput>,
    view: Entity<StructureView>,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

/// Opens the dialog that lays out a new table in `namespace`. It does nothing for a
/// database without tables to create, such as MongoDB.
pub fn new_table(
    state: Entity<AppState>,
    connection: String,
    namespace: Vec<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(session) = state.read(cx).session(&connection) else {
        return;
    };
    // The menus offer nothing here on a read-only connection; this is the backstop.
    if session.read_only {
        return;
    }
    let conn = session.conn.clone();
    let Some(flavor) = Flavor::of(session.kind, &conn.server_version()).filter(|f| f.columns())
    else {
        return;
    };
    let dialog = cx.new(|cx| {
        let name = cx.new(|cx| TextInput::new("Table name", cx));
        let view = cx.new(|cx| {
            StructureView::new_table(
                state.clone(),
                connection.clone(),
                namespace.clone(),
                flavor,
                window,
                cx,
            )
        });
        let subs = vec![cx.subscribe_in(
            &name,
            window,
            |this: &mut NewTableDialog, _, e, window, cx| match e {
                InputEvent::Submit => this.review(window, cx),
                InputEvent::Cancel => close_dialog(cx),
                InputEvent::Changed => {
                    // New indexes and keys take their names from the table's.
                    let name = this.name.read(cx).text().trim().to_owned();
                    this.view.update(cx, |v, _| v.object.name = name);
                    this.error = None;
                    cx.notify();
                }
            },
        )];
        NewTableDialog {
            state: state.clone(),
            connection: connection.clone(),
            conn: conn.clone(),
            namespace: namespace.clone(),
            flavor,
            name,
            view,
            error: None,
            _subs: subs,
        }
    });
    let any: AnyView = dialog.clone().into();
    dialog.update(cx, |d, cx| {
        d.view.update(cx, |v, _| v.host = Some(any.clone()))
    });
    let focus = dialog.read(cx).name.read(cx).focus_handle(cx);
    open_dialog(any, window, cx);
    window.focus(&focus);
}

impl NewTableDialog {
    fn review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim().to_owned();
        let table = self.view.read(cx).new_table_def(&name);
        let plan = ddl::create_table(self.flavor, &table);
        let (state, connection, namespace) = (
            self.state.clone(),
            self.connection.clone(),
            self.namespace.clone(),
        );
        let object = ObjectRef {
            namespace: namespace.clone(),
            name: name.clone(),
            kind: ObjectKind::Table,
        };
        review(
            Review {
                title: format!("Create {}", if name.is_empty() { "a table" } else { &name }),
                subtitle: namespace.join("."),
                connection: self.conn.clone(),
                namespace: namespace.clone(),
                plan,
                transactional: self.flavor.transactional(),
                confirm: None,
                environment: Environment::of(self.state.read(cx), &self.connection),
                schema: true,
                action: "Create table",
                back: Some(cx.entity().into()),
                on_done: Box::new(move |cx| {
                    state.update(cx, |s, cx| {
                        s.load_objects(&connection, &namespace, cx);
                        s.open(
                            PanelKind::Table {
                                connection: connection.clone(),
                                object,
                            },
                            true,
                            cx,
                        );
                    });
                }),
            },
            window,
            cx,
        );
    }
}

impl Render for NewTableDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(option_row(
                "Name",
                field(&self.name, &t, window, cx).flex_1(),
                &t,
            ))
            .child(
                div()
                    .h(px(360.))
                    .flex()
                    .flex_col()
                    .rounded(t.radius_small)
                    .border_1()
                    .border_color(t.border)
                    .overflow_hidden()
                    .child(self.view.clone()),
            )
            .child(note(
                "Click a cell to change it, and the key mark to choose the primary key.",
                false,
                &t,
            ));
        if let Some(e) = &self.error {
            body = body.child(note(e.clone(), true, &t));
        }
        let buttons = vec![
            button("new-table-cancel", "Cancel", ButtonStyle::Ghost, &t)
                .on_click(|_, _, cx| close_dialog(cx))
                .into_any_element(),
            button("new-table-review", "Review SQL…", ButtonStyle::Primary, &t)
                .on_click(cx.listener(|this, _, window, cx| this.review(window, cx)))
                .into_any_element(),
        ];
        dialog_frame(
            "New table",
            Some(SharedString::from(self.namespace.join("."))),
            body,
            buttons,
            &t,
        )
        .w(px(860.))
    }
}

// ---------------------------------------------------------------------------------------
// Dropping and emptying.

fn destructive(
    conn: Arc<dyn Connection>,
    environment: Option<Environment>,
    object: &ObjectRef,
    drop: bool,
    on_done: Box<dyn FnOnce(&mut App)>,
    window: &mut Window,
    cx: &mut App,
) {
    let flavor = Flavor::of(conn.kind(), &conn.server_version());
    let plan = flavor
        .map(|f| Plan {
            statements: vec![if drop {
                ddl::drop_table(f, object)
            } else {
                ddl::truncate_table(f, object)
            }],
            notes: Vec::new(),
        })
        .ok_or_else(|| "This database cannot do that here.".to_string());
    let what = match object.kind {
        ObjectKind::View | ObjectKind::MaterializedView => "view",
        ObjectKind::Collection => "collection",
        _ => "table",
    };
    let (title, action) = if drop {
        (format!("Drop {what} {}?", object.name), "Drop")
    } else {
        (format!("Empty {what} {}?", object.name), "Delete all rows")
    };
    review(
        Review {
            title,
            subtitle: path_of(object),
            connection: conn,
            namespace: object.namespace.clone(),
            plan,
            transactional: false,
            confirm: Some(object.name.clone()),
            environment,
            schema: false,
            action,
            back: None,
            on_done,
        },
        window,
        cx,
    );
}

/// Asks for the object's name, then drops it. A staging or production `environment`
/// shows in the dialog.
pub fn confirm_drop(
    conn: Arc<dyn Connection>,
    environment: Option<Environment>,
    object: &ObjectRef,
    on_done: Box<dyn FnOnce(&mut App)>,
    window: &mut Window,
    cx: &mut App,
) {
    destructive(conn, environment, object, true, on_done, window, cx);
}

/// Asks for the table's name, then deletes every row. A staging or production
/// `environment` shows in the dialog.
pub fn confirm_truncate(
    conn: Arc<dyn Connection>,
    environment: Option<Environment>,
    object: &ObjectRef,
    on_done: Box<dyn FnOnce(&mut App)>,
    window: &mut Window,
    cx: &mut App,
) {
    destructive(conn, environment, object, false, on_done, window, cx);
}
