use std::{
    fmt,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use image::{ColorType, ImageEncoder, codecs::png::PngEncoder};
use integration_tests::read_rgba_texture;
use smelter::{
    config::read_config,
    logger,
    state::pipeline_options_from_config,
};
use smelter_core::{
    *,
    codecs::VideoDecoderOptions,
    graphics_context::GraphicsContext,
    protocols::{
        Mp4InputOptions, Mp4InputSource, Mp4InputVideoDecoders, RawDataOutputOptions,
        RawDataOutputReceiver, RawDataOutputVideoOptions,
    },
};
use smelter_render::{
    Frame, FrameData, InputId, OutputId, Resolution,
    scene::{Component, InputStreamComponent},
};
use tokio::runtime::Runtime;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    input: PathBuf,
    #[arg(long, value_enum)]
    decoder: Option<Decoder>,
    #[arg(long)]
    output_dir: PathBuf,
    #[arg(long, default_value_t = 30)]
    frames: usize,
    #[arg(long, default_value_t = 1280)]
    width: usize,
    #[arg(long, default_value_t = 720)]
    height: usize,
    #[arg(long, default_value_t = false)]
    no_write: bool,
    #[arg(long, default_value_t = false)]
    no_readback: bool,
    #[arg(long, default_value_t = 30)]
    progress_every: usize,
    #[arg(long, default_value_t = false)]
    quiet_frames: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Decoder {
    FfmpegH264,
    VaapiH264,
}

impl fmt::Display for Decoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Decoder::FfmpegH264 => f.write_str("ffmpeg-h264"),
            Decoder::VaapiH264 => f.write_str("vaapi-h264"),
        }
    }
}

impl Args {
    fn decoder(&self) -> Decoder {
        self.decoder.unwrap_or_else(default_decoder)
    }
}

#[cfg(feature = "vaapi")]
fn default_decoder() -> Decoder {
    Decoder::VaapiH264
}

#[cfg(not(feature = "vaapi"))]
fn default_decoder() -> Decoder {
    Decoder::FfmpegH264
}

#[cfg(feature = "vaapi")]
fn vaapi_h264_decoder() -> Result<VideoDecoderOptions> {
    Ok(VideoDecoderOptions::VaapiH264)
}

#[cfg(not(feature = "vaapi"))]
fn vaapi_h264_decoder() -> Result<VideoDecoderOptions> {
    bail!("VA-API H264 decoder requires running this example with --features vaapi")
}

fn main() -> Result<()> {
    ffmpeg_next::format::network::init();
    logger::init_logger(read_config().logger);
    let args = Args::parse();
    if !args.no_write {
        fs::create_dir_all(&args.output_dir)?;
    }

    let mut config = read_config();
    config.ahead_of_time_processing = true;
    config.never_drop_output_frames = true;

    let graphics = GraphicsContext::new(Default::default())?;
    let device = Arc::clone(&graphics.device);
    let queue = Arc::clone(&graphics.queue);
    let runtime = Arc::new(Runtime::new()?);
    let mut pipeline_options = pipeline_options_from_config(&config, &runtime, &None);
    pipeline_options.wgpu_options = PipelineWgpuOptions::Context(graphics);
    pipeline_options.tokio_rt = None;
    pipeline_options.whip_whep_server = PipelineWhipWhepServerOptions::Disable;
    pipeline_options.rtmp_server = PipelineRtmpServerOptions::Disable;
    drop(runtime);

    let pipeline = Pipeline::new(pipeline_options)?;
    let pipeline = Arc::new(Mutex::new(pipeline));

    let input_id = InputId("input".into());
    let output_id = OutputId("output".into());
    Pipeline::register_input(
        &pipeline,
        input_id.clone(),
        RegisterInputOptions {
            input_options: ProtocolInputOptions::Mp4(Mp4InputOptions {
                source: Mp4InputSource::File(args.input.clone().into()),
                should_loop: false,
                video_decoders: Mp4InputVideoDecoders {
                    h264: Some(match args.decoder() {
                        Decoder::FfmpegH264 => VideoDecoderOptions::FfmpegH264,
                        Decoder::VaapiH264 => vaapi_h264_decoder()?,
                    }),
                },
                seek: None,
                buffer: InputBufferOptions::Const(None),
            }),
            queue_options: QueueInputOptions {
                required: true,
                offset: Some(Duration::ZERO),
            },
        },
    )?;

    let RawDataOutputReceiver { video, .. } = Pipeline::register_raw_data_output(
        &pipeline,
        output_id.clone(),
        RegisterRawDataOutputOptions {
            output_options: RawDataOutputOptions {
                video: Some(RawDataOutputVideoOptions {
                    resolution: Resolution { width: args.width, height: args.height },
                }),
                audio: None,
            },
            video: Some(RegisterOutputVideoOptions {
                initial: Component::InputStream(InputStreamComponent {
                    id: None,
                    input_id: input_id.clone(),
                }),
                end_condition: PipelineOutputEndCondition::Never,
            }),
            audio: None,
        },
    )?;

    let video = video.context("raw video receiver was not created")?;
    Pipeline::start(&pipeline);

    let stats = collect_frames(&args, &video, &device, &queue);
    cleanup_pipeline(&pipeline, &input_id, &output_id);
    drop(video);
    drop(pipeline);

    let stats = stats?;
    if stats.written == 0 {
        bail!("decoder produced no frames");
    }

    print_summary(stats.written, stats.elapsed, stats.gaps);
    Ok(())
}

struct RunStats {
    written: usize,
    elapsed: Duration,
    gaps: Vec<Duration>,
}

fn collect_frames(
    args: &Args,
    video: &crossbeam_channel::Receiver<PipelineEvent<Frame>>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
) -> Result<RunStats> {
    let started_at = Instant::now();
    let mut written = 0usize;
    let mut gaps = Vec::with_capacity(args.frames.saturating_sub(1));
    let mut previous_frame_at = None;

    for event in video.iter() {
        let PipelineEvent::Data(frame) = event else {
            break;
        };
        let now = Instant::now();
        if let Some(previous) = previous_frame_at {
            gaps.push(now.duration_since(previous));
        }
        previous_frame_at = Some(now);

        inspect_frame(
            &args.output_dir,
            written,
            frame,
            device,
            queue,
            !args.no_write,
            !args.no_readback,
            args.quiet_frames,
        )?;
        written += 1;
        if args.progress_every > 0 && written % args.progress_every == 0 {
            let elapsed = started_at.elapsed();
            println!(
                "progress frames={written} elapsed_ms={} fps={:.2}",
                elapsed.as_millis(),
                written as f64 / elapsed.as_secs_f64()
            );
        }
        if written >= args.frames {
            break;
        }
    }

    Ok(RunStats { written, elapsed: started_at.elapsed(), gaps })
}

fn cleanup_pipeline(
    pipeline: &Arc<Mutex<Pipeline>>,
    input_id: &InputId,
    output_id: &OutputId,
) {
    let mut pipeline = pipeline.lock().unwrap();
    let _ = pipeline.unregister_output(output_id);
    let _ = pipeline.unregister_input(input_id);
}

fn inspect_frame(
    output_dir: &PathBuf,
    index: usize,
    frame: Frame,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    write_png: bool,
    readback: bool,
    quiet: bool,
) -> Result<()> {
    let FrameData::Rgba8UnormWgpuTexture(texture) = frame.data else {
        bail!("raw output produced a non-RGBA texture");
    };
    if readback {
        let size = texture.size();
        let data = read_rgba_texture(device, queue, &texture);
        let sum: u64 = data.iter().map(|byte| *byte as u64).sum();
        let nonzero = data.iter().filter(|byte| **byte != 0).count();
        if write_png {
            let path = output_dir.join(format!("{index:04}.png"));
            let file = fs::File::create(path)?;
            PngEncoder::new(file).write_image(
                &data,
                size.width,
                size.height,
                ColorType::Rgba8.into(),
            )?;
        }
        if !quiet {
            println!(
                "{index:04} pts_ms={} sum={sum} nonzero={nonzero}",
                frame.pts.as_millis()
            );
        }
    } else if !quiet {
        println!("{index:04} pts_ms={}", frame.pts.as_millis());
    }
    Ok(())
}

fn print_summary(frame_count: usize, elapsed: Duration, mut gaps: Vec<Duration>) {
    gaps.sort_unstable();
    let fps = frame_count as f64 / elapsed.as_secs_f64();
    let p50 = percentile(&gaps, 0.50);
    let p95 = percentile(&gaps, 0.95);
    let p99 = percentile(&gaps, 0.99);
    let max = gaps.last().copied().unwrap_or_default();
    println!(
        "summary frames={frame_count} elapsed_ms={} fps={fps:.2} gap_p50_ms={:.3} gap_p95_ms={:.3} gap_p99_ms={:.3} gap_max_ms={:.3}",
        elapsed.as_millis(),
        p50.as_secs_f64() * 1000.0,
        p95.as_secs_f64() * 1000.0,
        p99.as_secs_f64() * 1000.0,
        max.as_secs_f64() * 1000.0,
    );
}

fn percentile(durations: &[Duration], percentile: f64) -> Duration {
    if durations.is_empty() {
        return Duration::ZERO;
    }
    let index = ((durations.len() - 1) as f64 * percentile).round() as usize;
    durations[index]
}
