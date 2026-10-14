use nox_engine::{EncodeQuality, ExportSettings, VideoCodec};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage: nox-cli <command>

commands:
  version          print Nox and FFmpeg versions
  probe <file>...  print stream information
  encoders         list the encoders that work on this system
  render <project.otio> <output> [options]
                   render a timeline to a file (container from the
                   extension: .mp4, .mov, .mkv)
    --codec h264|hevc|prores   (default: h264, or prores for .mov)
    --quality high|medium|low  (default: high)
    --software                 no hardware encoders
    --encoder NAME             this encoder (see `encoders`), e.g. hevc_nvenc
    --no-audio                 leave out the audio track
    --frames START-END         sequence frames START..END (END exclusive)
    --in-out                   only the frames between the In and Out marks
    --hdr pq|hlg               HDR10 (PQ) or HLG in BT.2020, 10 bit (HEVC,
                               the default with --hdr, or ProRes)
    --peak NITS                mastering display peak for PQ (default: 1000)";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NOX_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("version") => {
            version();
            ExitCode::SUCCESS
        }
        Some("probe") if args.len() > 1 => probe(&args[1..]),
        Some("encoders") => {
            encoders();
            ExitCode::SUCCESS
        }
        Some("render") if args.len() >= 3 => match render(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("nox-cli: {err}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
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
    use nox_media::OutputEncoding;
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

fn probe(paths: &[String]) -> ExitCode {
    let mut status = ExitCode::SUCCESS;
    for path in paths {
        match nox_media::probe(Path::new(path)) {
            Ok(info) => print_info(&info),
            Err(err) => {
                eprintln!("{path}: {err}");
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

fn render(args: &[String]) -> Result<(), String> {
    let project = Path::new(&args[0]);
    let output = PathBuf::from(&args[1]);
    let is_mov = output
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mov"));
    let mut settings = ExportSettings {
        path: output.clone(),
        codec: if is_mov {
            VideoCodec::ProRes
        } else {
            VideoCodec::H264
        },
        quality: EncodeQuality::High,
        hardware: true,
        encoder: None,
        range: None,
        audio: true,
        encoding: nox_media::OutputEncoding::Sdr,
        peak_nits: 1000.0,
    };
    let mut in_out = false;
    let mut codec_given = false;
    let mut rest = args[2..].iter();
    while let Some(arg) = rest.next() {
        let mut value = || rest.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--codec" => {
                codec_given = true;
                settings.codec = match value()?.as_str() {
                    "h264" => VideoCodec::H264,
                    "hevc" => VideoCodec::Hevc,
                    "prores" => VideoCodec::ProRes,
                    other => return Err(format!("unknown codec {other}")),
                }
            }
            "--quality" => {
                settings.quality = match value()?.as_str() {
                    "high" => EncodeQuality::High,
                    "medium" => EncodeQuality::Medium,
                    "low" => EncodeQuality::Low,
                    other => return Err(format!("unknown quality {other}")),
                }
            }
            "--software" => settings.hardware = false,
            "--encoder" => settings.encoder = Some(value()?.clone()),
            "--no-audio" => settings.audio = false,
            "--frames" => {
                let range = value()?;
                let (start, end) = range
                    .split_once('-')
                    .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
                    .filter(|(a, b): &(i64, i64)| a < b && *a >= 0)
                    .ok_or(format!("invalid frame range {range}"))?;
                settings.range = Some(start..end);
            }
            "--in-out" => in_out = true,
            "--hdr" => {
                settings.encoding = match value()?.as_str() {
                    "pq" => nox_media::OutputEncoding::Pq,
                    "hlg" => nox_media::OutputEncoding::Hlg,
                    other => return Err(format!("unknown HDR format {other}")),
                }
            }
            "--peak" => {
                let peak = value()?;
                settings.peak_nits = peak
                    .parse()
                    .ok()
                    .filter(|p: &f32| (100.0..=10_000.0).contains(p))
                    .ok_or(format!("invalid peak {peak} (100–10000 nits)"))?;
            }
            other => return Err(format!("unknown option {other}\n\n{USAGE}")),
        }
    }
    // HDR is HEVC unless the codec was chosen (H.264 is SDR only).
    if settings.encoding.is_hdr() && !codec_given && settings.codec == VideoCodec::H264 {
        settings.codec = VideoCodec::Hevc;
    }
    let timeline =
        nox_otio::read_file(project).map_err(|e| format!("{}: {e}", project.display()))?;
    if in_out {
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
