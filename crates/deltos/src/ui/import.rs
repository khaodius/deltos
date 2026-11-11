//! Importing connections from other tools, in the connection list's right-hand pane: the
//! files found on this computer, or picked with "Choose file…", each connection with a
//! checkbox. Connections that are already saved, listed twice or not supported are shown
//! but left out.

use std::path::PathBuf;

use deltos_data::DriverKind;
use deltos_settings::SavedConnection;
use gpui::{
    AnyElement, Context, Entity, EventEmitter, FontWeight, IntoElement, Render, SharedString,
    Window, div, prelude::*, px,
};

use crate::import::{self, Found, Imported, Source};
use crate::state::AppState;
use crate::theme::{Theme, theme};
use crate::ui::icons::{Icon, icon};
use crate::ui::{ButtonStyle, button, caption, environment};

pub enum ImportEvent {
    /// The person is done; `added` connections were saved.
    Close { added: usize },
}

struct Group {
    source: Source,
    path: PathBuf,
    error: Option<String>,
}

struct Row {
    group: usize,
    imported: Imported,
    /// Why the row can't be picked.
    skip: Option<String>,
    checked: bool,
}

pub struct ImportView {
    state: Entity<AppState>,
    groups: Vec<Group>,
    rows: Vec<Row>,
    scanning: bool,
    error: Option<SharedString>,
}

impl EventEmitter<ImportEvent> for ImportView {}

impl ImportView {
    /// Looks in the usual places when `scan` is set; otherwise starts empty, for a file.
    pub fn new(state: Entity<AppState>, scan: bool, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            state,
            groups: Vec::new(),
            rows: Vec::new(),
            scanning: false,
            error: None,
        };
        if scan {
            this.scan(cx);
        }
        this
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        self.scanning = true;
        let job = cx.background_executor().spawn(async { import::scan() });
        cx.spawn(async move |this, cx| {
            let found = job.await;
            this.update(cx, |this, cx| {
                this.scanning = false;
                for f in found {
                    this.add_found(f, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Asks for files to import and reads them.
    pub fn choose_file(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Ok(job) = cx.update(|cx| {
                cx.background_executor().spawn(async move {
                    paths
                        .into_iter()
                        .map(|p| match import::detect(&p) {
                            Some(source) => Ok(import::read(source, &p)),
                            None => Err(p),
                        })
                        .collect::<Vec<_>>()
                })
            }) else {
                return;
            };
            let results = job.await;
            this.update(cx, |this, cx| {
                this.error = None;
                for r in results {
                    match r {
                        Ok(found) => this.add_found(found, cx),
                        Err(path) => {
                            this.error = Some(
                                format!(
                                    "{} is not a DBeaver, JetBrains or TablePlus connection file.",
                                    path.display()
                                )
                                .into(),
                            )
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn add_found(&mut self, found: Found, cx: &mut Context<Self>) {
        if self.groups.iter().any(|g| g.path == found.path) {
            return;
        }
        let group = self.groups.len();
        let (imported, error) = match found.result {
            Ok(list) => (list, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        self.groups.push(Group {
            source: found.source,
            path: found.path,
            error,
        });
        let saved = self.state.read(cx).settings.connections.clone();
        for imported in imported {
            let listed: Vec<SavedConnection> = self
                .rows
                .iter()
                .filter(|r| r.skip.is_none())
                .map(|r| r.imported.saved.clone())
                .collect();
            let skip = if let Some(why) = &imported.unsupported {
                Some(why.clone())
            } else if import::is_duplicate(&imported.saved, &saved) {
                Some("Already saved".to_owned())
            } else if import::is_duplicate(&imported.saved, &listed) {
                Some("Listed above".to_owned())
            } else {
                None
            };
            self.rows.push(Row {
                group,
                imported,
                checked: skip.is_none(),
                skip,
            });
        }
    }

    fn add(&mut self, cx: &mut Context<Self>) {
        let picked: Vec<SavedConnection> = self
            .rows
            .iter()
            .filter(|r| r.checked && r.skip.is_none())
            .map(|r| r.imported.saved.clone())
            .collect();
        let added = picked.len();
        self.state.update(cx, |s, cx| {
            for mut saved in picked {
                saved.id = s.settings.new_connection_id(&saved.name);
                s.save_connection(saved, cx);
            }
        });
        cx.emit(ImportEvent::Close { added });
    }

    fn row(&self, ix: usize, row: &Row, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let saved = &row.imported.saved;
        let pickable = row.skip.is_none();
        let checked = row.checked && pickable;
        // An unsupported database has nothing to describe; its note says why.
        let detail = row
            .imported
            .folder
            .clone()
            .into_iter()
            .chain(row.imported.unsupported.is_none().then(|| describe(saved)))
            .collect::<Vec<_>>()
            .join(" · ");
        let note: Option<SharedString> = match &row.skip {
            Some(why) => Some(why.clone().into()),
            None if !row.imported.notes.is_empty() => Some(row.imported.notes.join(" · ").into()),
            None => None,
        };
        div()
            .id(("import-row", ix))
            .w_full()
            .px(px(10.))
            .py(px(6.))
            .flex()
            .items_center()
            .gap(px(10.))
            .rounded(t.radius_small)
            .when(pickable, |el| {
                el.cursor_pointer()
                    .hover(|s| s.bg(t.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(r) = this.rows.get_mut(ix) {
                            r.checked = !r.checked;
                        }
                        cx.notify();
                    }))
            })
            .when(!pickable, |el| el.opacity(0.55))
            .child(
                div()
                    .flex_none()
                    .size(px(14.))
                    .rounded(px(3.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_1()
                    .when(checked, |el| {
                        el.bg(t.accent)
                            .border_color(t.accent)
                            .child(icon(Icon::Check).size(px(10.)).text_color(t.accent_text))
                    })
                    .when(!checked, |el| el.bg(t.input).border_color(t.border_strong)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(t.ui_size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.text)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(saved.name.clone()),
                    )
                    .when(!detail.is_empty(), |el| {
                        el.child(
                            div()
                                .text_size(t.small_size)
                                .text_color(t.text_faint)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(detail),
                        )
                    })
                    .children(note.map(|n| {
                        div()
                            .text_size(t.small_size)
                            .text_color(t.text_muted)
                            .whitespace_normal()
                            .child(n)
                    })),
            )
            .children(environment::badge(saved.tag, t))
            .when(saved.read_only, |el| {
                el.child(icon(Icon::Lock).size(px(11.)).text_color(t.text_faint))
            })
            .into_any_element()
    }
}

/// `PostgreSQL · db.example.com:5432/shop via SSH bastion`.
fn describe(c: &SavedConnection) -> String {
    let kind = c.kind.label();
    if c.kind == DriverKind::Sqlite {
        return format!("{kind} · {}", c.config.database);
    }
    let mut out = format!("{kind} · ");
    if !c.config.user.is_empty() {
        out.push_str(&c.config.user);
        out.push('@');
    }
    out.push_str(if c.config.host.is_empty() {
        "localhost"
    } else {
        &c.config.host
    });
    if let Some(port) = c.config.port {
        out.push_str(&format!(":{port}"));
    }
    if !c.config.database.is_empty() {
        out.push('/');
        out.push_str(&c.config.database);
    }
    if c.config.ssh.enabled {
        out.push_str(&format!(" via SSH {}", c.config.ssh.host));
    }
    out
}

impl Render for ImportView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let picked = self
            .rows
            .iter()
            .filter(|r| r.checked && r.skip.is_none())
            .count();
        let mut list = div().flex().flex_col().gap(px(14.));
        for (g, group) in self.groups.iter().enumerate() {
            let mut section = div().flex().flex_col().gap(px(2.)).child(
                div()
                    .px(px(10.))
                    .pb(px(2.))
                    .flex()
                    .items_baseline()
                    .gap(px(8.))
                    .child(caption(group.source.label(), &t))
                    .child(
                        div()
                            .min_w_0()
                            .text_size(t.small_size)
                            .text_color(t.text_faint)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(group.path.display().to_string()),
                    ),
            );
            if let Some(e) = &group.error {
                section = section.child(
                    div()
                        .px(px(10.))
                        .text_size(t.small_size)
                        .text_color(t.danger)
                        .whitespace_normal()
                        .child(e.clone()),
                );
            }
            let rows: Vec<(usize, &Row)> = self
                .rows
                .iter()
                .enumerate()
                .filter(|(_, r)| r.group == g)
                .collect();
            if rows.is_empty() && group.error.is_none() {
                section = section.child(
                    div()
                        .px(px(10.))
                        .text_size(t.small_size)
                        .text_color(t.text_faint)
                        .child("No connections in this file."),
                );
            }
            for (ix, row) in rows {
                section = section.child(self.row(ix, row, &t, cx));
            }
            list = list.child(section);
        }
        let empty = self.groups.is_empty();
        div()
            .id("import")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .max_w(px(620.))
                    .mx_auto()
                    .px(px(24.))
                    .py(px(24.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(
                        div()
                            .text_size(t.s(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.text)
                            .child("Import connections"),
                    )
                    .child(
                        div()
                            .text_size(t.small_size)
                            .text_color(t.text_muted)
                            .whitespace_normal()
                            .child("From DBeaver, DataGrip and the other JetBrains IDEs, and TablePlus. Passwords stay where they are; Deltos asks for each one the first time it connects."),
                    )
                    .when(self.scanning, |el| {
                        el.child(
                            div()
                                .text_size(t.small_size)
                                .text_color(t.text_faint)
                                .child("Looking for connections…"),
                        )
                    })
                    .when(empty && !self.scanning, |el| {
                        el.child(
                            div()
                                .py(px(12.))
                                .text_size(t.ui_size)
                                .text_color(t.text_muted)
                                .whitespace_normal()
                                .child("No connection files found in the usual places. Choose a DBeaver data-sources.json, a JetBrains dataSources.xml or a TablePlus connection list."),
                        )
                    })
                    .children(self.error.clone().map(|e| {
                        div()
                            .text_size(t.small_size)
                            .text_color(t.danger)
                            .whitespace_normal()
                            .child(e)
                    }))
                    .child(list)
                    .child(
                        div()
                            .pt(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                button("import-choose", "Choose file…", ButtonStyle::Secondary, &t)
                                    .on_click(cx.listener(|this, _, _, cx| this.choose_file(cx))),
                            )
                            .child(div().flex_1())
                            .child(
                                button("import-cancel", "Cancel", ButtonStyle::Ghost, &t)
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(ImportEvent::Close { added: 0 })
                                    })),
                            )
                            .child(
                                button(
                                    "import-add",
                                    match picked {
                                        1 => "Add 1 connection".to_owned(),
                                        n => format!("Add {n} connections"),
                                    },
                                    ButtonStyle::Primary,
                                    &t,
                                )
                                .when(picked == 0, |el| el.opacity(0.5))
                                .when(picked > 0, |el| {
                                    el.on_click(cx.listener(|this, _, _, cx| this.add(cx)))
                                }),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_data::ConnectionConfig;

    #[test]
    fn describes_connections() {
        let mut c = SavedConnection {
            kind: DriverKind::Postgres,
            config: ConnectionConfig {
                host: "db".into(),
                port: Some(5432),
                user: "me".into(),
                database: "shop".into(),
                ..ConnectionConfig::default()
            },
            ..SavedConnection::default()
        };
        assert_eq!(describe(&c), "PostgreSQL · me@db:5432/shop");
        c.config.ssh.enabled = true;
        c.config.ssh.host = "bastion".into();
        assert_eq!(describe(&c), "PostgreSQL · me@db:5432/shop via SSH bastion");
    }
}
