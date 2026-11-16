//! Native menu-bar entry and a GPUI project popover. The daemon outlives all windows.
use super::{Desktop, INK, LINE, MUTED, Quit, project_status};
use anyhow::{Context as _, Result};
use gpui::{prelude::*, *};
use gpui_component::{Root, button::*};
use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{NSView, NSWindow};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

struct StatusBar {
    icon: Option<TrayIcon>,
    desktop: Entity<Desktop>,
    main_window: WindowHandle<Root>,
    popover: Option<WindowHandle<Root>>,
}
impl Global for StatusBar {}

pub fn install(
    desktop: Entity<Desktop>,
    main_window: WindowHandle<Root>,
    cx: &mut App,
) -> Result<()> {
    cx.set_global(StatusBar {
        icon: None,
        desktop,
        main_window,
        popover: None,
    });
    let icon = TrayIconBuilder::new()
        .with_id("dauerbot-projects")
        .with_tooltip("Dauerbot")
        .with_icon(status_icon()?)
        .with_icon_as_template(true)
        .with_menu_on_left_click(false)
        .build()
        .context("Cannot create the macOS menu-bar icon")?;
    cx.global_mut::<StatusBar>().icon = Some(icon);
    let (sender, receiver) = flume::unbounded();
    TrayIconEvent::set_event_handler(Some(move |event| {
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left | MouseButton::Right,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            let _ = sender.send(());
        }
    }));
    cx.spawn(async move |cx| {
        while receiver.recv_async().await.is_ok() {
            if cx.update(toggle_popover).is_err() {
                break;
            }
        }
    })
    .detach();
    Ok(())
}

fn native_window(window: &Window) -> Option<Retained<NSWindow>> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // GPUI's AppKit handle is the live NSView. This is only used on its main thread.
    unsafe { handle.ns_view.cast::<NSView>().as_ref().window() }
}

pub fn hide_window(window: &Window) {
    if let Some(window) = native_window(window) {
        window.orderOut(None);
    }
}

pub fn open_main(project: Option<String>, page: usize, cx: &mut App) {
    let Some(status) = cx.try_global::<StatusBar>() else {
        return;
    };
    let desktop = status.desktop.clone();
    let main_window = status.main_window;
    let popover = cx.global_mut::<StatusBar>().popover.take();
    if let Some(popover) = popover {
        let _ = popover.update(cx, |_, window, _| window.remove_window());
    }
    desktop.update(cx, |desktop, cx| {
        if let Some(project) = project {
            desktop.select_project(project, cx);
        }
        desktop.go(page);
        cx.notify();
    });
    cx.activate(true);
    let _ = main_window.update(cx, |_, window, _| window.activate_window());
}

fn toggle_popover(cx: &mut App) {
    let previous = cx.global_mut::<StatusBar>().popover.take();
    if let Some(window) = previous
        && window
            .update(cx, |_, window, _| window.remove_window())
            .is_ok()
    {
        return;
    }
    let desktop = cx.global::<StatusBar>().desktop.clone();
    let language = desktop.read(cx).setup.language;
    let count = desktop.read(cx).state["projects"]
        .as_array()
        .map_or(0, Vec::len);
    let height = (222. + count.clamp(1, 7) as f32 * 58.).min(640.);
    let result = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(0.), px(0.)),
                size(px(324.), px(height)),
            ))),
            titlebar: None,
            kind: WindowKind::PopUp,
            show: false,
            focus: false,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        },
        |window, cx| {
            let panel = cx.new(|cx| MenuPanel::new(desktop, window, cx));
            cx.new(|cx| Root::new(panel, window, cx))
        },
    );
    match result {
        Ok(window) => {
            cx.global_mut::<StatusBar>().popover = Some(window);
            cx.activate(true);
            let _ = window.update(cx, |_, window, cx| {
                anchor_popover(window, cx);
                window.activate_window();
            });
        }
        Err(error) => {
            cx.global::<StatusBar>()
                .desktop
                .clone()
                .update(cx, |desktop, cx| {
                    desktop.notice =
                        format!("{}: {error}", language.text("Cannot open menu-bar panel"));
                    desktop.error = true;
                    cx.notify();
                });
            open_main(None, 6, cx);
        }
    }
}

fn anchor_popover(window: &Window, cx: &App) {
    let Some(marker) = MainThreadMarker::new() else {
        return;
    };
    let Some(status_window) = cx
        .global::<StatusBar>()
        .icon
        .as_ref()
        .and_then(TrayIcon::ns_status_item)
        .and_then(|item| item.button(marker))
        .and_then(|button| button.window())
    else {
        return;
    };
    let Some(native) = native_window(window) else {
        return;
    };
    let anchor = status_window.frame();
    let mut frame = native.frame();
    if let Some(screen) = status_window.screen() {
        let visible = screen.visibleFrame();
        let [left, bottom, width, height] = constrained_popover(
            [frame.size.width, frame.size.height],
            [anchor.origin.x + anchor.size.width / 2., anchor.origin.y],
            [
                visible.origin.x,
                visible.origin.y,
                visible.size.width,
                visible.size.height,
            ],
        );
        frame.origin.x = left;
        frame.origin.y = bottom;
        frame.size.width = width;
        frame.size.height = height;
        // AppKit resize notifications update GPUI's viewport; the project list scrolls.
        native.setFrame_display(frame, false);
    } else {
        let mut top_left = anchor.origin;
        top_left.x += anchor.size.width / 2. - frame.size.width / 2.;
        top_left.y -= 6.;
        native.setFrameTopLeftPoint(top_left);
    }
}

/// Cocoa screen coordinates increase upward. Returns the panel's bottom-left frame.
fn constrained_popover(panel: [f64; 2], anchor: [f64; 2], visible: [f64; 4]) -> [f64; 4] {
    let width = panel[0].min((visible[2] - 16.).max(1.));
    let top = (anchor[1] - 6.).min(visible[1] + visible[3] - 8.);
    let height = panel[1].min((top - visible[1] - 8.).max(1.));
    let left_min = visible[0] + 8.;
    let left_max = (visible[0] + visible[2] - 8. - width).max(left_min);
    let left = (anchor[0] - width / 2.).clamp(left_min, left_max);
    [left, top - height, width, height]
}

struct MenuPanel {
    desktop: Entity<Desktop>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}
impl MenuPanel {
    fn new(desktop: Entity<Desktop>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window);
        let subscriptions = vec![
            cx.observe(&desktop, |_, _, cx| cx.notify()),
            cx.observe_window_activation(window, |_, window, _| {
                if !window.is_window_active() {
                    window.remove_window();
                }
            }),
        ];
        Self {
            desktop,
            focus,
            _subscriptions: subscriptions,
        }
    }
}
impl Render for MenuPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let desktop = self.desktop.read(cx);
        let language = desktop.setup.language;
        let projects = desktop.state["projects"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let connection_error = desktop.connection_error.is_some();
        let independent = desktop.independent_data;
        let mut rows = div()
            .id("menu-projects")
            .min_h_0()
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1();
        for project in projects.iter() {
            let id = project["id"].as_str().unwrap_or("").to_owned();
            rows = rows.child(
                div()
                    .id(SharedString::from(format!("menu-project-{id}")))
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .cursor_pointer()
                    .hover(|row| row.bg(rgb(0xeaf0e3)))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(project["name"].as_str().unwrap_or("Project").to_owned()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(language.text(project_status(project))),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        let id = id.clone();
                        cx.defer(move |cx| open_main(Some(id), 6, cx));
                    })),
            );
        }
        if projects.is_empty() {
            rows = rows.child(
                div()
                    .px_3()
                    .py_5()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(language.text("No projects yet")),
            );
        }
        div()
            .size_full()
            .p_2()
            .rounded_xl()
            .bg(rgb(0xf9fbf6))
            .border_1()
            .border_color(rgb(LINE))
            .font_family(".AppleSystemUIFont")
            .text_color(rgb(INK))
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, window, _| {
                if event.keystroke.key == "escape" {
                    window.remove_window();
                }
            }))
            .child(
                div()
                    .px_3()
                    .py_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Dauerbot"),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(language.text(
                        if connection_error {
                            "Disconnected"
                        } else {
                            "Projects"
                        },
                    ))),
            )
            .when(independent, |panel| {
                panel.child(
                    div()
                        .px_3()
                        .pb_2()
                        .text_xs()
                        .text_color(rgb(0x9a762c))
                        .child(language.text("Independent instance")),
                )
            })
            .child(rows)
            .child(
                div()
                    .mt_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(rgb(LINE))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        Button::new("menu-add-project")
                            .label(language.text("+ Add project"))
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.defer(|cx| open_main(None, 5, cx))),
                            ),
                    )
                    .child(
                        Button::new("menu-open-app")
                            .label(language.text("Open Dauerbot"))
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.defer(|cx| open_main(None, 6, cx))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .pt_1()
                            .child(
                                Button::new("menu-settings")
                                    .label(language.text("Settings"))
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.defer(|cx| open_main(None, 8, cx))
                                    })),
                            )
                            .child(
                                Button::new("menu-quit")
                                    .label(language.text("Quit Dauerbot"))
                                    .on_click(cx.listener(|_, _, window, cx| {
                                        window.remove_window();
                                        cx.defer(|cx| cx.dispatch_action(&Quit));
                                    })),
                            ),
                    ),
            )
    }
}

fn status_icon() -> Result<Icon> {
    let mut rgba = vec![0; 36 * 36 * 4];
    for y in 0..36usize {
        for x in 0..36usize {
            let body = (7..29).contains(&x) && (10..29).contains(&y);
            let border = body && (!(10..26).contains(&x) || !(13..26).contains(&y));
            let antenna = (17..20).contains(&x) && (4..10).contains(&y);
            let eyes = ((12..15).contains(&x) || (21..24).contains(&x)) && (17..20).contains(&y);
            let ears = ((3..7).contains(&x) || (29..33).contains(&x)) && (16..23).contains(&y);
            if border || antenna || eyes || ears {
                rgba[(y * 36 + x) * 4 + 3] = 255;
            }
        }
    }
    Ok(Icon::from_rgba(rgba, 36, 36)?)
}

#[cfg(test)]
mod placement_tests {
    use super::constrained_popover;

    #[test]
    fn tall_project_panel_stays_inside_short_and_offset_displays() {
        for (anchor, visible) in [
            ([1400., 600.], [0., 0., 1440., 580.]),
            ([-10., 580.], [-1280., 0., 1280., 560.]),
            ([1400., -25.], [0., -900., 1440., 875.]),
        ] {
            let [left, bottom, width, height] = constrained_popover([324., 628.], anchor, visible);
            assert!(left >= visible[0] + 8.);
            assert!(left + width <= visible[0] + visible[2] - 8.);
            assert!(bottom >= visible[1] + 8.);
            assert!(bottom + height <= visible[1] + visible[3] - 8.);
            assert!(bottom + height <= anchor[1] - 6.);
            assert_eq!(width, 324.);
        }
        let frame = constrained_popover([324., 628.], [1400., 600.], [0., 0., 1440., 580.]);
        assert!(frame[3] < 628.);
        let frame = constrained_popover([324., 628.], [1400., 920.], [0., 0., 1440., 900.]);
        assert_eq!(frame[3], 628.);
    }
}
