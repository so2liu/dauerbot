//! Ordered, bounded terminal ingestion, adapted from Herdr App's Apache-2.0
//! `output_turn` at 321c9a84. Legacy frames can be several MiB, so split them
//! into the reference core's maximum 16 KiB parser units before time slicing.
use crate::view::TerminalFrame;
use std::time::{Duration, Instant};

pub(crate) const OUTPUT_TURN: Duration = Duration::from_millis(6);
pub(crate) const PARSER_CHUNK: usize = 16 * 1024;
pub(crate) const REDRAW_INTERVAL: Duration = Duration::from_millis(33);
pub(crate) const INTERACTIVE_WINDOW: Duration = Duration::from_millis(200);

#[derive(Default)]
pub(crate) struct FrameIngest {
    pending: Option<TerminalFrame>,
    offset: usize,
}
impl FrameIngest {
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_none()
    }

    pub(crate) fn begin(&mut self, frame: TerminalFrame) {
        assert!(
            self.pending.is_none(),
            "the previous delta must be consumed first"
        );
        self.pending = Some(frame);
        self.offset = 0;
    }

    pub(crate) fn apply_turn(
        &mut self,
        frames: &flume::Receiver<TerminalFrame>,
        budget: Duration,
        mut apply: impl FnMut(&[u8], Option<(usize, usize)>),
    ) {
        let deadline = Instant::now() + budget;
        loop {
            if self.pending.is_none() {
                let Ok(frame) = frames.try_recv() else { break };
                self.begin(frame);
            }
            let frame = self.pending.as_mut().unwrap();
            let end = (self.offset + PARSER_CHUNK).min(frame.bytes.len());
            // Resize exactly once, before the first byte of this source frame.
            // The VTE parser retains UTF-8/escape state between bounded chunks.
            apply(&frame.bytes[self.offset..end], frame.size.take());
            self.offset = end;
            if end == frame.bytes.len() {
                self.pending = None;
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_frames_yield_in_bounded_chunks_without_losing_deltas_or_repeating_resize() {
        let (tx, rx) = flume::unbounded();
        let mut source = vec![b'x'; PARSER_CHUNK * 3 + 7];
        source[PARSER_CHUNK - 1..PARSER_CHUNK + 2].copy_from_slice("界".as_bytes());
        tx.send(TerminalFrame {
            bytes: b"\x1b[31mdelta".to_vec(),
            size: Some((81, 24)),
        })
        .unwrap();
        drop(tx);
        let mut ingest = FrameIngest::default();
        ingest.begin(TerminalFrame {
            bytes: source.clone(),
            size: Some((80, 24)),
        });
        let mut received = Vec::new();
        let mut sizes = Vec::new();
        let mut turns = 0;
        while !ingest.is_empty() || !rx.is_empty() {
            let mut chunks = 0;
            ingest.apply_turn(&rx, Duration::ZERO, |bytes, size| {
                assert!(bytes.len() <= PARSER_CHUNK);
                chunks += 1;
                received.extend_from_slice(bytes);
                if let Some(size) = size {
                    sizes.push(size);
                }
            });
            assert_eq!(
                chunks, 1,
                "a saturated turn overshoots by at most one 16 KiB parser unit"
            );
            turns += 1;
        }
        assert_eq!(turns, 5);
        source.extend_from_slice(b"\x1b[31mdelta");
        assert_eq!(received, source);
        assert_eq!(sizes, vec![(80, 24), (81, 24)]);
        assert!(
            rx.is_disconnected() && ingest.is_empty(),
            "EOF follows the last pending delta chunk"
        );
    }

    #[test]
    fn resize_only_frames_are_applied_once_in_order() {
        let (tx, rx) = flume::unbounded();
        tx.send(TerminalFrame {
            bytes: vec![],
            size: Some((100, 30)),
        })
        .unwrap();
        drop(tx);
        let mut ingest = FrameIngest::default();
        let mut sizes = Vec::new();
        ingest.apply_turn(&rx, Duration::ZERO, |bytes, size| {
            assert!(bytes.is_empty());
            sizes.push(size);
        });
        ingest.apply_turn(&rx, Duration::ZERO, |_, _| {
            panic!("completed resize was repeated")
        });
        assert_eq!(sizes, vec![Some((100, 30))]);
    }

    #[test]
    fn parser_state_survives_escape_and_unicode_boundaries_between_turns() {
        use crate::{GpuiEventProxy, TerminalState};
        use alacritty_terminal::{
            index::{Column, Line},
            vte::ansi::{Color, Rgb},
        };
        let (events, _) = std::sync::mpsc::channel();
        let mut terminal = TerminalState::new(4, 2, GpuiEventProxy::new(events));
        let mut bytes = vec![0; PARSER_CHUNK - 2];
        bytes.extend_from_slice(b"\x1b[38;2;18;52;86m");
        bytes.resize(PARSER_CHUNK * 2 - 1, 0);
        bytes.extend_from_slice("界e\u{301}".as_bytes());
        let (tx, rx) = flume::unbounded();
        tx.send(TerminalFrame {
            bytes: b"\x1b[31mX".to_vec(),
            size: None,
        })
        .unwrap();
        drop(tx);
        let mut ingest = FrameIngest::default();
        ingest.begin(TerminalFrame {
            bytes,
            size: Some((80, 24)),
        });
        while !ingest.is_empty() || !rx.is_empty() {
            ingest.apply_turn(&rx, Duration::ZERO, |bytes, size| {
                terminal.process_frame(bytes, size)
            });
        }
        terminal.with_term(|term| {
            let row = &term.grid()[Line(0)];
            assert_eq!(row[Column(0)].c, '界');
            assert_eq!(
                row[Column(0)].fg,
                Color::Spec(Rgb {
                    r: 18,
                    g: 52,
                    b: 86
                })
            );
            assert_eq!(row[Column(2)].c, 'e');
            assert_eq!(row[Column(2)].zerowidth(), Some(['\u{301}'].as_slice()));
            assert_eq!(row[Column(3)].c, 'X');
        });
    }
}
