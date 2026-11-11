//! The Tasks panel, which lists long-running work with a Cancel button for what still
//! runs, and the title bar's count of running tasks, which opens it.

use std::time::Instant;

use deltos_settings::PanelKind;
use gpui::{
    App, Context, Entity, FontWeight, IntoElement, Render, SharedString, Subscription, Window, div,
    prelude::*, px,
};

use crate::tasks::{self, TaskEntry, TaskList, TaskOutcome, elapsed_text, running_text};
use crate::theme::{Theme, theme};
use crate::ui::workspace::Workspace;
use crate::ui::{ButtonStyle, Tooltip, button, caption, dot, empty_state, thousands};

pub struct TasksPanel {
    list: Option<Entity<TaskList>>,
    _subs: Vec<Subscription>,
}

impl TasksPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let list = tasks::list(cx);
        let subs = list
            .iter()
            .map(|l| cx.observe(l, |_, _, cx| cx.notify()))
            .collect();
        Self { list, _subs: subs }
    }
}

/// The line under a task's label: where it runs, how many rows, how long, how it ended.
fn detail(e: &TaskEntry, now: Instant) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(c) = &e.connection {
        parts.push(c.to_string());
    }
    if let Some(n) = e.rows {
        parts.push(format!(
            "{} {}",
            thousands(n),
            if n == 1 { "row" } else { "rows" }
        ));
    }
    let elapsed = e.elapsed(now);
    parts.push(if e.is_running() {
        running_text(elapsed)
    } else {
        elapsed_text(elapsed)
    });
    match &e.outcome {
        None => {}
        Some(TaskOutcome::Done) => parts.push("done".into()),
        Some(TaskOutcome::Cancelled) => parts.push("cancelled".into()),
        Some(TaskOutcome::Failed(m)) => parts.push(m.lines().next().unwrap_or("").to_owned()),
    }
    parts.join(" · ")
}

fn row(e: &TaskEntry, now: Instant, t: &Theme) -> gpui::Stateful<gpui::Div> {
    let color = match &e.outcome {
        None => t.accent,
        Some(TaskOutcome::Done) => t.success,
        Some(TaskOutcome::Cancelled) => t.text_faint,
        Some(TaskOutcome::Failed(_)) => t.danger,
    };
    let failed = matches!(e.outcome, Some(TaskOutcome::Failed(_)));
    let id = e.id;
    div()
        .id(SharedString::from(format!("task-{id:?}")))
        .min_h(t.s(40.))
        .px(px(10.))
        .py(px(6.))
        .flex()
        .items_center()
        .gap(px(10.))
        .border_b_1()
        .border_color(t.grid_line)
        .child(dot(color, 7.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(t.ui_size)
                        .text_color(t.text)
                        .child(e.label.clone()),
                )
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(t.small_size)
                        .text_color(if failed { t.danger } else { t.text_faint })
                        .child(detail(e, now)),
                ),
        )
        .when(e.can_cancel(), |el| {
            el.child(
                button(
                    SharedString::from(format!("cancel-{id:?}")),
                    "Cancel",
                    ButtonStyle::Secondary,
                    t,
                )
                .h(t.s(22.))
                .on_click(move |_, _, cx| tasks::cancel(id, cx)),
            )
        })
}

impl Render for TasksPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let now = Instant::now();
        let base = div().size_full().bg(t.sidebar).flex().flex_col();
        let Some(list) = &self.list else {
            return base.into_any_element();
        };
        let entries = list.read(cx).registry.entries();
        let running: Vec<&TaskEntry> = entries
            .iter()
            .rev()
            .filter(|e| e.is_running() && e.is_visible(now))
            .collect();
        let finished: Vec<&TaskEntry> = entries.iter().rev().filter(|e| !e.is_running()).collect();
        if running.is_empty() && finished.is_empty() {
            return base
                .child(empty_state(
                    "Nothing running",
                    Some("Queries, table loads and counts that take a while show here.".into()),
                    &t,
                ))
                .into_any_element();
        }
        let section = |title: String| {
            div()
                .px(px(10.))
                .pt(px(10.))
                .pb(px(4.))
                .flex()
                .items_center()
                .child(caption(title, &t))
        };
        let mut body = div()
            .id("tasks-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col();
        if !running.is_empty() {
            body = body.child(section(format!("Running · {}", running.len())));
            for e in &running {
                body = body.child(row(e, now, &t));
            }
        }
        if !finished.is_empty() {
            body = body.child(
                section("Finished".into()).child(div().flex_1()).child(
                    button("tasks-clear", "Clear", ButtonStyle::Ghost, &t)
                        .h(t.s(20.))
                        .on_click(|_, _, cx| tasks::clear_finished(cx)),
                ),
            );
            for e in &finished {
                body = body.child(row(e, now, &t));
            }
        }
        base.child(body).into_any_element()
    }
}

/// "2 running" in the title bar while tasks run; a click opens the Tasks panel.
pub struct TasksIndicator {
    workspace: Entity<Workspace>,
    /// Running tasks that are listed. The indicator sits in the title bar, so it redraws
    /// only when this changes, not with every tick of the clocks.
    count: usize,
    _subs: Vec<Subscription>,
}

impl TasksIndicator {
    pub fn new(workspace: Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        let subs = tasks::list(cx)
            .iter()
            .map(|l| {
                cx.observe(l, |this: &mut Self, list, cx| {
                    let count = list.read(cx).registry.running_count(Instant::now());
                    if count != this.count {
                        this.count = count;
                        cx.notify();
                    }
                })
            })
            .collect();
        Self {
            workspace,
            count: 0,
            _subs: subs,
        }
    }
}

/// Shows the Tasks panel, opening it where it was or under the documents.
pub fn show_tasks(workspace: &Entity<Workspace>, cx: &mut App) {
    workspace.update(cx, |w, cx| w.open(PanelKind::Tasks, true, cx));
}

impl Render for TasksIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let count = self.count;
        if count == 0 {
            return div().into_any_element();
        }
        let ws = self.workspace.clone();
        div()
            .id("tasks-indicator")
            .h(t.s(20.))
            .px(px(8.))
            .mr(px(4.))
            .flex()
            .items_center()
            .gap(px(6.))
            .rounded_full()
            .bg(t.accent_soft)
            .text_color(t.accent)
            .text_size(t.small_size)
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .hover(|s| s.bg(t.hover))
            .tooltip(|_, cx| Tooltip::view("Show the running tasks", cx))
            .child(dot(t.accent, 6.))
            .child(format!("{count} running"))
            .on_click(move |_, _, cx| show_tasks(&ws, cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::Registry;
    use std::time::Duration;

    #[test]
    fn details_say_where_how_many_and_how_it_ended() {
        let mut r = Registry::default();
        let t0 = Instant::now();
        let id = r.start("SELECT 1".into(), Some("Shop".into()), None, t0);
        r.progress(id, 1200);
        assert_eq!(
            detail(r.get(id).unwrap(), t0 + Duration::from_secs(2)),
            "Shop · 1,200 rows · 2 s"
        );
        r.finish(
            id,
            TaskOutcome::Failed("syntax error\nat line 1".into()),
            t0 + Duration::from_secs(3),
        );
        assert_eq!(
            detail(r.get(id).unwrap(), t0 + Duration::from_secs(9)),
            "Shop · 1,200 rows · 3.0 s · syntax error"
        );
    }
}
