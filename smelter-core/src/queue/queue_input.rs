use std::{
    collections::BTreeMap,
    ops::DerefMut,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender, bounded};
use smelter_render::{Frame, InputId};
use tracing::info;

use crate::{
    event::EventEmitter,
    queue::{
        QueueContext,
        audio_input::AudioQueueInput,
        side_channel::{AudioSideChannel, VideoSideChannel},
        utils::PauseState,
        video_input::VideoQueueInput,
    },
    types::Ref,
};

use crate::prelude::*;

/// Maximum number of tracks waiting to be started. `QueueInput::queue_new_track`
/// blocks until there is room.
const MAX_PENDING_TRACKS: usize = 5;

enum TrackEndCondition {
    AllTracks,
    Pts(TrackOffset),
}

struct PendingTrack {
    video: Option<VideoQueueInput>,
    audio: Option<AudioQueueInput>,
    track_offset: TrackOffset,
    end_condition: TrackEndCondition,
}

/// A media item that can enter a queue track, with the per-track policies the
/// sender applies before the item reaches the track's channel.
pub(crate) trait QueueItem: Clone + Sized {
    type SideChannel;

    /// Drop or truncate content at or past the track duration.
    fn clip(self, duration: Duration) -> Option<Self>;
    fn shift_pts(&mut self, delay: Duration);
    fn forward(&self, side_channel: &Self::SideChannel);
}

/// Per-input, per-modality side-channel delivery order. Tracks decode
/// concurrently, but subscribers expect one PTS-ordered stream per input:
/// only the delivery head forwards live; later tracks buffer their early
/// items (decode backpressure keeps that to the first few) and flush when
/// the head's sender drops, i.e. when its content is fully forwarded.
/// Tracks discarded before reaching the head never forward.
struct SideChannelLane<T: QueueItem>(Mutex<LaneState<T>>);

struct LaneState<T: QueueItem> {
    next_seq: u64,
    head: u64,
    head_channel: Option<T::SideChannel>,
    queued: BTreeMap<u64, QueuedLane<T>>,
}

struct QueuedLane<T: QueueItem> {
    channel: T::SideChannel,
    buffer: Vec<T>,
    closed: bool,
    abandoned: bool,
}

impl<T: QueueItem> SideChannelLane<T> {
    fn new() -> Self {
        Self(Mutex::new(LaneState {
            next_seq: 0,
            head: 0,
            head_channel: None,
            queued: BTreeMap::new(),
        }))
    }

    fn register(&self, channel: T::SideChannel) -> u64 {
        let mut state = self.0.lock().unwrap();
        let seq = state.next_seq;
        state.next_seq += 1;
        if seq == state.head {
            state.head_channel = Some(channel);
        } else {
            state.queued.insert(
                seq,
                QueuedLane { channel, buffer: Vec::new(), closed: false, abandoned: false },
            );
        }
        seq
    }

    fn forward(&self, seq: u64, item: &T) {
        let mut state = self.0.lock().unwrap();
        if seq == state.head {
            if let Some(channel) = &state.head_channel {
                item.forward(channel);
            }
        } else if let Some(queued) = state.queued.get_mut(&seq)
            && !queued.abandoned
        {
            queued.buffer.push(item.clone());
        }
    }

    fn close(&self, seq: u64) {
        let mut state = self.0.lock().unwrap();
        if seq != state.head {
            if let Some(queued) = state.queued.get_mut(&seq) {
                queued.closed = true;
            }
            return;
        }
        state.head_channel = None;
        loop {
            state.head += 1;
            let head = state.head;
            let Some(queued) = state.queued.remove(&head) else {
                return;
            };
            if queued.abandoned {
                continue;
            }
            for item in &queued.buffer {
                item.forward(&queued.channel);
            }
            if !queued.closed {
                state.head_channel = Some(queued.channel);
                return;
            }
        }
    }

    /// The queue discarded every track behind the current one; their items
    /// must never reach subscribers.
    fn abandon_queued(&self) {
        let mut state = self.0.lock().unwrap();
        for queued in state.queued.values_mut() {
            queued.abandoned = true;
            queued.buffer.clear();
        }
    }
}

/// Sender half of a track's media channel. Items are clipped to the track
/// duration, shifted by the side-channel delay and forwarded to the side
/// channel as they arrive from the input, before the (bounded) channel send —
/// so side-channel subscribers observe media on arrival, independent of when
/// the queue consumes the track.
pub(crate) struct QueueSender<T: QueueItem> {
    sender: crossbeam_channel::Sender<T>,
    delay: Duration,
    duration: Option<Duration>,
    lane: Option<(Arc<SideChannelLane<T>>, u64)>,
}

impl<T: QueueItem> QueueSender<T> {
    pub fn send(&self, item: T) -> Result<(), crossbeam_channel::SendError<T>> {
        match self.prepare(item) {
            Some(item) => self.sender.send(item),
            None => Ok(()),
        }
    }

    #[allow(dead_code)]
    pub fn try_send(&self, item: T) -> Result<(), crossbeam_channel::TrySendError<T>> {
        match self.prepare(item) {
            Some(item) => self.sender.try_send(item),
            None => Ok(()),
        }
    }

    fn prepare(&self, item: T) -> Option<T> {
        let mut item = match self.duration {
            Some(duration) => item.clip(duration)?,
            None => item,
        };
        item.shift_pts(self.delay);
        if let Some((lane, seq)) = &self.lane {
            lane.forward(*seq, &item);
        }
        Some(item)
    }
}

impl<T: QueueItem> Drop for QueueSender<T> {
    fn drop(&mut self) {
        if let Some((lane, seq)) = &self.lane {
            lane.close(*seq);
        }
    }
}

impl<T: QueueItem> std::fmt::Debug for QueueSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueSender").finish()
    }
}

pub(super) struct InnerQueueInput {
    queue_ctx: QueueContext,
    event_emitter: Arc<EventEmitter>,
    input_ref: Ref<InputId>,

    video: Option<VideoQueueInput>,
    audio: Option<AudioQueueInput>,
    track_offset: TrackOffset,
    end_condition: TrackEndCondition,
    queued_end: Option<TrackOffset>,
    pause_state: PauseState,

    pending_sender: crossbeam_channel::Sender<PendingTrack>,
    pending_receiver: crossbeam_channel::Receiver<PendingTrack>,
    required: bool,
    video_side_channel: Option<VideoSideChannel>,
    audio_side_channel: Option<AudioSideChannel>,
    video_lane: Option<Arc<SideChannelLane<Frame>>>,
    audio_lane: Option<Arc<SideChannelLane<InputAudioSamples>>>,
    side_channel_delay: Duration,
}

impl InnerQueueInput {
    fn maybe_start_next_track(&mut self) {
        let pts = self.queue_ctx.effective_last_pts();
        let video_eos_sent = self.video.as_ref().map(|v| v.eos_sent()).unwrap_or(true);
        let audio_eos_sent = self.audio.as_ref().map(|a| a.eos_sent()).unwrap_or(true);
        let ended = match &self.end_condition {
            TrackEndCondition::AllTracks => video_eos_sent && audio_eos_sent,
            TrackEndCondition::Pts(end) => end
                .get()
                .map_or(video_eos_sent && audio_eos_sent, |end| pts >= end),
        };
        if ended {
            self.replace_track()
        }
    }

    /// Replace current track with the next pending, do nothing if there is no pending
    fn replace_track(&mut self) {
        let Ok(pending) = self.pending_receiver.try_recv() else {
            return;
        };
        info!(input_id=%self.input_ref, "Push track to queue");

        self.video = pending.video;
        self.audio = pending.audio;
        self.track_offset = pending.track_offset;
        self.end_condition = pending.end_condition;
        if self.pause_state.is_paused() {
            let pts = self.queue_ctx.effective_last_pts();
            if let Some(v) = self.video.as_mut() {
                // trigger enqueue so new track can start with a frame
                match self.queue_ctx.start_pts.value() {
                    Some(start_pts) => {
                        v.is_ready_for_pts(pts, start_pts);
                    }
                    None => v.drop_old_frames_before_start(),
                };
                v.pause()
            }
            if let Some(a) = self.audio.as_mut() {
                a.pause()
            }
            self.pause_state.reset(pts);
        }
    }

    fn new_pending_track(
        &mut self,
        opts: QueueTrackOptions,
        timing: Option<QueueTrackTiming>,
    ) -> (
        PendingTrack,
        Option<QueueSender<Frame>>,
        Option<QueueSender<InputAudioSamples>>,
    ) {
        let input_id = self.input_ref.to_string();
        info!(?opts, input_id, "Create new queue track");
        let duration = timing.map(|timing| match timing {
            QueueTrackTiming::Finite(duration) | QueueTrackTiming::Continuation(duration) => {
                duration
            }
        });
        let (track_offset, offset_from_start) = match timing {
            Some(QueueTrackTiming::Continuation(_)) => (
                self.queued_end
                    .clone()
                    .expect("continuation requires a preceding finite track"),
                None,
            ),
            _ => match opts.offset {
                QueueTrackOffset::None => (TrackOffset::default(), None),
                QueueTrackOffset::Pts(duration) => (TrackOffset::new(duration), None),
                QueueTrackOffset::FromStart(duration) => (TrackOffset::default(), Some(duration)),
            },
        };
        self.queued_end = duration.map(|duration| track_offset.after(duration));
        let end_condition = duration
            .map(|duration| {
                TrackEndCondition::Pts(track_offset.after(duration + self.side_channel_delay))
            })
            .unwrap_or(TrackEndCondition::AllTracks);
        let (video_input, video_sender) = if opts.video {
            let (video_input, video_sender) = VideoQueueInput::new(
                &self.queue_ctx,
                &self.event_emitter,
                &self.input_ref,
                self.required,
                offset_from_start,
                track_offset.clone(),
                duration.is_some(),
            );
            let lane = self.video_lane.as_ref().map(|lane| {
                let channel = self
                    .video_side_channel
                    .as_ref()
                    .expect("lane exists only with a side channel")
                    .with_track_offset(&track_offset);
                (lane.clone(), lane.register(channel))
            });
            let sender = QueueSender {
                sender: video_sender,
                delay: self.side_channel_delay,
                duration,
                lane,
            };
            (Some(video_input), Some(sender))
        } else {
            (None, None)
        };
        let (audio_input, audio_sender) = if opts.audio {
            let (audio_input, audio_sender) = AudioQueueInput::new(
                &self.queue_ctx,
                &self.event_emitter,
                &self.input_ref,
                self.required,
                offset_from_start,
                track_offset.clone(),
                duration.is_some(),
            );
            let lane = self.audio_lane.as_ref().map(|lane| {
                let channel = self
                    .audio_side_channel
                    .as_ref()
                    .expect("lane exists only with a side channel")
                    .with_track_offset(&track_offset);
                (lane.clone(), lane.register(channel))
            });
            let sender = QueueSender {
                sender: audio_sender,
                delay: self.side_channel_delay,
                duration,
                lane,
            };
            (Some(audio_input), Some(sender))
        } else {
            (None, None)
        };
        (
            PendingTrack {
                video: video_input,
                audio: audio_input,
                track_offset,
                end_condition,
            },
            video_sender,
            audio_sender,
        )
    }

    /// Remember the start pts. On resume shift offset by the pts difference:
    /// - If input already started, add to track offset pts diff
    /// - If input did not started, track_offset was not initialized yet
    pub fn pause(&mut self) {
        if self.pause_state.is_paused() {
            return;
        }
        // zero before queue start
        let pts = self.queue_ctx.effective_last_pts();
        self.pause_state.pause(pts);
        if let Some(v) = self.video.as_mut() {
            v.pause()
        }
        if let Some(a) = self.audio.as_mut() {
            a.pause()
        }
    }

    pub fn resume(&mut self) {
        if !self.pause_state.is_paused() {
            return;
        }
        let pts = self.queue_ctx.effective_last_pts();
        if let Some(pause_time) = self.pause_state.resume(pts) {
            self.track_offset.map_add(pause_time);
        }
        if let Some(v) = self.video.as_mut() {
            v.resume()
        }
        if let Some(a) = self.audio.as_mut() {
            a.resume()
        }
    }
}

#[derive(Debug)]
pub(crate) enum QueueTrackOffset {
    None,
    /// Effectively offset from sync point
    Pts(Duration),
    /// Offset from start point
    FromStart(Duration),
}

#[derive(Clone, Copy)]
pub(crate) enum QueueTrackTiming {
    Finite(Duration),
    Continuation(Duration),
}

#[derive(Debug)]
pub(crate) struct QueueTrackOptions {
    pub video: bool,
    pub audio: bool,
    pub offset: QueueTrackOffset,
}

#[derive(Clone)]
pub(crate) struct QueueInput(Arc<Mutex<InnerQueueInput>>);

#[derive(Clone)]
pub(crate) struct WeakQueueInput(Weak<Mutex<InnerQueueInput>>);

impl std::fmt::Debug for WeakQueueInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeakQueueInput").finish()
    }
}

#[derive(Debug, Default, Clone)]
pub enum InputSideChannel<T> {
    #[default]
    Disabled,
    UnixSocket,
    Native(Sender<T>),
}

impl<T> InputSideChannel<T> {
    pub fn native(capacity: usize) -> (Self, Receiver<T>) {
        let (sender, receiver) = bounded(capacity);
        (Self::Native(sender), receiver)
    }
}

impl<T> PartialEq for InputSideChannel<T> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Disabled, Self::Disabled) | (Self::UnixSocket, Self::UnixSocket) => true,
            (Self::Native(left), Self::Native(right)) => left.same_channel(right),
            _ => false,
        }
    }
}

impl<T> From<bool> for InputSideChannel<T> {
    fn from(enabled: bool) -> Self {
        match enabled {
            true => Self::UnixSocket,
            false => Self::Disabled,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct QueueInputOptions {
    pub required: bool,
    pub audio_side_channel: InputSideChannel<InputAudioSamples>,
    pub video_side_channel: InputSideChannel<Frame>,
    pub side_channel_delay: Duration,
}

impl QueueInput {
    pub fn new(ctx: &Arc<PipelineCtx>, input_ref: &Ref<InputId>, opts: QueueInputOptions) -> Self {
        let socket_dir = ctx.queue_ctx.side_channel_socket_dir.as_deref();
        let video_side_channel = match (&opts.video_side_channel, socket_dir) {
            (InputSideChannel::UnixSocket, Some(dir)) => VideoSideChannel::new(ctx, input_ref, dir),
            (InputSideChannel::Native(sender), _) => {
                Some(VideoSideChannel::native(ctx, sender.clone()))
            }
            _ => None,
        };
        let audio_side_channel = match (&opts.audio_side_channel, socket_dir) {
            (InputSideChannel::UnixSocket, Some(dir)) => AudioSideChannel::new(ctx, input_ref, dir),
            (InputSideChannel::Native(sender), _) => {
                Some(AudioSideChannel::native(ctx, sender.clone()))
            }
            _ => None,
        };
        Self::new_inner(
            ctx.queue_ctx.clone(),
            ctx.event_emitter.clone(),
            input_ref,
            opts,
            video_side_channel,
            audio_side_channel,
        )
    }

    pub(super) fn new_inner(
        queue_ctx: QueueContext,
        event_emitter: Arc<EventEmitter>,
        input_ref: &Ref<InputId>,
        opts: QueueInputOptions,
        video_side_channel: Option<VideoSideChannel>,
        audio_side_channel: Option<AudioSideChannel>,
    ) -> Self {
        let (pending_sender, pending_receiver) = crossbeam_channel::bounded(MAX_PENDING_TRACKS);
        Self(Arc::new(Mutex::new(InnerQueueInput {
            queue_ctx,
            event_emitter,
            input_ref: input_ref.clone(),

            video: None,
            audio: None,
            track_offset: TrackOffset::default(),
            end_condition: TrackEndCondition::AllTracks,
            queued_end: None,

            pending_sender,
            pending_receiver,

            required: opts.required,
            pause_state: PauseState::new(),
            video_lane: video_side_channel.as_ref().map(|_| Arc::new(SideChannelLane::new())),
            audio_lane: audio_side_channel.as_ref().map(|_| Arc::new(SideChannelLane::new())),
            video_side_channel,
            audio_side_channel,
            side_channel_delay: opts.side_channel_delay,
        })))
    }

    /// Blocks (without holding the inner mutex) if `MAX_PENDING_TRACKS` tracks
    /// are already pending, until some of them are dequeued.
    pub fn queue_new_track(
        &self,
        opts: QueueTrackOptions,
    ) -> (
        Option<QueueSender<Frame>>,
        Option<QueueSender<InputAudioSamples>>,
    ) {
        self.queue_new_track_inner(opts, None)
    }

    pub(crate) fn queue_new_timed_track(
        &self,
        opts: QueueTrackOptions,
        timing: QueueTrackTiming,
    ) -> (
        Option<QueueSender<Frame>>,
        Option<QueueSender<InputAudioSamples>>,
    ) {
        self.queue_new_track_inner(opts, Some(timing))
    }

    fn queue_new_track_inner(
        &self,
        opts: QueueTrackOptions,
        timing: Option<QueueTrackTiming>,
    ) -> (
        Option<QueueSender<Frame>>,
        Option<QueueSender<InputAudioSamples>>,
    ) {
        if !opts.video && !opts.audio {
            return (None, None);
        }
        let mut guard = self.0.lock().unwrap();
        if matches!(timing, Some(QueueTrackTiming::Finite(_))) {
            while guard.pending_receiver.try_recv().is_ok() {}
            if let Some(lane) = &guard.video_lane {
                lane.abandon_queued();
            }
            if let Some(lane) = &guard.audio_lane {
                lane.abandon_queued();
            }
        }
        let (track, video_sender, audio_sender) = guard.new_pending_track(opts, timing);
        let pending_sender = guard.pending_sender.clone();
        drop(guard);
        // receiver is owned by InnerQueueInput, so send can't fail while
        // we hold the Arc
        let _ = pending_sender.send(track);
        (video_sender, audio_sender)
    }

    pub fn abort_old_track(&self) {
        self.0.lock().unwrap().replace_track()
    }

    pub fn pause(&self) {
        self.0.lock().unwrap().pause();
    }

    pub fn resume(&self) {
        self.0.lock().unwrap().resume();
    }

    pub fn downgrade(&self) -> WeakQueueInput {
        WeakQueueInput(Arc::downgrade(&self.0))
    }

    pub(super) fn maybe_start_next_track(&self) {
        self.0.lock().unwrap().maybe_start_next_track();
    }
}

impl WeakQueueInput {
    pub(super) fn video<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut VideoQueueInput) -> R,
    {
        let arc = self.0.upgrade()?;
        let mut inner = arc.lock().unwrap();
        let video = inner.video.as_mut()?;
        Some(f(video))
    }

    pub(super) fn audio<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut AudioQueueInput) -> R,
    {
        let arc = self.0.upgrade()?;
        let mut inner = arc.lock().unwrap();
        let audio = inner.audio.as_mut()?;
        Some(f(audio))
    }

    pub(crate) fn upgrade(&self) -> Option<QueueInput> {
        self.0.upgrade().map(QueueInput)
    }
}

#[derive(Default, Clone)]
pub(super) struct TrackOffset(Arc<Mutex<Option<Duration>>>, Duration);

impl TrackOffset {
    pub fn new(value: Duration) -> Self {
        Self(Arc::new(Mutex::new(Some(value))), Duration::ZERO)
    }

    fn after(&self, duration: Duration) -> Self {
        Self(self.0.clone(), self.1 + duration)
    }

    pub fn get(&self) -> Option<Duration> {
        self.0.lock().unwrap().map(|base| base + self.1)
    }

    pub fn get_or_init(&self, offset: Duration) -> Duration {
        let mut base = self.0.lock().unwrap();
        *base.get_or_insert(offset.saturating_sub(self.1)) + self.1
    }

    pub fn map_add(&self, duration: Duration) {
        if let Some(offset) = self.0.lock().unwrap().deref_mut() {
            *offset += duration
        }
    }
}
