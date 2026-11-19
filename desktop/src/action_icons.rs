use gpui::{prelude::*, *};
use gpui_component::{Disableable, Sizable, button::*};

macro_rules! action_icons {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        #[derive(Clone, Copy)]
        pub enum ActionIcon { $($variant),+ }

        impl ActionIcon {
            fn path(self) -> &'static str {
                match self { $(Self::$variant => concat!("action/", $name, ".svg")),+ }
            }
        }

        pub fn asset(path: &str) -> Option<&'static [u8]> {
            match path {
                $(concat!("action/", $name, ".svg") => Some(include_bytes!(concat!("../assets/action-icons/", $name, ".svg"))),)+
                _ => None,
            }
        }
    };
}

action_icons! {
    Sidebar => "sidebar",
    Plus => "plus",
    Settings => "settings",
    Language => "language",
    Handoff => "handoff",
    Agents => "agents",
    Folder => "folder",
    Copy => "copy",
    Close => "close",
    Chevron => "chevron",
    Model => "model",
    Refresh => "refresh",
    Clock => "clock",
    Pause => "pause",
    Play => "play",
    Terminal => "terminal",
    Keyboard => "keyboard",
    PanelRight => "panel-right",
    Window => "window",
    Power => "power",
    Brand => "brand",
    Search => "search",
    Repair => "repair",
    Info => "info",
}

impl ActionIcon {
    pub fn draw(self, color: u32, size: f32) -> Svg {
        svg()
            .path(self.path())
            .size(px(size))
            .flex_shrink_0()
            .text_color(rgb(color))
    }
    pub fn button(
        self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        color: u32,
    ) -> Button {
        Button::new(id)
            .ghost()
            .small()
            .compact()
            .size(px(28.))
            .cursor_pointer()
            .child(self.draw(color, 16.))
            .tooltip(label)
    }
}

pub trait IconButtonExt {
    fn icon_disabled(self, disabled: bool) -> Self;
}

impl IconButtonExt for Button {
    fn icon_disabled(self, disabled: bool) -> Self {
        // The native button's disabled foreground does not tint explicit SVG colors or brand images.
        self.disabled(disabled)
            .when(disabled, |button| button.opacity(0.5).cursor_default())
    }
}
