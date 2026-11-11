//! A query plan as an indented tree: each node with what it does, its conditions, and
//! bars for its own share of the cost and the time, with the most expensive node marked.

use gpui::{AnyElement, App, FontWeight, IntoElement, SharedString, Window, div, prelude::*, px};

use crate::lang::plan::{Plan, PlanNode, PlanSource};
use crate::theme::Theme;
use crate::ui::{Tooltip, caption, switch, thousands};

/// Lines drawn at most; a plan larger than this is cut with a note.
const MAX_LINES: usize = 2_000;

/// The header above a plan: the totals and, where the plan can be measured, the switch
/// that runs the statement to measure it.
pub fn header(
    plan: Option<&Plan>,
    source: PlanSource,
    analyze: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    t: &Theme,
) -> AnyElement {
    let mut facts: Vec<String> = Vec::new();
    if let Some(plan) = plan {
        if let Some(ms) = plan.planning_ms {
            facts.push(format!("Planning {}", millis(ms)));
        }
        if let Some(ms) = plan.execution_ms {
            facts.push(format!("Execution {}", millis(ms)));
        }
        if let Some(cost) = plan.total_cost {
            facts.push(format!("Cost {}", number(cost)));
        }
    }
    div()
        .flex_none()
        .h(t.bar_height)
        .px(px(10.))
        .flex()
        .items_center()
        .gap(px(12.))
        .bg(t.base)
        .border_b_1()
        .border_color(t.border)
        .text_size(t.small_size)
        .child(caption("Plan", t))
        .child(div().text_color(t.text_muted).child(facts.join("  ·  ")))
        .child(div().flex_1())
        .when(source.can_analyze(), |el| {
            let label = match source {
                PlanSource::Mongo => "Execution stats",
                _ => "Analyze",
            };
            el.when(analyze, |el| {
                el.child(
                    div()
                        .text_color(t.warning)
                        .child("Runs the statement to measure it"),
                )
            })
            .child(div().text_color(t.text_muted).child(label))
            .child(
                switch("plan-analyze", analyze, t)
                    .tooltip(|_, cx| {
                        Tooltip::view("Run the statement and show real row counts and times", cx)
                    })
                    .on_click(move |_, window, cx| on_toggle(window, cx)),
            )
        })
        .into_any_element()
}

/// The tree itself.
pub fn tree(plan: &Plan, t: &Theme) -> AnyElement {
    let lines = plan.lines();
    if lines.is_empty() {
        return crate::ui::empty_state("The plan is empty", None, t).into_any_element();
    }
    let hottest = plan.hottest();
    let max_self_cost = plan.max_of(|n| n.self_cost);
    let max_self_time = plan.max_of(|n| n.self_time_ms);
    let show_cost = lines.iter().any(|l| l.node.cost.is_some());
    let show_rows = lines
        .iter()
        .any(|l| l.node.estimated_rows.is_some() || l.node.actual_rows.is_some());
    let show_time = lines.iter().any(|l| l.node.time_ms.is_some());
    let mut list = div()
        .id("plan-tree")
        .size_full()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .w_full()
                .px(px(10.))
                .py(px(4.))
                .gap(px(10.))
                .border_b_1()
                .border_color(t.border)
                .text_size(t.small_size)
                .text_color(t.text_faint)
                .child(div().flex_1().child("Step"))
                .when(show_cost, |el| el.child(metric_head("Cost")))
                .when(show_rows, |el| el.child(metric_head("Rows")))
                .when(show_time, |el| el.child(metric_head("Time"))),
        );
    for (ix, line) in lines.iter().enumerate().take(MAX_LINES) {
        let hot = hottest == Some(ix);
        list = list.child(row(
            line.node,
            line.depth,
            hot,
            (show_cost, show_rows, show_time),
            (max_self_cost, max_self_time),
            t,
        ));
    }
    if lines.len() > MAX_LINES {
        list = list.child(
            div()
                .p(px(10.))
                .text_size(t.small_size)
                .text_color(t.text_faint)
                .child(format!(
                    "{} more steps are not shown.",
                    thousands((lines.len() - MAX_LINES) as u64)
                )),
        );
    }
    list.into_any_element()
}

fn metric_head(label: &'static str) -> impl IntoElement {
    div()
        .w(metric_width(label == "Rows"))
        .flex_none()
        .text_right()
        .child(label)
}

/// Rows hold two counts side by side once the plan is measured.
fn metric_width(rows: bool) -> gpui::Pixels {
    px(if rows { 150. } else { 112. })
}

fn row(
    node: &PlanNode,
    depth: usize,
    hot: bool,
    (show_cost, show_rows, show_time): (bool, bool, bool),
    (max_cost, max_time): (f64, f64),
    t: &Theme,
) -> impl IntoElement {
    let title_color = if hot { t.warning } else { t.text };
    div()
        .flex()
        .w_full()
        .items_start()
        .px(px(10.))
        .py(px(5.))
        .gap(px(10.))
        .border_b_1()
        .border_color(t.grid_line)
        .when(hot, |el| el.bg(t.warning.opacity(0.07)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_start()
                .child(div().flex_none().w(px(depth as f32 * 18.)))
                .child(
                    div()
                        .flex_none()
                        .w(px(14.))
                        .text_size(t.small_size)
                        .text_color(t.text_faint)
                        .child(if depth > 0 { "└" } else { "" }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_size(t.ui_size)
                                .font_weight(if hot {
                                    FontWeight::SEMIBOLD
                                } else {
                                    FontWeight::MEDIUM
                                })
                                .text_color(title_color)
                                .child(SharedString::from(node.title.clone()))
                                .when(hot, |el| {
                                    el.child(
                                        div()
                                            .px(px(5.))
                                            .rounded(t.radius_small)
                                            .bg(t.warning.opacity(0.15))
                                            .text_size(t.small_size)
                                            .text_color(t.warning)
                                            .child("most expensive"),
                                    )
                                })
                                .when(node.warning && !hot, |el| {
                                    el.child(
                                        div()
                                            .text_size(t.small_size)
                                            .text_color(t.text_faint)
                                            .child("·  worth a look"),
                                    )
                                }),
                        )
                        .children(node.details.iter().map(|d| {
                            div()
                                .w_full()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .font_family(t.mono_font.clone())
                                .text_size(t.small_size)
                                .text_color(t.text_faint)
                                .child(SharedString::from(d.clone()))
                        })),
                ),
        )
        .when(show_cost, |el| {
            el.child(metric(
                node.cost.map(number),
                node.self_cost.map(|c| fraction(c, max_cost)),
                hot,
                t,
            ))
        })
        .when(show_rows, |el| {
            let text = match (node.estimated_rows, node.actual_rows) {
                (Some(e), Some(a)) => Some(format!("{} → {}", count(e), count(a))),
                (Some(e), None) => Some(count(e)),
                (None, Some(a)) => Some(count(a)),
                (None, None) => None,
            };
            el.child(metric(text, None, hot, t).w(metric_width(true)))
        })
        .when(show_time, |el| {
            el.child(metric(
                node.time_ms.map(millis),
                node.self_time_ms.map(|v| fraction(v, max_time)),
                hot,
                t,
            ))
        })
}

/// A number with a bar under it for its share of the largest.
fn metric(text: Option<String>, share: Option<f32>, hot: bool, t: &Theme) -> gpui::Div {
    div()
        .w(metric_width(false))
        .flex_none()
        .whitespace_nowrap()
        .flex()
        .flex_col()
        .items_end()
        .gap(px(3.))
        .text_size(t.small_size)
        .text_color(t.text_muted)
        .child(text.unwrap_or_default())
        .children(share.map(|share| {
            div()
                .w(px(80.))
                .h(px(3.))
                .rounded_full()
                .bg(t.border)
                .child(div().h_full().w(px(80. * share)).rounded_full().bg(if hot {
                    t.warning
                } else {
                    t.accent
                }))
        }))
}

fn fraction(value: f64, max: f64) -> f32 {
    if max > 0.0 {
        (value / max).clamp(0.0, 1.0) as f32
    } else {
        0.0
    }
}

fn millis(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else if ms >= 10.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{ms:.2} ms")
    }
}

fn number(v: f64) -> String {
    if v >= 1000.0 {
        thousands(v.round() as u64)
    } else {
        format!("{v:.2}")
    }
}

fn count(v: f64) -> String {
    thousands(v.max(0.0).round() as u64)
}
