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
        if let Some(pulled) = input.get_frame(cursor, Duration::ZERO) {
            if let Some(frame) = pulled.frame {
                if seen.last() != Some(&frame.pts) {
                    seen.push(frame.pts);
                }
            }
            if pulled.is_eos {
                eos = true;
                break;
            }
        }
        cursor += ms(5);
    }
    (seen, eos)
}

/// Contract 1: `is_eos` rides every pull and latches true exactly once per
/// track — on the *same* pull that drains the final data when the producer
/// is already gone, and on empty pulls otherwise (a decoder dying before
/// its first frame still reports EOS). The flag is consumed by the pull
/// that sees it.
#[test]
fn eos_delivered_once_after_drain() {
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
    // The producer dropped before the pop, so the final batch and the EOS
    // flag arrive together — one pull, no empty tail required.
    let batch = drained.input.pop_samples((ms(0), ms(50)), Duration::ZERO).unwrap();
    assert_eq!(batch.samples.len(), 1);
    assert!(batch.is_eos);

    // EOS is once-per-track: subsequent pulls are empty and un-flagged.
    let after = drained.input.get_frame(ms(60), Duration::ZERO).unwrap();
    assert!(after.frame.is_none() && !after.is_eos);
    let after = drained.input.pop_samples((ms(70), ms(80)), Duration::ZERO).unwrap();
    assert!(after.samples.is_empty() && !after.is_eos);

    let (video_eos, audio_eos) = eos_events(&drained.events);
    assert_eq!((video_eos, audio_eos), (1, 1));
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
    let held = drained.input.get_frame(ms(35), Duration::ZERO).unwrap();
    assert!(
        held.frame.is_none(),
        "track 2 frames must not surface while track 1 audio is live"
    );

    // Audio 1 ends: the drain that empties it carries the EOS flag, the
    // swap takes effect, and track 2's frame surfaces at its track-local
    // PTS.
    drop(audio1);
    let tail = drained.input.pop_samples((ms(0), ms(60)), Duration::ZERO).unwrap();
    assert!(tail.is_eos);
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
        let batch = drained.input.pop_samples((ms(0), ms(40)), Duration::ZERO).unwrap();
        assert!(batch.is_eos);
        drained.input.maybe_start_next_track();
    }
    let (video_eos, audio_eos) = eos_events(&drained.events);
    assert_eq!((video_eos, audio_eos), (3, 3));
}
