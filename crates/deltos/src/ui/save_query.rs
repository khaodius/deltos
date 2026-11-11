//! The bar a query panel shows to save its text by name, for its connection or for every
//! connection.

use gpui::{
    Context, Entity, EventEmitter, Focusable, IntoElement, Render, Subscription, Window, div,
    prelude::*, px,
};

use crate::snippets;
use crate::theme::theme;
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{ButtonStyle, button, field, segmented};

pub enum SaveEvent {
    /// Saved or given up: the panel closes the bar.
    Closed,
}

pub struct SaveBar {
    name: Entity<TextInput>,
    text: String,
    /// The panel's connection, and what to call it.
    connection: Option<(String, String)>,
    /// Saved for every connection rather than this one.
    shared: bool,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<SaveEvent> for SaveBar {}

impl SaveBar {
    /// A bar to save `text`. `connection` is the panel's connection id and name.
    pub fn new(
        text: String,
        connection: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            let mut input = TextInput::new("Name", cx);
            if let Some(suggested) = suggested_name(&text) {
                input.set_text(suggested, cx);
                input.select_everything(cx);
            }
            input
        });
        window.focus(&name.focus_handle(cx));
        let subs = vec![cx.subscribe(&name, |this, _, e, cx| match e {
            InputEvent::Changed => {
                this.error = None;
                cx.notify();
            }
            InputEvent::Submit => this.save(cx),
            InputEvent::Cancel => cx.emit(SaveEvent::Closed),
        })];
        Self {
            name,
            text,
            shared: connection.is_none(),
            connection,
            error: None,
            _subs: subs,
        }
    }

    fn scope(&self) -> Option<String> {
        if self.shared {
            None
        } else {
            self.connection.as_ref().map(|(id, _)| id.clone())
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim().to_owned();
        if name.is_empty() {
            self.error = Some("Give it a name.".into());
            cx.notify();
            return;
        }
        let Some(store) = snippets::store(cx) else {
            self.error = Some("Saved queries are not available.".into());
            cx.notify();
            return;
        };
        let (text, scope) = (self.text.clone(), self.scope());
        store.update(cx, |s, cx| s.save(&name, &text, scope, cx));
        cx.emit(SaveEvent::Closed);
    }
}

/// A name from the first comment line, as in `-- Orders per day`.
fn suggested_name(text: &str) -> Option<String> {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let note = first
        .strip_prefix("--")
        .or_else(|| first.strip_prefix("//"))?
        .trim();
    let note: String = note.chars().take(80).collect();
    (!note.is_empty()).then_some(note)
}

impl Render for SaveBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let name = self.name.read(cx).text().trim().to_owned();
        let scope = self.scope();
        let replaces = snippets::store(cx).is_some_and(|s| {
            !name.is_empty() && s.read(cx).clash(&name, scope.as_deref()).is_some()
        });
        let weak = cx.entity().downgrade();
        let note = match &self.error {
            Some(e) => Some((e.clone(), t.danger)),
            None if replaces => Some(("Replaces the saved query of that name".into(), t.warning)),
            None => None,
        };
        div()
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
            .child(div().flex_none().text_color(t.text_muted).child("Save as"))
            .child(field(&self.name, &t, window, cx).w(px(240.)).flex_shrink())
            .when_some(self.connection.clone(), |el, (_, conn_name)| {
                let w = weak.clone();
                el.child(segmented(
                    "save-scope",
                    &[("This connection", false), ("All connections", true)],
                    self.shared,
                    move |shared, _, cx| {
                        w.update(cx, |this, cx| {
                            this.shared = shared;
                            cx.notify();
                        })
                        .ok();
                    },
                    &t,
                ))
                .when(!self.shared, |el| {
                    el.child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(t.text_faint)
                            .child(conn_name),
                    )
                })
            })
            .children(note.map(|(text, color)| {
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(color)
                    .child(text)
            }))
            .child(div().flex_1())
            .child(
                button("save-cancel", "Cancel", ButtonStyle::Ghost, &t)
                    .h(t.s(24.))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SaveEvent::Closed))),
            )
            .child(
                button(
                    "save-confirm",
                    if replaces { "Replace" } else { "Save" },
                    ButtonStyle::Primary,
                    &t,
                )
                .h(t.s(24.))
                .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_a_first_comment() {
        assert_eq!(
            suggested_name("\n  -- Orders per day \nselect 1").as_deref(),
            Some("Orders per day")
        );
        assert_eq!(
            suggested_name("// recent\ndb.a.find()").as_deref(),
            Some("recent")
        );
        assert_eq!(suggested_name("select 1 -- not this"), None);
        assert_eq!(suggested_name("--\nselect 1"), None);
        assert_eq!(suggested_name(""), None);
    }
}
