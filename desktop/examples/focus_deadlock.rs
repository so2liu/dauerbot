//! Native regression for issue #56. Run with scripts/focus-deadlock-smoke.py.
//! Uses no Dauerbot backend, configuration, project, or terminal.
//! Popup-to-normal sequence: https://github.com/zed-industries/zed/issues/42821.
#[cfg(target_os = "macos")]
fn main() {
    use gpui::{AppContext, Application, EmptyView, WindowKind, WindowOptions};
    use std::time::Duration;

    Application::new().run(|cx| {
        cx.open_window(
            WindowOptions {
                kind: WindowKind::PopUp,
                ..Default::default()
            },
            |_, cx| cx.new(|_| EmptyView),
        )
        .expect("create isolated popup");
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let normal = cx
                .update(|cx| {
                    println!("FOCUS_REENTRANCY_BEGIN");
                    let normal = cx
                        .open_window(WindowOptions::default(), |_, cx| cx.new(|_| EmptyView))
                        .expect("open normal window from popup");
                    println!("FOCUS_REENTRANCY_RETURNED");
                    cx.activate(true);
                    normal
                        .update(cx, |_, window, _| window.activate_window())
                        .expect("activate normal window");
                    normal
                })
                .expect("native popup-to-normal activation");
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            cx.update(|cx| {
                normal
                    .update(cx, |_, window, _| {
                        // A background CLI launch need not win foreground focus.
                        // The watchdog checks that native activation returns and
                        // this main-thread callback still runs.
                        println!("FOCUS_NORMAL_ACTIVE={}", window.is_window_active());
                    })
                    .expect("normal window remains alive");
                println!("FOCUS_EVENT_LOOP_ALIVE");
                cx.quit();
            })
            .expect("event-loop heartbeat");
        })
        .detach();
    });
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This regression requires a macOS graphical login session.");
    std::process::exit(1);
}
