use crate::Resolution;
use bytes::Bytes;
use tracing::error;

use crate::transformations::web_renderer::{FrameData, SharedView, SourceTransforms};

use super::{
    GET_FRAME_POSITIONS_MESSAGE,
    transformation_matrices::{Position, vertices_transformation_matrix},
};

#[derive(Clone)]
pub(super) struct BrowserClient {
    frame_data: FrameData,
    source_transforms: SourceTransforms,
    view: SharedView,
}

impl libcef::Client for BrowserClient {
    type RenderHandlerType = RenderHandler;

    fn render_handler(&self) -> Option<Self::RenderHandlerType> {
        Some(RenderHandler {
            frame_data: self.frame_data.clone(),
            view: self.view.clone(),
        })
    }

    fn on_process_message_received(
        &mut self,
        _browser: &libcef::Browser,
        _frame: &libcef::Frame,
        _source_process: libcef::ProcessId,
        message: &libcef::ProcessMessage,
    ) -> bool {
        match message.name().as_str() {
            GET_FRAME_POSITIONS_MESSAGE => {
                let layout_size = self.view.lock().unwrap().shown.layout;
                let mut transforms_matrices = Vec::new();
                for i in (0..message.size()).step_by(4) {
                    let position = match Self::read_frame_position(message, i) {
                        Ok(position) => position,
                        Err(err) => {
                            error!(
                                "Error occurred while reading frame positions from IPC message: {err}"
                            );
                            return true;
                        }
                    };

                    let transformations_matrix =
                        vertices_transformation_matrix(&position, &layout_size);

                    transforms_matrices.push(transformations_matrix);
                }

                let mut source_transforms = self.source_transforms.lock().unwrap();
                *source_transforms = transforms_matrices;
            }
            ty => error!("Unknown process message type \"{ty}\""),
        }
        true
    }
}

impl BrowserClient {
    pub fn new(
        frame_data: FrameData,
        source_transforms: SourceTransforms,
        view: SharedView,
    ) -> Self {
        Self {
            frame_data,
            source_transforms,
            view,
        }
    }

    fn read_frame_position(
        msg: &libcef::ProcessMessage,
        index: usize,
    ) -> Result<Position, libcef::ProcessMessageError> {
        let x = msg.read_double(index)?;
        let y = msg.read_double(index + 1)?;
        let width = msg.read_double(index + 2)?;
        let height = msg.read_double(index + 3)?;

        Ok(Position {
            top: y as f32,
            left: x as f32,
            width: width as f32,
            height: height as f32,
            rotation_degrees: 0.0,
        })
    }
}

pub(super) struct RenderHandler {
    frame_data: FrameData,
    view: SharedView,
}

impl libcef::RenderHandler for RenderHandler {
    fn resolution(&self, _browser: &libcef::Browser) -> libcef::Resolution {
        let Resolution { width, height } = self.view.lock().unwrap().shown.layout;
        libcef::Resolution { width, height }
    }

    fn scale_factor(&self, _browser: &libcef::Browser) -> f32 {
        self.view.lock().unwrap().shown.scale_factor
    }

    fn on_paint(&self, _browser: &libcef::Browser, buffer: &[u8], resolution: libcef::Resolution) {
        // Copied before the lock, which the render thread takes each tick.
        let paint = Bytes::copy_from_slice(buffer);
        let resolution = Resolution {
            width: resolution.width,
            height: resolution.height,
        };
        self.view.lock().unwrap().painted(resolution);
        *self.frame_data.lock().unwrap() = Some((paint, resolution));
    }
}
