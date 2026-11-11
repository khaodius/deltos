//! Layout presets: named arrangements of the main window's panels, applied from the
//! title bar's layout menu, the command palette or Settings, and saved, renamed and
//! deleted on the Workspace page of Settings. A preset moves panels around and opens or
//! closes tool panels; it never closes a table or a query.

use deltos_settings::{
    BUILT_IN_PRESETS, LayoutPreset, PRESET_LIMIT, PanelKind, PresetPanel, Settings, unique_name,
};
use gpui::{
    App, AppContext, Context, Entity, Focusable, Global, IntoElement, Render, SharedString,
    Subscription, WeakEntity, Window, div, prelude::*, px,
};

use crate::state::AppState;
use crate::theme::theme;
use crate::ui::icons::{Icon, icon};
use crate::ui::palette::providers::{Kind, PaletteItem, Sources, show_settings};
use crate::ui::settings::{Section, gap, group, toggle_row};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::workspace::{PanelView, Workspace};
use crate::ui::{ButtonStyle, MenuItem, Tooltip, button, field, icon_button, menu};

/// The workspace presets apply to, for views that only hold the app state.
struct Current(WeakEntity<Workspace>);

impl Global for Current {}

pub fn init(workspace: &Entity<Workspace>, cx: &mut App) {
    cx.set_global(Current(workspace.downgrade()));
}

fn current_workspace(cx: &App) -> Option<Entity<Workspace>> {
    cx.try_global::<Current>().and_then(|c| c.0.upgrade())
}

/// The built-in presets, then the person's own.
pub fn all(settings: &Settings) -> Vec<LayoutPreset> {
    let mut out = LayoutPreset::built_in();
    out.extend(settings.layout_presets.iter().cloned());
    out
}

pub fn is_built_in(name: &str) -> bool {
    BUILT_IN_PRESETS.contains(&name)
}

/// What a preset shows, in reading order: "Objects, tabs, inspector".
pub fn summary(preset: &LayoutPreset) -> String {
    let mut slots = Vec::new();
    preset.root.panels(&mut slots);
    let mut words: Vec<&str> = Vec::new();
    for slot in slots {
        let word = match slot {
            PresetPanel::Documents => "tabs",
            PresetPanel::Objects => "objects",
            PresetPanel::Inspector => "inspector",
            PresetPanel::History => "history",
        };
        if !words.contains(&word) {
            words.push(word);
        }
    }
    let mut text = words.join(", ");
    if let Some(first) = text.get(..1) {
        text = first.to_uppercase() + &text[1..];
    }
    if preset.query {
        text.push_str("; a query in front");
    }
    text
}

/// Rearranges the workspace as preset `name` says. A preset that wants a query in
/// front shows the open one, or opens one for the active connection.
pub fn apply(workspace: &Entity<Workspace>, name: &str, cx: &mut App) {
    let state = workspace.read(cx).state.clone();
    let Some(preset) = all(&state.read(cx).settings)
        .into_iter()
        .find(|p| p.name == name)
    else {
        return;
    };
    let connection = state.read(cx).active.clone();
    workspace.update(cx, |w, cx| {
        let mut front = w.active_document();
        if preset.query {
            let is_query = |id| matches!(w.view(id), Some(PanelView::Query(_)));
            let query = front.filter(|id| is_query(*id)).or_else(|| {
                w.layout
                    .panels
                    .iter()
                    .find(|p| matches!(p.kind, PanelKind::Query { .. }))
                    .map(|p| p.id)
            });
            front = match query {
                Some(q) => Some(q),
                None => {
                    w.open(
                        PanelKind::Query {
                            connection,
                            text: String::new(),
                        },
                        true,
                        cx,
                    );
                    w.active_document()
                }
            };
        }
        let layout = preset.arrange(&w.layout, front);
        w.replace_layout(layout, cx);
        if let Some(f) = front {
            w.show(f, cx);
        }
    });
}

/// Saves the workspace's arrangement as a preset. The name gets a number when it is
/// taken; the result is the name it was saved under.
pub fn save_current(
    workspace: &Entity<Workspace>,
    name: &str,
    cx: &mut App,
) -> Result<String, &'static str> {
    let (layout, front) = {
        let w = workspace.read(cx);
        (w.layout.clone(), w.active_document())
    };
    let state = workspace.read(cx).state.clone();
    if state.read(cx).settings.layout_presets.len() >= PRESET_LIMIT {
        return Err("That is as many layouts as can be kept; delete one first.");
    }
    let name = {
        let presets = all(&state.read(cx).settings);
        let taken: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
        unique_name(name, &taken)
    };
    let preset = LayoutPreset::capture(name.clone(), &layout, front);
    state.update(cx, |s, cx| {
        s.update_settings(cx, |st| st.layout_presets.push(preset))
    });
    Ok(name)
}

/// Puts the workspace's arrangement into the person's preset `name`.
pub fn update_preset(workspace: &Entity<Workspace>, name: &str, cx: &mut App) {
    let (layout, front) = {
        let w = workspace.read(cx);
        (w.layout.clone(), w.active_document())
    };
    let state = workspace.read(cx).state.clone();
    let name = name.to_owned();
    state.update(cx, |s, cx| {
        s.update_settings(cx, |st| {
            if let Some(p) = st.layout_presets.iter_mut().find(|p| p.name == name) {
                let query = p.query;
                *p = LayoutPreset::capture(name.clone(), &layout, front);
                p.query = query;
            }
        })
    });
}

/// Renames the person's preset `old`. A name another preset has gets a number.
pub fn rename(state: &Entity<AppState>, old: &str, new: &str, cx: &mut App) {
    if new.trim().is_empty() || is_built_in(old) {
        return;
    }
    let presets = all(&state.read(cx).settings);
    let taken: Vec<&str> = presets
        .iter()
        .map(|p| p.name.as_str())
        .filter(|n| *n != old)
        .collect();
    let new = unique_name(new, &taken);
    let old = old.to_owned();
    state.update(cx, |s, cx| {
        s.update_settings(cx, |st| {
            if let Some(p) = st.layout_presets.iter_mut().find(|p| p.name == old) {
                p.name = new;
            }
        })
    });
}

pub fn delete(state: &Entity<AppState>, name: &str, cx: &mut App) {
    let name = name.to_owned();
    state.update(cx, |s, cx| {
        s.update_settings(cx, |st| st.layout_presets.retain(|p| p.name != name))
    });
}

/// Opens Settings at the Workspace page with the cursor in the new layout's name.
pub fn manage(workspace: &Entity<Workspace>, focus_name: bool, window: &mut Window, cx: &mut App) {
    let Some(settings) = show_settings(workspace, Section::Workspace, cx) else {
        return;
    };
    if focus_name {
        let page = settings.read(cx).layouts_page();
        let input = page.read(cx).name.clone();
        window.focus(&input.focus_handle(cx));
    }
}

/// The palette's layout items: every preset, and saving the current arrangement.
pub fn provider(s: &Sources, out: &mut Vec<PaletteItem>) {
    for preset in all(&s.app_state().settings) {
        let ws = s.workspace.clone();
        let name = preset.name.clone();
        out.push(
            PaletteItem::new(
                format!("layout:{name}"),
                Kind::Layout,
                Icon::Columns,
                format!("Layout: {name}"),
                move |_, cx| apply(&ws, &name, cx),
            )
            .detail(summary(&preset))
            .search("preset arrangement panels"),
        );
    }
    let ws = s.workspace.clone();
    out.push(
        PaletteItem::new(
            "layout-save",
            Kind::Layout,
            Icon::Columns,
            "Save the current layout…",
            move |window, cx| manage(&ws, true, window, cx),
        )
        .search("preset arrangement panels new"),
    );
}

/// The title bar's layout button and its menu of presets.
pub struct LayoutMenu {
    workspace: Entity<Workspace>,
    open: bool,
}

impl LayoutMenu {
    pub fn new(workspace: Entity<Workspace>) -> Self {
        Self {
            workspace,
            open: false,
        }
    }
}

impl Render for LayoutMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let button = icon_button("layout-menu", Icon::Columns, &t)
            .when(self.open, |el| el.bg(t.hover).text_color(t.text))
            .when(!self.open, |el| {
                el.tooltip(|_, cx| Tooltip::view("Layouts", cx))
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.open = !this.open;
                cx.notify();
            }));
        let mut el = div().relative().child(button);
        if self.open {
            let state = self.workspace.read(cx).state.clone();
            let presets = all(&state.read(cx).settings);
            let mut items: Vec<MenuItem> = Vec::new();
            for (i, p) in presets.iter().enumerate() {
                if i == BUILT_IN_PRESETS.len() {
                    items.push(MenuItem::separator());
                }
                let (ws, name) = (self.workspace.clone(), p.name.clone());
                items.push(MenuItem::new(p.name.clone(), move |_, cx| {
                    apply(&ws, &name, cx)
                }));
            }
            items.push(MenuItem::separator());
            let ws = self.workspace.clone();
            items.push(MenuItem::new(
                "Save the current layout…",
                move |window, cx| manage(&ws, true, window, cx),
            ));
            let ws = self.workspace.clone();
            items.push(MenuItem::new("Manage layouts…", move |window, cx| {
                manage(&ws, false, window, cx)
            }));
            let weak = cx.entity().downgrade();
            // Placed under the button's bottom edge, so the menu opens below it.
            el = el.child(div().absolute().top_full().left_0().child(menu(
                "layout-menu-list",
                items,
                200.,
                move |_, cx| {
                    weak.update(cx, |this, cx| {
                        this.open = false;
                        cx.notify();
                    })
                    .ok();
                },
                &t,
            )));
        }
        el
    }
}

/// The Workspace page of Settings: restoring the last session, and the presets.
pub struct LayoutsPage {
    state: Entity<AppState>,
    /// The name of a layout to save.
    pub name: Entity<TextInput>,
    /// The preset being renamed, and its field.
    renaming: Option<(String, Entity<TextInput>, Subscription)>,
    /// The preset whose menu is open.
    menu: Option<String>,
    note: Option<SharedString>,
    _subs: Vec<Subscription>,
}

impl LayoutsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new("Name", cx));
        let subs = vec![
            cx.subscribe(&name, |this, _, e, cx| match e {
                InputEvent::Submit => this.save(cx),
                InputEvent::Cancel => this.name.update(cx, |i, cx| i.clear(cx)),
                InputEvent::Changed => {}
            }),
            cx.observe(&state, |_, _, cx| cx.notify()),
        ];
        Self {
            state,
            name,
            renaming: None,
            menu: None,
            note: None,
            _subs: subs,
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = current_workspace(cx) else {
            return;
        };
        let typed = self.name.read(cx).text().to_owned();
        self.note = Some(match save_current(&ws, &typed, cx) {
            Ok(name) => {
                self.name.update(cx, |i, cx| i.clear(cx));
                format!("Saved as “{name}”.").into()
            }
            Err(e) => e.into(),
        });
        cx.notify();
    }

    fn start_rename(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            let mut i = TextInput::new("Name", cx);
            i.set_text(name.clone(), cx);
            i.select_everything(cx);
            i
        });
        let sub = cx.subscribe(&input, |this, input, e, cx| match e {
            InputEvent::Submit => {
                if let Some((old, _, _)) = this.renaming.take() {
                    let new = input.read(cx).text().to_owned();
                    rename(&this.state, &old, &new, cx);
                }
                cx.notify();
            }
            InputEvent::Cancel => {
                this.renaming = None;
                cx.notify();
            }
            InputEvent::Changed => {}
        });
        window.focus(&input.focus_handle(cx));
        self.renaming = Some((name, input, sub));
        self.menu = None;
        cx.notify();
    }

    fn preset_row(
        &self,
        ix: usize,
        preset: &LayoutPreset,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let t = theme(cx).clone();
        let built_in = is_built_in(&preset.name);
        let renaming = self
            .renaming
            .as_ref()
            .filter(|(n, _, _)| *n == preset.name)
            .map(|(_, input, _)| input.clone());
        let title = match renaming {
            Some(input) => field(&input, &t, window, cx).w_full().into_any_element(),
            None => div()
                .text_size(t.ui_size)
                .text_color(t.text)
                .child(preset.name.clone())
                .into_any_element(),
        };
        let detail = if built_in {
            format!("{} · built in", summary(preset))
        } else {
            summary(preset)
        };
        let name = preset.name.clone();
        let apply_button = button(("layout-apply", ix), "Apply", ButtonStyle::Secondary, &t)
            .on_click(move |_, _, cx| {
                if let Some(ws) = current_workspace(cx) {
                    apply(&ws, &name, cx);
                }
            });
        let more = (!built_in).then(|| {
            let name = preset.name.clone();
            let open = self.menu.as_deref() == Some(preset.name.as_str());
            let button = icon_button(("layout-more", ix), Icon::More, &t)
                .when(open, |el| el.bg(t.hover).text_color(t.text))
                .when(!open, |el| el.tooltip(|_, cx| Tooltip::view("More", cx)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.menu = Some(name.clone());
                    cx.notify();
                }));
            let mut more = div().relative().child(button);
            if open {
                let weak = cx.entity().downgrade();
                let (n1, n2, n3) = (
                    preset.name.clone(),
                    preset.name.clone(),
                    preset.name.clone(),
                );
                let (w1, w2) = (weak.clone(), weak.clone());
                let state = self.state.clone();
                let items = vec![
                    MenuItem::new("Replace with the current layout", move |_, cx| {
                        if let Some(ws) = current_workspace(cx) {
                            update_preset(&ws, &n1, cx);
                        }
                    }),
                    MenuItem::new("Rename", move |window, cx| {
                        let name = n2.clone();
                        w1.update(cx, |this, cx| this.start_rename(name, window, cx))
                            .ok();
                    }),
                    MenuItem::separator(),
                    MenuItem::new("Delete", move |_, cx| delete(&state, &n3, cx)).danger(),
                ];
                more = more.child(div().absolute().top_full().left_0().child(menu(
                    "layout-more-menu",
                    items,
                    220.,
                    move |_, cx| {
                        w2.update(cx, |this, cx| {
                            this.menu = None;
                            cx.notify();
                        })
                        .ok();
                    },
                    &t,
                )));
            }
            more
        });
        div()
            .min_h(t.s(40.))
            .py(px(7.))
            .flex()
            .items_center()
            .gap(px(12.))
            .border_b_1()
            .border_color(t.grid_line)
            .child(
                icon(Icon::Columns)
                    .size(px(13.))
                    .flex_none()
                    .text_color(t.text_faint),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(title)
                    .child(
                        div()
                            .text_size(t.small_size)
                            .text_color(t.text_faint)
                            .child(detail),
                    ),
            )
            .child(apply_button)
            .child(div().w(t.s(22.)).flex_none().children(more))
    }
}

impl Render for LayoutsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let settings = &self.state.read(cx).settings;
        let restore = settings.session.restore;
        let presets = all(settings);
        let state = self.state.clone();
        let session = group("Session", &t).child(toggle_row(
            "Restore last session on launch",
            Some(
                "Opens the tabs, query text, panel windows and connections that were open. \
                 Connections that ask for a password wait for a click.",
            ),
            ("restore-session", restore),
            move |_, _, cx| {
                state.update(cx, |s, cx| {
                    s.update_settings(cx, |st| st.session.restore = !st.session.restore)
                })
            },
            &t,
        ));
        let mut list = group("Layouts", &t).child(
            div()
                .pb(px(6.))
                .text_size(t.small_size)
                .text_color(t.text_faint)
                .child(
                    "A layout places the panels; it keeps every open tab. \
                     Apply one from here, the layout button in the title bar or the command \
                     palette.",
                ),
        );
        for (ix, p) in presets.iter().enumerate() {
            list = list.child(self.preset_row(ix, p, window, cx));
        }
        let save = div()
            .pt(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(field(&self.name, &t, window, cx).flex_1())
            .child(
                button(
                    "layout-save",
                    "Save the current layout",
                    ButtonStyle::Primary,
                    &t,
                )
                .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            );
        let note = self.note.clone().map(|n| {
            div()
                .pt(px(6.))
                .text_size(t.small_size)
                .text_color(t.text_muted)
                .child(n)
        });
        div()
            .flex()
            .flex_col()
            .child(session)
            .child(gap())
            .child(list.child(save).children(note))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_name_the_slots() {
        let built_in = LayoutPreset::built_in();
        assert_eq!(summary(&built_in[0]), "Objects, tabs, inspector");
        assert_eq!(summary(&built_in[1]), "Tabs");
        assert_eq!(summary(&built_in[2]), "Objects, tabs; a query in front");
        assert!(built_in.iter().all(|p| is_built_in(&p.name)));
    }

    #[test]
    fn the_built_ins_list_first() {
        let mut s = Settings::default();
        s.layout_presets
            .push(LayoutPreset::capture("Mine", &s.layout, None));
        let names: Vec<String> = all(&s).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["Default", "Focus", "Query", "Mine"]);
    }
}
