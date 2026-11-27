use kinodisc_domain::{AspectRatio, DvdProfile, FrameRate, TvStandard};
use kinodisc_dvd::pulldown::{inspect, NTSC_MAX_GOP_FIELDS};
use kinodisc_dvd::{AudioEncodeConfig, DvdEncoder, EncodeRequest, VideoEncodeConfig};
use kinodisc_toolchain::resolve_binary;
use std::path::Path;
use std::process::Command;

fn ffprobe(args: &[&str], file: &Path) -> String {
    let output = Command::new(resolve_binary("ffprobe"))
        .args(["-v", "error"])
        .args(args)
        .arg(file)
        .output()
        .expect("ffprobe");
    assert!(output.status.success(), "ffprobe {args:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A 24 fps film on an NTSC DVD: 24 pictures stored, 3:2 repeat flags, and a
/// program stream that lasts as long as the film.
#[tokio::test]
#[ignore = "requiere ffmpeg y ffprobe"]
async fn a_film_is_stored_as_24_pictures_with_repeat_flags() {
    let dir = std::env::temp_dir().join(format!("kinodisc-pulldown-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let source = dir.join("film.mp4");
    let made = Command::new(resolve_binary("ffmpeg"))
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=24000/1001:duration=6",
        ])
        .args([
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=6",
        ])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ])
        .args(["-c:a", "aac", "-shortest"])
        .arg(&source)
        .status()
        .expect("ffmpeg");
    assert!(made.success(), "source clip");

    let request = EncodeRequest {
        input: source,
        output_dir: dir.clone(),
        stem: "film".to_string(),
        video: VideoEncodeConfig {
            video_kbps: 4_000,
            maxrate_kbps: 8_000,
            profile: DvdProfile::D1,
            standard: TvStandard::Ntsc,
            aspect: AspectRatio::SixteenNine,
            gop_frames: 18,
            b_frames: 2,
            interlaced: false,
            top_field_first: true,
            scale_mode: kinodisc_domain::VideoScaleMode::Contain,
            pre_filter: None,
            full_first_pass: false,
            film_pulldown: true,
        },
        audio_tracks: vec![AudioEncodeConfig {
            source_stream: 1,
            bitrate_kbps: 192,
            channels: 2,
            sample_rate: 48_000,
            pre_filter: None,
            gain_db: 0.0,
            delay_ms: 0,
            duration_ms: 0,
            silent: false,
        }],
        trim: None,
        trim_start_ms: None,
        trim_end_ms: None,
        fps: FrameRate::NTSC,
        passlog: dir.join("film.passlog"),
    };
    let outputs = DvdEncoder::default()
        .encode(&request)
        .await
        .expect("encode");

    // The elementary stream: 29.97 in its header, the film's pictures stored
    // once (six seconds are 143 or 144 of them).
    let summary = inspect(&outputs.video_m2v).expect("inspect");
    assert_eq!(summary.frame_rate_code, 4, "{summary:?}");
    assert!(!summary.progressive_sequence, "{summary:?}");
    let pictures = summary.pictures;
    assert!((143..=144).contains(&pictures), "{summary:?}");
    // Every other picture, from the first, lasts three fields.
    assert_eq!(summary.repeated, pictures.div_ceil(2), "{summary:?}");
    assert_eq!(
        summary.fields,
        2 * pictures + pictures.div_ceil(2),
        "{summary:?}"
    );
    assert_eq!(summary.parity_breaks, 0, "{summary:?}");
    assert_eq!(summary.not_progressive, 0, "{summary:?}");
    assert!(summary.max_gop_fields <= NTSC_MAX_GOP_FIELDS, "{summary:?}");

    // What a demuxer makes of it: an NTSC stream, half of whose stored
    // pictures are shown for one field more.
    let rate = ffprobe(
        &[
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=r_frame_rate",
            "-of",
            "csv=p=0",
        ],
        &outputs.muxed_mpg,
    );
    // FFmpeg reports the field rate for a stream with repeat flags.
    assert!(
        matches!(rate.as_str(), "30000/1001" | "60000/1001"),
        "stream rate {rate}"
    );
    let repeats = ffprobe(
        &[
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=repeat_pict",
            "-of",
            "csv=p=0",
        ],
        &outputs.muxed_mpg,
    );
    let repeats: Vec<&str> = repeats
        .lines()
        .map(|line| line.trim_end_matches(','))
        .collect();
    assert_eq!(repeats.len() as u64, pictures, "decoded pictures");
    assert_eq!(
        repeats.iter().filter(|value| **value == "1").count() as u64,
        summary.repeated
    );

    // Six seconds of film stay six seconds on the disc, next to their sound.
    let duration: f64 = ffprobe(
        &["-show_entries", "format=duration", "-of", "csv=p=0"],
        &outputs.muxed_mpg,
    )
    .parse()
    .expect("duration");
    assert!((duration - 6.0).abs() < 0.15, "muxed duration {duration}");
    let first_and_last = ffprobe(
        &[
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
        ],
        &outputs.muxed_mpg,
    );
    // One packet per picture. A program stream does not stamp every one (a
    // pack carries the time of the first picture that starts in it), so the
    // packets are counted and the span is taken between the stamped ones.
    let packets = first_and_last
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count() as u64;
    assert_eq!(packets, pictures, "video packets");
    let mut times: Vec<f64> = first_and_last
        .lines()
        .filter_map(|line| line.trim_end_matches(',').parse().ok())
        .collect();
    times.sort_by(f64::total_cmp);
    assert!(
        times.len() as u64 * 10 >= pictures * 9,
        "{} of {pictures} video packets carry a time",
        times.len()
    );
    let span = times[times.len() - 1] - times[0];
    // From the first picture to the last one: every field but the last
    // picture's. The margin covers a first or last picture without a stamp.
    let fields_before_last = (summary.fields - 3) as f64;
    assert!(
        (span - fields_before_last / 59.94).abs() < 0.07,
        "picture times span {span}"
    );

    let _ = std::fs::remove_dir_all(dir);
}
