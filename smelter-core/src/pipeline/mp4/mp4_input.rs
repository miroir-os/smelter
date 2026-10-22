use std::{
    cmp::Ordering,
    fs::File,
    path::Path,
    sync::Arc,
    thread::{self, JoinHandle, ThreadId},
    time::Duration,
};

use crossbeam_channel::{Receiver, unbounded};
use smelter_render::{AlphaPlane, FrameData, error::ErrorStack};
use tracing::{Level, debug, error, span, trace, warn};

use crate::{
    pipeline::{
        decoder::{
            DecoderThreadHandle, EncodedInputEvent,
            decoder_thread_audio::{AudioDecoderThread, AudioDecoderThreadOptions},
            decoder_thread_video::{VideoDecoderThread, VideoDecoderThreadOptions},
            fdk_aac, ffmpeg_h264, vulkan_h264,
        },
        input::Input,
        mp4::reader::{DecoderOptions, Mp4FileReader, Track},
        utils::H264AvccToAnnexB,
    },
    queue::{QueueInput, QueueSender, QueueTrackOffset, QueueTrackOptions, WeakQueueInput},
    utils::{
        InitializableThread, ShutdownCondition,
        channel::{SendTimeoutError, Sender},
    },
};

use crate::prelude::*;

/// Max channel capacity between input and decoder
const MAX_CHUNK_BUFFER_DURATION: Duration = Duration::from_secs(2);
/// Min channel capacity between input and decoder
const MIN_CHUNK_BUFFER_DURATION: Duration = Duration::from_millis(200);

/// MP4 input - reads from a local file or downloaded URL, demuxes H.264/AAC tracks,
/// decodes, and feeds frames/samples into the queue. Supports seek, pause, and resume.
///
/// ## Timestamps
///
/// ### On input register
/// - File is opened immediately and tracks are discovered.
/// - With offset (`opts.offset = Some(offset)`)
///   - PTS of first frame should be zero
///   - Register track with `QueueTrackOffset::FromStart(offset)`
/// - Without offset (`opts.offset = None`)
///   - PTS of first frame should be zero
///   - Register track with `QueueTrackOffset::None`
///
/// ### On loop (`opts.should_loop = true`)
/// - When the video reader reaches the end (or the audio reader if there is no
///   video track), a new track is created with `QueueTrackOffset::None` and the
///   remaining in-progress track is aborted.
/// - PTS of first frame starts from zero again (same as initial registration).
///
/// ### Pause / Resume / Seek
/// - Pausing stops queue consumption, reader threads continue running.
/// - Resuming restores queue consumption from where it stopped.
/// - Seeking while paused should change the frame.
/// - On seek
///   - New track is created with `QueueTrackOffset::None`, old tracks are aborted.
///   - PTS of first frame after seek is zero (reader subtracts seek position from
///     file timestamps). For video, reading starts from the nearest keyframe before
///     the seek point (some pre-seek samples are decoded but not presented).
///
/// ### Unsupported scenarios
/// - If ahead of time processing is enabled, initial registration will happen on pts already
///   processed by the queue, but queue will wait and eventually stream will show up, with
///   the portion at the start cut off.
pub struct Mp4Input {
    events_sender: crossbeam_channel::Sender<StateEvent>,
}

impl Mp4Input {
    pub fn seek(&self, position: Duration) {
        if self.events_sender.send(StateEvent::Seek(position)).is_err() {
            debug!("Failed to handle seek event. Channel closed.")
        }
    }

    pub fn pause(&self) {
        if self.events_sender.send(StateEvent::Pause).is_err() {
            debug!("Failed to handle pause event. Channel closed.")
        }
    }

    pub fn resume(&self) {
        if self.events_sender.send(StateEvent::Resume).is_err() {
            debug!("Failed to handle resume event. Channel closed.")
        }
    }
}

impl Mp4Input {
    pub fn new_input(
        ctx: Arc<PipelineCtx>,
        input_ref: Ref<InputId>,
        options: Mp4InputOptions,
    ) -> Result<(Input, InputInitInfo, QueueInput), InputInitError> {
        let source_file = match options.source.clone() {
            Mp4InputSource::Url(url) => Self::download_remote_file(&ctx, &url)?,
            Mp4InputSource::File(path) => Arc::new(SourceFile {
                path,
                remove_on_drop: false,
            }),
        };

        let color_track = Mp4FileReader::from_path(&source_file.path)?.try_new_h264_track(0);
        let video_tracks = match (color_track, options.alpha_track) {
            (None, false) => None,
            (Some(color), false) => Some(VideoTracks::Opaque(color)),
            (color, true) => {
                let alpha = Mp4FileReader::from_path(&source_file.path)?
                    .try_new_h264_track(1)
                    .ok_or(Mp4InputError::NoAlphaTrack)?;
                let color = color.ok_or(Mp4InputError::NoTrack)?;
                if !color.pairs_with(&alpha) {
                    return Err(Mp4InputError::AlphaTrackMismatch.into());
                }
                Some(VideoTracks::WithAlpha {
                    color,
                    alpha: Box::new(alpha),
                })
            }
        };
        let video_duration = video_tracks
            .as_ref()
            .and_then(|tracks| tracks.color().duration());
        let audio_track = Mp4FileReader::from_path(&source_file.path)?.try_new_aac_track();
        let audio_duration = audio_track.as_ref().and_then(|track| track.duration());

        if video_tracks.is_none() && audio_track.is_none() {
            return Err(Mp4InputError::NoTrack.into());
        }

        if let Some(DecoderOptions::H264(_)) = video_tracks
            .as_ref()
            .map(|tracks| tracks.color().decoder_options())
            && options.video_decoders.h264 == Some(VideoDecoderOptions::VulkanH264)
            && !ctx.graphics_context.has_vulkan_decoder_support()
        {
            return Err(InputInitError::DecoderError(
                DecoderInitError::VulkanContextRequiredForVulkanDecoder,
            ));
        }

        let queue_input = QueueInput::new(&ctx, &input_ref, options.queue_options.clone());
        let (video_sender, audio_sender) = queue_input.queue_new_track(QueueTrackOptions {
            video: video_tracks.is_some(),
            audio: audio_track.is_some(),
            offset: match options.offset {
                Some(offset) => QueueTrackOffset::FromStart(offset),
                None => QueueTrackOffset::None,
            },
        });

        // Buffer needs to be smaller than half of the longest track, otherwise
        // reader threads of short looped files finish too far ahead of playback.
        let chunk_buffer_duration = match Option::max(video_duration, audio_duration) {
            Some(duration) => Duration::clamp(
                duration / 2,
                MIN_CHUNK_BUFFER_DURATION,
                MAX_CHUNK_BUFFER_DURATION,
            ),
            None => MAX_CHUNK_BUFFER_DURATION,
        };

        let initial_seek = options.seek;
        let (mut reader, events_sender) = TrackManagerThread::new(
            &ctx,
            &input_ref,
            options,
            source_file,
            chunk_buffer_duration,
            queue_input.downgrade(),
        );

        if let (Some(tracks), Some(sender)) = (video_tracks, video_sender) {
            reader.spawn_video(tracks, sender, initial_seek)?;
        }
        if let (Some(track), Some(sender)) = (audio_track, audio_sender) {
            reader.spawn_audio(track, sender, initial_seek)?;
        }
        std::thread::Builder::new()
            .name("mp4 reader".to_string())
            .spawn(move || {
                reader.run();
            })
            .unwrap();

        Ok((
            Input::Mp4(Self { events_sender }),
            InputInitInfo::Mp4 {
                video_duration,
                audio_duration,
            },
            queue_input,
        ))
    }

    fn download_remote_file(
        ctx: &Arc<PipelineCtx>,
        url: &str,
    ) -> Result<Arc<SourceFile>, Mp4InputError> {
        let file_response = reqwest::blocking::get(url)?;
        let mut file_response = file_response.error_for_status()?;

        let path = ctx
            .download_dir
            .join(format!("smelter-user-file-{}.mp4", rand::random::<u64>()));

        let mut file = std::fs::File::create(&path)?;

        std::io::copy(&mut file_response, &mut file)?;

        Ok(Arc::new(SourceFile {
            path: path.into(),
            remove_on_drop: true,
        }))
    }
}

impl Drop for Mp4Input {
    fn drop(&mut self) {
        if self.events_sender.send(StateEvent::InputShutdown).is_err() {
            error!("Failed to send InputShutdown event. Channel closed")
        }
    }
}

#[derive(Debug)]
enum StateEvent {
    Seek(Duration),
    Pause,
    Resume,
    ThreadFinished(ThreadId),
    InputShutdown,
}

/// The video of an MP4 input: its track, with the track its alpha is coded in.
enum VideoTracks {
    Opaque(Track<File>),
    WithAlpha {
        color: Track<File>,
        alpha: Box<Track<File>>,
    },
}

impl VideoTracks {
    fn color(&self) -> &Track<File> {
        match self {
            VideoTracks::Opaque(color) | VideoTracks::WithAlpha { color, .. } => color,
        }
    }
}

/// `VideoTracks` with the decoders their chunks go to.
enum VideoReading {
    Opaque {
        track: Track<File>,
        decoder: DecoderThreadHandle,
    },
    WithAlpha {
        color: Track<File>,
        alpha: Box<Track<File>>,
        color_decoder: DecoderThreadHandle,
        alpha_decoder: DecoderThreadHandle,
    },
}

#[derive(Clone)]
struct TrackContext {
    event_sender: crossbeam_channel::Sender<StateEvent>,

    _source_file: Arc<SourceFile>,
}

struct TrackManagerThread {
    ctx: Arc<PipelineCtx>,
    input_ref: Ref<InputId>,
    options: Mp4InputOptions,
    events_receiver: Receiver<StateEvent>,
    input_shutdown_condition: ShutdownCondition,
    track_ctx: TrackContext,
    video_thread: Option<(JoinHandle<VideoTracks>, ShutdownCondition)>,
    audio_thread: Option<(JoinHandle<Track<File>>, ShutdownCondition)>,
    chunk_buffer_duration: Duration,
    queue_input: WeakQueueInput,
}

impl TrackManagerThread {
    fn new(
        ctx: &Arc<PipelineCtx>,
        input_ref: &Ref<InputId>,
        options: Mp4InputOptions,
        source_file: Arc<SourceFile>,
        chunk_buffer_duration: Duration,
        queue_input: WeakQueueInput,
    ) -> (Self, crossbeam_channel::Sender<StateEvent>) {
        let (events_sender, events_receiver) = unbounded();

        let track_ctx = TrackContext {
            event_sender: events_sender.clone(),
            _source_file: source_file.clone(),
        };

        (
            Self {
                ctx: ctx.clone(),
                input_ref: input_ref.clone(),
                options,
                events_receiver,
                input_shutdown_condition: ShutdownCondition::default(),
                track_ctx,
                video_thread: None,
                audio_thread: None,
                chunk_buffer_duration,
                queue_input,
            },
            events_sender,
        )
    }

    fn run(mut self) {
        while let Ok(event) = self.events_receiver.recv() {
            debug!(?event, "Received MP4 input life cycle event.");
            match event {
                StateEvent::Pause => {
                    let Some(input) = self.queue_input.upgrade() else {
                        return;
                    };
                    input.pause();
                }
                StateEvent::Resume => {
                    let Some(input) = self.queue_input.upgrade() else {
                        return;
                    };
                    input.resume();
                }
                StateEvent::Seek(seek) => {
                    self.restart_threads(Some(seek));
                }
                StateEvent::ThreadFinished(thread_id) => {
                    // Loop restart is driven by the video track (audio if no video track).
                    let primary_thread = (self
                        .video_thread
                        .as_ref()
                        .map(|(handle, _)| handle.thread()))
                    .or(self
                        .audio_thread
                        .as_ref()
                        .map(|(handle, _)| handle.thread()));
                    let primary_finished =
                        primary_thread.is_some_and(|thread| thread.id() == thread_id);

                    // when not looping do not break because user can still send seek request
                    if self.options.should_loop && primary_finished {
                        self.restart_threads(None);
                    }
                }
                StateEvent::InputShutdown => {
                    self.input_shutdown_condition.mark_for_shutdown();
                    return;
                }
            }
        }
    }

    fn restart_threads(&mut self, seek: Option<Duration>) {
        let (video_sender, audio_sender) = {
            let Some(queue_input) = self.queue_input.upgrade() else {
                return;
            };
            queue_input.queue_new_track(QueueTrackOptions {
                video: self.video_thread.is_some(),
                audio: self.audio_thread.is_some(),
                offset: QueueTrackOffset::None,
            })
        };

        if let Some((_, cond)) = self.video_thread.as_ref() {
            cond.mark_for_shutdown()
        }
        if let Some((_, cond)) = self.audio_thread.as_ref() {
            cond.mark_for_shutdown()
        }

        let video_tracks = self
            .video_thread
            .take()
            .map(|(handle, _)| handle.join().unwrap());
        let audio_track = self
            .audio_thread
            .take()
            .map(|(handle, _)| handle.join().unwrap());

        if let (Some(tracks), Some(sender)) = (video_tracks, video_sender)
            && let Err(err) = self.spawn_video(tracks, sender, seek)
        {
            warn!(
                "Failed to start video thread: {}",
                ErrorStack::new(&err).into_string()
            );
        }
        if let (Some(track), Some(sender)) = (audio_track, audio_sender)
            && let Err(err) = self.spawn_audio(track, sender, seek)
        {
            warn!(
                "Failed to start audio thread: {}",
                ErrorStack::new(&err).into_string()
            );
        }

        // sleep so the new frames have time to go through decoder,
        // only when seeking, we don't want to cut off any remaining
        // frames when looping
        if seek.is_some() {
            // TODO: wait until next track is ready
            thread::sleep(Duration::from_millis(500));
            let Some(queue_input) = self.queue_input.upgrade() else {
                return;
            };
            queue_input.abort_old_track();
        };
    }

    fn spawn_video(
        &mut self,
        tracks: VideoTracks,
        frame_sender: QueueSender<Frame>,
        seek: Option<Duration>,
    ) -> Result<(), InputInitError> {
        let shutdown_condition = self.input_shutdown_condition.child_condition();
        let track_thread = TrackThread {
            ctx: self.track_ctx.clone(),
            shutdown_condition: shutdown_condition.clone(),
            seek,
        };
        let reading = match tracks {
            VideoTracks::Opaque(track) => VideoReading::Opaque {
                decoder: self.spawn_video_decoder_thread(&track, frame_sender)?,
                track,
            },
            VideoTracks::WithAlpha { color, alpha } => {
                let (color_sender, color_frames) = crossbeam_channel::bounded(1);
                let (alpha_sender, alpha_frames) = crossbeam_channel::bounded(1);
                let color_decoder =
                    self.spawn_video_decoder_thread(&color, QueueSender::new(color_sender))?;
                let alpha_decoder =
                    self.spawn_video_decoder_thread(&alpha, QueueSender::new(alpha_sender))?;
                std::thread::Builder::new()
                    .name("mp4 alpha pairing".to_string())
                    .spawn(move || pair_alpha(color_frames, alpha_frames, frame_sender))
                    .unwrap();
                VideoReading::WithAlpha {
                    color,
                    alpha,
                    color_decoder,
                    alpha_decoder,
                }
            }
        };
        let input_id = self.input_ref.to_string();
        let handle = std::thread::Builder::new()
            .name("mp4 reader - video".to_string())
            .spawn(move || {
                let _span = span!(Level::INFO, "MP4 video", input_id = input_id).entered();
                track_thread.read_video(reading)
            })
            .unwrap();
        self.video_thread = Some((handle, shutdown_condition));
        Ok(())
    }

    fn spawn_audio(
        &mut self,
        track: Track<File>,
        samples_sender: QueueSender<InputAudioSamples>,
        seek: Option<Duration>,
    ) -> Result<(), InputInitError> {
        let decoder_handle = self.spawn_audio_decoder_thread(&track, samples_sender)?;

        let shutdown_condition = self.input_shutdown_condition.child_condition();
        let track_thread = TrackThread {
            ctx: self.track_ctx.clone(),
            shutdown_condition: shutdown_condition.clone(),
            seek,
        };
        let input_id = self.input_ref.to_string();
        let handle = std::thread::Builder::new()
            .name("mp4 reader - audio".to_string())
            .spawn(move || {
                let _span = span!(Level::INFO, "MP4 audio", input_id = input_id).entered();
                track_thread.read(track, decoder_handle)
            })
            .unwrap();
        self.audio_thread = Some((handle, shutdown_condition));
        Ok(())
    }

    fn spawn_video_decoder_thread(
        &self,
        track: &Track<File>,
        frame_sender: QueueSender<Frame>,
    ) -> Result<DecoderThreadHandle, InputInitError> {
        let vulkan_supported = self.ctx.graphics_context.has_vulkan_decoder_support();
        let h264_decoder = self.options.video_decoders.h264.unwrap_or({
            match vulkan_supported {
                true => VideoDecoderOptions::VulkanH264,
                false => VideoDecoderOptions::FfmpegH264,
            }
        });
        let handle = match (track.decoder_options(), h264_decoder) {
            (DecoderOptions::H264(h264_config), VideoDecoderOptions::FfmpegH264) => {
                VideoDecoderThread::<ffmpeg_h264::FfmpegH264Decoder, _>::spawn(
                    self.input_ref.clone(),
                    VideoDecoderThreadOptions {
                        ctx: self.ctx.clone(),
                        input_ref: self.input_ref.clone(),
                        transformer: Some(H264AvccToAnnexB::new(h264_config.clone())),
                        frame_sender,
                        input_buffer_size: self.chunk_buffer_duration,
                    },
                )?
            }
            (DecoderOptions::H264(h264_config), VideoDecoderOptions::VulkanH264) => {
                if !vulkan_supported {
                    return Err(InputInitError::DecoderError(
                        DecoderInitError::VulkanContextRequiredForVulkanDecoder,
                    ));
                }
                VideoDecoderThread::<vulkan_h264::VulkanH264Decoder, _>::spawn(
                    self.input_ref.clone(),
                    VideoDecoderThreadOptions {
                        ctx: self.ctx.clone(),
                        input_ref: self.input_ref.clone(),
                        transformer: Some(H264AvccToAnnexB::new(h264_config.clone())),
                        frame_sender,
                        input_buffer_size: self.chunk_buffer_duration,
                    },
                )?
            }
            _ => {
                return Err(Mp4InputError::Unknown("Non H264 decoder options returned.").into());
            }
        };
        Ok(handle)
    }

    fn spawn_audio_decoder_thread(
        &self,
        track: &Track<File>,
        samples_sender: QueueSender<InputAudioSamples>,
    ) -> Result<DecoderThreadHandle, InputInitError> {
        let handle = match track.decoder_options() {
            DecoderOptions::Aac(data) => AudioDecoderThread::<fdk_aac::FdkAacDecoder>::spawn(
                self.input_ref.clone(),
                AudioDecoderThreadOptions {
                    ctx: self.ctx.clone(),
                    input_ref: self.input_ref.clone(),
                    decoder_options: FdkAacDecoderOptions {
                        asc: Some(data.clone()),
                    },
                    samples_sender,
                    input_buffer_size: self.chunk_buffer_duration,
                },
            )?,
            _ => {
                return Err(Mp4InputError::Unknown("Non AAC decoder options returned.").into());
            }
        };
        Ok(handle)
    }
}

struct TrackThread {
    ctx: TrackContext,
    shutdown_condition: ShutdownCondition,
    seek: Option<Duration>,
}

impl TrackThread {
    fn read(self, mut track: Track<File>, decoder: DecoderThreadHandle) -> Track<File> {
        for (chunk, _duration) in track.chunks(self.seek) {
            trace!(pts=?chunk.pts, "MP4 reader produced a chunk.");
            if !self.send(chunk, &decoder) {
                debug!("Failed to send a chunk. Channel closed.");
                break;
            }
        }
        self.finish();
        track
    }

    fn read_video(self, reading: VideoReading) -> VideoTracks {
        match reading {
            VideoReading::Opaque { track, decoder } => {
                VideoTracks::Opaque(self.read(track, decoder))
            }
            VideoReading::WithAlpha {
                mut color,
                mut alpha,
                color_decoder,
                alpha_decoder,
            } => {
                for ((color_chunk, _), (alpha_chunk, _)) in color.chunks_with(&mut alpha, self.seek)
                {
                    trace!(pts=?color_chunk.pts, "MP4 reader produced a chunk with its alpha.");
                    if !self.send(color_chunk, &color_decoder)
                        || !self.send(alpha_chunk, &alpha_decoder)
                    {
                        debug!("Failed to send a chunk. Channel closed.");
                        break;
                    }
                }
                self.finish();
                VideoTracks::WithAlpha { color, alpha }
            }
        }
    }

    fn send(&self, chunk: EncodedInputChunk, decoder: &DecoderThreadHandle) -> bool {
        Self::try_send(
            PipelineEvent::Data(EncodedInputEvent::Chunk(chunk)),
            &decoder.chunk_sender,
            &self.shutdown_condition,
        )
    }

    fn finish(&self) {
        let _ = self
            .ctx
            .event_sender
            .send(StateEvent::ThreadFinished(thread::current().id()));
    }

    fn try_send(
        event: PipelineEvent<EncodedInputEvent>,
        sender: &Sender<PipelineEvent<EncodedInputEvent>>,
        shutdown_condition: &ShutdownCondition,
    ) -> bool {
        let mut event_state = Some(event);
        while let Some(event) = event_state.take() {
            match sender.send_timeout(event, Duration::from_millis(100)) {
                Ok(_) => {
                    if shutdown_condition.should_close() {
                        return false;
                    }
                    return true;
                }
                Err(SendTimeoutError::Timeout(event)) => {
                    event_state = Some(event);
                    if shutdown_condition.should_close() {
                        return false;
                    }
                }
                Err(SendTimeoutError::Disconnected(_)) => return false,
            }
        }
        false
    }
}

struct SourceFile {
    path: Arc<Path>,
    remove_on_drop: bool,
}

impl Drop for SourceFile {
    fn drop(&mut self) {
        if self.remove_on_drop
            && let Err(e) = std::fs::remove_file(&self.path)
        {
            error!("Error while removing the downloaded mp4 file: {e}");
        }
    }
}

/// Forwards each color frame with the alpha frame of the same pts, as one frame. A frame whose
/// partner its decoder lost is dropped; either decoder ending ends the video.
fn pair_alpha(color: Receiver<Frame>, alpha: Receiver<Frame>, sender: QueueSender<Frame>) {
    let (mut color_head, mut alpha_head) = (None, None);
    loop {
        let Some(color_frame) = color_head.take().or_else(|| color.recv().ok()) else {
            return;
        };
        let Some(alpha_frame) = alpha_head.take().or_else(|| alpha.recv().ok()) else {
            return;
        };
        match color_frame.pts.cmp(&alpha_frame.pts) {
            Ordering::Less => {
                warn!(pts=?color_frame.pts, "Dropping a frame without its alpha.");
                alpha_head = Some(alpha_frame);
            }
            Ordering::Greater => {
                warn!(pts=?alpha_frame.pts, "Dropping an alpha without its frame.");
                color_head = Some(color_frame);
            }
            Ordering::Equal => {
                let pts = alpha_frame.pts;
                let Some(alpha) = alpha_plane(alpha_frame.data) else {
                    warn!(
                        ?pts,
                        "Dropping a frame whose alpha has no limited-range luma."
                    );
                    continue;
                };
                let frame = Frame {
                    data: FrameData::WithAlpha {
                        color: Box::new(color_frame.data),
                        alpha,
                    },
                    ..color_frame
                };
                if sender.send(frame).is_err() {
                    return;
                }
            }
        }
    }
}

fn alpha_plane(data: FrameData) -> Option<AlphaPlane> {
    match data {
        FrameData::PlanarYuv420(planes)
        | FrameData::PlanarYuv422(planes)
        | FrameData::PlanarYuv444(planes) => Some(AlphaPlane::Bytes(planes.y_plane)),
        FrameData::Nv12(planes) => Some(AlphaPlane::Bytes(planes.y_plane)),
        FrameData::Nv12WgpuTexture(texture) => Some(AlphaPlane::Nv12Texture(texture)),
        FrameData::Nv12WgpuPlanes { y_plane, .. } => Some(AlphaPlane::R8Texture(y_plane)),
        _ => None,
    }
}
