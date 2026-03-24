use crate::prelude::*;

pub trait InputDataSender: Send + Sync {
    fn send_video_frame(&self, frame: Frame);
    fn send_audio_samples(&self, samples: InputAudioSamples);
}
