use std::{collections::VecDeque, time::Duration};

use crate::queue::QueueSender;
use crate::utils::TimedValue;

/// Buffer specific duration of data before returning first timestamp
pub(crate) struct InputDelayBuffer<T: TimedValue> {
    buffer: VecDeque<T>,
    size: Duration,
    ready: bool,
    end: bool,
}

impl<T: TimedValue> InputDelayBuffer<T> {
    pub fn new(size: Duration) -> Self {
        Self {
            buffer: VecDeque::new(),
            size,
            ready: false,
            end: false,
        }
    }

    pub fn write(&mut self, item: T) {
        self.buffer.push_back(item);
        if !self.ready {
            let first_ts = self.buffer.iter().find_map(|i| i.timestamp_range());
            let last_ts = self.buffer.iter().rev().find_map(|i| i.timestamp_range());
            if let (Some(first), Some(last)) = (first_ts, last_ts) {
                self.ready = last.1.abs_diff(first.0) > self.size;
            }
        }
    }

    pub fn read(&mut self) -> Option<T> {
        match self.ready {
            true => self.buffer.pop_front(),
            false => None,
        }
    }

    pub fn mark_end(&mut self) {
        self.ready = true;
        self.end = true;
    }

    pub fn is_done(&self) -> bool {
        self.end && self.buffer.is_empty()
    }
}

/// Holds items until the buffer accumulates its configured span, then forwards
/// 1-for-1 (blocking send) into the queue track sender. PTS passes through
/// untouched: capture PTS is already `sync_point`-relative, the track's
/// `Pts(buffer_duration)` offset does the shifting. Exits when `receiver` is
/// dropped (capture inputs have no EOS).
pub(crate) fn spawn_delay_repacking_thread<T: TimedValue + Send + 'static>(
    thread_name: String,
    receiver: crossbeam_channel::Receiver<T>,
    mut buffer: InputDelayBuffer<T>,
    sender: QueueSender<T>,
) {
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            for item in receiver.into_iter() {
                buffer.write(item);
                while let Some(item) = buffer.read() {
                    if sender.send(item).is_err() {
                        tracing::debug!("Failed to send media. Channel closed.");
                        return;
                    }
                }
            }
        })
        .unwrap();
}
