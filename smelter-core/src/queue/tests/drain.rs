//! Contract tests for the consumer-side drain API (`QueueInput::get_frame` /
//! `pop_samples` / `maybe_start_next_track`), used by callers that construct
//! inputs on a detached [`QueueContext`] and drive them without a queue
//! thread. These pin behaviors external drains rely on; a change here is a
//! breaking change for such callers.

use std::{sync::Arc, time::Duration};

use smelter_render::InputId;

use crate::{
    event::{Event, EventEmitter},
    prelude::*,
    queue::{QueueInput, QueueInputOptions, QueueTrackOffset, QueueTrackOptions},
    types::Ref,
};

use super::harness::{ms, test_frame, test_samples};

struct DrainInput {
    input: QueueInput,
    events: crossbeam_channel::Receiver<Event>,
}

fn drain_input() -> DrainInput {
    let event_emitter = Arc::new(EventEmitter::new());
    let events = event_emitter.subscribe();
    let input_id = InputId("drain".into());
    let input = QueueInput::new_inner(
        crate::queue::QueueContext::new_detached(std::time::Instant::now()),
        event_emitter,
        &Ref::new(&input_id),
        QueueInputOptions::default(),
        None,
        None,
    );
    DrainInput { input, events }
}

fn media_track() -> QueueTrackOptions {
    QueueTrackOptions {
        video: true,
        audio: true,
        offset: QueueTrackOffset::None,
    }
}

fn eos_events(events: &crossbeam_channel::Receiver<Event>) -> (usize, usize) {
    let mut video = 0;
    let mut audio = 0;
    for event in events.try_iter() {
        match event {
            Event::VideoInputStreamEos(_) => video += 1,
            Event::AudioInputStreamEos(_) => audio += 1,
            _ => {}
        }
    }
    (video, audio)
}

/// A media-cursor drain: query PTS advance from zero per track, so
/// `QueueTrackOffset::None` resolves each track's offset to ~0 and frame PTS
/// stay track-local — the pattern an external clip drain uses.
fn pull_all_video(input: &QueueInput, until: Duration) -> (Vec<Duration>, bool) {
    let mut seen = Vec::new();
    let mut eos = false;
    let mut cursor = Duration::ZERO;
    while cursor <= until {
        match input.get_frame(cursor, Duration::ZERO) {
            Some(PipelineEvent::Data(frame)) => {
                if seen.last() != Some(&frame.pts) {
                    seen.push(frame.pts);
                }
            }
            Some(PipelineEvent::EOS) => {
                eos = true;
                break;
            }
            None => {}
        }
        cursor += ms(5);
    }
    (seen, eos)
}

/// Contract 1: the once-per-track EOS is delivered by `get_frame` /
/// `pop_samples` *after* the track's buffer drains — and it is delivered at
/// most once. A drain must pull until it observes EOS before advancing
/// tracks; `maybe_start_next_track` at the instant both tracks are done
/// swaps the track and the undelivered EOS is silently skipped.
#[test]
fn eos_skipped_when_advancing_before_pull() {
    // Pulled-first path: EOS observed, exactly one event emitted.
    let drained = drain_input();
    let (video, audio) = drained.input.queue_new_track(media_track());
    drained.input.maybe_start_next_track();
    let (video, audio) = (video.unwrap(), audio.unwrap());
    video.send(test_frame(0, ms(0))).unwrap();
    audio.send(test_samples(ms(0), ms(20))).unwrap();
    drop((video, audio));

    let (frames, eos) = pull_all_video(&drained.input, ms(50));
    assert_eq!(frames, vec![ms(0)]);
    assert!(eos, "EOS must surface once the drained track is done");
    match drained.input.pop_samples((ms(0), ms(50)), Duration::ZERO) {
        Some(PipelineEvent::Data(batches)) => assert_eq!(batches.len(), 1),
        other => panic!("expected one audio batch, got {other:?}"),
    }
    assert!(matches!(
        drained.input.pop_samples((ms(50), ms(70)), Duration::ZERO),
        Some(PipelineEvent::EOS)
    ));
    // EOS is once-per-track: subsequent pulls return None / empty data.
    assert!(drained.input.get_frame(ms(60), Duration::ZERO).is_none());
    let (video_eos, audio_eos) = eos_events(&drained.events);
    assert_eq!((video_eos, audio_eos), (1, 1));

    // Advanced-first path: the same scenario, but the drain swaps tracks the
    // moment both sides report done (data consumed, EOS not yet pulled) —
    // the EOS event is lost with the track.
    let skipped = drain_input();
    let (video, audio) = skipped.input.queue_new_track(media_track());
    skipped.input.maybe_start_next_track();
    let (video, audio) = (video.unwrap(), audio.unwrap());
    video.send(test_frame(0, ms(0))).unwrap();
    audio.send(test_samples(ms(0), ms(20))).unwrap();
    drop((video, audio));
    assert!(matches!(
        skipped.input.get_frame(ms(0), Duration::ZERO),
        Some(PipelineEvent::Data(_))
    ));
    match skipped.input.pop_samples((ms(0), ms(50)), Duration::ZERO) {
        Some(PipelineEvent::Data(batches)) => assert_eq!(batches.len(), 1),
        other => panic!("expected one audio batch, got {other:?}"),
    }
    assert!(skipped.input.is_video_done() && skipped.input.is_audio_done());
    let (video2, audio2) = skipped.input.queue_new_track(media_track());
    skipped.input.maybe_start_next_track();
    video2.unwrap().send(test_frame(1, ms(0))).unwrap();
    assert!(matches!(
        skipped.input.get_frame(ms(0), Duration::ZERO),
        Some(PipelineEvent::Data(_))
    ));
    drop(audio2);
    let (video_eos, audio_eos) = eos_events(&skipped.events);
    assert_eq!(
        (video_eos, audio_eos),
        (0, 0),
        "track 1's EOS must be skipped when the swap precedes the pull — \
         if this starts delivering it, external drain ordering can change"
    );
}

/// Contract 2: a pending track (the next loop iteration, queued by media
/// inputs at EOF) does not replace the current one until *both* current
/// tracks are done and the drain calls `maybe_start_next_track`; after the
/// swap, frames flow with track-local PTS again (the drain owns anchor
/// advancement across the seam).
#[test]
fn pending_track_waits_for_both_tracks_done() {
    let drained = drain_input();
    let (video1, audio1) = drained.input.queue_new_track(media_track());
    drained.input.maybe_start_next_track();
    let (video1, audio1) = (video1.unwrap(), audio1.unwrap());
    video1.send(test_frame(0, ms(0))).unwrap();
    audio1.send(test_samples(ms(0), ms(40))).unwrap();

    // Next iteration queued while track 1 is still live (the mp4 loop shape).
    let (video2, _audio2) = drained.input.queue_new_track(media_track());
    let video2 = video2.unwrap();
    video2.send(test_frame(1, ms(0))).unwrap();

    // Video 1 ends; audio 1 still open — the swap must not happen.
    drop(video1);
    let (frames, eos) = pull_all_video(&drained.input, ms(30));
    assert_eq!(frames, vec![ms(0)]);
    assert!(eos);
    assert!(!drained.input.is_audio_done());
    drained.input.maybe_start_next_track();
    assert!(
        drained.input.get_frame(ms(35), Duration::ZERO).is_none(),
        "track 2 frames must not surface while track 1 audio is live"
    );

    // Audio 1 ends: after draining it, the swap takes effect and track 2's
    // frame surfaces at its track-local PTS.
    drop(audio1);
    let _ = drained.input.pop_samples((ms(0), ms(60)), Duration::ZERO);
    assert!(matches!(
        drained.input.pop_samples((ms(60), ms(80)), Duration::ZERO),
        Some(PipelineEvent::EOS)
    ));
    assert!(drained.input.is_video_done() && drained.input.is_audio_done());
    drained.input.maybe_start_next_track();
    let (frames, _) = pull_all_video(&drained.input, ms(10));
    assert_eq!(frames, vec![ms(0)], "track 2 PTS are track-local");
}

/// Contract 3: EOS events fire on the emitter for *every* track, looping
/// media included — one pair per drained-to-completion track. The
/// "suppress EOS for looping media" rule is the drain's job, not the
/// mailbox's; if the mailbox ever starts filtering, drains double-filter.
#[test]
fn eos_events_fire_per_track() {
    let drained = drain_input();
    for track in 0..3u32 {
        let (video, audio) = drained.input.queue_new_track(media_track());
    drained.input.maybe_start_next_track();
        let (video, audio) = (video.unwrap(), audio.unwrap());
        video.send(test_frame(track, ms(0))).unwrap();
        audio.send(test_samples(ms(0), ms(20))).unwrap();
        drop((video, audio));

        let (frames, eos) = pull_all_video(&drained.input, ms(40));
        assert_eq!(frames, vec![ms(0)]);
        assert!(eos);
        let _ = drained.input.pop_samples((ms(0), ms(40)), Duration::ZERO);
        assert!(matches!(
            drained.input.pop_samples((ms(40), ms(60)), Duration::ZERO),
            Some(PipelineEvent::EOS)
        ));
        drained.input.maybe_start_next_track();
    }
    let (video_eos, audio_eos) = eos_events(&drained.events);
    assert_eq!((video_eos, audio_eos), (3, 3));
}
