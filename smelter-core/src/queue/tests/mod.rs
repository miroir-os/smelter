mod audio;
mod events;
mod harness;
mod track_switch;
mod video;

#[test]
fn scheduled_event_precedes_equal_timestamp_media() {
    use super::{Queue, QueueOptions};
    use crate::{LateEventPolicy, Timestamp};
    use std::{sync::Arc, time::Duration};
    let queue = Queue::new(QueueOptions {
        output_framerate: smelter_render::Framerate { num: 50, den: 1 },
        ahead_of_time_processing: false,
        run_late_scheduled_events: true,
        never_drop_output_frames: true,
        side_channel_socket_dir: None,
        tick_duration: Duration::from_millis(1),
    });
    let (video_sender, video_receiver) = crossbeam_channel::unbounded();
    let (audio_sender, _audio_receiver) = crossbeam_channel::unbounded();
    let (result_sender, result_receiver) = crossbeam_channel::bounded(1);
    let context = queue.ctx();
    queue.schedule_event(
        Timestamp::ZERO + Duration::from_millis(100),
        LateEventPolicy::AlwaysRun,
        Box::new(move || {
            let start = context.start_pts().unwrap();
            let times: Vec<_> = video_receiver
                .try_iter()
                .map(|frame: super::QueueVideoOutput| (frame.pts - start).as_millis())
                .collect();
            result_sender.send(times).unwrap();
        }),
    );
    Arc::clone(&queue).start(video_sender, audio_sender);
    let times = result_receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    queue.shutdown();
    assert_eq!(times, vec![0, 20, 40, 60, 80]);
}
