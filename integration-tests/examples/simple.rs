use std::{thread, time::Duration};

use anyhow::Result;
use serde_json::json;
use smelter_api::Resolution;

use integration_tests::{
    examples::{self, TestSample, download_all_assets, get_asset_path, run_example},
    ffmpeg::start_ffmpeg_rtmp_receive,
    gstreamer::{start_gst_send_tcp, start_gst_send_udp},
};

const VIDEO_RESOLUTION: Resolution = Resolution {
    width: 1280,
    height: 720,
};

const OUTPUT_PORT: u16 = 8004;
const INPUT_PORT: u16 = 8006;

fn main() {
    run_example(client_code);
}

fn client_code() -> Result<()> {
    download_all_assets()?;
    start_ffmpeg_rtmp_receive(OUTPUT_PORT)?;
    examples::post(
        "output/output_1/register",
        &json!({
            "type": "rtmp_client",
            "url": format!("rtmp://127.0.0.1:{OUTPUT_PORT}"),
            "video": {
                "resolution": {
                    "width": VIDEO_RESOLUTION.width,
                    "height": VIDEO_RESOLUTION.height,
                },
                "encoder": {
                    "type": "ffmpeg_h264",
                    "preset": "ultrafast"
                },
                "initial": {
                    "root": {
                        "type": "rescaler",
                        "child": {
                            "id": "input_1",
                            "type": "input_stream",
                            "input_id": "input_1",
                        },
                    }
                }
            },
            "audio": {
                "initial": {
                    "inputs": [
                        {"input_id": "input_1"},
                    ]
                },
                "channels": "stereo",
                "encoder": {
                    "type": "aac",
                }
            }
        }),
    )?;

    //examples::post(
    //    "input/input_2/register",
    //    &json!({
    //        "type": "mp4",
    //        "path": get_asset_path(TestSample::BigBuckBunnyH264AAC)?,
    //    }),
    //)?;
    //

    examples::post("start", &json!({}))?;

    thread::sleep(Duration::from_secs(12));

    examples::post(
        "input/input_1/register",
        &json!({
            "type": "rtp_stream",
            "transport_protocol":  "udp",
            "port": INPUT_PORT,
            "offset_ms": 10000,
            "required": true,
            "video": {
                "decoder": "ffmpeg_h264",
            },
            "audio": {
                "decoder": "opus",
            }
        }),
    )?;

    start_gst_send_tcp(
        "127.0.0.1",
        Some(INPUT_PORT),
        Some(INPUT_PORT),
        TestSample::BigBuckBunnyH264Opus,
    )
    .unwrap();


    Ok(())
}
