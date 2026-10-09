use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: nox-cli <command>

commands:
  version          print Nox and FFmpeg versions
  probe <file>...  print stream information";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("version") => {
            version();
            ExitCode::SUCCESS
        }
        Some("probe") if args.len() > 1 => probe(&args[1..]),
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
