use clap::{Args, Parser, Subcommand, ValueEnum};
use nox_engine::{AudioCodec, Container, EncodeQuality, ExportSettings, VideoCodec};
use nox_media::OutputEncoding;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Probes media and renders Nox timelines without the editor.
#[derive(Parser)]
#[command(name = "nox-cli", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print Nox and FFmpeg versions
    Version,
    /// Print stream information
    Probe {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// List the encoders that work on this system
    Encoders,
    /// Render a timeline to a file (container from the extension: .mp4,
    /// .mov, .mkv, .webm, .avi)
    Render(RenderArgs),
}

#[derive(Args)]
struct RenderArgs {
    /// The timeline (.otio)
    project: PathBuf,
    output: PathBuf,
    /// Video codec [default: the container's usual one: H.264, ProRes for
    /// .mov, VP9 for .webm; HEVC with --hdr]
    #[arg(long, value_enum)]
    codec: Option<CodecArg>,
    /// Audio codec [default: the container's usual one]
    #[arg(long, value_enum)]
    audio: Option<AudioArg>,
    #[arg(long, value_enum, default_value_t = QualityArg::High)]
    quality: QualityArg,
    /// No hardware encoders
    #[arg(long)]
    software: bool,
    /// This encoder (see `encoders`), e.g. hevc_nvenc
    #[arg(long, value_name = "NAME")]
    encoder: Option<String>,
    /// Leave out the audio track
    #[arg(long)]
    no_audio: bool,
    /// Sequence frames START..END (END exclusive)
    #[arg(long, value_name = "START-END", value_parser = parse_frames, conflicts_with = "in_out")]
    frames: Option<Range<i64>>,
    /// Only the frames between the In and Out marks
    #[arg(long)]
    in_out: bool,
    /// HDR10 (PQ) or HLG in BT.2020, 10 bit
    #[arg(long, value_enum)]
    hdr: Option<HdrArg>,
    /// Mastering display peak for PQ, in nits (100–10000)
    #[arg(long, default_value_t = 1000.0, value_parser = parse_peak)]
    peak: f32,
}

#[derive(Clone, Copy, ValueEnum)]
enum CodecArg {
    H264,
    Hevc,
    Prores,
    Av1,
    Vp9,
}

#[derive(Clone, Copy, ValueEnum)]
enum AudioArg {
    Aac,
    Opus,
    Mp3,
    Flac,
    Pcm16,
    Pcm24,
}

#[derive(Clone, Copy, ValueEnum)]
enum QualityArg {
    High,
    Medium,
    Low,
}

#[derive(Clone, Copy, ValueEnum)]
enum HdrArg {
    Pq,
    Hlg,
}

fn parse_frames(range: &str) -> Result<Range<i64>, String> {
    range
        .split_once('-')
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        .filter(|&(a, b): &(i64, i64)| a < b && a >= 0)
        .map(|(a, b)| a..b)
        .ok_or_else(|| format!("invalid frame range {range}"))
}

fn parse_peak(peak: &str) -> Result<f32, String> {
    peak.parse()
        .ok()
        .filter(|p: &f32| (100.0..=10_000.0).contains(p))
        .ok_or_else(|| format!("invalid peak {peak} (100–10000 nits)"))
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NOX_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Command::Version => version(),
        Command::Probe { files } => return probe(&files),
        Command::Encoders => encoders(),
        Command::Render(args) => {
            if let Err(err) = render(args) {
                eprintln!("nox-cli: {err}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

fn version() {
    let v = nox_media::ffmpeg_versions();
    println!("nox {}", env!("CARGO_PKG_VERSION"));
    println!("  libavutil   {}.{}.{}", v.avutil.0, v.avutil.1, v.avutil.2);
    println!(
        "  libavcodec  {}.{}.{}",
        v.avcodec.0, v.avcodec.1, v.avcodec.2
    );
    println!(
        "  libavformat {}.{}.{}",
        v.avformat.0, v.avformat.1, v.avformat.2
    );
}

fn encoders() {
    for codec in VideoCodec::ALL {
        for (range, encoding) in [("SDR", OutputEncoding::Sdr), ("HDR", OutputEncoding::Pq)] {
            let found = nox_engine::available_encoders(codec, encoding);
            let names: Vec<String> = found
                .iter()
                .map(|e| match e.gpu_frames {
                    Some(api) => format!("{} (frames via {api})", e.name),
                    None => e.name.to_owned(),
                })
                .collect();
            let names = if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            };
            println!("{} {range}: {names}", codec.name());
        }
    }
}

fn probe(paths: &[PathBuf]) -> ExitCode {
    let mut status = ExitCode::SUCCESS;
    for path in paths {
        match nox_media::probe(path) {
            Ok(info) => print_info(&info),
            Err(err) => {
                eprintln!("{}: {err}", path.display());
                status = ExitCode::FAILURE;
            }
        }
    }
    status
}

fn print_info(info: &nox_media::MediaInfo) {
    println!("{}", info.path.display());
    for line in info.to_string().lines() {
        println!("  {line}");
    }
}

fn render(args: RenderArgs) -> Result<(), String> {
    let project = &args.project;
    let container = Container::for_path(&args.output);
    let encoding = match args.hdr {
        None => OutputEncoding::Sdr,
        Some(HdrArg::Pq) => OutputEncoding::Pq,
        Some(HdrArg::Hlg) => OutputEncoding::Hlg,
    };
    let codec = match args.codec {
        Some(CodecArg::H264) => VideoCodec::H264,
        Some(CodecArg::Hevc) => VideoCodec::Hevc,
        Some(CodecArg::Prores) => VideoCodec::ProRes,
        Some(CodecArg::Av1) => VideoCodec::Av1,
        Some(CodecArg::Vp9) => VideoCodec::Vp9,
        // The container's usual codec; HDR needs one that carries it.
        None => match container.map_or(VideoCodec::H264, |c| c.codecs()[0]) {
            codec if encoding.is_hdr() && !codec.does_hdr() => VideoCodec::Hevc,
            codec => codec,
        },
    };
    let mut settings = ExportSettings {
        path: args.output.clone(),
        codec,
        container,
        quality: match args.quality {
            QualityArg::High => EncodeQuality::High,
            QualityArg::Medium => EncodeQuality::Medium,
            QualityArg::Low => EncodeQuality::Low,
        },
        hardware: !args.software,
        decode: nox_media::HwDecode::from_env(),
        encoder: args.encoder,
        range: args.frames,
        audio: !args.no_audio,
        audio_codec: args.audio.map(|audio| match audio {
            AudioArg::Aac => AudioCodec::Aac,
            AudioArg::Opus => AudioCodec::Opus,
            AudioArg::Mp3 => AudioCodec::Mp3,
            AudioArg::Flac => AudioCodec::Flac,
            AudioArg::Pcm16 => AudioCodec::Pcm16,
            AudioArg::Pcm24 => AudioCodec::Pcm24,
        }),
        encoding,
        peak_nits: args.peak,
    };
    let mut timeline =
        nox_otio::read_file(project).map_err(|e| format!("{}: {e}", project.display()))?;
    // Relative media paths are relative to the project, as in the app.
    timeline.media_dir = std::path::absolute(project)
        .ok()
        .and_then(|p| p.parent().map(Path::to_owned));
    if args.in_out {
        let range = nox_engine::marked_range(&timeline)
            .ok_or(format!("{} has no In and Out marks", project.display()))?;
        settings.range = Some(range);
    }
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let summary = nox_engine::export(&timeline, &settings, |done, total| {
        if interactive && (done % 10 == 0 || done == total) {
            eprint!(
                "\rrendering frame {done}/{total} ({:.0}%)",
                done as f64 * 100.0 / total as f64
            );
            let _ = std::io::stderr().flush();
        }
        true
    })
    .map_err(|e| e.to_string())?;
    if interactive {
        eprintln!();
    }
    println!(
        "{}: {} frames in {:.1} s ({:.0} fps), {}{}",
        summary.path.display(),
        summary.frames,
        summary.elapsed.as_secs_f64(),
        summary.fps(),
        summary.encoder,
        if summary.zero_copy { ", zero copy" } else { "" }
    );
    Ok(())
}
