//! Native terminal transport for one existing legacy HERDR execution.
use base64::Engine;
use gpui::{AppContext, Context, Entity};
use gpui_terminal::{
    MAX_SCROLL_STEPS, TerminalConfig, TerminalFrame, TerminalScroll, TerminalView,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

#[derive(Clone, Deserialize)]
pub struct Descriptor {
    pub project_id: String,
    pub generation: String,
    #[serde(default)]
    pub terminal: String,
    #[serde(default)]
    pub terminal_id: String,
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub herdr: String,
    #[serde(default)]
    pub program: String,
    #[serde(default = "default_session")]
    pub session: String,
    #[serde(default)]
    pub config_path: String,
}
fn default_session() -> String {
    "dauerbot".into()
}

enum Control {
    Input(Vec<u8>),
    Resize(usize, usize),
    Scroll(TerminalScroll),
}

pub struct Bridge {
    pub descriptor: Descriptor,
    pub view: Entity<TerminalView>,
    pub controlling: bool,
    pub notice: String,
    notices: mpsc::Receiver<String>,
    closed: Arc<AtomicBool>,
    mode: Arc<AtomicBool>,
}
impl Bridge {
    pub fn build<T: 'static>(mut descriptor: Descriptor, cx: &mut Context<T>) -> Self {
        if descriptor.herdr.is_empty() {
            descriptor.herdr.clone_from(&descriptor.program);
        }
        if descriptor.terminal.is_empty() {
            descriptor.terminal.clone_from(&descriptor.terminal_id);
        }
        let (tx, commands) = mpsc::sync_channel(128);
        let (output, reader) = flume::bounded(8);
        let (notices_tx, notices) = mpsc::channel();
        let closed = Arc::new(AtomicBool::new(false));
        let mode = Arc::new(AtomicBool::new(false));
        let stop = closed.clone();
        let desired_mode = mode.clone();
        let target = descriptor.clone();
        let errors = notices_tx.clone();
        let input_notices = notices_tx.clone();
        thread::spawn(move || {
            if let Err(error) = supervise(target, commands, output, notices_tx, stop, desired_mode)
            {
                let _ = errors.send(error.to_string());
            }
        });
        let input = StreamWriter {
            tx: tx.clone(),
            notices: input_notices,
        };
        let resize = tx.clone();
        let scroll = tx.clone();
        let view = cx.new(|cx| {
            TerminalView::from_frames(
                input,
                reader,
                TerminalConfig {
                    font_family: "Menlo".into(),
                    font_size: gpui::px(13.0),
                    padding: gpui::Edges::all(gpui::px(10.0)),
                    ..Default::default()
                },
                cx,
            )
            .with_resize_callback(move |cols, rows| {
                let _ = resize.try_send(Control::Resize(cols, rows));
            })
            .with_scroll_callback(move |event| {
                let _ = scroll.try_send(Control::Scroll(event));
            })
        });
        Self {
            descriptor,
            view,
            controlling: false,
            notice: String::new(),
            notices,
            closed,
            mode,
        }
    }
    pub fn set_control(&mut self, active: bool, cx: &mut gpui::App) {
        self.controlling = active;
        // Mode changes cannot be dropped behind a burst of keyboard input.
        self.mode.store(active, Ordering::Release);
        self.view
            .update(cx, |view, cx| view.set_writable(active, cx));
    }
    pub fn receive(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.notices.try_recv() {
            self.notice = message;
            changed = true;
        }
        changed
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}

// StreamWriter never blocks GPUI waiting for a socket or subprocess.
struct StreamWriter {
    tx: mpsc::SyncSender<Control>,
    notices: mpsc::Sender<String>,
}
impl Write for StreamWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > 1024 * 1024 {
            let _ = self.notices.send("Paste is larger than 1 MiB".into());
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Paste is larger than 1 MiB",
            ));
        }
        self.tx
            .try_send(Control::Input(bytes.to_vec()))
            .map_err(|_| {
                let _ = self
                    .notices
                    .send("Terminal input is busy; try again.".into());
                io::Error::new(io::ErrorKind::WouldBlock, "Terminal input is busy")
            })?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Connection {
    child: Child,
    input: Option<ChildStdin>,
    reader: Option<thread::JoinHandle<()>>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(input) = &mut self.input {
            let _ = writeln!(input, "{}", json!({"type":"terminal.release"}));
        }
        self.input.take();
        // This child is an observe/control bridge, never the hosted Agent.
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Each reconnect starts a fresh full frame. Finish publishing the old
        // stream first, so a delayed delta cannot corrupt the new baseline.
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
fn open(
    target: &Descriptor,
    control: bool,
    cols: usize,
    rows: usize,
    output: flume::Sender<TerminalFrame>,
    notices: mpsc::Sender<String>,
) -> io::Result<Connection> {
    let mut command = Command::new(&target.herdr);
    for key in [
        "HC_SESSION",
        "HC_TERMINAL_ID",
        "HC_TERMINAL_SOCKET",
        "HC_SOCKET",
        "HERDR_ENV",
        "HERDR_SOCKET_PATH",
        "HERDR_PANE_ID",
        "HERDR_TERMINAL_ID",
        "HERDR_SESSION",
    ] {
        command.env_remove(key);
    }
    command.args([
        "--session",
        &target.session,
        "terminal",
        "session",
        if control { "control" } else { "observe" },
        if target.pane_id.is_empty() {
            &target.terminal
        } else {
            &target.pane_id
        },
        "--cols",
        &cols.to_string(),
        "--rows",
        &rows.to_string(),
    ]);
    if control {
        command.arg("--takeover");
    }
    if !target.config_path.is_empty() {
        command.env("HERDR_CONFIG_PATH", &target.config_path);
    }
    command
        .stdin(if control {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("HERDR terminal stream is unavailable"))?;
    if let Some(stderr) = child.stderr.take() {
        let messages = notices.clone();
        thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.take(8192).read_to_string(&mut text);
            if !text.trim().is_empty() {
                let _ = messages.send(text.trim().to_owned());
            }
        });
    }
    let reader = thread::spawn(move || {
        let mut source = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match Read::by_ref(&mut source)
                .take(8 * 1024 * 1024)
                .read_line(&mut line)
            {
                Ok(0) => break,
                Ok(_) if !line.ends_with('\n') => {
                    let _ = notices.send("Terminal frame exceeded the size limit".into());
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = notices.send(error.to_string());
                    break;
                }
            }
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if frame["type"] == "terminal.closed" {
                let _ =
                    notices.send("Terminal connection closed. Reconnect to view it again.".into());
                break;
            }
            match decode_frame(&frame) {
                Ok(Some(frame)) => {
                    if output.send(frame).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = notices.send(error);
                    break;
                }
            }
        }
    });
    let input = child.stdin.take();
    Ok(Connection {
        child,
        input,
        reader: Some(reader),
    })
}
fn supervise(
    target: Descriptor,
    commands: mpsc::Receiver<Control>,
    output: flume::Sender<TerminalFrame>,
    notices: mpsc::Sender<String>,
    closed: Arc<AtomicBool>,
    mode: Arc<AtomicBool>,
) -> io::Result<()> {
    let (mut cols, mut rows) = (80, 24);
    let mut controlling = false;
    let mut pending_resize = None;
    let mut connection = open(&target, false, cols, rows, output.clone(), notices.clone())?;
    while !closed.load(Ordering::Acquire) {
        let requested_mode = mode.load(Ordering::Acquire);
        if requested_mode != controlling {
            drop(connection);
            controlling = requested_mode;
            connection = open(
                &target,
                controlling,
                cols,
                rows,
                output.clone(),
                notices.clone(),
            )?;
            let _ = notices.send(String::new());
        }
        if let Some(status) = connection.child.try_wait()? {
            let _ = notices.send(format!(
                "Terminal connection ended ({status}). Use Reconnect to reopen it."
            ));
            break;
        }
        if pending_resize
            .is_some_and(|at: std::time::Instant| at.elapsed() >= Duration::from_millis(150))
        {
            pending_resize = None;
            if let Some(input) = &mut connection.input {
                writeln!(
                    input,
                    "{}",
                    json!({"type":"terminal.resize","cols":cols,"rows":rows})
                )?;
                input.flush()?;
            } else {
                drop(connection);
                connection = open(&target, false, cols, rows, output.clone(), notices.clone())?;
            }
        }
        match commands.recv_timeout(Duration::from_millis(100)) {
            Ok(Control::Resize(c, r)) => {
                cols = c.clamp(2, 1000);
                rows = r.clamp(1, 500);
                pending_resize = Some(std::time::Instant::now());
            }
            Ok(Control::Input(bytes)) if controlling && mode.load(Ordering::Acquire) => {
                if let Some(input) = &mut connection.input {
                    writeln!(
                        input,
                        "{}",
                        json!({"type":"terminal.input","bytes":base64::engine::general_purpose::STANDARD.encode(bytes)})
                    )?;
                    input.flush()?;
                }
            }
            Ok(Control::Scroll(event))
                if controlling && mode.load(Ordering::Acquire) && pending_resize.is_none() =>
            {
                if let Some(input) = &mut connection.input {
                    write_scroll(input, event, (cols, rows), &mode)?;
                }
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn write_scroll(
    input: &mut impl Write,
    event: TerminalScroll,
    viewport: (usize, usize),
    controlling: &AtomicBool,
) -> io::Result<()> {
    // Ignore queued gestures hit-tested against a frame predating a resize.
    if event.size != viewport
        || usize::from(event.column) >= viewport.0
        || usize::from(event.row) >= viewport.1
    {
        return Ok(());
    }
    // HERDR 0.9.5 applies `lines` only to host scrollback. Its mouse and 1007
    // branches send one native event, so preserve magnitude as bounded unit events.
    // Let HERDR inspect source modes; the local repaint emulator cannot do that.
    let command = json!({
        "type": "terminal.scroll",
        "direction": if event.lines > 0 { "up" } else { "down" },
        "lines": 1,
        "source": "wheel",
        "column": event.column,
        "row": event.row,
        "modifiers": event.modifiers,
    });
    for _ in 0..event.lines.unsigned_abs().min(MAX_SCROLL_STEPS as u32) {
        if !controlling.load(Ordering::Acquire) {
            break;
        }
        writeln!(input, "{command}")?;
    }
    input.flush()
}

fn decode_frame(frame: &Value) -> Result<Option<TerminalFrame>, String> {
    if frame["type"] != "terminal.frame" {
        return Ok(None);
    }
    if frame["encoding"] != "ansi" {
        return Err("Unsupported terminal frame encoding".into());
    }
    let cols = frame["width"]
        .as_u64()
        .filter(|n| (1..=1000).contains(n))
        .ok_or("Invalid terminal frame width")?;
    let rows = frame["height"]
        .as_u64()
        .filter(|n| (1..=500).contains(n))
        .ok_or("Invalid terminal frame height")?;
    let encoded = frame["bytes"]
        .as_str()
        .ok_or("Terminal frame has no output")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid terminal frame output")?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("Terminal frame exceeds 4 MiB".into());
    }
    Ok(Some(TerminalFrame {
        bytes,
        size: Some((cols as usize, rows as usize)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_terminal::{GpuiEventProxy, TerminalState};

    #[test]
    fn native_wheel_units_preserve_geometry_and_never_forward_without_control() {
        let event = TerminalScroll {
            lines: -20,
            column: 17,
            row: 9,
            size: (80, 24),
            modifiers: 7,
        };
        let mode = AtomicBool::new(false);
        let mut output = Vec::new();
        write_scroll(&mut output, event, (80, 24), &mode).unwrap();
        assert!(
            output.is_empty(),
            "Observation must not inject wheel or keys"
        );
        mode.store(true, Ordering::Release);
        write_scroll(&mut output, event, (100, 30), &mode).unwrap();
        assert!(
            output.is_empty(),
            "A stale frame must not target resized cells"
        );
        write_scroll(&mut output, event, (80, 24), &mode).unwrap();
        let commands: Vec<Value> = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(
            commands.len(),
            20,
            "Application wheel count must survive legacy HERDR"
        );
        assert!(commands.iter().all(|command| command
            == &json!({
                "type": "terminal.scroll", "direction": "down", "lines": 1,
                "source": "wheel", "column": 17, "row": 9, "modifiers": 7
            })));
        output.clear();
        write_scroll(
            &mut output,
            TerminalScroll {
                lines: i32::MAX,
                ..event
            },
            (80, 24),
            &mode,
        )
        .unwrap();
        assert_eq!(
            output.iter().filter(|byte| **byte == b'\n').count(),
            MAX_SCROLL_STEPS as usize
        );

        struct ReleaseAfterFirst<'a> {
            bytes: Vec<u8>,
            mode: &'a AtomicBool,
        }
        impl Write for ReleaseAfterFirst<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                if bytes.contains(&b'\n') {
                    self.mode.store(false, Ordering::Release);
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = ReleaseAfterFirst {
            bytes: Vec::new(),
            mode: &mode,
        };
        write_scroll(&mut writer, event, (80, 24), &mode).unwrap();
        assert_eq!(
            writer.bytes.iter().filter(|byte| **byte == b'\n').count(),
            1,
            "Releasing control must stop the remainder of a wheel burst"
        );
    }

    #[test]
    fn rendered_frame_resizes_before_wide_unicode_is_parsed() {
        let (events, _) = mpsc::channel();
        let mut terminal = TerminalState::new(4, 3, GpuiEventProxy::new(events));
        let frame = decode_frame(
            &json!({"type":"terminal.frame","encoding":"ansi","width":12,"height":3,
            "bytes":base64::engine::general_purpose::STANDARD.encode("hello世界") }),
        )
        .unwrap()
        .unwrap();
        terminal.process_frame(&frame.bytes, frame.size);
        terminal.with_term(|term| {
            assert_eq!(
                term.grid().cursor.point.line.0,
                0,
                "A frame parsed at the old width wraps into another row"
            );
            assert_eq!(term.grid().cursor.point.column.0, 9);
        });
    }

    #[test]
    fn malformed_terminal_frames_do_not_enter_the_emulator() {
        assert!(
            decode_frame(
                &json!({"type":"terminal.frame","encoding":"ansi","width":0,"height":3,"bytes":""})
            )
            .is_err()
        );
        assert!(decode_frame(&json!({"type":"terminal.frame","encoding":"ansi","width":80,"height":24,"bytes":"not base64"})).is_err());
        assert!(
            decode_frame(&json!({"type":"terminal.closed"}))
                .unwrap()
                .is_none()
        );
    }
}
