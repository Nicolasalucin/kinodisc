//! "Test the quality": encode a few seconds of an episode exactly as the
//! disc will (same encoder, arguments, bitrate from the plan and picture
//! fixes), then compare against a lossless reference scaled the same way.
//! PSNR measures only what the compression loses; the two middle frames go
//! to the side-by-side view.
//!
//! "Try the options" (`compare_options`) encodes the same seconds with each
//! option (denoise, content type, preset, GOP) and estimates what it saves:
//! the current settings are encoded at four bitrates around the plan's, and
//! the bitrate they would need for each option's quality gives the saving.
//! It measures SSIM (PSNR barely moves between presets) against the bits
//! each encode really used.

use crate::picture::{picture_filter, PictureSource, PictureTarget};
use kinodisc_domain::{
    AspectRatio, AuthoringMode, Denoise, DvdProfile, Episode, EpisodeId, FrameRate, Project,
    VideoConfig,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const SAMPLE_SECONDS: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct QualitySample {
    /// Average luma+chroma PSNR of the sample against the source.
    pub psnr_db: f64,
    pub video_kbps: u32,
    pub start_ms: f64,
    pub seconds: f64,
    /// PNG of the source frame (scaled like the disc).
    pub original_png: String,
    /// PNG of the same frame after disc compression.
    pub disc_png: String,
}

/// One option to try on the sample, on top of the current settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SampleOption {
    /// The picture fix "reduce noise" at this level.
    Denoise { level: Denoise },
    /// AVCHD/Blu-ray: another content type.
    Content {
        content: kinodisc_toolchain::ContentType,
    },
    /// AVCHD/Blu-ray: another x264 preset (`fast`, `medium`, `slow`, `slower`).
    Preset {
        encoder: kinodisc_toolchain::EncoderPreference,
    },
    /// AVCHD/Blu-ray: seconds between key frames.
    Gop { seconds: u8 },
}

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct OptionResult {
    pub option: SampleOption,
    /// SSIM against the source, in dB.
    pub ssim_db: f64,
    /// Bits saved for the same quality against the current settings, in
    /// percent (negative: it needs more bits), within ±`SAVING_LIMIT`.
    pub saving_percent: f64,
    pub disc_png: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct OptionComparison {
    pub video_kbps: u32,
    pub start_ms: f64,
    pub seconds: f64,
    /// SSIM (dB) of the current settings at the plan's bitrate.
    pub current_ssim_db: f64,
    pub original_png: String,
    pub current_png: String,
    pub options: Vec<OptionResult>,
}

/// Quality wording for a PSNR value (also used by the UI).
pub fn verdict(psnr_db: f64) -> &'static str {
    if psnr_db >= 42.0 {
        "excellent"
    } else if psnr_db >= 38.0 {
        "veryGood"
    } else if psnr_db >= 34.0 {
        "good"
    } else {
        "visible"
    }
}

/// Parses `average:NN.NN` from FFmpeg's psnr filter (`inf` = identical).
pub fn parse_psnr(stderr: &str) -> Option<f64> {
    let line = stderr.lines().rev().find(|line| line.contains("PSNR"))?;
    let value = &line[line.find("average:")? + "average:".len()..];
    let token: String = value.chars().take_while(|ch| !ch.is_whitespace()).collect();
    if token.starts_with("inf") {
        return Some(99.0);
    }
    token.parse::<f64>().ok()
}

/// Parses `All:0.98 (17.20)` from FFmpeg's ssim filter: the value in dB.
pub fn parse_ssim_db(stderr: &str) -> Option<f64> {
    let line = stderr.lines().rev().find(|line| line.contains("SSIM"))?;
    let all = &line[line.find("All:")? + "All:".len()..];
    let open = all.find('(')?;
    let close = all[open..].find(')')? + open;
    let token = all[open + 1..close].trim();
    if token.starts_with("inf") {
        return Some(99.0);
    }
    token.parse::<f64>().ok()
}

/// Savings are shown within this many percent: past the measured curve the
/// estimate is only an extrapolation.
pub const SAVING_LIMIT: f64 = 75.0;

/// SSIM differences below this are noise on a 10-second sample.
pub const SSIM_NOISE_DB: f64 = 0.15;

/// The bitrate the curve `(kbps, quality)` needs for `quality`, interpolated
/// on log(bitrate) and extended past the ends with the nearest segment.
pub fn equivalent_kbps(curve: &[(f64, f64)], quality: f64) -> Option<f64> {
    let mut points: Vec<(f64, f64)> = curve
        .iter()
        .filter(|(kbps, value)| *kbps > 0.0 && value.is_finite())
        .map(|(kbps, value)| (kbps.ln(), *value))
        .collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    if points.len() < 2 {
        return None;
    }
    let segment = points
        .windows(2)
        .find(|pair| quality <= pair[1].1)
        .unwrap_or(&points[points.len() - 2..]);
    let ((x0, y0), (x1, y1)) = (segment[0], segment[1]);
    if (y1 - y0).abs() < 1e-6 {
        return None;
    }
    Some((x0 + (quality - y0) * (x1 - x0) / (y1 - y0)).exp())
}

/// `1 - kbps / equivalent`, in percent with one decimal, within
/// ±`SAVING_LIMIT`.
pub fn saving_percent(curve: &[(f64, f64)], kbps: f64, quality: f64) -> f64 {
    equivalent_kbps(curve, quality)
        .filter(|equivalent| *equivalent > 0.0)
        .map(|equivalent| {
            let percent = ((1.0 - kbps / equivalent) * 1000.0).round() / 10.0;
            percent.clamp(-SAVING_LIMIT, SAVING_LIMIT)
        })
        .unwrap_or(0.0)
}

async fn ffmpeg(args: Vec<String>) -> Result<String, String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15 * 60),
        tokio::process::Command::from(kinodisc_toolchain::background_command(
            kinodisc_toolchain::resolve_binary("ffmpeg"),
        ))
        .args(&args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output(),
    )
    .await
    .map_err(|_| "FFmpeg tardó demasiado con la muestra.".to_string())?
    .map_err(|error| format!("No se pudo ejecutar FFmpeg: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        Ok(stderr)
    } else {
        Err(format!(
            "FFmpeg falló con la muestra: {}",
            stderr.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
        ))
    }
}

fn input_args(path: &Path, start_ms: u64, seconds: u64) -> Vec<String> {
    vec![
        "-hide_banner".to_string(),
        "-y".to_string(),
        "-ss".to_string(),
        format!("{:.3}", start_ms as f64 / 1000.0),
        "-protocol_whitelist".to_string(),
        crate::MEDIA_PROTOCOL_WHITELIST.to_string(),
        "-i".to_string(),
        path.display().to_string(),
        "-t".to_string(),
        seconds.to_string(),
    ]
}

/// The seconds of the episode a sample covers.
struct Setup<'a> {
    project: &'a Project,
    episode: &'a Episode,
    input: PathBuf,
    source: Option<PictureSource>,
    fps: FrameRate,
    start: u64,
    seconds: u64,
}

/// What one encode of the sample uses.
#[derive(Clone, Copy)]
struct Variant<'a> {
    filters: &'a kinodisc_domain::VideoFilters,
    x264_preset: &'static str,
    content: kinodisc_toolchain::ContentType,
    gop_seconds: Option<u8>,
}

async fn prepare<'a>(
    project: &'a Project,
    project_dir: &Path,
    episode_id: EpisodeId,
    at_ms: u64,
) -> Result<Setup<'a>, String> {
    let episode = project
        .episodes
        .iter()
        .find(|episode| episode.id == episode_id)
        .ok_or("Episodio inexistente.")?;
    let asset = project
        .asset(episode.video)
        .ok_or("El episodio no tiene vídeo.")?;
    let input = crate::asset_path(project_dir, asset).ok_or("Ruta de vídeo inválida.")?;
    if !input.is_file() {
        return Err(format!("No se encuentra «{}».", input.display()));
    }
    let probe = kinodisc_media::probe(&input)
        .await
        .map_err(|error| error.to_string())?;
    let duration = probe.duration_ms.unwrap_or(0);
    let trim_start = episode.trim_start_ms.unwrap_or(0);
    let trim_end = episode.trim_end_ms.unwrap_or(duration).min(duration.max(1));
    let span = trim_end.saturating_sub(trim_start);
    if span < 1_000 {
        return Err("El episodio es demasiado corto para probar la calidad.".to_string());
    }
    let seconds = SAMPLE_SECONDS.min(span / 1000).max(1);
    let start = trim_start + at_ms.min(span.saturating_sub(seconds * 1000));
    let fps = probe
        .video
        .as_ref()
        .map(|video| video.frame_rate)
        .unwrap_or_else(|| project.disc.standard.frame_rate());
    Ok(Setup {
        project,
        episode,
        input,
        source: PictureSource::from_probe(&probe),
        fps,
        start,
        seconds,
    })
}

/// Encodes the sample with `variant` at `video_kbps` as `<stem>` and returns
/// the encoded video and the picture + scale chain of its reference.
async fn encode(
    setup: &Setup<'_>,
    variant: Variant<'_>,
    video_kbps: u32,
    out_dir: &Path,
    stem: &str,
) -> Result<(PathBuf, String), String> {
    let project = setup.project;
    let episode = setup.episode;
    let source = setup.source;
    let trim_start_ms = Some(setup.start);
    let trim_end_ms = Some(setup.start + setup.seconds * 1000);
    match (&project.disc.mode, &project.disc.video) {
        (AuthoringMode::Avchd, VideoConfig::Avchd(config)) => {
            let progressive = !config.resolution.interlaced();
            let pre = picture_filter(
                variant.filters,
                source,
                PictureTarget {
                    display_aspect: 16.0 / 9.0,
                    progressive,
                    scale_mode: episode.scale_mode,
                },
            );
            let blu_ray = project.disc.capacity.is_blu_ray();
            let max = if blu_ray {
                kinodisc_avchd::BLURAY_MAX_VIDEO_KBPS
            } else {
                kinodisc_avchd::AVCHD_MAX_VIDEO_KBPS
            };
            let video_kbps = video_kbps.clamp(1, max);
            let mut config = config.clone();
            if let Some(seconds) = variant.gop_seconds {
                config.gop_seconds = seconds;
            }
            let request = kinodisc_avchd::AvchdEncodeRequest {
                input: setup.input.clone(),
                output_dir: out_dir.to_path_buf(),
                stem: stem.to_string(),
                config: config.clone(),
                disc_capacity: project.disc.capacity,
                aspect: project.disc.aspect,
                video_kbps,
                maxrate_kbps: if blu_ray {
                    config.vbv_maxrate_kbps.min(max).max(video_kbps)
                } else {
                    max
                },
                vbv_bufsize_kbps: if blu_ray {
                    config.vbv_bufsize_kbps.clamp(
                        kinodisc_avchd::BLURAY_VBV_MIN_KBITS,
                        kinodisc_avchd::BLURAY_VBV_MAX_KBITS,
                    )
                } else {
                    max
                },
                audios: Vec::new(),
                subtitles: Vec::new(),
                trim: None,
                trim_start_ms,
                trim_end_ms,
                scale_mode: episode.scale_mode,
                pre_filter: pre.clone(),
                fps: setup.fps,
                duration_ms: setup.seconds * 1000,
                hardware_encoder: None,
                x264_preset: variant.x264_preset,
                video_copy: false,
                passlog: Some(out_dir.join(format!("{stem}-x264"))),
                progressive_source: !crate::picture::keeps_interlacing(
                    variant.filters,
                    source.is_some_and(|source| source.interlaced),
                ),
                content: variant.content,
            };
            let spec = kinodisc_avchd::ResolutionSpec::from_config(&config);
            let scale = kinodisc_avchd::build_filter(&request, spec);
            let encoded =
                kinodisc_avchd::AvchdEncoder::new(kinodisc_toolchain::resolve_binary("ffmpeg"))
                    .encode(&request)
                    .await
                    .map_err(|error| error.to_string())?;
            let chain = match pre {
                Some(pre) => format!("{pre},{scale}"),
                None => scale,
            };
            Ok((encoded.stream_mkv, chain))
        }
        _ => {
            let interlaced = crate::picture::keeps_interlacing(
                variant.filters,
                source.is_some_and(|source| source.interlaced),
            );
            let pre = picture_filter(
                variant.filters,
                source,
                PictureTarget {
                    display_aspect: if project.disc.aspect == AspectRatio::SixteenNine {
                        16.0 / 9.0
                    } else {
                        4.0 / 3.0
                    },
                    progressive: !source.is_some_and(|source| source.interlaced),
                    scale_mode: episode.scale_mode,
                },
            );
            let video_kbps = video_kbps.clamp(1, kinodisc_dvd::DVD_MAX_VIDEO_KBPS);
            let video = kinodisc_dvd::VideoEncodeConfig {
                video_kbps,
                maxrate_kbps: (video_kbps + 3_000).min(kinodisc_dvd::DVD_MAX_VIDEO_KBPS),
                profile: DvdProfile::D1,
                standard: project.disc.standard,
                aspect: project.disc.aspect,
                gop_frames: match project.disc.standard {
                    kinodisc_domain::TvStandard::Ntsc => 18,
                    kinodisc_domain::TvStandard::Pal => 15,
                },
                b_frames: 2,
                interlaced,
                top_field_first: true,
                scale_mode: episode.scale_mode,
                pre_filter: pre.clone(),
                full_first_pass: crate::full_first_pass(project),
            };
            let scale = kinodisc_dvd::build_scale_filter(&video);
            let request = kinodisc_dvd::EncodeRequest {
                input: setup.input.clone(),
                output_dir: out_dir.to_path_buf(),
                stem: stem.to_string(),
                video,
                audio_tracks: Vec::new(),
                trim: None,
                trim_start_ms,
                trim_end_ms,
                fps: setup.fps,
                passlog: out_dir.join(format!("{stem}.passlog")),
            };
            let encoded =
                kinodisc_dvd::DvdEncoder::new(kinodisc_toolchain::resolve_binary("ffmpeg"))
                    .encode(&request)
                    .await
                    .map_err(|error| error.to_string())?;
            let chain = match pre {
                Some(pre) => format!("{pre},{scale}"),
                None => scale,
            };
            Ok((encoded.video_m2v, chain))
        }
    }
}

/// Lossless reference through `chain`.
async fn reference(setup: &Setup<'_>, chain: String, out_dir: &Path) -> Result<PathBuf, String> {
    let reference = out_dir.join("reference.mkv");
    let mut args = input_args(&setup.input, setup.start, setup.seconds);
    args.extend([
        "-an".into(),
        "-vf".into(),
        chain,
        "-c:v".into(),
        "ffv1".into(),
    ]);
    args.push(reference.display().to_string());
    ffmpeg(args).await?;
    Ok(reference)
}

/// Inputs of a metric: a raw .m2v has no timestamps, so it is read at the
/// disc's frame rate to stay frame-aligned with the reference.
fn metric_inputs(setup: &Setup<'_>, video: &Path, reference: &Path) -> Vec<String> {
    let mut args = vec!["-hide_banner".to_string()];
    if video
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("m2v"))
    {
        let rate = setup.project.disc.standard.frame_rate();
        args.extend(["-r".to_string(), format!("{}/{}", rate.num, rate.den)]);
    }
    args.extend([
        "-i".to_string(),
        video.display().to_string(),
        "-i".to_string(),
        reference.display().to_string(),
    ]);
    args
}

async fn psnr(setup: &Setup<'_>, video: &Path, reference: &Path) -> Result<f64, String> {
    let mut args = metric_inputs(setup, video, reference);
    args.extend([
        "-lavfi".into(),
        "[0:v]setsar=1[a];[1:v]setsar=1[b];[a][b]psnr".into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]);
    let metrics = ffmpeg(args).await?;
    let value = parse_psnr(&metrics).ok_or("No se pudo medir la calidad.")?;
    Ok((value * 100.0).round() / 100.0)
}

async fn ssim(setup: &Setup<'_>, video: &Path, reference: &Path) -> Result<f64, String> {
    let mut args = metric_inputs(setup, video, reference);
    args.extend([
        "-lavfi".into(),
        "[0:v]setsar=1[a];[1:v]setsar=1[b];[a][b]ssim".into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]);
    let metrics = ffmpeg(args).await?;
    let value = parse_ssim_db(&metrics).ok_or("No se pudo medir la calidad.")?;
    Ok((value * 1000.0).round() / 1000.0)
}

/// The kb/s an encode really used (its file over the sample's seconds).
fn achieved_kbps(setup: &Setup<'_>, video: &Path, requested: u32) -> f64 {
    std::fs::metadata(video)
        .ok()
        .map(|metadata| metadata.len() as f64 * 8.0 / setup.seconds.max(1) as f64 / 1000.0)
        .filter(|kbps| *kbps > 0.0)
        .unwrap_or(f64::from(requested))
}

/// The middle frame as a square-pixel PNG. Picked by number: a raw .m2v has
/// no timestamps on its B-frames, so seeking by time lands on different
/// frames in each file.
async fn frame_png(setup: &Setup<'_>, video: &Path, png: PathBuf) -> Result<String, String> {
    let disc_fps = match &setup.project.disc.video {
        VideoConfig::Avchd(config) => config.frame_rate.as_f64(),
        _ => setup.project.disc.standard.frame_rate().as_f64(),
    };
    let middle_frame = ((setup.seconds as f64 * disc_fps) / 2.0).round() as u64;
    ffmpeg(vec![
        "-hide_banner".to_string(),
        "-y".to_string(),
        "-i".to_string(),
        video.display().to_string(),
        "-frames:v".to_string(),
        "1".to_string(),
        "-vf".to_string(),
        format!("select=eq(n\\,{middle_frame}),scale=iw*sar:ih,setsar=1"),
        png.display().to_string(),
    ])
    .await?;
    Ok(png.display().to_string())
}

/// Encodes `SAMPLE_SECONDS` of the episode from `at_ms` (offset inside its
/// trimmed range) at `video_kbps` into `out_dir` and measures it. HD samples
/// use `x264_preset` (the encoder the production window recommends), so
/// the sample shows what that choice gives (QA 1.2.3: always `medium`),
/// with the same content tuning, two passes and progressive/interlaced
/// coding as the disc.
#[allow(clippy::too_many_arguments)]
pub async fn quality_sample(
    project: &Project,
    project_dir: &Path,
    episode_id: EpisodeId,
    at_ms: u64,
    video_kbps: u32,
    out_dir: &Path,
    x264_preset: &'static str,
    content: kinodisc_toolchain::ContentType,
) -> Result<QualitySample, String> {
    let setup = prepare(project, project_dir, episode_id, at_ms).await?;
    std::fs::create_dir_all(out_dir).map_err(|error| error.to_string())?;
    let variant = Variant {
        filters: &setup.episode.filters,
        x264_preset,
        content,
        gop_seconds: None,
    };
    let (disc_video, chain) = encode(&setup, variant, video_kbps, out_dir, "sample").await?;
    let reference = reference(&setup, chain, out_dir).await?;
    let psnr_db = psnr(&setup, &disc_video, &reference).await?;
    let original_png = frame_png(&setup, &reference, out_dir.join("original.png")).await?;
    let disc_png = frame_png(&setup, &disc_video, out_dir.join("disc.png")).await?;
    Ok(QualitySample {
        psnr_db,
        video_kbps,
        start_ms: setup.start as f64,
        seconds: setup.seconds as f64,
        original_png,
        disc_png,
    })
}

/// Bitrate steps of the current settings' quality curve.
const CURVE_STEPS: [f64; 4] = [0.6, 0.8, 1.0, 1.3];

/// Encodes the sample with the current settings at four bitrates and with
/// each option at `video_kbps`, and estimates each option's saving. All
/// options are measured against the same reference (the source through the
/// episode's current picture fixes), so a denoise is counted as a change
/// of the picture: the number says what it costs or saves in bits; the
/// pictures show what it does.
#[allow(clippy::too_many_arguments)]
pub async fn compare_options(
    project: &Project,
    project_dir: &Path,
    episode_id: EpisodeId,
    at_ms: u64,
    video_kbps: u32,
    out_dir: &Path,
    x264_preset: &'static str,
    content: kinodisc_toolchain::ContentType,
    options: &[SampleOption],
) -> Result<OptionComparison, String> {
    if options.is_empty() {
        return Err("No hay opciones para probar.".to_string());
    }
    let setup = prepare(project, project_dir, episode_id, at_ms).await?;
    std::fs::create_dir_all(out_dir).map_err(|error| error.to_string())?;
    let current = Variant {
        filters: &setup.episode.filters,
        x264_preset,
        content,
        gop_seconds: None,
    };
    let mut curve = Vec::new();
    let mut current_video = None;
    let mut reference_path = None;
    for (index, step) in CURVE_STEPS.iter().enumerate() {
        let kbps = ((f64::from(video_kbps) * step).round() as u32).max(100);
        let (video, chain) =
            encode(&setup, current, kbps, out_dir, &format!("current-{index}")).await?;
        if reference_path.is_none() {
            reference_path = Some(reference(&setup, chain, out_dir).await?);
        }
        let quality = ssim(&setup, &video, reference_path.as_deref().unwrap_or(&video)).await?;
        let used = achieved_kbps(&setup, &video, kbps);
        if (*step - 1.0).abs() < f64::EPSILON {
            current_video = Some((video, quality));
        }
        curve.push((used, quality));
    }
    let reference = reference_path.ok_or("No se pudo preparar la referencia.")?;
    let (current_video, current_ssim) = current_video.ok_or("Falta la muestra actual.")?;
    let original_png = frame_png(&setup, &reference, out_dir.join("original.png")).await?;
    let current_png = frame_png(&setup, &current_video, out_dir.join("current.png")).await?;
    let hd = matches!(project.disc.video, VideoConfig::Avchd(_));
    let mut results = Vec::new();
    for (index, option) in options.iter().enumerate() {
        let mut filters = setup.episode.filters.clone();
        let mut variant = current;
        match *option {
            SampleOption::Denoise { level } => filters.denoise = level,
            SampleOption::Content { content } if hd => variant.content = content,
            SampleOption::Preset { encoder } if hd => variant.x264_preset = encoder.x264_preset(),
            SampleOption::Gop { seconds } if hd && (1..=3).contains(&seconds) => {
                variant.gop_seconds = Some(seconds)
            }
            // Encoder options do not apply to DVD-Video (MPEG-2).
            _ => continue,
        }
        variant.filters = &filters;
        let stem = format!("option-{index}");
        let (video, _) = encode(&setup, variant, video_kbps, out_dir, &stem).await?;
        let quality = ssim(&setup, &video, &reference).await?;
        let used = achieved_kbps(&setup, &video, video_kbps);
        let disc_png = frame_png(&setup, &video, out_dir.join(format!("{stem}.png"))).await?;
        results.push(OptionResult {
            option: *option,
            ssim_db: quality,
            // Within the run-to-run noise of a 10 s sample (and at a
            // comfortable bitrate, where every option looks alike) it is
            // "no change" rather than a made-up saving.
            saving_percent: if (quality - current_ssim).abs() < SSIM_NOISE_DB {
                0.0
            } else {
                saving_percent(&curve, used, quality)
            },
            disc_png,
        });
    }
    Ok(OptionComparison {
        video_kbps,
        start_ms: setup.start as f64,
        seconds: setup.seconds as f64,
        current_ssim_db: current_ssim,
        original_png,
        current_png,
        options: results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psnr_lines_are_parsed() {
        let stderr =
            "[Parsed_psnr_2 @ 0x1] PSNR y:44.12 u:47.30 v:47.80 average:45.20 min:41.00 max:50.00";
        assert_eq!(parse_psnr(stderr), Some(45.2));
        assert_eq!(
            parse_psnr("[Parsed_psnr_2 @ 0x1] PSNR y:inf u:inf v:inf average:inf min:inf"),
            Some(99.0)
        );
        assert_eq!(parse_psnr("nothing"), None);
    }

    #[test]
    fn verdicts_follow_psnr() {
        assert_eq!(verdict(45.0), "excellent");
        assert_eq!(verdict(39.0), "veryGood");
        assert_eq!(verdict(35.0), "good");
        assert_eq!(verdict(30.0), "visible");
    }

    #[test]
    fn ssim_lines_are_parsed() {
        let stderr = "[Parsed_ssim_2 @ 0x1] SSIM Y:0.98 (17.0) U:0.99 (20.0) V:0.99 (20.0) All:0.985 (18.24)";
        assert_eq!(parse_ssim_db(stderr), Some(18.24));
        assert_eq!(parse_ssim_db("nothing"), None);
    }

    #[test]
    fn savings_come_from_the_quality_curve() {
        // 1 dB per doubling of the bitrate.
        let curve = [
            (500.0, 30.0),
            (1000.0, 31.0),
            (2000.0, 32.0),
            (4000.0, 33.0),
        ];
        // The current quality at the same bitrate saves nothing.
        assert_eq!(saving_percent(&curve, 1000.0, 31.0), 0.0);
        // The quality of 2000 kb/s reached at 1000 kb/s: half the bits.
        assert_eq!(saving_percent(&curve, 1000.0, 32.0), 50.0);
        // Worse than the current settings: more bits, never past the limit.
        assert_eq!(saving_percent(&curve, 1000.0, 30.0), -SAVING_LIMIT);
        assert_eq!(saving_percent(&curve, 1000.0, 20.0), -SAVING_LIMIT);
        // Past the top of the curve: extended, then limited.
        assert_eq!(saving_percent(&curve, 1000.0, 34.0), SAVING_LIMIT);
        assert_eq!(saving_percent(&[(1000.0, 31.0)], 1000.0, 32.0), 0.0);
    }
}
