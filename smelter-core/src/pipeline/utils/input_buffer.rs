use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use smelter_render::Frame;
use tracing::{debug, info, trace};

use crate::{InputBufferOptions, PipelineCtx};

pub(crate) trait TimedValue {
    fn pts(&self) -> Duration;
}

impl TimedValue for Frame {
    fn pts(&self) -> Duration {
        self.pts
    }
}

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
        if !self.ready
            && let (Some(first), Some(last)) = (self.buffer.front(), self.buffer.back())
        {
            self.ready = last.pts().abs_diff(first.pts()) > self.size
        }
    }

    pub fn read(&mut self) -> Option<T> {
        self.buffer.pop_front()
    }

    pub fn mark_end(&mut self) {
        self.ready = true;
        self.end = true;
    }

    pub fn is_done(&self) -> bool {
        self.end && self.buffer.is_empty()
    }
}

#[derive(Clone)]
pub(crate) enum InputBuffer {
    // If input is required or has an offset do not add any buffer.
    //
    // - If input is required, buffering is not necessary.
    // - If offset is in the future then extra buffering is not necessary
    // - If offset in in the past, it already causing drops
    None,
    Const { size: Duration },
    LatencyOptimized(Arc<Mutex<LatencyOptimizedBuffer>>),
    Adaptive(Arc<Mutex<AdaptiveBuffer>>),
}

impl fmt::Debug for InputBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "None"),
            Self::Const { size: buffer } => {
                f.debug_struct("Const").field("buffer", buffer).finish()
            }
            Self::LatencyOptimized(_) => f.debug_struct("LatencyOptimized").finish(),
            Self::Adaptive(_) => f.debug_struct("Adaptive").finish(),
        }
    }
}

impl InputBuffer {
    pub fn new(ctx: &PipelineCtx, opts: InputBufferOptions) -> Self {
        match opts {
            InputBufferOptions::None => InputBuffer::None,
            InputBufferOptions::Const { size } => InputBuffer::Const { size },
            InputBufferOptions::LatencyOptimized { reference_time } => {
                InputBuffer::LatencyOptimized(Arc::new(Mutex::new(LatencyOptimizedBuffer::new(
                    ctx,
                    reference_time,
                ))))
            }
            InputBufferOptions::Adaptive => {
                InputBuffer::Adaptive(Arc::new(Mutex::new(AdaptiveBuffer::new(ctx))))
            }
        }
    }

    pub fn recalculate_buffer(&self, pts: Duration) {
        match self {
            InputBuffer::LatencyOptimized(buffer) => buffer.lock().unwrap().recalculate_buffer(pts),
            InputBuffer::Adaptive(buffer) => buffer.lock().unwrap().recalculate_buffer(pts),
            _ => (),
        }
    }

    pub fn size(&self) -> Duration {
        match self {
            InputBuffer::None => Duration::ZERO,
            InputBuffer::Const { size: buffer } => *buffer,
            InputBuffer::LatencyOptimized(buffer) => buffer.lock().unwrap().dynamic_buffer,
            InputBuffer::Adaptive(buffer) => buffer.lock().unwrap().dynamic_buffer,
        }
    }
}

/// Buffer intended for low latency inputs, if input stream is not delivered on time,
/// it quickly increases. However, when buffer is stable for some time it starts to shrink to
/// minimize the latency.
pub(crate) struct LatencyOptimizedBuffer {
    reference_time: Instant,
    state: LatencyOptimizedBufferState,
    dynamic_buffer: Duration,

    /// effective_buffer = next_pts - queue_sync_point.elapsed()
    /// Estimates how much time packet has to reach the queue.

    /// If effective_buffer is above this threshold for a period of time, aggressively shrink
    /// the buffer.
    shrink_threshold_1: Duration,
    /// If effective_buffer is above this threshold for a period of time, shrink the buffer
    /// quickly
    shrink_threshold_2: Duration,
    /// If effective_buffer is above this threshold for a period of time, slowly shrink the buffer.
    max_desired_buffer: Duration,
    /// If effective_buffer is below this value, slowly increase the buffer with every packet.
    min_desired_buffer: Duration,
    /// If effective_buffer is below this threshold, aggressively and immediately increase the buffer.
    grow_threshold: Duration,
}

impl LatencyOptimizedBuffer {
    fn new(ctx: &PipelineCtx, reference_time: Instant) -> Self {
        // As a result for default numbers if effective_buffer is between 80ms and 240ms, no
        // adjustment/optimization will be triggered
        let grow_threshold = ctx.default_buffer_duration;
        let min_desired_buffer = grow_threshold + ctx.default_buffer_duration;
        let max_desired_buffer = min_desired_buffer + ctx.default_buffer_duration;
        let shrink_threshold_2 = max_desired_buffer + Duration::from_millis(400);
        let shrink_threshold_1 = shrink_threshold_2 + Duration::from_millis(400);
        Self {
            reference_time,
            dynamic_buffer: ctx.default_buffer_duration,
            state: LatencyOptimizedBufferState::Ok,

            grow_threshold,
            min_desired_buffer,
            max_desired_buffer,
            shrink_threshold_1,
            shrink_threshold_2,
        }
    }

    /// pts is a value relative to time elapsed from sync_point.
    /// If `reference_time.elapsed() == pts` that means that effective buffer is zero
    fn recalculate_buffer(&mut self, pts: Duration) {
        // Increment duration is larger than decrement, because when buffer is too small
        // we don't have much time to adjust to a difference.
        const INCREMENT_DURATION: Duration = Duration::from_micros(500);
        const SMALL_DECREMENT_DURATION: Duration = Duration::from_micros(200);
        const LARGE_DECREMENT_DURATION: Duration = Duration::from_micros(500);

        // Duration that defines at what point we can consider state stable enough
        // to consider shrinking the buffer
        const STABLE_STATE_DURATION: Duration = Duration::from_secs(10);

        let reference_time = self.reference_time;
        let next_pts = pts + self.dynamic_buffer;
        trace!(
            effective_buffer=?next_pts.saturating_sub(reference_time.elapsed()),
            dynamic_buffer=?self.dynamic_buffer,
            ?pts
        );

        if next_pts > reference_time.elapsed() + self.shrink_threshold_1 {
            let first_pts = self.state.set_too_large(next_pts);
            if next_pts.saturating_sub(first_pts) > STABLE_STATE_DURATION {
                self.dynamic_buffer = self
                    .dynamic_buffer
                    .saturating_sub(self.dynamic_buffer / 1000);
            }
        } else if next_pts > reference_time.elapsed() + self.shrink_threshold_2 {
            let first_pts = self.state.set_too_large(next_pts);
            if next_pts.saturating_sub(first_pts) > STABLE_STATE_DURATION {
                self.dynamic_buffer = self.dynamic_buffer.saturating_sub(LARGE_DECREMENT_DURATION);
            }
        } else if next_pts > reference_time.elapsed() + self.max_desired_buffer {
            let first_pts = self.state.set_too_large(next_pts);
            if next_pts.saturating_sub(first_pts) > STABLE_STATE_DURATION {
                self.dynamic_buffer = self.dynamic_buffer.saturating_sub(SMALL_DECREMENT_DURATION);
            }
        } else if next_pts > reference_time.elapsed() + self.min_desired_buffer {
            self.state.set_ok();
        } else if next_pts > reference_time.elapsed() + self.grow_threshold {
            trace!(
                old=?self.dynamic_buffer,
                new=?self.dynamic_buffer + INCREMENT_DURATION,
                "Increase latency optimized buffer"
            );
            self.state.set_too_small();
            self.dynamic_buffer += INCREMENT_DURATION;
        } else {
            let new_buffer =
                (reference_time.elapsed() + self.max_desired_buffer).saturating_sub(pts);
            debug!(
                old=?self.dynamic_buffer,
                new=?new_buffer,
                "Increase latency optimized buffer (force)"
            );
            self.state.set_too_small();
            // adjust buffer so:
            // pts + self.dynamic_buffer == self.sync_point.elapsed() + self.max_desired_buffer
            self.dynamic_buffer = new_buffer
        }
    }
}

enum LatencyOptimizedBufferState {
    Ok,
    TooSmall,
    TooLarge { first_pts: Duration },
}

impl LatencyOptimizedBufferState {
    fn set_too_large(&mut self, pts: Duration) -> Duration {
        match &self {
            LatencyOptimizedBufferState::TooLarge { first_pts } => *first_pts,
            _ => {
                *self = LatencyOptimizedBufferState::TooLarge { first_pts: pts };
                pts
            }
        }
    }

    fn set_too_small(&mut self) {
        *self = LatencyOptimizedBufferState::TooSmall
    }

    fn set_ok(&mut self) {
        *self = LatencyOptimizedBufferState::Ok
    }
}

pub(crate) struct AdaptiveBuffer {
    sync_point: Option<Instant>,
    desired_buffer: Duration,
    dynamic_buffer: Duration,
    min_buffer: Duration,
}

impl AdaptiveBuffer {
    fn new(ctx: &PipelineCtx) -> Self {
        Self {
            sync_point: None,
            desired_buffer: ctx.default_buffer_duration,
            min_buffer: Duration::min(Duration::from_millis(20), ctx.default_buffer_duration),
            dynamic_buffer: ctx.default_buffer_duration,
        }
    }

    fn recalculate_buffer(&mut self, pts: Duration) {
        const INCREMENT_DURATION: Duration = Duration::from_micros(100);

        let sync_point = self.sync_point.get_or_insert_with(Instant::now);

        let next_pts = pts + self.dynamic_buffer;
        if next_pts > sync_point.elapsed() + self.desired_buffer {
            // ok
        } else if next_pts > sync_point.elapsed() + self.min_buffer {
            debug!(
                old=?self.dynamic_buffer,
                new=?self.dynamic_buffer + INCREMENT_DURATION,
                "Increase adaptive buffer"
            );
            self.dynamic_buffer += INCREMENT_DURATION;
        } else {
            let new_buffer = (sync_point.elapsed() + self.desired_buffer).saturating_sub(pts);
            info!(
                old=?self.dynamic_buffer,
                new=?new_buffer,
                "Increase adaptive buffer (force)"
            );
            self.dynamic_buffer = new_buffer
        }
    }
}
