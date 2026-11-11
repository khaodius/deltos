//! The interface icons: 16px stroked outlines in `assets/ui-icons`, compiled into the
//! binary and served to GPUI's SVG renderer, which draws them in the text color.

use std::borrow::Cow;

use gpui::{
    App, AssetSource, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Result, SharedString, StyleRefinement, Styled, Svg, Window, svg,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    /// The Deltos mark, for the title bar.
    Logo,
    Check,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    Close,
    Code,
    Collection,
    Columns,
    Copy,
    Database,
    Dock,
    Export,
    Eyedropper,
    Filter,
    Function,
    Grid,
    History,
    Key,
    Link,
    Lock,
    Maximize,
    Minimize,
    More,
    PanelBottom,
    Play,
    Plug,
    Plus,
    PopOut,
    Refresh,
    Restore,
    Schema,
    Search,
    Sequence,
    Settings,
    SidebarLeft,
    SidebarRight,
    SortAsc,
    SortDesc,
    Stop,
    Structure,
    Table,
    Terminal,
    Trash,
    Undo,
    View,
}

impl Icon {
    pub const ALL: [Icon; 46] = [
        Self::Logo,
        Self::Check,
        Self::ChevronDown,
        Self::ChevronLeft,
        Self::ChevronRight,
        Self::Close,
        Self::Code,
        Self::Collection,
        Self::Columns,
        Self::Copy,
        Self::Database,
        Self::Dock,
        Self::Export,
        Self::Eyedropper,
        Self::Filter,
        Self::Function,
        Self::Grid,
        Self::History,
        Self::Key,
        Self::Link,
        Self::Lock,
        Self::Maximize,
        Self::Minimize,
        Self::More,
        Self::PanelBottom,
        Self::Play,
        Self::Plug,
        Self::Plus,
        Self::PopOut,
        Self::Refresh,
        Self::Restore,
        Self::Schema,
        Self::Search,
        Self::Sequence,
        Self::Settings,
        Self::SidebarLeft,
        Self::SidebarRight,
        Self::SortAsc,
        Self::SortDesc,
        Self::Stop,
        Self::Structure,
        Self::Table,
        Self::Terminal,
        Self::Trash,
        Self::Undo,
        Self::View,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Logo => "logo",
            Self::Check => "check",
            Self::ChevronDown => "chevron-down",
            Self::ChevronLeft => "chevron-left",
            Self::ChevronRight => "chevron-right",
            Self::Close => "close",
            Self::Code => "code",
            Self::Collection => "collection",
            Self::Columns => "columns",
            Self::Copy => "copy",
            Self::Database => "database",
            Self::Dock => "dock",
            Self::Export => "export",
            Self::Eyedropper => "eyedropper",
            Self::Filter => "filter",
            Self::Function => "function",
            Self::Grid => "grid",
            Self::History => "history",
            Self::Key => "key",
            Self::Link => "link",
            Self::Lock => "lock",
            Self::Maximize => "maximize",
            Self::Minimize => "minimize",
            Self::More => "more",
            Self::PanelBottom => "panel-bottom",
            Self::Play => "play",
            Self::Plug => "plug",
            Self::Plus => "plus",
            Self::PopOut => "pop-out",
            Self::Refresh => "refresh",
            Self::Restore => "restore",
            Self::Schema => "schema",
            Self::Search => "search",
            Self::Sequence => "sequence",
            Self::Settings => "settings",
            Self::SidebarLeft => "sidebar-left",
            Self::SidebarRight => "sidebar-right",
            Self::SortAsc => "sort-asc",
            Self::SortDesc => "sort-desc",
            Self::Stop => "stop",
            Self::Structure => "structure",
            Self::Table => "table",
            Self::Terminal => "terminal",
            Self::Trash => "trash",
            Self::Undo => "undo",
            Self::View => "view",
        }
    }

    pub fn path(self) -> SharedString {
        format!("ui-icons/{}.svg", self.name()).into()
    }

    const fn document(self) -> &'static [u8] {
        match self {
            Self::Logo => include_bytes!("../../../../assets/icons/deltos-glyph.svg"),
            Self::Check => include_bytes!("../../../../assets/ui-icons/check.svg"),
            Self::ChevronDown => include_bytes!("../../../../assets/ui-icons/chevron-down.svg"),
            Self::ChevronLeft => include_bytes!("../../../../assets/ui-icons/chevron-left.svg"),
            Self::ChevronRight => include_bytes!("../../../../assets/ui-icons/chevron-right.svg"),
            Self::Close => include_bytes!("../../../../assets/ui-icons/close.svg"),
            Self::Code => include_bytes!("../../../../assets/ui-icons/code.svg"),
            Self::Collection => include_bytes!("../../../../assets/ui-icons/collection.svg"),
            Self::Columns => include_bytes!("../../../../assets/ui-icons/columns.svg"),
            Self::Copy => include_bytes!("../../../../assets/ui-icons/copy.svg"),
            Self::Database => include_bytes!("../../../../assets/ui-icons/database.svg"),
            Self::Dock => include_bytes!("../../../../assets/ui-icons/dock.svg"),
            Self::Export => include_bytes!("../../../../assets/ui-icons/export.svg"),
            Self::Eyedropper => include_bytes!("../../../../assets/ui-icons/eyedropper.svg"),
            Self::Filter => include_bytes!("../../../../assets/ui-icons/filter.svg"),
            Self::Function => include_bytes!("../../../../assets/ui-icons/function.svg"),
            Self::Grid => include_bytes!("../../../../assets/ui-icons/grid.svg"),
            Self::History => include_bytes!("../../../../assets/ui-icons/history.svg"),
            Self::Key => include_bytes!("../../../../assets/ui-icons/key.svg"),
            Self::Link => include_bytes!("../../../../assets/ui-icons/link.svg"),
            Self::Lock => include_bytes!("../../../../assets/ui-icons/lock.svg"),
            Self::Maximize => include_bytes!("../../../../assets/ui-icons/maximize.svg"),
            Self::Minimize => include_bytes!("../../../../assets/ui-icons/minimize.svg"),
            Self::More => include_bytes!("../../../../assets/ui-icons/more.svg"),
            Self::PanelBottom => include_bytes!("../../../../assets/ui-icons/panel-bottom.svg"),
            Self::Play => include_bytes!("../../../../assets/ui-icons/play.svg"),
            Self::Plug => include_bytes!("../../../../assets/ui-icons/plug.svg"),
            Self::Plus => include_bytes!("../../../../assets/ui-icons/plus.svg"),
            Self::PopOut => include_bytes!("../../../../assets/ui-icons/pop-out.svg"),
            Self::Refresh => include_bytes!("../../../../assets/ui-icons/refresh.svg"),
            Self::Restore => include_bytes!("../../../../assets/ui-icons/restore.svg"),
            Self::Schema => include_bytes!("../../../../assets/ui-icons/schema.svg"),
            Self::Search => include_bytes!("../../../../assets/ui-icons/search.svg"),
            Self::Sequence => include_bytes!("../../../../assets/ui-icons/sequence.svg"),
            Self::Settings => include_bytes!("../../../../assets/ui-icons/settings.svg"),
            Self::SidebarLeft => include_bytes!("../../../../assets/ui-icons/sidebar-left.svg"),
            Self::SidebarRight => include_bytes!("../../../../assets/ui-icons/sidebar-right.svg"),
            Self::SortAsc => include_bytes!("../../../../assets/ui-icons/sort-asc.svg"),
            Self::SortDesc => include_bytes!("../../../../assets/ui-icons/sort-desc.svg"),
            Self::Stop => include_bytes!("../../../../assets/ui-icons/stop.svg"),
            Self::Structure => include_bytes!("../../../../assets/ui-icons/structure.svg"),
            Self::Table => include_bytes!("../../../../assets/ui-icons/table.svg"),
            Self::Terminal => include_bytes!("../../../../assets/ui-icons/terminal.svg"),
            Self::Trash => include_bytes!("../../../../assets/ui-icons/trash.svg"),
            Self::Undo => include_bytes!("../../../../assets/ui-icons/undo.svg"),
            Self::View => include_bytes!("../../../../assets/ui-icons/view.svg"),
        }
    }
}

pub fn icon(icon: Icon) -> IconElement {
    IconElement(svg().path(icon.path()).flex_none().size_4())
}

/// An SVG icon that takes the surrounding text color unless given its own. GPUI's plain
/// `svg()` draws nothing without an explicit color.
pub struct IconElement(Svg);

impl Styled for IconElement {
    fn style(&mut self) -> &mut StyleRefinement {
        self.0.style()
    }
}

impl IntoElement for IconElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for IconElement {
    type RequestLayoutState = <Svg as Element>::RequestLayoutState;
    type PrepaintState = <Svg as Element>::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.0.id()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.0.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.0.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.0
            .prepaint(id, inspector_id, bounds, layout, window, cx)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let text = self.0.style().text.get_or_insert_default();
        if text.color.is_none() {
            text.color = Some(window.text_style().color);
        }
        self.0
            .paint(id, inspector_id, bounds, layout, prepaint, window, cx);
    }
}

/// Serves the icons to GPUI.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let Some(name) = path
            .strip_prefix("ui-icons/")
            .and_then(|p| p.strip_suffix(".svg"))
        else {
            return Ok(None);
        };
        Ok(Icon::ALL
            .iter()
            .find(|i| i.name() == name)
            .map(|i| Cow::Borrowed(i.document())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(if path.is_empty() || path.starts_with("ui-icons") {
            Icon::ALL.iter().map(|i| i.path()).collect()
        } else {
            Vec::new()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_listed_once_and_is_svg() {
        let mut names: Vec<_> = Icon::ALL.iter().map(|i| i.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Icon::ALL.len());
        for i in Icon::ALL {
            let doc = std::str::from_utf8(i.document()).unwrap();
            assert!(doc.trim_start().starts_with("<svg"));
            assert!(Assets.load(&i.path()).unwrap().is_some());
        }
    }
}
