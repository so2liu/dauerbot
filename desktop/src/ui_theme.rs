//! Explicit local appearance; no daemon settings or native CLI palette changes.
use gpui::{App, rgb};
use gpui_component::{Theme, ThemeColor, ThemeMode};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    #[default]
    Light,
    Dark,
}
impl<'de> Deserialize<'de> for Appearance {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(if value.as_str() == Some("dark") {
            Self::Dark
        } else {
            Self::Light
        })
    }
}
impl Appearance {
    pub fn palette(self) -> Palette {
        match self {
            Self::Light => Palette {
                asset_surface: 0xffffff,
                foreground: 0x1c1c21,
                muted: 0x595963,
                line: 0xd7d7df,
                accent: 0x4338ca,
                canvas: 0xf4f4f6,
                surface: 0xffffff,
                pill: 0xf1f1f4,
                hover: 0xe9e9ee,
                selected: 0xe5e3fa,
                selected_text: 0x1c1c21,
                success: 0x14653b,
                warning: 0x825000,
                error: 0xa83232,
                warning_surface: 0xfff1ce,
                on_accent: 0xffffff,
            },
            Self::Dark => Palette {
                asset_surface: 0xffffff,
                foreground: 0xededf1,
                muted: 0xb9b9c1,
                line: 0x555561,
                accent: 0xa5b4fc,
                canvas: 0x1a1a1e,
                surface: 0x232327,
                pill: 0x2d2d33,
                hover: 0x36363d,
                selected: 0x33345a,
                selected_text: 0xededf1,
                success: 0x6fd99b,
                warning: 0xf2b544,
                error: 0xf28b82,
                warning_surface: 0x3f3220,
                on_accent: 0x14142a,
            },
        }
    }
}

/// RGB tokens are shared by custom drawing, native controls and contrast tests.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Un-tinted SVG brand assets keep a stable light plate.
    pub asset_surface: u32,
    pub foreground: u32,
    pub muted: u32,
    pub line: u32,
    pub accent: u32,
    pub canvas: u32,
    pub surface: u32,
    pub pill: u32,
    pub hover: u32,
    pub selected: u32,
    pub selected_text: u32,
    pub success: u32,
    pub warning: u32,
    pub error: u32,
    pub warning_surface: u32,
    pub on_accent: u32,
}
impl Palette {
    pub fn native_colors(self, mut colors: ThemeColor) -> ThemeColor {
        macro_rules! set { ($value:expr; $($field:ident),+ $(,)?) => { $(colors.$field = rgb($value).into();)+ }; }
        set!(self.canvas; background, sidebar, tab_bar, title_bar, scrollbar);
        set!(self.surface; popover, list, table, group_box, tiles, accordion);
        set!(self.foreground; foreground, popover_foreground, sidebar_foreground, secondary_foreground, tab_foreground, table_head_foreground, group_box_foreground);
        set!(self.muted; muted_foreground, description_list_label_foreground, scrollbar_thumb, scrollbar_thumb_hover);
        set!(self.pill; muted, secondary, tab, table_head, list_head, list_even, table_even, description_list_label, skeleton, tab_bar_segmented);
        set!(self.hover; secondary_hover, secondary_active, list_hover, table_hover, accordion_hover);
        set!(self.line; border, input, sidebar_border, table_row_border, title_bar_border, window_border, slider_bar, switch);
        set!(self.accent; primary, primary_hover, primary_active, ring, caret, link, link_hover, link_active, list_active_border, table_active_border, drag_border, sidebar_primary, progress_bar, slider_thumb);
        set!(self.on_accent; primary_foreground, sidebar_primary_foreground, success_foreground, warning_foreground, danger_foreground, info_foreground);
        set!(self.selected; selection, accent, list_active, table_active, tab_active, sidebar_accent, drop_target);
        set!(self.selected_text; accent_foreground, tab_active_foreground, sidebar_accent_foreground);
        set!(self.success; success, success_hover, success_active, green, bullish);
        set!(self.warning; warning, warning_hover, warning_active, yellow);
        set!(self.error; danger, danger_hover, danger_active, red, bearish);
        set!(self.accent; info, info_hover, info_active, blue);
        colors
    }
}
pub fn palette(cx: &App) -> Palette {
    if Theme::global(cx).is_dark() {
        Appearance::Dark
    } else {
        Appearance::Light
    }
    .palette()
}
pub fn apply(appearance: Appearance, cx: &mut App) {
    Theme::change(
        match appearance {
            Appearance::Light => ThemeMode::Light,
            Appearance::Dark => ThemeMode::Dark,
        },
        None,
        cx,
    );
    let theme = Theme::global_mut(cx);
    theme.colors = appearance.palette().native_colors(theme.colors);
    theme.font_family = ".AppleSystemUIFont".into();
    // Preserve REM geometry; UI text continues to use explicit pixel sizes.
    theme.font_size = gpui::px(16.);
    cx.refresh_windows();
}

#[cfg(test)]
mod tests {
    use super::{Appearance, Palette};
    fn luminance(rgb: u32) -> f64 {
        fn linear(v: u32) -> f64 {
            let v = v as f64 / 255.;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * linear((rgb >> 16) & 255)
            + 0.7152 * linear((rgb >> 8) & 255)
            + 0.0722 * linear(rgb & 255)
    }
    fn contrast(a: u32, b: u32) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }
    fn check_palette(p: Palette) {
        for background in [
            p.canvas,
            p.surface,
            p.pill,
            p.hover,
            p.selected,
            p.warning_surface,
        ] {
            for text in [p.foreground, p.muted, p.success, p.warning, p.error] {
                assert!(
                    contrast(text, background) >= 4.5,
                    "normal text {text:06x} on {background:06x}: {}",
                    contrast(text, background)
                );
            }
            assert!(contrast(p.accent, background) >= 4.5, "link text/focus ring contrast");
        }
        assert!(contrast(p.selected_text, p.selected) >= 4.5);
        assert!(contrast(0x000000, p.asset_surface) >= 4.5);
        for fill in [p.accent, p.success, p.warning, p.error] {
            assert!(
                contrast(p.on_accent, fill) >= 4.5,
                "native filled control contrast"
            );
        }
        let native = p.native_colors(Default::default());
        assert_eq!(native.foreground, gpui::rgb(p.foreground).into());
        assert_eq!(native.background, gpui::rgb(p.canvas).into());
        assert_eq!(native.primary_foreground, gpui::rgb(p.on_accent).into());
        assert_eq!(native.primary, gpui::rgb(p.accent).into());
        assert_eq!(native.accent_foreground, gpui::rgb(p.selected_text).into());
        assert_eq!(native.accent, gpui::rgb(p.selected).into());
        assert_eq!(native.success, gpui::rgb(p.success).into());
        assert_eq!(native.warning, gpui::rgb(p.warning).into());
        assert_eq!(native.danger, gpui::rgb(p.error).into());
        for foreground in [native.success_foreground, native.warning_foreground, native.danger_foreground] {
            assert_eq!(foreground, gpui::rgb(p.on_accent).into());
        }
    }
    #[test]
    fn both_palettes_meet_wcag_aa_text_and_nontext_thresholds() {
        assert_eq!(contrast(0x000000, 0xffffff), 21.);
        assert_eq!(contrast(0x123456, 0x123456), 1.);
        check_palette(Appearance::Light.palette());
        check_palette(Appearance::Dark.palette());
    }
}
