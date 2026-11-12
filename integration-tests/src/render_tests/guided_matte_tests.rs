use std::time::Duration;

use smelter_render::{
    Frame, FrameData, OutputFrameFormat, Resolution, YuvPlanes,
    scene::{Component, InputStreamComponent, RGBAColor, ViewComponent},
};

use super::harness::{
    input::TestInput,
    test_case::{Step, TestCase},
};

/// A guided matte composited over an opaque view renders as `guided_matte/reference.py` decodes it,
/// to within rounding: white text outlined in black against the frame's edge, a white box inside a
/// half-transparent panel, a soft round edge across a colour edge, over all three backdrops.
#[test]
fn guided_matte_conformance() {
    let resolution = Resolution {
        width: 32,
        height: 16,
    };
    let half = Resolution {
        width: 16,
        height: 8,
    };
    let planes = |bytes: &'static [u8], Resolution { width, height }: Resolution| {
        let (luma, chroma) = (width * height, width * height / 4);
        YuvPlanes {
            y_plane: bytes::Bytes::from_static(&bytes[..luma]),
            u_plane: bytes::Bytes::from_static(&bytes[luma..luma + chroma]),
            v_plane: bytes::Bytes::from_static(&bytes[luma + chroma..]),
        }
    };
    let picture = planes(
        include_bytes!("guided_matte/guided_matte.picture.bin"),
        resolution,
    );
    let matte = planes(include_bytes!("guided_matte/guided_matte.matte.bin"), half);
    let expected = include_bytes!("guided_matte/guided_matte.expected.bin");

    let input = TestInput {
        name: "input_0".into(),
        resolution,
        data: FrameData::WithAlpha {
            color: Box::new(FrameData::PlanarYuv420(picture)),
            alpha: Box::new(Frame {
                data: FrameData::PlanarYuv420(matte),
                resolution: half,
                pts: Duration::ZERO,
            }),
        },
    };
    let test_case = TestCase {
        inputs: vec![input],
        resolution,
        output_format: OutputFrameFormat::RgbaWgpuTexture,
        steps: vec![
            Step::UpdateScene(Component::View(ViewComponent {
                background_color: RGBAColor(51, 128, 77, 255),
                children: vec![Component::InputStream(InputStreamComponent {
                    id: None,
                    input_id: smelter_render::InputId("input_0".into()),
                })],
                ..Default::default()
            })),
            Step::RenderWithSnapshot(Duration::ZERO),
        ],
        ..Default::default()
    };
    let actual = &test_case.generate_snapshots()[0].data;
    let worst = actual
        .iter()
        .zip(expected)
        .map(|(a, e)| a.abs_diff(*e))
        .max()
        .unwrap();
    assert!(
        worst <= 2,
        "differs from the reference decoder by up to {worst}"
    );
}
