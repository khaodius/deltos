//! The password prompt: when a connection without a password is turned away for the
//! lack of one (an imported connection, one that keeps no password, a link), the main
//! window asks for it and tries again.

use deltos_data::DriverKind;
use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, Render,
    SharedString, Subscription, Window, div, prelude::*, px,
};

use crate::state::AppState;
use crate::theme::theme;
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{ButtonStyle, button, field, switch};

/// Whether a failed connection attempt should ask for a password: it had none, and the
/// server or the driver says it needs one. SSH errors are the tunnel's, not the
/// database's.
pub fn needs_password(kind: DriverKind, error: &str, had_password: bool) -> bool {
    if had_password || kind == DriverKind::Sqlite {
        return false;
    }
    let e = error.to_ascii_lowercase();
    if e.starts_with("ssh:") {
        return false;
    }
    [
        "password",
        "authentication failed",
        "auth failed",
        "access denied",
        "authentication error",
        "unauthorized",
        "scram",
    ]
    .iter()
    .any(|p| e.contains(p))
}

/// What the prompt says under the field: the server's answer to a password that was
/// tried, or a plain line when none was.
pub fn explain(error: &str) -> String {
    let e = error.to_ascii_lowercase();
    if e.contains("password missing") || e.contains("using password: no") {
        "This connection needs a password.".into()
    } else {
        error.to_owned()
    }
}

pub struct PasswordPrompt {
    state: Entity<AppState>,
    input: Entity<TextInput>,
    remember: bool,
    /// The request on show.
    serial: u64,
    _subs: Vec<Subscription>,
}

impl PasswordPrompt {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Password", cx).masked());
        let subs = vec![cx.subscribe(&input, |this, _, e, cx| match e {
            InputEvent::Submit => this.submit(cx),
            InputEvent::Cancel => this.cancel(cx),
            InputEvent::Changed => {}
        })];
        Self {
            state,
            input,
            remember: true,
            serial: 0,
            _subs: subs,
        }
    }

    /// Readies the prompt for the request waiting in the state, once per request, and
    /// puts the cursor in the field.
    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self.state.read(cx).password_request.as_ref() else {
            return;
        };
        if request.serial == self.serial {
            return;
        }
        self.serial = request.serial;
        self.remember = true;
        self.input.update(cx, |i, cx| i.clear(cx));
        window.focus(&self.input.read(cx).focus_handle(cx));
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let password = self.input.read(cx).text().to_owned();
        self.input.update(cx, |i, cx| i.clear(cx));
        let remember = self.remember;
        self.state
            .update(cx, |s, cx| s.answer_password(password, remember, cx));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.input.update(cx, |i, cx| i.clear(cx));
        self.state.update(cx, |s, cx| s.cancel_password(cx));
    }
}

impl Focusable for PasswordPrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl Render for PasswordPrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let s = self.state.read(cx);
        let Some(request) = s.password_request.as_ref() else {
            return div().into_any_element();
        };
        let saved = &request.saved;
        let c = &request.config;
        let target: SharedString = {
            let mut text = String::new();
            if !c.user.is_empty() {
                text.push_str(&c.user);
                text.push('@');
            }
            text.push_str(if c.host.is_empty() {
                "localhost"
            } else {
                &c.host
            });
            if !c.database.is_empty() {
                text.push('/');
                text.push_str(&c.database);
            }
            text.into()
        };
        // Only a saved connection has a keychain entry to keep the password in.
        let can_remember = s.settings.connection(&saved.id).is_some();
        let title: SharedString = format!("Password for {}", saved.name).into();
        let error: SharedString = explain(&request.error).into();
        let remember = self.remember;
        div()
            .id("password-prompt")
            .w(px(380.))
            .p(px(18.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .rounded(t.radius)
            .bg(t.floating)
            .border_1()
            .border_color(t.border)
            .shadow_lg()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .text_size(t.s(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.text)
                    .child(title),
            )
            .child(
                div()
                    .text_size(t.small_size)
                    .text_color(t.text_muted)
                    .child(target),
            )
            .child(field(&self.input, &t, window, cx))
            .child(
                div()
                    .text_size(t.small_size)
                    .text_color(t.text_faint)
                    .whitespace_normal()
                    .child(error),
            )
            .when(can_remember, |el| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            switch("password-remember", remember, &t).on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.remember = !this.remember;
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            div()
                                .text_size(t.small_size)
                                .text_color(t.text_muted)
                                .child("Keep it in the keychain"),
                        ),
                )
            })
            .child(
                div()
                    .pt(px(4.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1())
                    .child(
                        button("password-cancel", "Cancel", ButtonStyle::Ghost, &t)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        button("password-connect", "Connect", ButtonStyle::Primary, &t)
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_only_when_a_password_is_missing() {
        let pg = DriverKind::Postgres;
        assert!(needs_password(
            pg,
            "invalid configuration: password missing",
            false
        ));
        assert!(needs_password(
            pg,
            "db error: FATAL: password authentication failed for user \"me\"",
            false
        ));
        assert!(needs_password(
            DriverKind::Mysql,
            "Access denied for user 'root'@'localhost' (using password: NO)",
            false
        ));
        assert!(needs_password(
            DriverKind::Mongodb,
            "Authentication failed.",
            false
        ));
        assert!(needs_password(
            pg,
            "Tunnel OK, database: password authentication failed",
            false
        ));
        assert!(!needs_password(pg, "password authentication failed", true));
        assert!(!needs_password(pg, "connection refused", false));
        assert!(!needs_password(
            pg,
            "SSH: auth failed for me@bastion",
            false
        ));
        assert!(!needs_password(DriverKind::Sqlite, "password", false));
    }

    #[test]
    fn says_why_it_asks() {
        assert_eq!(
            explain("could not connect: invalid configuration: password missing"),
            "This connection needs a password."
        );
        let wrong = "could not connect: password authentication failed for user \"me\"";
        assert_eq!(explain(wrong), wrong);
    }
}
