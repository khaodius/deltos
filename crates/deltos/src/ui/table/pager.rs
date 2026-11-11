//! The row range and the page control in a table's status bar.
//!
//! The grid scrolls the whole table, reading pages by key as they come into view, so a
//! "page" here is a place to jump to, not a separate read: previous, next, a page number
//! or a row number scrolls the grid there, and only what is then on screen is loaded. The
//! page size is the "Rows per page" setting, which is also how many rows each read asks
//! for. The total is the database's estimate until it is known; on demand it is counted
//! exactly in the background, and the count can be stopped.

use deltos_grid::RowCount;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, Focusable, SharedString, Subscription, Task,
    Window, div, prelude::*, px,
};

use super::TablePanel;
use crate::net::Net;
use crate::theme::Theme;
use crate::ui::icons::Icon;
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{Tooltip, field, icon_button, segmented, thousands};

/// The page sizes the control offers; the same steps as the setting.
const PAGE_SIZES: [(&str, u32); 7] = [
    ("50", 50),
    ("100", 100),
    ("250", 250),
    ("500", 500),
    ("1k", 1000),
    ("2k", 2000),
    ("5k", 5000),
];

/// "rows 1,201 to 1,260 of about 50,000,000", or what of it is known.
pub fn range_text(first: u64, last: u64, count: RowCount) -> String {
    let total = match count {
        RowCount::Exact(0) => return "No rows".to_owned(),
        RowCount::Unknown => None,
        RowCount::Estimated(n) => Some(format!("about {}", thousands(n))),
        RowCount::AtLeast(n) => Some(format!("{}+", thousands(n))),
        RowCount::Exact(n) => Some(thousands(n)),
    };
    let shown = if last <= first {
        String::new()
    } else if last == first + 1 {
        format!("row {}", thousands(first + 1))
    } else {
        format!("rows {} to {}", thousands(first + 1), thousands(last))
    };
    match (shown.is_empty(), total) {
        (true, Some(total)) => format!("{total} rows"),
        (true, None) => "– rows".to_owned(),
        (false, Some(total)) => format!("{shown} of {total}"),
        (false, None) => shown,
    }
}

/// The page that `row` is on, from 0, and how many pages `count` rows make.
pub fn page_of(row: u64, page_size: u32) -> u64 {
    row / u64::from(page_size.max(1))
}

pub fn pages(count: u64, page_size: u32) -> u64 {
    count.div_ceil(u64::from(page_size.max(1))).max(1)
}

/// A typed page or row number, from 1, read leniently: spaces and thousands separators
/// are ignored. `None` for anything else.
pub fn parse_number(text: &str) -> Option<u64> {
    let digits: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',' && *c != '_')
        .collect();
    digits.parse::<u64>().ok().filter(|n| *n > 0)
}

/// The status bar's page control state.
pub struct Pager {
    pub open: bool,
    pub page: Entity<TextInput>,
    pub row: Entity<TextInput>,
    /// An exact count running on request; dropping it stops waiting.
    pub counting: Option<Task<()>>,
    _subs: [Subscription; 2],
}

impl Pager {
    pub fn new(cx: &mut Context<TablePanel>) -> Self {
        let page = cx.new(|cx| TextInput::new("Page", cx));
        let row = cx.new(|cx| TextInput::new("Row", cx));
        let subs = [
            cx.subscribe(&page, |this, input, e, cx| match e {
                InputEvent::Submit => {
                    if let Some(n) = parse_number(input.read(cx).text()) {
                        let size = this.page_size(cx);
                        this.go_to_row((n - 1).saturating_mul(u64::from(size)), false, cx);
                    }
                }
                InputEvent::Cancel => this.close_pager(cx),
                InputEvent::Changed => {}
            }),
            cx.subscribe(&row, |this, input, e, cx| match e {
                InputEvent::Submit => {
                    if let Some(n) = parse_number(input.read(cx).text()) {
                        this.go_to_row(n - 1, true, cx);
                    }
                }
                InputEvent::Cancel => this.close_pager(cx),
                InputEvent::Changed => {}
            }),
        ];
        Self {
            open: false,
            page,
            row,
            counting: None,
            _subs: subs,
        }
    }
}

impl TablePanel {
    fn page_size(&self, cx: &App) -> u32 {
        self.state.read(cx).settings.data.page_size.max(1)
    }

    fn close_pager(&mut self, cx: &mut Context<Self>) {
        self.pager.open = false;
        cx.notify();
    }

    /// The table rows on screen, from 0: staged new rows shown above the table's are not
    /// counted, so page and row numbers stay the table's own.
    fn table_rows_in_view(&self, cx: &App) -> std::ops::Range<u64> {
        let grid = self.grid.read(cx);
        let inserted = grid.inserted();
        let view = grid.rows_in_view(cx);
        view.start.saturating_sub(inserted)..view.end.saturating_sub(inserted)
    }

    /// Scrolls the grid so table row `row` is at the top, selecting it when asked.
    fn go_to_row(&mut self, row: u64, select: bool, cx: &mut Context<Self>) {
        let total = self.grid.read(cx).row_count();
        let row = match total {
            RowCount::Exact(n) => row.min(n.saturating_sub(1)),
            _ => row,
        };
        // Grid rows start with the staged new rows.
        let row = row.saturating_add(self.grid.read(cx).inserted());
        self.grid.update(cx, |g, cx| {
            g.scroll_to_row(row, cx);
            if select && !g.columns().is_empty() {
                let col = g.selection.map_or(0, |s| s.head.1);
                g.selection = Some(crate::ui::grid::Selection {
                    anchor: (row, col),
                    head: (row, col),
                });
            }
        });
        if select {
            self.inspect_row(row, cx);
        }
        cx.notify();
    }

    fn step_page(&mut self, step: i64, cx: &mut Context<Self>) {
        let size = self.page_size(cx);
        let first = self.table_rows_in_view(cx).start;
        let page = page_of(first, size) as i64 + step;
        let page = page.max(0) as u64;
        self.go_to_row(page.saturating_mul(u64::from(size)), false, cx);
    }

    /// Counts the filtered rows exactly in the background.
    fn count_exactly(&mut self, cx: &mut Context<Self>) {
        let Some(loader) = self.loader.clone() else {
            return;
        };
        let job = cx.global::<Net>().run(loader.count_exact());
        self.pager.counting = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.pager.counting = None;
                match result {
                    Some(Ok(_)) | Some(Err(deltos_data::Error::Cancelled)) | None => {}
                    Some(Err(e)) => this.notify_user(format!("Could not count: {e}"), true, cx),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn stop_count(&mut self, cx: &mut Context<Self>) {
        if let Some(loader) = &self.loader {
            loader.stop_counting();
        }
        self.pager.counting = None;
        cx.notify();
    }

    /// The row range, the count button and the page control, for the status bar.
    pub(super) fn pager_bar(
        &mut self,
        t: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = self.grid.read(cx).row_count();
        let view = self.table_rows_in_view(cx);
        let size = self.page_size(cx);
        let range: SharedString = range_text(view.start, view.end, count).into();
        let counting = self.pager.counting.is_some();
        let can_count = self.loader.is_some() && !count.is_exact() && count != RowCount::Unknown;
        let total = count.value().unwrap_or(0);
        let page = page_of(view.start, size) + 1;
        let last_page = pages(total, size);
        let page_label: SharedString = match count {
            RowCount::Exact(_) => format!("Page {} of {}", thousands(page), thousands(last_page)),
            RowCount::Estimated(_) => {
                format!("Page {} of about {}", thousands(page), thousands(last_page))
            }
            _ => format!("Page {}", thousands(page)),
        }
        .into();
        let at_end = count.is_exact() && page >= last_page;

        // In a narrow panel the range gives way first, cut short, so the page control and
        // the reload button stay in the status bar.
        let mut out = div().min_w_0().flex().items_center().gap(px(6.)).child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(t.text)
                .child(range),
        );
        if counting {
            out = out
                .child(div().text_color(t.text_faint).child("Counting…"))
                .child(
                    icon_button("count-stop", Icon::Close, t)
                        .tooltip(|_, cx| Tooltip::view("Stop counting", cx))
                        .on_click(cx.listener(|this, _, _, cx| this.stop_count(cx))),
                );
        } else if can_count {
            out = out.child(
                div()
                    .id("count-exact")
                    .px(px(4.))
                    .rounded(t.radius_small)
                    .text_color(t.accent)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.hover))
                    .child("Count")
                    .tooltip(|_, cx| Tooltip::view("Count the rows exactly", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.count_exactly(cx))),
            );
        }

        let mut label = div()
            .id("page-label")
            .relative()
            .px(px(6.))
            .h(t.s(20.))
            .flex()
            .items_center()
            .rounded(t.radius_small)
            .cursor_pointer()
            .hover(|s| s.bg(t.hover))
            .child(page_label)
            .on_click(cx.listener(|this, _, window, cx| {
                this.pager.open = !this.pager.open;
                if this.pager.open {
                    let page = this.pager.page.clone();
                    page.update(cx, |i, cx| {
                        i.clear(cx);
                    });
                    this.pager.row.update(cx, |i, cx| i.clear(cx));
                    window.focus(&page.focus_handle(cx));
                }
                cx.notify();
            }));
        if self.pager.open {
            label = label.child(self.pager_popover(t, size, window, cx));
        }
        out.child(div().w(px(1.)).h(px(12.)).bg(t.border))
            .child(
                icon_button("page-previous", Icon::ChevronLeft, t)
                    .when(page <= 1, |el| el.opacity(0.4))
                    .tooltip(|_, cx| Tooltip::view("Previous page", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.step_page(-1, cx))),
            )
            .child(label)
            .child(
                icon_button("page-next", Icon::ChevronRight, t)
                    .when(at_end, |el| el.opacity(0.4))
                    .tooltip(|_, cx| Tooltip::view("Next page", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.step_page(1, cx))),
            )
            .into_any_element()
    }

    fn pager_popover(
        &self,
        t: &Theme,
        size: u32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.entity().downgrade();
        let close = weak.clone();
        let line = |label: &'static str, input: &Entity<TextInput>| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .w(px(84.))
                        .flex_none()
                        .text_color(t.text_muted)
                        .child(label),
                )
                .child(div().w(px(120.)).child(field(input, t, window, cx)))
        };
        let panel = div()
            .id("pager-popover")
            .occlude()
            .w(px(300.))
            .p(px(10.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .rounded(t.radius)
            .bg(t.floating)
            .border_1()
            .border_color(t.border_strong)
            .shadow_lg()
            .text_size(t.ui_size)
            .text_color(t.text)
            .on_mouse_down_out(move |_, _, cx| {
                close.update(cx, |this, cx| this.close_pager(cx)).ok();
            })
            .child(line("Go to page", &self.pager.page))
            .child(line("Go to row", &self.pager.row))
            .child(div().h(px(1.)).bg(t.border))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(div().text_color(t.text_muted).child("Rows per page"))
                    .child(segmented(
                        "page-size",
                        &PAGE_SIZES,
                        size,
                        move |n, _, cx| {
                            weak.update(cx, |this, cx| {
                                this.state.update(cx, |s, cx| {
                                    s.update_settings(cx, |s| s.data.page_size = n)
                                });
                                cx.notify();
                            })
                            .ok();
                        },
                        t,
                    )),
            );
        gpui::deferred(
            gpui::anchored()
                .anchor(gpui::Corner::BottomLeft)
                .snap_to_window_with_margin(px(8.))
                .child(div().mb(px(4.)).child(panel)),
        )
        .with_priority(1)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_read_naturally() {
        assert_eq!(
            range_text(1200, 1260, RowCount::Exact(50_000_000)),
            "rows 1,201 to 1,260 of 50,000,000"
        );
        assert_eq!(
            range_text(0, 40, RowCount::Estimated(50_000_000)),
            "rows 1 to 40 of about 50,000,000"
        );
        assert_eq!(range_text(0, 1, RowCount::Exact(1)), "row 1 of 1");
        assert_eq!(
            range_text(0, 40, RowCount::AtLeast(500)),
            "rows 1 to 40 of 500+"
        );
        assert_eq!(range_text(0, 0, RowCount::Unknown), "– rows");
        assert_eq!(range_text(0, 0, RowCount::Exact(0)), "No rows");
        assert_eq!(range_text(0, 0, RowCount::Estimated(10)), "about 10 rows");
    }

    #[test]
    fn pages_and_numbers() {
        assert_eq!(page_of(0, 500), 0);
        assert_eq!(page_of(1499, 500), 2);
        assert_eq!(pages(0, 500), 1);
        assert_eq!(pages(1000, 500), 2);
        assert_eq!(pages(1001, 500), 3);
        assert_eq!(page_of(10, 0), 10);
        assert_eq!(parse_number(" 1,201 "), Some(1201));
        assert_eq!(parse_number("0"), None);
        assert_eq!(parse_number("x"), None);
        assert_eq!(parse_number(""), None);
    }
}
