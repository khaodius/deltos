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
    Tag,
}

/// The result of the last Test press.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Test {
    Running,
    /// The server's version and how long the connection took, in milliseconds.
    Passed(String, u128),
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
    /// What the form did with a pasted URL, shown under the URL field.
    notice: Option<SharedString>,
    /// A database URL found on the clipboard when a new connection was started.
    clipboard_url: Option<String>,
    /// Text the form put into the URL and host fields itself, so their change handlers
    /// leave it alone.
    ignore_url: Option<String>,
    ignore_host: Option<String>,
    /// The field lengths before the last change, to tell a paste from typing.
    url_len: usize,
    host_len: usize,
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
            url: input("Paste a connection URL to fill in the form", cx),
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
            &form.port,
            &form.user,
            &form.password,
            &form.database,
        ] {
            subs.push(cx.subscribe(input, |this, _, e, cx| match e {
                InputEvent::Submit => this.save(true, cx),
                InputEvent::Changed => this.edited(cx),
                InputEvent::Cancel => {}
            }));
        }
        // A URL or `host:port` pasted into the host field is split into the fields.
        subs.push(cx.subscribe(&form.host, |this, input, e, cx| match e {
            InputEvent::Changed => {
                let text = input.read(cx).text().to_owned();
                let pasted = text.len() > this.host_len + 1;
                this.host_len = text.len();
                if this.ignore_host.take().is_some_and(|t| t == text) || !pasted {
                    this.edited(cx);
                    return;
                }
                if text.contains("://") {
                    this.apply_url(&text, cx);
                } else if let Some((host, port)) = split_host_port(&text) {
                    this.set_field(Field::Host, host, cx);
                    this.set_field(Field::Port, port.to_string(), cx);
                }
                this.edited(cx);
            }
            InputEvent::Submit => this.save(true, cx),
            InputEvent::Cancel => {}
        }));
        // A pasted URL fills in the form; Enter does the same for a typed one, then
        // connects.
        subs.push(cx.subscribe(&form.url, |this, input, e, cx| match e {
            InputEvent::Changed => {
                let text = input.read(cx).text().to_owned();
                let pasted = text.len() > this.url_len + 1;
                this.url_len = text.len();
                if this.ignore_url.take().is_some_and(|t| t == text) {
                    this.edited(cx);
                    return;
                }
                if text.trim().is_empty() {
                    this.notice = None;
                } else if pasted {
                    this.apply_url(&text, cx);
                } else if let Some(kind) = deltos_drivers::kind_from_url(&text) {
                    this.set_kind(kind, cx);
                }
                this.edited(cx);
            }
            InputEvent::Submit => {
                let text = input.read(cx).text().to_owned();
                if !text.trim().is_empty() {
                    this.apply_url(&text, cx);
                }
                this.save(true, cx);
            }
            InputEvent::Cancel => {}
        }));
        let mut this = Self {
            state,
            selected: None,
            form,
            menu: None,
            test: None,
            error: None,
            notice: None,
            clipboard_url: None,
            ignore_url: None,
            ignore_host: None,
            url_len: 0,
            host_len: 0,
            _subs: subs,
        };
        if this.state.read(cx).settings.connections.is_empty() {
            this.select(Some(Selected::New), cx);
        }
        this
    }

    /// After any edit: the last test no longer applies, and the name the connection
    /// would get follows the fields.
    fn edited(&mut self, cx: &mut Context<Self>) {
        self.test = None;
        let name = self.auto_name(cx);
        self.form
            .name
            .update(cx, |i, cx| i.set_placeholder(name, cx));
        cx.notify();
    }

    fn set_field(&mut self, field: Field, text: impl Into<String>, cx: &mut Context<Self>) {
        let text = text.into();
        let input = match field {
            Field::Url => {
                self.url_len = text.len();
                self.ignore_url = Some(text.clone());
                &self.form.url
            }
            Field::Host => {
                self.host_len = text.len();
                self.ignore_host = Some(text.clone());
                &self.form.host
            }
            Field::Port => &self.form.port,
            Field::User => &self.form.user,
            Field::Password => &self.form.password,
            Field::Database => &self.form.database,
        };
        input.update(cx, |i, cx| i.set_text(text, cx));
    }

    /// Fills the form from a connection URL. A URL the fields can hold whole is cleared
    /// once it is split; one with options, several hosts or SRV lookup is kept, as it
    /// is what the driver will use, and the fields show what it points at. Returns
    /// whether the text was a URL.
    fn apply_url(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        let text = text.trim();
        let Ok((kind, config)) = deltos_drivers::parse_url(text) else {
            self.notice = Some("That is not a connection URL Deltos knows.".into());
            return false;
        };
        let lossless = url_fits_fields(kind, text, &config);
        self.set_kind(kind, cx);
        self.set_field(Field::Host, config.host.clone(), cx);
        self.set_field(
            Field::Port,
            config.port.map(|p| p.to_string()).unwrap_or_default(),
            cx,
        );
        self.set_field(Field::User, config.user.clone(), cx);
        if let Some(password) = config.password.clone() {
            self.set_field(Field::Password, password, cx);
        }
        self.set_field(Field::Database, config.database.clone(), cx);
        self.form.tls = config.tls;
        if lossless {
            self.set_field(Field::Url, String::new(), cx);
            self.notice = Some("Filled in from the URL.".into());
        } else {
            self.set_field(Field::Url, text.to_owned(), cx);
            self.notice = Some(
                "The URL is kept because it has settings the fields can't hold. The fields show where it connects."
                    .into(),
            );
        }
        self.clipboard_url = None;
        self.edited(cx);
        true
    }

    fn set_kind(&mut self, kind: DriverKind, cx: &mut Context<Self>) {
        self.form.kind = kind;
        let (port, user, database) = match kind {
            DriverKind::Postgres => ("5432", "postgres", "postgres"),
            DriverKind::Mysql => ("3306", "root", "optional"),
            DriverKind::Sqlite => ("", "", "/path/to/file.db"),
            DriverKind::Mongodb => ("27017", "optional", "admin"),
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
        self.form
            .name
            .update(cx, |i, cx| i.set_text(c.name.clone(), cx));
        self.set_field(Field::Url, c.config.url.clone().unwrap_or_default(), cx);
        self.set_field(Field::Host, c.config.host.clone(), cx);
        self.set_field(
            Field::Port,
            c.config.port.map(|p| p.to_string()).unwrap_or_default(),
            cx,
        );
        self.set_field(Field::User, c.config.user.clone(), cx);
        self.set_field(Field::Password, String::new(), cx);
        self.form
            .password
            .update(cx, |i, cx| i.set_masked(true, cx));
        self.set_field(Field::Database, c.config.database.clone(), cx);
        self.form.tag = c.tag;
        self.form.tls = c.config.tls;
        self.form.save_password = c.save_password;
        self.set_kind(c.kind, cx);
        // A new connection offers a database URL that is on the clipboard.
        self.clipboard_url = (selected == Some(Selected::New))
            .then(|| cx.read_from_clipboard().and_then(|c| c.text()))
            .flatten()
            .map(|t| t.trim().to_owned())
            .filter(|t| {
                t.len() < 4096
                    && t.contains("://")
                    && deltos_drivers::kind_from_url(t).is_some()
                    && deltos_drivers::parse_url(t).is_ok()
            });
        self.selected = selected;
        self.test = None;
        self.error = None;
        self.notice = None;
        self.menu = None;
        cx.notify();
    }

    /// The name a connection gets when the Name field is left empty.
    fn auto_name(&self, cx: &App) -> String {
        let text = |i: &Entity<TextInput>| i.read(cx).text().trim().to_owned();
        let f = &self.form;
        let (url, host, database) = (text(&f.url), text(&f.host), text(&f.database));
        match f.kind {
            DriverKind::Sqlite if !database.is_empty() => database
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("SQLite")
                .to_owned(),
            _ if url.is_empty() && !host.is_empty() && !database.is_empty() => {
                format!("{database} on {host}")
            }
            _ if url.is_empty() && !host.is_empty() => host,
            kind => kind.label().to_owned(),
        }
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
            name = self.auto_name(cx);
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
        self.clipboard_url = None;
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
        let started = std::time::Instant::now();
        let attempt = open_connection(saved.kind, saved.config, cx);
        cx.spawn(async move |this, cx| {
            let result = attempt.await;
            let took = started.elapsed().as_millis();
            this.update(cx, |this, cx| {
                this.test = Some(match result {
                    Ok(conn) => {
                        let version = conn.server_version();
                        let net = cx.global::<crate::net::Net>();
                        net.spawn(async move { conn.close().await });
                        Test::Passed(version, took)
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

    /// One tile per database this build can open; the selected one is lit.
    fn kind_tiles(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div().flex().flex_wrap().gap(px(6.));
        for kind in deltos_drivers::available() {
            let on = kind == self.form.kind;
            row = row.child(
                div()
                    .id(SharedString::from(format!("kind-{}", kind.label())))
                    .flex_1()
                    .min_w(px(118.))
                    .h(t.s(30.))
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(6.))
                    .rounded(t.radius_small)
                    .border_1()
                    .cursor_pointer()
                    .text_size(t.small_size)
                    .when(on, |el| {
                        el.bg(t.accent.opacity(0.14))
                            .border_color(t.accent)
                            .text_color(t.text)
                            .font_weight(FontWeight::MEDIUM)
                    })
                    .when(!on, |el| {
                        el.bg(t.input)
                            .border_color(t.border_strong)
                            .text_color(t.text_muted)
                            .hover(|s| s.bg(t.hover).text_color(t.text))
                    })
                    .child(icon(Icon::Database).size(px(12.)).text_color(if on {
                        t.accent
                    } else {
                        t.text_faint
                    }))
                    .child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(short_label(kind)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_kind(kind, cx);
                        this.edited(cx);
                    })),
            );
        }
        row.into_any_element()
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
        let kind_tiles = self.kind_tiles(t, cx);
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
        let note = |text: SharedString, color: gpui::Hsla| {
            div()
                .text_size(t.small_size)
                .text_color(color)
                .whitespace_normal()
                .child(text)
                .into_any_element()
        };
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
                    this.test = None;
                    cx.notify();
                })
                .ok();
            },
            t,
        );
        let save_pw = f.save_password;
        let masked = f.password.read(cx).is_masked();
        let kind = f.kind;
        let test_line = self.test.clone().map(|test| {
            let (c, text, hint): (_, SharedString, Option<&'static str>) = match test {
                Test::Running => (t.text_faint, "Testing…".into(), None),
                Test::Passed(v, ms) => (
                    t.success,
                    if v.is_empty() {
                        format!("Connected in {ms} ms").into()
                    } else {
                        format!("Connected to {v} in {ms} ms").into()
                    },
                    None,
                ),
                Test::Failed(e) => {
                    let hint = hint_for(kind, &e);
                    (t.danger, e.into(), hint)
                }
            };
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(note(text, c))
                .children(hint.map(|h| note(h.into(), t.text_muted)))
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
        let clipboard = self.clipboard_url.clone().map(|url| {
            let shown: SharedString = describe_url(&url).into();
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(10.))
                .py(px(8.))
                .rounded(t.radius_small)
                .bg(t.accent.opacity(0.08))
                .border_1()
                .border_color(t.accent.opacity(0.35))
                .child(icon(Icon::Link).size(px(13.)).text_color(t.accent))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(t.small_size)
                                .text_color(t.text_muted)
                                .child("Connection URL on the clipboard"),
                        )
                        .child(
                            div()
                                .text_size(t.ui_size)
                                .text_color(t.text)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(shown),
                        ),
                )
                .child(
                    button("conn-use-clipboard", "Use it", ButtonStyle::Secondary, t).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.apply_url(&url, cx);
                        }),
                    ),
                )
                .child(
                    icon_button("conn-dismiss-clipboard", Icon::Close, t).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.clipboard_url = None;
                            cx.notify();
                        },
                    )),
                )
        });
        div()
            .id("conn-form")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .max_w(px(580.))
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
                    .children(clipboard)
                    .child(row("Type", kind_tiles))
                    .child(row(
                        "Name",
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(div().flex_1().child(field(&f.name, t, window, cx)))
                            .child(div().w(px(130.)).child(tag))
                            .into_any_element(),
                    ))
                    .child(div().h(px(1.)).bg(t.border).my(px(4.)))
                    .child(row("URL", field(&f.url, t, window, cx).into_any_element()))
                    .children(self.notice.clone().map(|n| row("", note(n, t.text_faint))))
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
                                .gap(px(8.))
                                .child(div().flex_1().child(field(&f.password, t, window, cx)))
                                .child(
                                    button(
                                        "conn-show-pw",
                                        if masked { "Show" } else { "Hide" },
                                        ButtonStyle::Ghost,
                                        t,
                                    )
                                    .h(t.s(24.))
                                    .px(px(8.))
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.form.password.update(cx, |i, cx| {
                                                i.set_masked(!i.is_masked(), cx)
                                            });
                                        },
                                    )),
                                )
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
                            note(
                                "Leave the password empty to keep the saved one.".into(),
                                t.text_faint,
                            ),
                        ))
                    })
                    .children(
                        self.error
                            .clone()
                            .map(|e| row("", note(e.into(), t.danger))),
                    )
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
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .text_size(t.small_size)
                            .text_color(t.text_faint)
                            .child("Tab moves between fields. Enter connects."),
                    ),
            )
            .into_any_element()
    }
}

#[derive(Debug, Clone, Copy)]
enum Field {
    Url,
    Host,
    Port,
    User,
    Password,
    Database,
}

/// The name on a database tile.
fn short_label(kind: DriverKind) -> &'static str {
    match kind {
        DriverKind::Mysql => "MySQL/MariaDB",
        kind => kind.label(),
    }
}

/// `db.example.com:5433` as host and port. IPv6 addresses need brackets: `[::1]:5433`.
fn split_host_port(text: &str) -> Option<(String, u16)> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        return Some((host.to_owned(), tail.strip_prefix(':')?.parse().ok()?));
    }
    let (host, port) = text.rsplit_once(':')?;
    if host.is_empty() || host.contains(':') {
        return None;
    }
    Some((host.to_owned(), port.parse().ok()?))
}

/// Whether the form's fields hold everything in `url`, so it can be split and dropped.
fn url_fits_fields(kind: DriverKind, url: &str, config: &ConnectionConfig) -> bool {
    if kind == DriverKind::Sqlite {
        return config.options.is_empty();
    }
    let lower = url.to_ascii_lowercase();
    let authority = url
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?']).next().unwrap_or(""))
        .unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    config.options.is_empty() && !host.contains(',') && !lower.starts_with("mongodb+srv")
}

/// A URL for display, with any password left out.
fn describe_url(url: &str) -> String {
    match deltos_drivers::parse_url(url) {
        Ok((DriverKind::Sqlite, config)) => format!("SQLite · {}", config.database),
        Ok((kind, config)) => {
            let mut out = format!("{} · ", short_label(kind));
            if !config.user.is_empty() {
                out.push_str(&config.user);
                out.push('@');
            }
            out.push_str(&config.host);
            if let Some(port) = config.port {
                out.push_str(&format!(":{port}"));
            }
            if !config.database.is_empty() {
                out.push('/');
                out.push_str(&config.database);
            }
            out
        }
        Err(_) => String::new(),
    }
}

/// What to try next after a failed connection, for the errors people meet most.
fn hint_for(kind: DriverKind, error: &str) -> Option<&'static str> {
    let e = error.to_ascii_lowercase();
    let mysql = kind == DriverKind::Mysql;
    Some(if e.contains("access denied") && mysql {
        "Check the user and password. A server on another machine must also allow this user from your address, for example a user created as 'name'@'%'."
    } else if e.contains("password authentication failed")
        || e.contains("authentication failed")
        || e.contains("access denied")
    {
        "Check the user and password."
    } else if e.contains("refused") {
        if mysql {
            "Nothing answered on that port. Check that the server runs and listens on more than localhost (bind-address in the MariaDB or MySQL config)."
        } else {
            "Nothing answered on that port. Check the port, and that the server runs and accepts connections from other machines."
        }
    } else if e.contains("timed out") || e.contains("did not answer") || e.contains("timeout") {
        "The server did not answer. A firewall is most likely blocking the port, or the host is wrong."
    } else if e.contains("lookup")
        || e.contains("resolve")
        || e.contains("not known")
        || e.contains("no such host")
    {
        "The host name could not be found. Check its spelling."
    } else if e.contains("certificate") || e.contains("tls") || e.contains("ssl") {
        "TLS failed. Try Prefer, or Off for a server without TLS. Verify needs a certificate that matches the host name."
    } else if e.contains("unknown database") || e.contains("does not exist") {
        "That database does not exist on the server. Check its name."
    } else {
        return None;
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_host_and_port() {
        assert_eq!(
            split_host_port("db.local:3307"),
            Some(("db.local".into(), 3307))
        );
        assert_eq!(split_host_port("[::1]:5432"), Some(("::1".into(), 5432)));
        assert_eq!(split_host_port("db.local"), None);
        assert_eq!(split_host_port("::1"), None);
        assert_eq!(split_host_port("db.local:port"), None);
    }

    #[test]
    fn keeps_urls_the_fields_cannot_hold() {
        let fits = |url: &str| {
            let (kind, config) = deltos_drivers::parse_url(url).expect("parses");
            url_fits_fields(kind, url, &config)
        };
        assert!(fits("mysql://fivem:pw@155.1.2.3:3306/HomiesRP"));
        assert!(fits("postgres://u@h/db?sslmode=require"));
        assert!(!fits("postgres://u@h/db?application_name=x"));
        assert!(!fits("mongodb://a:1,b:2/app"));
        assert!(!fits("mongodb+srv://u@cluster0.example.net/app"));
    }

    #[test]
    fn hides_the_password() {
        let shown = describe_url("mysql://fivem:secret@db.local:3306/game");
        assert_eq!(shown, "MySQL/MariaDB · fivem@db.local:3306/game");
        assert!(!shown.contains("secret"));
    }

    #[test]
    fn hints_at_common_failures() {
        assert!(
            hint_for(DriverKind::Mysql, "Access denied for user 'x'@'1.2.3.4'")
                .is_some_and(|h| h.contains("'%'"))
        );
        assert!(hint_for(DriverKind::Postgres, "connection refused (os error 111)").is_some());
        assert!(hint_for(DriverKind::Mysql, "something else").is_none());
    }
}
