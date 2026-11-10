//! The connection list and the form behind it: saved connections on the left, the
//! selected one's settings on the right. Passwords go to the system keychain, never to
//! the settings file.

use deltos_data::{ConnectionConfig, DriverKind, TlsMode};
use deltos_settings::{ConnectionTag, PanelKind, SavedConnection};
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FontWeight, IntoElement, MouseButton, Render,
    SharedString, Subscription, Window, div, prelude::*, px,
};

use crate::state::{AppState, ConnectState, DEMO_ID, StateEvent, open_connection};
use crate::theme::{Theme, color, theme};
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{
    ButtonStyle, MenuItem, Tooltip, button, caption, dot, empty_state, field, icon_button, menu,
    segmented, switch,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Selected {
    New,
    Saved(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenMenu {
    Kind,
    Tag,
}

/// The result of the last Test press.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Test {
    Running,
    Passed(String),
    Failed(String),
}

struct Form {
    kind: DriverKind,
    tag: ConnectionTag,
    tls: TlsMode,
    save_password: bool,
    name: Entity<TextInput>,
    url: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    user: Entity<TextInput>,
    password: Entity<TextInput>,
    database: Entity<TextInput>,
}

pub struct ConnectionsPanel {
    state: Entity<AppState>,
    selected: Option<Selected>,
    form: Form,
    menu: Option<OpenMenu>,
    test: Option<Test>,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

impl ConnectionsPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| TextInput::new(placeholder, cx))
        };
        let form = Form {
            kind: DriverKind::Postgres,
            tag: ConnectionTag::None,
            tls: TlsMode::Prefer,
            save_password: true,
            name: input("My database", cx),
            url: input("postgres://user@host:5432/db", cx),
            host: input("localhost", cx),
            port: input("5432", cx),
            user: input("postgres", cx),
            password: cx.new(|cx| TextInput::new("Password", cx).masked()),
            database: input("postgres", cx),
        };
        let mut subs = vec![cx.subscribe(&state, |_, _, e, cx| {
            if matches!(
                e,
                StateEvent::Sessions | StateEvent::Settings | StateEvent::ActiveSession
            ) {
                cx.notify();
            }
        })];
        for input in [
            &form.name,
            &form.host,
            &form.port,
            &form.user,
            &form.password,
            &form.database,
        ] {
            subs.push(cx.subscribe(input, |this, _, e, cx| match e {
                InputEvent::Submit => this.save(true, cx),
                InputEvent::Changed => {
                    this.test = None;
                    cx.notify();
                }
                InputEvent::Cancel => {}
            }));
        }
        // A pasted URL picks the kind of database.
        subs.push(cx.subscribe(&form.url, |this, input, e, cx| match e {
            InputEvent::Changed => {
                if let Some(kind) = deltos_drivers::kind_from_url(input.read(cx).text()) {
                    this.set_kind(kind, cx);
                }
                this.test = None;
                cx.notify();
            }
            InputEvent::Submit => this.save(true, cx),
            InputEvent::Cancel => {}
        }));
        let mut this = Self {
            state,
            selected: None,
            form,
            menu: None,
            test: None,
            error: None,
            _subs: subs,
        };
        if this.state.read(cx).settings.connections.is_empty() {
            this.select(Some(Selected::New), cx);
        }
        this
    }

    fn set_kind(&mut self, kind: DriverKind, cx: &mut Context<Self>) {
        self.form.kind = kind;
        let (port, user, database) = match kind {
            DriverKind::Postgres => ("5432", "postgres", "postgres"),
            DriverKind::Mysql => ("3306", "root", "database"),
            DriverKind::Sqlite => ("", "", "/path/to/file.db"),
            DriverKind::Mongodb => ("27017", "", "admin"),
            DriverKind::Mssql => ("1433", "sa", "master"),
            _ => ("", "", ""),
        };
        let port: SharedString = kind
            .default_port()
            .map_or_else(|| port.into(), |p| p.to_string().into());
        self.form
            .port
            .update(cx, |i, cx| i.set_placeholder(port, cx));
        self.form
            .user
            .update(cx, |i, cx| i.set_placeholder(user, cx));
        self.form
            .database
            .update(cx, |i, cx| i.set_placeholder(database, cx));
        cx.notify();
    }

    /// Fills the form from a saved connection, or empties it for a new one.
    fn select(&mut self, selected: Option<Selected>, cx: &mut Context<Self>) {
        let saved = match &selected {
            Some(Selected::Saved(id)) => self.state.read(cx).settings.connection(id).cloned(),
            _ => None,
        };
        let c = saved.unwrap_or_default();
        let set = |input: &Entity<TextInput>, text: String, cx: &mut Context<Self>| {
            input.update(cx, |i, cx| i.set_text(text, cx))
        };
        set(&self.form.name, c.name.clone(), cx);
        set(&self.form.url, c.config.url.clone().unwrap_or_default(), cx);
        set(&self.form.host, c.config.host.clone(), cx);
        set(
            &self.form.port,
            c.config.port.map(|p| p.to_string()).unwrap_or_default(),
            cx,
        );
        set(&self.form.user, c.config.user.clone(), cx);
        set(&self.form.password, String::new(), cx);
        set(&self.form.database, c.config.database.clone(), cx);
        self.form.tag = c.tag;
        self.form.tls = c.config.tls;
        self.form.save_password = c.save_password;
        self.set_kind(c.kind, cx);
        self.selected = selected;
        self.test = None;
        self.error = None;
        self.menu = None;
        cx.notify();
    }

    /// The form as a saved connection; the password is only in the returned config.
    fn build(&self, cx: &App) -> Result<SavedConnection, String> {
        let text = |i: &Entity<TextInput>| i.read(cx).text().trim().to_owned();
        let f = &self.form;
        let port = match text(&f.port) {
            p if p.is_empty() => None,
            p => Some(
                p.parse::<u16>()
                    .map_err(|_| format!("“{p}” is not a port number."))?,
            ),
        };
        let url = Some(text(&f.url)).filter(|u| !u.is_empty());
        let host = text(&f.host);
        let database = text(&f.database);
        if url.is_none() && f.kind == DriverKind::Sqlite && database.is_empty() {
            return Err("Give the path of the database file.".into());
        }
        let mut name = text(&f.name);
        if name.is_empty() {
            name = match (&url, f.kind) {
                (_, DriverKind::Sqlite) => database
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or("SQLite")
                    .to_owned(),
                (None, _) if !host.is_empty() => host.clone(),
                _ => f.kind.label().to_owned(),
            };
        }
        let password = f.password.read(cx).text().to_owned();
        let settings = &self.state.read(cx).settings;
        let id = match &self.selected {
            Some(Selected::Saved(id)) => id.clone(),
            _ => settings.new_connection_id(&name),
        };
        Ok(SavedConnection {
            id,
            name,
            kind: f.kind,
            config: ConnectionConfig {
                host,
                port,
                user: text(&f.user),
                password: (!password.is_empty()).then_some(password),
                database,
                tls: f.tls,
                url,
                options: Vec::new(),
            },
            tag: f.tag,
            save_password: f.save_password,
        })
    }

    fn save(&mut self, connect: bool, cx: &mut Context<Self>) {
        let saved = match self.build(cx) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        self.error = None;
        let config = saved.config.clone();
        let id = saved.id.clone();
        self.state.update(cx, |s, cx| {
            s.save_connection(saved.clone(), cx);
            if connect {
                if s.session(&id).is_some() {
                    s.disconnect(&id, cx);
                }
                // The keychain write may still be on its way, so connect with what the form has.
                s.connect_with(saved, config, cx);
            }
        });
        self.selected = Some(Selected::Saved(id));
        cx.notify();
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        let saved = match self.build(cx) {
            Ok(s) => s,
            Err(e) => {
                self.test = Some(Test::Failed(e));
                cx.notify();
                return;
            }
        };
        self.test = Some(Test::Running);
        let attempt = open_connection(saved.kind, saved.config, cx);
        cx.spawn(async move |this, cx| {
            let result = attempt.await;
            this.update(cx, |this, cx| {
                this.test = Some(match result {
                    Ok(conn) => {
                        let version = conn.server_version();
                        let net = cx.global::<crate::net::Net>();
                        net.spawn(async move { conn.close().await });
                        Test::Passed(version)
                    }
                    Err(e) => Test::Failed(e),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn list(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state.read(cx);
        let mut rows = div().flex().flex_col().gap(px(1.)).px(px(6.));
        let mut entries: Vec<(String, SharedString, SharedString, Option<u32>)> = s
            .settings
            .connections
            .iter()
            .map(|c| {
                let target = c.config.url.clone().unwrap_or_else(|| match c.kind {
                    DriverKind::Sqlite => c.config.database.clone(),
                    _ => format!(
                        "{}{}{}",
                        if c.config.host.is_empty() {
                            "localhost"
                        } else {
                            &c.config.host
                        },
                        c.config.port.map(|p| format!(":{p}")).unwrap_or_default(),
                        if c.config.database.is_empty() {
                            String::new()
                        } else {
                            format!("/{}", c.config.database)
                        }
                    ),
                });
                (
                    c.id.clone(),
                    c.name.clone().into(),
                    format!("{} · {}", c.kind.label(), target).into(),
                    c.tag.color(),
                )
            })
            .collect();
        entries.insert(
            0,
            (
                DEMO_ID.to_owned(),
                "Demo".into(),
                "Built-in sample data · 50M rows".into(),
                ConnectionTag::Local.color(),
            ),
        );
        for (id, name, detail, tag) in entries {
            let selected = self.selected == Some(Selected::Saved(id.clone()));
            let connected = s.session(&id).is_some();
            let status = s.connecting.get(&id).cloned();
            let (pick, open) = (id.clone(), id.clone());
            rows = rows.child(
                div()
                    .id(SharedString::from(format!("conn-{id}")))
                    .h(t.s(44.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .rounded(t.radius_small)
                    .cursor_pointer()
                    .when(selected, |el| el.bg(t.selected))
                    .when(!selected, |el| el.hover(|s| s.bg(t.hover)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                            if e.click_count >= 2 {
                                let id = open.clone();
                                this.state.update(cx, |s, cx| s.connect(&id, cx));
                            } else if pick == DEMO_ID {
                                this.selected = Some(Selected::Saved(pick.clone()));
                                cx.notify();
                            } else {
                                this.select(Some(Selected::Saved(pick.clone())), cx);
                            }
                        }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .size(t.s(28.))
                            .rounded(t.radius_small)
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(tag.map_or(t.surface_alt, |c| color(c).opacity(0.16)))
                            .child(
                                icon(Icon::Database)
                                    .size(px(14.))
                                    .text_color(tag.map_or(t.text_muted, color)),
                            ),
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
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_size(t.small_size)
                                    .text_color(match &status {
                                        Some(ConnectState::Failed(_)) => t.danger,
                                        _ => t.text_faint,
                                    })
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(match &status {
                                        Some(ConnectState::Connecting) => {
                                            SharedString::from("Connecting…")
                                        }
                                        Some(ConnectState::Failed(e)) => e.clone().into(),
                                        None => detail,
                                    }),
                            ),
                    )
                    .when(connected, |el| el.child(dot(t.success, 7.))),
            );
        }
        rows.into_any_element()
    }

    fn picker(
        &self,
        id: &'static str,
        label: SharedString,
        t: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .relative()
            .h(t.s(26.))
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
            .text_size(t.ui_size)
            .child(label)
            .child(
                icon(Icon::ChevronDown)
                    .size(px(12.))
                    .text_color(t.text_faint),
            )
    }

    fn form(&mut self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(selected) = self.selected.clone() else {
            return empty_state(
                "Select a connection",
                Some("Or create one with the + button. Double-click to connect.".into()),
                t,
            )
            .into_any_element();
        };
        if selected == Selected::Saved(DEMO_ID.into()) {
            let state = self.state.clone();
            let connected = self.state.read(cx).session(DEMO_ID).is_some();
            return empty_state(
                "Demo",
                Some("A built-in PostgreSQL-like database with a 50 million row table, a 300 column table and JSON documents. Nothing leaves your computer.".into()),
                t,
            )
            .child(
                button("demo-connect", if connected { "Open" } else { "Connect" }, ButtonStyle::Primary, t)
                    .mt(px(10.))
                    .on_click(move |_, _, cx| state.update(cx, |s, cx| s.open_demo(cx))),
            )
            .into_any_element();
        }
        let available = deltos_drivers::available();
        let f = &self.form;
        let sqlite = f.kind == DriverKind::Sqlite;
        let row = |label: &'static str, content: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(
                    div()
                        .w(px(96.))
                        .flex_none()
                        .text_size(t.ui_size)
                        .text_color(t.text_muted)
                        .flex()
                        .justify_end()
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(content))
        };
        // Kind picker, with kinds this build cannot open marked as coming soon.
        let mut kind = self
            .picker("conn-kind", f.kind.label().into(), t)
            .on_click(cx.listener(|this, _, _, cx| {
                this.menu = Some(OpenMenu::Kind);
                cx.notify();
            }));
        if self.menu == Some(OpenMenu::Kind) {
            let weak = cx.entity().downgrade();
            let items = DriverKind::ALL
                .iter()
                .map(|k| {
                    let (w, k) = (weak.clone(), *k);
                    let ok = available.contains(&k);
                    let item = MenuItem::new(k.label(), move |_, cx| {
                        w.update(cx, |this, cx| this.set_kind(k, cx)).ok();
                    })
                    .enabled(ok)
                    .checked(k == f.kind);
                    if ok { item } else { item.detail("coming soon") }
                })
                .collect();
            let close = weak.clone();
            kind = kind.child(menu(
                "kind-menu",
                items,
                220.,
                move |_, cx| {
                    close
                        .update(cx, |this, cx| {
                            this.menu = None;
                            cx.notify();
                        })
                        .ok();
                },
                t,
            ));
        }
        let mut tag = self
            .picker("conn-tag", f.tag.label().into(), t)
            .on_click(cx.listener(|this, _, _, cx| {
                this.menu = Some(OpenMenu::Tag);
                cx.notify();
            }));
        if self.menu == Some(OpenMenu::Tag) {
            let weak = cx.entity().downgrade();
            let items = ConnectionTag::ALL
                .iter()
                .map(|g| {
                    let (w, g) = (weak.clone(), *g);
                    MenuItem::new(g.label(), move |_, cx| {
                        w.update(cx, |this, cx| {
                            this.form.tag = g;
                            cx.notify();
                        })
                        .ok();
                    })
                    .checked(g == f.tag)
                })
                .collect();
            let close = weak.clone();
            tag = tag.child(menu(
                "tag-menu",
                items,
                160.,
                move |_, cx| {
                    close
                        .update(cx, |this, cx| {
                            this.menu = None;
                            cx.notify();
                        })
                        .ok();
                },
                t,
            ));
        }
        let weak = cx.entity().downgrade();
        let tls = segmented(
            "conn-tls",
            &[
                ("Off", TlsMode::Disable),
                ("Prefer", TlsMode::Prefer),
                ("Require", TlsMode::Require),
                ("Verify", TlsMode::VerifyFull),
            ],
            f.tls,
            move |m, _, cx| {
                weak.update(cx, |this, cx| {
                    this.form.tls = m;
                    cx.notify();
                })
                .ok();
            },
            t,
        );
        let save_pw = f.save_password;
        let test_line = self.test.clone().map(|test| {
            let (c, text): (_, SharedString) = match test {
                Test::Running => (t.text_faint, "Testing…".into()),
                Test::Passed(v) => (t.success, format!("Connected · {v}").into()),
                Test::Failed(e) => (t.danger, e.into()),
            };
            div()
                .text_size(t.small_size)
                .text_color(c)
                .whitespace_normal()
                .child(text)
        });
        let saved_id = match &selected {
            Selected::Saved(id) => Some(id.clone()),
            Selected::New => None,
        };
        let connected = saved_id
            .as_ref()
            .is_some_and(|id| self.state.read(cx).session(id).is_some());
        let title: SharedString = if saved_id.is_some() {
            "Edit connection".into()
        } else {
            "New connection".into()
        };
        div()
            .id("conn-form")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .max_w(px(560.))
                    .mx_auto()
                    .px(px(24.))
                    .py(px(24.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(
                        div()
                            .pb(px(6.))
                            .text_size(t.s(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.text)
                            .child(title),
                    )
                    .child(row(
                        "Name",
                        field(&f.name, t, window, cx).into_any_element(),
                    ))
                    .child(row(
                        "Type",
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(div().flex_1().child(kind))
                            .child(div().w(px(130.)).child(tag))
                            .into_any_element(),
                    ))
                    .child(div().h(px(1.)).bg(t.border).my(px(4.)))
                    .child(row("URL", field(&f.url, t, window, cx).into_any_element()))
                    .when(!sqlite, |el| {
                        el.child(row(
                            "Host",
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(div().flex_1().child(field(&f.host, t, window, cx)))
                                .child(div().w(px(80.)).child(field(&f.port, t, window, cx)))
                                .into_any_element(),
                        ))
                        .child(row(
                            "User",
                            field(&f.user, t, window, cx).into_any_element(),
                        ))
                        .child(row(
                            "Password",
                            div()
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .child(div().flex_1().child(field(&f.password, t, window, cx)))
                                .child(switch("save-pw", save_pw, t).on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.form.save_password = !this.form.save_password;
                                        cx.notify();
                                    },
                                )))
                                .child(
                                    div()
                                        .text_size(t.small_size)
                                        .text_color(t.text_muted)
                                        .child("Keychain"),
                                )
                                .into_any_element(),
                        ))
                    })
                    .child(row(
                        if sqlite { "File" } else { "Database" },
                        field(&f.database, t, window, cx).into_any_element(),
                    ))
                    .when(!sqlite, |el| {
                        el.child(row("TLS", div().flex().child(tls).into_any_element()))
                    })
                    .when(saved_id.is_some(), |el| {
                        el.child(row(
                            "",
                            div()
                                .text_size(t.small_size)
                                .text_color(t.text_faint)
                                .child("Leave the password empty to keep the saved one.")
                                .into_any_element(),
                        ))
                    })
                    .children(self.error.clone().map(|e| {
                        row(
                            "",
                            div()
                                .text_size(t.small_size)
                                .text_color(t.danger)
                                .child(e)
                                .into_any_element(),
                        )
                    }))
                    .children(test_line.map(|l| row("", l.into_any_element())))
                    .child(
                        div()
                            .pt(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .when_some(saved_id.clone(), |el, id| {
                                el.child(
                                    button("conn-delete", "Delete", ButtonStyle::Danger, t)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let id = id.clone();
                                            this.state.update(cx, |s, cx| {
                                                s.disconnect(&id, cx);
                                                s.delete_connection(&id, cx);
                                            });
                                            this.select(None, cx);
                                        })),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                button("conn-test", "Test", ButtonStyle::Secondary, t)
                                    .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
                            )
                            .child(
                                button("conn-save", "Save", ButtonStyle::Secondary, t)
                                    .on_click(cx.listener(|this, _, _, cx| this.save(false, cx))),
                            )
                            .child(
                                button(
                                    "conn-connect",
                                    if connected { "Reconnect" } else { "Connect" },
                                    ButtonStyle::Primary,
                                    t,
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.save(true, cx))),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Render for ConnectionsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let list = self.list(&t, cx);
        let form = self.form(&t, window, cx);
        let state = self.state.clone();
        div()
            .size_full()
            .flex()
            .bg(t.surface)
            .child(
                div()
                    .w(px(300.))
                    .flex_none()
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(t.sidebar)
                    .border_r_1()
                    .border_color(t.border)
                    .child(
                        div()
                            .flex_none()
                            .h(t.bar_height)
                            .px(px(12.))
                            .flex()
                            .items_center()
                            .child(caption("Connections", &t))
                            .child(div().flex_1())
                            .child(
                                icon_button("conn-new", Icon::Plus, &t)
                                    .tooltip(|_, cx| Tooltip::view("New connection", cx))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.select(Some(Selected::New), cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .id("conn-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(list),
                    )
                    .child(
                        div()
                            .flex_none()
                            .p(px(8.))
                            .border_t_1()
                            .border_color(t.border)
                            .child(
                                button("conn-query", "New query", ButtonStyle::Ghost, &t).on_click(
                                    move |_, _, cx| {
                                        let connection = state.read(cx).active.clone();
                                        state.update(cx, |s, cx| {
                                            s.open(
                                                PanelKind::Query {
                                                    connection,
                                                    text: String::new(),
                                                },
                                                true,
                                                cx,
                                            )
                                        });
                                    },
                                ),
                            ),
                    ),
            )
            .child(div().flex_1().min_w_0().h_full().child(form))
    }
}
