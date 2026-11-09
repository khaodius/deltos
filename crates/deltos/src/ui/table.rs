//! A table, view or collection: its rows in the grid with a filter bar and a status bar,
//! or its structure (columns, indexes, foreign keys). Rows come through a
//! [`BrowseLoader`], which seeks by key, so any part of any size of table opens at once.

use std::sync::Arc;

use deltos_data::{
    Filter, FilterOp, ObjectInfo, ObjectKind, ObjectRef, Sort, SortDirection, Value,
};
use deltos_dock::PanelId;
use deltos_grid::{
    BrowseConfig, BrowseLoader, BrowseQuery, CacheConfig, GridCache, RowCount, Status,
};
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FontWeight, IntoElement, Render, SharedString,
    Subscription, Window, div, prelude::*, px, uniform_list,
};

use crate::net::Net;
use crate::state::{AppState, Inspected, StateEvent};
use crate::theme::{Theme, theme};
use crate::ui::grid::{GridEvent, GridView};
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{
    ButtonStyle, MenuItem, Tooltip, button, empty_state, field, icon_button, menu, segmented,
    thousands,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Data,
    Structure,
}

const OPS: [(FilterOp, &str); 10] = [
    (FilterOp::Eq, "="),
    (FilterOp::NotEq, "≠"),
    (FilterOp::Lt, "<"),
    (FilterOp::LtEq, "≤"),
    (FilterOp::Gt, ">"),
    (FilterOp::GtEq, "≥"),
    (FilterOp::Contains, "contains"),
    (FilterOp::StartsWith, "starts with"),
    (FilterOp::IsNull, "is NULL"),
    (FilterOp::IsNotNull, "is not NULL"),
];

fn op_label(op: FilterOp) -> &'static str {
    OPS.iter().find(|(o, _)| *o == op).map_or("=", |(_, l)| l)
}

struct FilterRow {
    column: usize,
    op: FilterOp,
    value: Entity<TextInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenMenu {
    Column(usize),
    Op(usize),
}

pub struct TablePanel {
    state: Entity<AppState>,
    connection: String,
    object: ObjectRef,
    grid: Entity<GridView>,
    loader: Option<Arc<BrowseLoader>>,
    mode: Mode,
    info: Option<Result<ObjectInfo, String>>,
    sort: Vec<Sort>,
    filters: Vec<FilterRow>,
    filters_open: bool,
    menu: Option<OpenMenu>,
    _subs: Vec<Subscription>,
}

impl TablePanel {
    pub fn new(
        state: Entity<AppState>,
        _id: PanelId,
        connection: String,
        object: ObjectRef,
        cx: &mut Context<Self>,
    ) -> Self {
        let grid = cx.new(GridView::new);
        let subs = vec![
            cx.subscribe(&grid, |this, grid, event, cx| match event {
                GridEvent::Sort(c) => this.cycle_sort(*c, cx),
                GridEvent::Selected(row) => {
                    let g = grid.read(cx);
                    let columns = g.columns().clone();
                    if let Some(values) = g.row_values(*row) {
                        let title = this.object.name.clone();
                        let row = *row;
                        this.state.update(cx, |s, cx| {
                            s.inspect(
                                Some(Inspected {
                                    title,
                                    row,
                                    columns,
                                    values,
                                }),
                                cx,
                            )
                        });
                    }
                }
            }),
            cx.subscribe(&state, |this, _, event, cx| {
                if let StateEvent::Sessions = event
                    && this.loader.is_none()
                {
                    this.attach(cx);
                }
            }),
            cx.observe(&grid, |_, _, cx| cx.notify()),
        ];
        let mut this = Self {
            state,
            connection,
            object,
            grid,
            loader: None,
            mode: Mode::Data,
            info: None,
            sort: Vec::new(),
            filters: Vec::new(),
            filters_open: false,
            menu: None,
            _subs: subs,
        };
        this.attach(cx);
        this
    }

    pub fn title(&self) -> SharedString {
        self.object.name.clone().into()
    }

    pub fn icon(&self) -> Icon {
        match self.object.kind {
            ObjectKind::View | ObjectKind::MaterializedView => Icon::View,
            ObjectKind::Collection => Icon::Collection,
            _ => Icon::Table,
        }
    }

    pub fn on_shown(&mut self, cx: &mut Context<Self>) {
        if self.loader.is_none() {
            self.attach(cx);
        }
    }

    /// Starts loading once the connection is open.
    fn attach(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self
            .state
            .read(cx)
            .session(&self.connection)
            .map(|s| s.conn.clone())
        else {
            return;
        };
        let data = self.state.read(cx).settings.data.clone();
        let loader = Arc::new(BrowseLoader::new(
            conn.clone(),
            self.object.clone(),
            self.query(cx),
            BrowseConfig {
                exact_count_threshold: if data.exact_counts {
                    u64::MAX
                } else {
                    BrowseConfig::default().exact_count_threshold
                },
            },
        ));
        let config = CacheConfig {
            page_rows: data.page_size,
            max_bytes: data.cache_memory_mb as usize * 1024 * 1024,
            ..CacheConfig::default()
        };
        self.grid.update(cx, |g, cx| {
            let hooks = g.make_hooks(cx);
            let cache = Arc::new(GridCache::new(loader.clone(), config, hooks));
            g.set_cache(Some(cache), false, cx);
        });
        self.loader = Some(loader);
        // The structure loads alongside, for the structure view and the filter columns.
        let object = self.object.clone();
        let job = cx
            .global::<Net>()
            .run(async move { conn.describe(&object).await.map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let info = job.await.unwrap_or_else(|| Err("cancelled".into()));
            this.update(cx, |this, cx| {
                this.info = Some(info);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn query(&self, cx: &App) -> BrowseQuery {
        let columns = self.grid.read(cx).columns().clone();
        let filters = self
            .filters
            .iter()
            .filter_map(|f| {
                let column = columns.get(f.column)?;
                let text = f.value.read(cx).text().to_owned();
                let value = match f.op {
                    FilterOp::IsNull | FilterOp::IsNotNull => Value::Null,
                    _ if column.kind.is_numeric() => text
                        .trim()
                        .parse::<i64>()
                        .map(Value::Int)
                        .or_else(|_| text.trim().parse::<f64>().map(Value::Float))
                        .unwrap_or_else(|_| Value::text(text.clone())),
                    _ if column.kind == deltos_data::ValueKind::Bool => match text.trim() {
                        "true" | "1" | "t" => Value::Bool(true),
                        "false" | "0" | "f" => Value::Bool(false),
                        _ => Value::text(text.clone()),
                    },
                    _ => Value::text(text.clone()),
                };
                Some(Filter {
                    column: column.name.clone(),
                    op: f.op,
                    value,
                })
            })
            .collect();
        BrowseQuery {
            sort: self.sort.clone(),
            filters,
            raw_where: None,
        }
    }

    /// Re-reads with the current sort and filters, keeping the column widths.
    fn requery(&mut self, cx: &mut Context<Self>) {
        let Some(loader) = self.loader.clone() else {
            return;
        };
        loader.set_query(self.query(cx));
        let columns = self.grid.read(cx).columns().clone();
        let sort: Vec<(usize, SortDirection)> = self
            .sort
            .iter()
            .filter_map(|s| {
                columns
                    .iter()
                    .position(|c| c.name == s.column)
                    .map(|i| (i, s.direction))
            })
            .collect();
        self.grid.update(cx, |g, cx| {
            if let Some(cache) = g.cache() {
                cache.invalidate();
            }
            g.selection = None;
            g.set_sort(sort, cx);
            g.refreshed(cx);
        });
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.loader.is_none() {
            self.attach(cx);
        } else {
            self.requery(cx);
        }
    }

    /// Ascending, then descending, then unsorted.
    fn cycle_sort(&mut self, column: usize, cx: &mut Context<Self>) {
        let Some(name) = self
            .grid
            .read(cx)
            .columns()
            .get(column)
            .map(|c| c.name.clone())
        else {
            return;
        };
        let next = match self.sort.first() {
            Some(s) if s.column == name && s.direction == SortDirection::Ascending => {
                Some(SortDirection::Descending)
            }
            Some(s) if s.column == name => None,
            _ => Some(SortDirection::Ascending),
        };
        self.sort = next
            .map(|direction| {
                vec![Sort {
                    column: name,
                    direction,
                }]
            })
            .unwrap_or_default();
        self.requery(cx);
    }

    fn add_filter(&mut self, cx: &mut Context<Self>) {
        let input = cx.new(|cx| TextInput::new("Value", cx));
        self._subs.push(cx.subscribe(&input, |this, _, e, cx| {
            if let InputEvent::Submit = e {
                this.requery(cx);
            }
        }));
        let column = self.grid.read(cx).selection.map_or(0, |s| s.head.1);
        self.filters.push(FilterRow {
            column,
            op: FilterOp::Eq,
            value: input,
        });
        self.filters_open = true;
        cx.notify();
    }

    fn filter_bar(&mut self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let columns = self.grid.read(cx).columns().clone();
        let mut bar = div()
            .flex_none()
            .px(px(8.))
            .py(px(6.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .bg(t.sidebar)
            .border_b_1()
            .border_color(t.border);
        for (i, f) in self.filters.iter().enumerate() {
            let column_name: SharedString = columns
                .get(f.column)
                .map_or_else(|| "Column".into(), |c| c.name.clone().into());
            let open = self.menu;
            let needs_value = !matches!(f.op, FilterOp::IsNull | FilterOp::IsNotNull);
            let picker = |id: (&'static str, usize), label: SharedString, width: f32| {
                div()
                    .id(id)
                    .relative()
                    .w(px(width))
                    .h(t.s(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .rounded(t.radius_small)
                    .bg(t.input)
                    .border_1()
                    .border_color(t.border_strong)
                    .cursor_pointer()
                    .text_size(t.ui_size)
                    .child(div().overflow_hidden().whitespace_nowrap().child(label))
                    .child(
                        icon(Icon::ChevronDown)
                            .size(px(12.))
                            .text_color(t.text_faint),
                    )
            };
            let mut col_picker = picker(("filter-col", i), column_name, 180.).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.menu = Some(OpenMenu::Column(i));
                    cx.notify();
                }),
            );
            if open == Some(OpenMenu::Column(i)) {
                let weak = cx.entity().downgrade();
                let items = columns
                    .iter()
                    .enumerate()
                    .map(|(c, col)| {
                        let weak = weak.clone();
                        MenuItem::new(col.name.clone(), move |_, cx| {
                            weak.update(cx, |this, cx| {
                                if let Some(f) = this.filters.get_mut(i) {
                                    f.column = c;
                                }
                                cx.notify();
                            })
                            .ok();
                        })
                        .detail(col.type_name.clone())
                        .checked(c == f.column)
                    })
                    .collect();
                let close = weak.clone();
                col_picker = col_picker.child(menu(
                    "column-menu",
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
            let mut op_picker = picker(("filter-op", i), op_label(f.op).into(), 110.).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.menu = Some(OpenMenu::Op(i));
                    cx.notify();
                }),
            );
            if open == Some(OpenMenu::Op(i)) {
                let weak = cx.entity().downgrade();
                let items = OPS
                    .iter()
                    .map(|(op, label)| {
                        let (weak, op) = (weak.clone(), *op);
                        MenuItem::new(*label, move |_, cx| {
                            weak.update(cx, |this, cx| {
                                if let Some(f) = this.filters.get_mut(i) {
                                    f.op = op;
                                }
                                cx.notify();
                            })
                            .ok();
                        })
                        .checked(op == f.op)
                    })
                    .collect();
                let close = weak.clone();
                op_picker = op_picker.child(menu(
                    "op-menu",
                    items,
                    140.,
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
            bar = bar.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(col_picker)
                    .child(op_picker)
                    .child(
                        div()
                            .flex_1()
                            .when(needs_value, |el| el.child(field(&f.value, t, window, cx)))
                            .when(!needs_value, |el| el.h(t.s(26.))),
                    )
                    .child(
                        icon_button(("filter-remove", i), Icon::Close, t)
                            .tooltip(|_, cx| Tooltip::view("Remove this condition", cx))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if i < this.filters.len() {
                                    this.filters.remove(i);
                                }
                                if this.filters.is_empty() {
                                    this.filters_open = false;
                                }
                                this.requery(cx);
                            })),
                    ),
            );
        }
        bar.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                    button("filter-add", "Add condition", ButtonStyle::Ghost, t)
                        .on_click(cx.listener(|this, _, _, cx| this.add_filter(cx))),
                )
                .child(div().flex_1())
                .child(
                    button("filter-clear", "Clear", ButtonStyle::Secondary, t).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.filters.clear();
                            this.filters_open = false;
                            this.requery(cx);
                        }),
                    ),
                )
                .child(
                    button("filter-apply", "Apply", ButtonStyle::Primary, t)
                        .on_click(cx.listener(|this, _, _, cx| this.requery(cx))),
                ),
        )
        .into_any_element()
    }

    fn status_bar(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let grid = self.grid.read(cx);
        let (count, status, memory) = match grid.cache() {
            Some(c) => (c.row_count(), c.status(), c.memory_used()),
            None => (RowCount::Unknown, Status::Idle, 0),
        };
        let rows: SharedString = match count {
            RowCount::Unknown => "– rows".into(),
            RowCount::Estimated(n) => format!("≈ {} rows", thousands(n)).into(),
            RowCount::AtLeast(n) => format!("{}+ rows", thousands(n)).into(),
            RowCount::Exact(n) => {
                format!("{} {}", thousands(n), if n == 1 { "row" } else { "rows" }).into()
            }
        };
        let selected = grid.selection.map(|s| s.head.0 + 1);
        let mode = self.mode;
        let filters = self.filters.len();
        div()
            .flex_none()
            .h(t.bar_height)
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(10.))
            .bg(t.base)
            .border_t_1()
            .border_color(t.border)
            .text_size(t.small_size)
            .text_color(t.text_muted)
            .child(segmented(
                "table-mode",
                &[("Data", Mode::Data), ("Structure", Mode::Structure)],
                mode,
                {
                    let weak = cx.entity().downgrade();
                    move |m, _, cx| {
                        weak.update(cx, |this, cx| {
                            this.mode = m;
                            cx.notify();
                        })
                        .ok();
                    }
                },
                t,
            ))
            .child(
                icon_button("toggle-filters", Icon::Filter, t)
                    .when(filters > 0, |el| el.text_color(t.accent))
                    .tooltip(|_, cx| Tooltip::view("Filter rows", cx))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.filters.is_empty() {
                            this.add_filter(cx);
                        } else {
                            this.filters_open = !this.filters_open;
                            cx.notify();
                        }
                    })),
            )
            .child(div().flex_1())
            .when_some(selected, |el, n| el.child(format!("Row {}", thousands(n))))
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.text)
                    .child(rows),
            )
            .child(match status {
                Status::Loading => div()
                    .text_color(t.text_faint)
                    .child("Loading…")
                    .into_any_element(),
                Status::Error(_) => div().text_color(t.danger).child("Error").into_any_element(),
                _ => div().into_any_element(),
            })
            .when(memory > 0, |el| {
                el.child(
                    div()
                        .text_color(t.text_faint)
                        .child(format!("{:.1} MB", memory as f64 / 1_048_576.0)),
                )
            })
            .child(
                icon_button("refresh", Icon::Refresh, t)
                    .tooltip(|_, cx| Tooltip::view("Reload", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .into_any_element()
    }

    fn structure(&mut self, t: &Theme) -> AnyElement {
        let info = match &self.info {
            None => return empty_state("Loading structure…", None, t).into_any_element(),
            Some(Err(e)) => {
                return empty_state("Could not read the structure", Some(e.clone().into()), t)
                    .into_any_element();
            }
            Some(Ok(info)) => info.clone(),
        };
        let columns = Arc::new(info.columns.clone());
        let n = columns.len();
        let t2 = t.clone();
        let cols = columns.clone();
        let header = |cells: &[(&'static str, f32)]| {
            let mut row = div()
                .flex_none()
                .h(t.row_height)
                .flex()
                .items_center()
                .bg(t.grid_header)
                .border_b_1()
                .border_color(t.border)
                .text_size(t.small_size)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.text_muted);
            for (label, w) in cells {
                row = row.child(div().w(px(*w)).px(px(8.)).flex_none().child(*label));
            }
            row
        };
        let list = uniform_list("structure-columns", n, move |range, _, _| {
            let t = &t2;
            range
                .map(|i| {
                    let c = &cols[i];
                    div()
                        .h(t.row_height)
                        .flex()
                        .items_center()
                        .when(t.zebra && i % 2 == 1, |el| el.bg(t.surface_alt))
                        .border_b_1()
                        .border_color(t.grid_line)
                        .font_family(t.mono_font.clone())
                        .text_size(t.mono_size)
                        .child(
                            div()
                                .w(px(220.))
                                .px(px(8.))
                                .flex()
                                .gap(px(6.))
                                .items_center()
                                .when(c.primary_key.is_some(), |el| {
                                    el.child(icon(Icon::Key).size(px(11.)).text_color(t.warning))
                                })
                                .child(c.name.clone()),
                        )
                        .child(
                            div()
                                .w(px(180.))
                                .px(px(8.))
                                .text_color(t.syntax_keyword)
                                .child(c.type_name.clone()),
                        )
                        .child(div().w(px(90.)).px(px(8.)).text_color(t.text_muted).child(
                            match c.nullable {
                                Some(true) => "YES",
                                Some(false) => "NO",
                                None => "",
                            },
                        ))
                        .child(
                            div()
                                .w(px(180.))
                                .px(px(8.))
                                .text_color(if c.default.is_some() {
                                    t.text
                                } else {
                                    t.text_faint
                                })
                                .child(c.default.clone().unwrap_or_else(|| t.null_text.to_owned())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .px(px(8.))
                                .text_color(t.text_muted)
                                .child(c.comment.clone().unwrap_or_default()),
                        )
                })
                .collect()
        })
        .flex_1();
        let mut extra = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(10.))
            .border_t_1()
            .border_color(t.border);
        if !info.indexes.is_empty() {
            extra = extra.child(crate::ui::caption("Indexes", t));
            for ix in &info.indexes {
                extra = extra.child(
                    div()
                        .flex()
                        .gap(px(10.))
                        .font_family(t.mono_font.clone())
                        .text_size(t.mono_size)
                        .child(div().w(px(240.)).child(ix.name.clone()))
                        .child(
                            div()
                                .text_color(t.text_muted)
                                .child(format!("({})", ix.columns.join(", "))),
                        )
                        .when(ix.primary, |el| {
                            el.child(div().text_color(t.warning).child("PRIMARY"))
                        })
                        .when(ix.unique && !ix.primary, |el| {
                            el.child(div().text_color(t.syntax_keyword).child("UNIQUE"))
                        })
                        .children(
                            ix.method
                                .clone()
                                .map(|m| div().text_color(t.text_faint).child(m)),
                        ),
                );
            }
        }
        if !info.foreign_keys.is_empty() {
            extra = extra.child(crate::ui::caption("Foreign keys", t).mt(px(6.)));
            for fk in &info.foreign_keys {
                extra = extra.child(
                    div()
                        .flex()
                        .gap(px(10.))
                        .font_family(t.mono_font.clone())
                        .text_size(t.mono_size)
                        .child(div().w(px(240.)).child(fk.name.clone()))
                        .child(div().text_color(t.text_muted).child(format!(
                            "({}) → {}({})",
                            fk.columns.join(", "),
                            fk.target.name,
                            fk.target_columns.join(", ")
                        ))),
                );
            }
        }
        let has_extra = !info.indexes.is_empty() || !info.foreign_keys.is_empty();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface)
            .child(header(&[
                ("Column", 220.),
                ("Type", 180.),
                ("Nullable", 90.),
                ("Default", 180.),
                ("Comment", 200.),
            ]))
            .child(list)
            .when(has_extra, |el| el.child(extra))
            .into_any_element()
    }
}

impl Render for TablePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        if self.loader.is_none() {
            let connecting = self
                .state
                .read(cx)
                .connecting
                .contains_key(&self.connection);
            let id = self.connection.clone();
            let name = self
                .state
                .read(cx)
                .settings
                .connection(&id)
                .map_or_else(|| id.clone(), |c| c.name.clone());
            return div()
                .size_full()
                .bg(t.surface)
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.))
                .child(
                    empty_state(
                        format!("{} is not connected", name),
                        Some(
                            format!("{}.{}", self.object.namespace.join("."), self.object.name)
                                .into(),
                        ),
                        &t,
                    )
                    .size_auto(),
                )
                .child(
                    button(
                        "connect",
                        if connecting {
                            "Connecting…"
                        } else {
                            "Connect"
                        },
                        ButtonStyle::Primary,
                        &t,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let id = id.clone();
                        this.state.update(cx, |s, cx| s.connect(&id, cx));
                    })),
                )
                .into_any_element();
        }
        let body = match self.mode {
            Mode::Data => self.grid.clone().into_any_element(),
            Mode::Structure => self.structure(&t),
        };
        let filters =
            (self.filters_open && self.mode == Mode::Data).then(|| self.filter_bar(&t, window, cx));
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface)
            .children(filters)
            .child(div().flex_1().min_h_0().child(body))
            .child(self.status_bar(&t, cx))
            .into_any_element()
    }
}
