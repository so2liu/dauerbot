//! Isolated native terminal acceptance fixture. No daemon, HERDR, real CLI,
//! project, credentials, or user configuration is opened. Only the explicitly
//! supplied input-recording file is written.
use gpui::{prelude::*, *};
use gpui_component::{Root, Sizable, button::*};
use gpui_terminal::{TerminalConfig, TerminalView};
use std::{
    fs::File,
    io::{self, Write},
    sync::{Arc, Mutex},
};

struct RecordingInput(Arc<Mutex<File>>);
impl Write for RecordingInput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        writeln!(
            self.0.lock().unwrap(),
            "{}",
            serde_json::json!({"bytes":bytes})
        )?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

struct Fixture {
    terminal: Entity<TerminalView>,
    controlling: bool,
}
impl Render for Fixture {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xf5f5f5))
            .child(
                div()
                    .p_3()
                    .flex()
                    .gap_3()
                    .items_center()
                    .child(
                        Button::new("take-input")
                            .small()
                            .label(if self.controlling {
                                "Release input"
                            } else {
                                "Take input"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controlling = !this.controlling;
                                this.terminal.update(cx, |terminal, cx| {
                                    terminal.set_writable(this.controlling, cx)
                                });
                                cx.notify();
                            })),
                    )
                    .child(if self.controlling {
                        "Input captured to the private fixture file"
                    } else {
                        "Observer: terminal writes are disabled"
                    }),
            )
            .child(div().flex_1().min_h_0().child(self.terminal.clone()))
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("provide an empty private input-recording path");
    let file =
        File::create_new(path).expect("create fixture input recording without overwriting files");
    let writer = Arc::new(Mutex::new(file));
    let mut screen = String::new();
    for line in 1..=40 {
        screen.push_str(&format!("history row {line:02}\r\n"));
    }
    screen.push_str("\x1b[2J\x1b[H\x1b[?2004h\x1b[?25h\x1b[5 q");
    screen.push_str("\x1b[1mDauerbot native terminal acceptance\x1b[0m\r\n\r\n");
    screen.push_str("\x1b[31mANSI red\x1b[32m  green\x1b[34m  blue\x1b[0m\r\n");
    screen.push_str("\x1b[38;2;255;128;64mTrue RGB orange\x1b[0m\r\n");
    screen.push_str("\x1b[7m inverse \x1b[0m  \x1b[2mdim\x1b[0m  \x1b[3mitalic\x1b[0m  \x1b[4munderline spaces   \x1b[0m\r\n");
    screen.push_str(
        "\x1b[48;2;30;90;120mWide background: 界界\x1b[0m  combining: e\u{301}  中文\r\n\r\n",
    );
    screen.push_str("1. Click Take input, then Return without clicking terminal cells.\r\n");
    screen.push_str("2. Press Option+M, Ctrl+Left, and type Chinese using IME.\r\n");
    screen.push_str("3. Paste multiline text; observe bracketed-paste recording.\r\n");
    screen.push_str("4. Scroll upward to view numbered history; no backend exists.\r\n\r\n> ");
    Application::new().run(move |cx| {
        gpui_component::init(cx);
        let bounds = Bounds::centered(None, size(px(900.), px(580.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Dauerbot Isolated Terminal QA".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let terminal = cx.new(|cx| {
                    TerminalView::new(
                        RecordingInput(writer),
                        io::Cursor::new(screen.into_bytes()),
                        TerminalConfig {
                            font_family: "Menlo".into(),
                            font_size: px(13.),
                            ..Default::default()
                        },
                        cx,
                    )
                });
                let fixture = cx.new(|_| Fixture {
                    terminal,
                    controlling: false,
                });
                cx.new(|cx| Root::new(fixture, window, cx))
            },
        )
        .expect("open isolated terminal fixture window");
        cx.activate(true);
    });
}
