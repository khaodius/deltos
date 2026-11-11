//! How a connection's environment shows wherever the connection is in use: a line along
//! the top of the window, the tab of each of its tables and queries, the status bars, the
//! connection list and the confirm bar. The tag colors come from
//! [`ConnectionTag::color`]; Local, Development and Testing stay quiet, Staging is
//! clear and Production is loud.

use deltos_settings::ConnectionTag;
use gpui::{AnyElement, App, FontWeight, Hsla, IntoElement, SharedString, div, prelude::*, px};

use crate::safety::Environment;
use crate::theme::{Theme, color};
use crate::ui::icons::{Icon, icon};
use crate::ui::workspace::Workspace;

/// How strongly an environment is marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Emphasis {
    Subtle,
    Strong,
    Loud,
}

pub fn emphasis(tag: ConnectionTag) -> Option<Emphasis> {
    match tag {
        ConnectionTag::None => None,
        ConnectionTag::Local | ConnectionTag::Development | ConnectionTag::Testing => {
            Some(Emphasis::Subtle)
        }
        ConnectionTag::Staging => Some(Emphasis::Strong),
        ConnectionTag::Production => Some(Emphasis::Loud),
    }
}

pub fn tag_color(tag: ConnectionTag) -> Option<Hsla> {
    tag.color().map(color)
}

/// The connection the window is about: the one of the tab in front when it is open,
/// otherwise the active connection.
pub fn shown_connection(workspace: &Workspace, cx: &App) -> Option<String> {
    let state = workspace.state.read(cx);
    workspace
        .active_document()
        .and_then(|p| workspace.view(p))
        .and_then(|v| v.connection(cx))
        .filter(|id| state.session(id).is_some())
        .or_else(|| state.active.clone())
}

/// A line across the window under the title bar, in the environment's color.
pub fn strip(env: Option<&Environment>) -> Option<AnyElement> {
    let env = env?;
    let c = tag_color(env.tag)?;
    let (height, alpha) = match emphasis(env.tag)? {
        Emphasis::Subtle => (2., 0.55),
        Emphasis::Strong => (3., 0.9),
        Emphasis::Loud => (3., 1.0),
    };
    Some(
        div()
            .flex_none()
            .w_full()
            .h(px(height))
            .bg(c.opacity(alpha))
            .into_any_element(),
    )
}

/// The environment's name as a small label: plain for the quiet ones, a colored chip
/// for staging and a bold capitalized one for production.
pub fn badge(tag: ConnectionTag, t: &Theme) -> Option<AnyElement> {
    let c = tag_color(tag)?;
    let label: SharedString = tag.label().into();
    let chip = || {
        div()
            .flex_none()
            .h(t.s(16.))
            .px(px(6.))
            .flex()
            .items_center()
            .rounded(t.radius_small)
            .text_size(t.s(10.5))
            .text_color(c)
    };
    Some(match emphasis(tag)? {
        Emphasis::Subtle => div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(5.))
            .text_size(t.small_size)
            .text_color(t.text_muted)
            .child(crate::ui::dot(c, 6.))
            .child(label)
            .into_any_element(),
        Emphasis::Strong => chip()
            .bg(c.opacity(0.14))
            .font_weight(FontWeight::MEDIUM)
            .child(label)
            .into_any_element(),
        Emphasis::Loud => chip()
            .bg(c.opacity(0.2))
            .border_1()
            .border_color(c.opacity(0.6))
            .font_weight(FontWeight::BOLD)
            .child(label.to_uppercase())
            .into_any_element(),
    })
}

/// A lock and "Read-only", for a read-only connection.
pub fn lock(env: &Environment, t: &Theme) -> Option<AnyElement> {
    env.read_only.then(|| {
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.))
            .text_size(t.small_size)
            .text_color(t.text_muted)
            .child(icon(Icon::Lock).size(px(11.)))
            .child("Read-only")
            .into_any_element()
    })
}

/// The environment and the lock, side by side, for a status bar.
pub fn status(env: Option<&Environment>, t: &Theme) -> Option<AnyElement> {
    let env = env?;
    let badge = badge(env.tag, t);
    let lock = lock(env, t);
    if badge.is_none() && lock.is_none() {
        return None;
    }
    Some(
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .children(badge)
            .children(lock)
            .into_any_element(),
    )
}

/// How a tab of a table or query shows its connection's environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct TabMark {
    /// The icon's color.
    pub color: Option<Hsla>,
    /// A line along the top of the tab, for staging and production.
    pub line: Option<Hsla>,
    pub read_only: bool,
}

pub fn tab_mark(env: Option<&Environment>) -> TabMark {
    let Some(env) = env else {
        return TabMark::default();
    };
    let c = tag_color(env.tag);
    TabMark {
        color: c,
        line: c.filter(|_| emphasis(env.tag) >= Some(Emphasis::Strong)),
        read_only: env.read_only,
    }
}

/// "Production · Main DB" in the environment's color, for the confirm bar. A connection
/// without a tag shows its name alone.
pub fn named(env: &Environment, t: &Theme) -> AnyElement {
    let c = tag_color(env.tag);
    let text: SharedString = match env.tag {
        ConnectionTag::None => env.name.clone().into(),
        tag => format!("{} · {}", tag.label(), env.name).into(),
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(6.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(c.unwrap_or(t.text))
        .children(c.map(|c| crate::ui::dot(c, 7.)))
        .child(text)
        .into_any_element()
}
