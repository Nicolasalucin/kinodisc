//! AVCHD/BDMV engine: H.264 encoding, AC-3 audio, concatenation of the
//! episodes and the built-in BDMV muxer (`native`). Menus are HDMV
//! Interactive Graphics streams compiled by `kinodisc-hdmv`.

use kinodisc_domain::{
    AspectRatio, AvchdResolution, AvchdVideoConfig, Diagnostic, DiscCapacity, FrameRange,
    FrameRate, H264Level, HdScan, VideoScaleMode,
};
use kinodisc_toolchain::{resolve_binary, CpuUsage, MEDIA_PROTOCOL_WHITELIST};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

pub mod native;

pub const CRATE_NAME: &str = "kinodisc-avchd";

/// AVCHD on DVD media allows 18 Mb/s of video (24 Mb/s is the memory-card
/// figure); Blu-ray media keep `BLURAY_MAX_VIDEO_KBPS`.
pub const AVCHD_MAX_VIDEO_KBPS: u32 = 18_000;
pub const BLURAY_MAX_VIDEO_KBPS: u32 = 38_000;
pub const BLURAY_VBV_MIN_KBITS: u32 = 30_000;
pub const BLURAY_VBV_MAX_KBITS: u32 = 40_000;
pub const AVCHD_TS_OVERHEAD_PPM: u32 = 50_000;
pub const AVCHD_HEADER_MARGIN_PPM: u32 = 30_000;
pub const AVCHD_UDF_REVISION: &str = "2.50";
pub const AVCHD_VIDEO_CODEC: &str = "h264";
pub const AVCHD_AUDIO_CODEC: &str = "ac3";
pub const AVCHD_X264_LEVEL: &str = "4.1";
/// A single FFmpeg stage may legitimately run for hours on slow hardware, but
/// an unresponsive process must not block a render forever.
pub const AVCHD_STAGE_TIMEOUT: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionSpec {
    pub width: u32,
    pub height: u32,
    pub interlaced: bool,
    /// Progressive pictures flagged as interlaced (1080p25/29.97 on Blu-ray).
    pub psf: bool,
}

impl ResolutionSpec {
    pub const fn from_resolution(resolution: AvchdResolution) -> Self {
        let (width, height) = resolution.dimensions();
        Self {
            width,
            height,
            interlaced: resolution.interlaced(),
            psf: false,
        }
    }

    pub fn from_config(config: &AvchdVideoConfig) -> Self {
        Self {
            psf: config.scan() == HdScan::Psf,
            ..Self::from_resolution(config.resolution)
        }
    }
}

/// x264/FFmpeg level string for an AVCHD/Blu-ray configuration.
pub fn x264_level(config: &AvchdVideoConfig) -> &'static str {
    match config.effective_level() {
        H264Level::Level41 => "4.1",
        H264Level::Level42 => "4.2",
    }
}

pub fn header_margin_bytes(capacity: DiscCapacity) -> u64 {
    (capacity.nominal_bytes() as u128 * AVCHD_HEADER_MARGIN_PPM as u128 / 1_000_000) as u64
}

pub fn ts_overhead_bytes(bytes: u64) -> u64 {
    (bytes as u128 * AVCHD_TS_OVERHEAD_PPM as u128 / 1_000_000) as u64
}

/// PAT and PMT ten times a second plus SDT twice (FFmpeg's M2TS defaults,
/// the native backend): 22 TS packets of 192 bytes every second.
pub const TS_TABLE_BYTES_PER_SEC: u64 = 22 * 192;
/// PES header and stuffing of the last TS packet: about 0.56 packet
/// (≈108 bytes) per video frame.
pub const TS_BYTES_PER_VIDEO_FRAME: u64 = 108;
/// Audio PES (several AC-3 frames each) add about 4 % to the audio.
pub const TS_AUDIO_EXTRA_PPM: u64 = 40_000;

/// The bytes an episode's M2TS adds to its elementary streams. On top of the
/// proportional TS packet cost (`AVCHD_TS_OVERHEAD_PPM`), the M2TS has a
/// fixed cost per second and per frame that does not shrink with the
/// bitrate: about 64 kbps at 29.97 fps. Measured on a 19.6-minute 480i
/// episode at 361 + 96 kbps (QA 1.2.4): the MKV streams were 67.07 MB, the
/// M2TS 79.10 MB (+17.9 %, against the 5 % assumed before; a 64-episode
/// DVD-5 plan came out at 5.08 GB). This estimate gives 79.75 MB.
pub fn ts_overhead_bytes_for(
    video_bytes: u64,
    audio_bytes: u64,
    duration_ms: u64,
    frame_rate: FrameRate,
) -> u64 {
    let proportional = ts_overhead_bytes(video_bytes.saturating_add(audio_bytes));
    let frames = if frame_rate.is_valid() {
        (duration_ms as f64 * frame_rate.as_f64() / 1000.0).ceil() as u64
    } else {
        duration_ms.div_ceil(1000) * 30
    };
    let fixed = duration_ms.div_ceil(1000) * TS_TABLE_BYTES_PER_SEC
        + frames * TS_BYTES_PER_VIDEO_FRAME
        + (audio_bytes as u128 * TS_AUDIO_EXTRA_PPM as u128 / 1_000_000) as u64;
    proportional.saturating_add(fixed)
}

pub fn config_errors(config: &AvchdVideoConfig) -> Vec<Diagnostic> {
    config
        .validate()
        .into_iter()
        .filter(Diagnostic::is_error)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvchdAudioCodec {
    Ac3,
    Dts,
    Lpcm,
}

#[derive(Debug, Clone)]
pub struct AvchdAudioConfig {
    pub source_stream: u32,
    pub bitrate_kbps: u16,
    pub channels: u8,
    pub sample_rate: u32,
    pub pre_filter: Option<String>,
    pub gain_db: f32,
    pub delay_ms: i32,
    pub duration_ms: u64,
    pub language: String,
    /// Stream-copy an already compatible DTS/LPCM/AC-3 stream.
    pub copy: bool,
    /// Codec the track has on the disc (stream copy or AC-3 encode).
    pub codec: AvchdAudioCodec,
    pub default: bool,
    pub commentary: bool,
    /// Silence instead of a source stream: a video without sound still gets
    /// the tracks the rest of the disc has (every title must share them).
    pub silent: bool,
}

/// One bitmap subtitle track passed through to the H.264/AC-3 MKV. Only
/// `hdmv_pgs_subtitle` tracks are valid here; the render layer filters them.
#[derive(Debug, Clone)]
pub struct AvchdSubtitleConfig {
    /// Embedded PGS stream index in the source video.
    pub source_stream: Option<u32>,
    /// External `.sup` file. Exactly one of `source_stream`/`source_path` is set.
    pub source_path: Option<PathBuf>,
    pub language: String,
    pub forced: bool,
    pub default: bool,
    pub delay_ms: i32,
}

#[derive(Debug, Clone)]
pub struct AvchdEncodeRequest {
    pub input: PathBuf,
    pub output_dir: PathBuf,
    pub stem: String,
    pub config: AvchdVideoConfig,
    pub disc_capacity: DiscCapacity,
    pub aspect: AspectRatio,
    pub video_kbps: u32,
    pub maxrate_kbps: u32,
    pub vbv_bufsize_kbps: u32,
    pub audios: Vec<AvchdAudioConfig>,
    pub subtitles: Vec<AvchdSubtitleConfig>,
    pub trim: Option<FrameRange>,
    pub trim_start_ms: Option<u64>,
    pub trim_end_ms: Option<u64>,
    pub scale_mode: VideoScaleMode,
    /// Runs before the scale chain: what the source's colour matrix is when
    /// the file does not say (`DiscColour::source_filter`), then the picture
    /// fixes.
    pub pre_filter: Option<String>,
    pub fps: FrameRate,
    pub duration_ms: u64,
    /// Resolved hardware encoder, or `None` for the CPU/libx264 path.
    pub hardware_encoder: Option<kinodisc_toolchain::HardwareEncoder>,
    /// x264 preset for the CPU path (`fast`, `medium` or `slow`).
    pub x264_preset: &'static str,
    /// Copy the compliant source video stream instead of re-encoding it.
    pub video_copy: bool,
    /// x264 two-pass statistics file (job-scoped). `Some` encodes the CPU
    /// path in two passes; hardware encoders and stream copy stay single.
    pub passlog: Option<PathBuf>,
    /// The source is progressive (or film-based): on 480i/576i the frames
    /// are coded progressive and flagged interlaced (PsF), like 1080p25/30,
    /// instead of MBAFF, which costs bits and ignores `fake-interlaced`.
    pub progressive_source: bool,
    /// Encoder tuning for the kind of picture.
    pub content: kinodisc_toolchain::ContentType,
}

/// Which pass of a two-pass encode, or a single pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodePass {
    Single,
    /// Analysis only: video statistics, no output file.
    First,
    Second,
}

/// One-second GOP (Blu-ray/AVCHD maximum): 24 at 23.976, 30 at 29.97.
pub fn gop_frames(config: &AvchdVideoConfig) -> u32 {
    let seconds = u32::from(config.gop_seconds.clamp(1, 3));
    (config.frame_rate.as_f64().round() as u32 * seconds).max(1)
}

/// Whether `request` codes interlaced fields (MBAFF) rather than
/// progressive frames flagged as interlaced.
pub fn codes_interlaced(request: &AvchdEncodeRequest) -> bool {
    request.config.resolution.interlaced() && !request.progressive_source
}

/// Whether the CPU path encodes `request` in two passes.
pub fn two_pass(request: &AvchdEncodeRequest) -> bool {
    request.passlog.is_some() && !request.video_copy && request.hardware_encoder.is_none()
}

impl AvchdEncodeRequest {
    pub fn stream_mkv(&self) -> PathBuf {
        self.output_dir.join(format!("{}.mkv", self.stem))
    }

    pub fn validate(&self) -> Result<(), AvchdError> {
        if self.stem.trim().is_empty() || self.stem.contains('/') || self.stem.contains('\\') {
            return Err(AvchdError::InvalidConfig(
                "el nombre base de salida no es válido".to_string(),
            ));
        }
        let errors = self.config.validate_for_capacity(self.disc_capacity);
        let errors: Vec<_> = errors.iter().filter(|item| item.is_error()).collect();
        if !errors.is_empty() {
            return Err(AvchdError::InvalidConfig(
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        let max_video = if self.disc_capacity.is_blu_ray() {
            BLURAY_MAX_VIDEO_KBPS
        } else {
            AVCHD_MAX_VIDEO_KBPS
        };
        if self.video_kbps == 0 || self.video_kbps > max_video {
            return Err(AvchdError::InvalidConfig(format!(
                "bitrate de video fuera de rango: {} kbps (máximo {})",
                self.video_kbps, max_video
            )));
        }
        if self.maxrate_kbps < self.video_kbps || self.maxrate_kbps > max_video {
            return Err(AvchdError::InvalidConfig(format!(
                "maxrate inválido: {} kbps (máximo {})",
                self.maxrate_kbps, max_video
            )));
        }
        if self.vbv_bufsize_kbps == 0 {
            return Err(AvchdError::InvalidConfig(
                "el VBV bufsize no puede ser cero".to_string(),
            ));
        }
        if self.duration_ms == 0 {
            return Err(AvchdError::InvalidConfig(
                "la duración AVCHD no puede ser cero".to_string(),
            ));
        }
        for audio in &self.audios {
            if audio.copy {
                continue;
            }
            if !kinodisc_domain::ac3_bitrate_supported(audio.bitrate_kbps, audio.channels) {
                return Err(AvchdError::InvalidConfig(format!(
                    "bitrate AC-3 no soportado: {} kbps",
                    audio.bitrate_kbps
                )));
            }
            if audio.sample_rate != 48_000 {
                return Err(AvchdError::InvalidConfig(
                    "el audio AVCHD debe ser AC-3 a 48 kHz".to_string(),
                ));
            }
            if !matches!(audio.channels, 1 | 2 | 6) {
                return Err(AvchdError::InvalidConfig(
                    "el audio AVCHD admite 1, 2 o 6 canales".to_string(),
                ));
            }
        }
        for subtitle in &self.subtitles {
            if subtitle.source_stream.is_some() == subtitle.source_path.is_some() {
                return Err(AvchdError::InvalidConfig(
                    "cada subtítulo AVCHD debe tener exactamente un origen".to_string(),
                ));
            }
        }
        if let Some(trim) = self.trim {
            if !trim.is_valid() {
                return Err(AvchdError::InvalidConfig(
                    "el rango de recorte no es válido".to_string(),
                ));
            }
            if !self.fps.is_valid() {
                return Err(AvchdError::InvalidConfig(
                    "el framerate del recorte no es válido".to_string(),
                ));
            }
        }
        if let (Some(start), Some(end)) = (self.trim_start_ms, self.trim_end_ms) {
            if end <= start {
                return Err(AvchdError::InvalidConfig(
                    "el fin del recorte debe ser posterior al inicio".to_string(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AvchdOutputs {
    pub stream_mkv: PathBuf,
}

#[derive(Debug, Clone)]
pub struct EncodeProgress {
    pub stage: &'static str,
    pub out_time_us: u64,
    pub speed_x: Option<f32>,
    pub fps: Option<f32>,
}

pub type EncodeProgressSink = mpsc::Sender<EncodeProgress>;

#[derive(Debug, Error)]
pub enum AvchdError {
    #[error("configuración AVCHD inválida: {0}")]
    InvalidConfig(String),
    #[error("error de E/S: {0}")]
    Io(#[from] std::io::Error),
    #[error("FFmpeg falló en {stage} con código {code}: {stderr}")]
    Ffmpeg {
        stage: &'static str,
        code: i32,
        stderr: String,
    },
    #[error("no se generó la salida esperada: {0}")]
    MissingOutput(String),
    #[error("la operación AVCHD excedió el tiempo máximo")]
    Timeout,
}

#[derive(Debug, Clone)]
pub struct AvchdEncoder {
    pub ffmpeg: PathBuf,
    /// Threads and priority of the encoder processes (never the output).
    pub cpu: CpuUsage,
}

impl Default for AvchdEncoder {
    fn default() -> Self {
        Self::new(resolve_binary("ffmpeg"))
    }
}

impl AvchdEncoder {
    pub fn new(ffmpeg: impl Into<PathBuf>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            cpu: CpuUsage::default(),
        }
    }

    pub fn with_cpu(mut self, cpu: CpuUsage) -> Self {
        self.cpu = cpu;
        self
    }

    pub async fn encode(&self, request: &AvchdEncodeRequest) -> Result<AvchdOutputs, AvchdError> {
        self.encode_with_progress(request, None).await
    }

    pub async fn encode_with_progress(
        &self,
        request: &AvchdEncodeRequest,
        progress: Option<EncodeProgressSink>,
    ) -> Result<AvchdOutputs, AvchdError> {
        request.validate()?;
        std::fs::create_dir_all(&request.output_dir)?;

        if two_pass(request) {
            let first = build_encode_args_for(request, EncodePass::First);
            run_stage(
                &self.ffmpeg,
                self.cpu,
                &first,
                "avchd-pass1",
                progress.as_ref(),
            )
            .await?;
            let second = build_encode_args_for(request, EncodePass::Second);
            run_stage(
                &self.ffmpeg,
                self.cpu,
                &second,
                "avchd-pass2",
                progress.as_ref(),
            )
            .await?;
        } else {
            let args = build_encode_args(request);
            run_stage(&self.ffmpeg, self.cpu, &args, "avchd", progress.as_ref()).await?;
        }

        Ok(AvchdOutputs {
            stream_mkv: request.stream_mkv(),
        })
    }
}

/// Appends a local file input with an explicit protocol whitelist so crafted
/// playlists or IPC paths cannot make FFmpeg open remote resources.
fn push_file_input(args: &mut Vec<String>, path: &Path) {
    args.extend([
        "-protocol_whitelist".to_string(),
        MEDIA_PROTOCOL_WHITELIST.to_string(),
        "-i".to_string(),
        path.display().to_string(),
    ]);
}

fn audio_filter_chain(audio: &AvchdAudioConfig) -> Option<String> {
    let mut filters = Vec::new();
    if let Some(pre_filter) = &audio.pre_filter {
        if !pre_filter.trim().is_empty() {
            filters.push(pre_filter.clone());
        }
    }
    if audio.gain_db.abs() > 0.01 {
        filters.push(format!("volume={:.3}dB", audio.gain_db));
    }
    if audio.delay_ms > 0 {
        filters.push(format!("adelay={}:all=1", audio.delay_ms));
    } else if audio.delay_ms < 0 {
        let mut filter = format!(
            "atrim=start={:.3},asetpts=PTS-STARTPTS",
            audio.delay_ms.unsigned_abs() as f64 / 1000.0
        );
        if audio.duration_ms > 0 {
            filter.push_str(&format!(
                ",apad=whole_dur={:.3}",
                audio.duration_ms as f64 / 1000.0
            ));
        }
        filters.push(filter);
    }
    (!filters.is_empty()).then(|| filters.join(","))
}

pub fn build_encode_args(request: &AvchdEncodeRequest) -> Vec<String> {
    build_encode_args_for(request, EncodePass::Single)
}

/// FFmpeg arguments for one pass. The first pass keeps only the video and
/// writes nothing but the x264 statistics.
pub fn build_encode_args_for(request: &AvchdEncodeRequest, pass: EncodePass) -> Vec<String> {
    let mut args = vec!["-hide_banner".to_string(), "-y".to_string()];
    if request.trim_start_ms.is_some() || request.trim_end_ms.is_some() {
        if let Some(start) = request.trim_start_ms {
            if start > 0 {
                args.push("-ss".to_string());
                args.push(format_millis(start));
            }
        }
    } else if let Some(trim) = request.trim {
        args.push("-ss".to_string());
        args.push(format_seconds(trim.start, request.fps));
    }
    push_file_input(&mut args, &request.input);
    let silence = request.audios.is_empty() || request.audios.iter().all(|audio| audio.silent);
    if silence {
        args.extend([
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "anullsrc=r=48000:cl=stereo".to_string(),
        ]);
    }
    let mut external_inputs = Vec::new();
    let mut next_input = if silence { 2usize } else { 1usize };
    for subtitle in &request.subtitles {
        if let Some(path) = &subtitle.source_path {
            if subtitle.delay_ms != 0 {
                args.extend([
                    "-itsoffset".to_string(),
                    format!("{:.3}", f64::from(subtitle.delay_ms) / 1000.0),
                ]);
            }
            args.extend(["-f".to_string(), "sup".to_string()]);
            push_file_input(&mut args, path);
            external_inputs.push(next_input);
            next_input += 1;
        }
    }
    let duration_ms = if request.trim_end_ms.is_some() {
        let start = request.trim_start_ms.unwrap_or(0);
        request
            .trim_end_ms
            .unwrap_or(request.duration_ms)
            .saturating_sub(start)
            .max(1)
    } else {
        let base = request.duration_ms.max(1);
        match request.trim {
            Some(trim) if request.fps.is_valid() => {
                let trim_ms = ((trim.duration() as f64 * 1000.0) / request.fps.as_f64())
                    .round()
                    .max(1.0) as u64;
                base.min(trim_ms).max(1)
            }
            _ => base,
        }
    };
    args.extend(["-t".to_string(), format_millis(duration_ms)]);

    if request.video_copy {
        // Smart Stream Copy: the planner verified codec, profile, level,
        // geometry, rate, scan and bitrate. Only the container changes.
        args.extend(["-c:v".to_string(), "copy".to_string()]);
    } else {
        let spec = ResolutionSpec::from_config(&request.config);
        let mut chain = Vec::new();
        if let Some(pre) = &request.pre_filter {
            chain.push(pre.clone());
        }
        chain.push(build_filter(request, spec));
        let mut filter = chain.join(",");
        let keyint = gop_frames(&request.config);
        let interlaced = codes_interlaced(request);
        // Progressive frames on an interlaced format, flagged with pic_struct.
        let psf = spec.psf || (spec.interlaced && !interlaced);
        let maxrate = request.maxrate_kbps.max(request.video_kbps);
        let bufsize = request.vbv_bufsize_kbps.max(1);
        // Hardware encoders cannot emit interlaced or PsF (fake-interlaced)
        // streams: those stay on x264.
        let hardware = request
            .hardware_encoder
            .filter(|_| !spec.psf && !spec.interlaced);
        let disc = disc_colour(request.config.resolution);
        let colour = disc.matrix();
        let level = x264_level(&request.config);
        let codec = hardware
            .map(kinodisc_toolchain::HardwareEncoder::ffmpeg_name)
            .unwrap_or("libx264");
        if interlaced {
            // Field order for the encoder; `-top` is gone in FFmpeg 9.
            filter.push_str(",setfield=tff");
        }
        if hardware == Some(kinodisc_toolchain::HardwareEncoder::VaapiH264) {
            filter.push_str(",format=nv12,hwupload");
        }
        args.extend([
            "-vf".to_string(),
            filter,
            "-c:v".to_string(),
            codec.to_string(),
        ]);
        if let Some(hardware) = hardware {
            if hardware == kinodisc_toolchain::HardwareEncoder::NvencH264 {
                args.extend(["-preset".to_string(), "p5".to_string()]);
            } else if hardware == kinodisc_toolchain::HardwareEncoder::QsvH264 {
                args.extend(["-preset".to_string(), "medium".to_string()]);
            }
            args.extend([
                "-profile:v".to_string(),
                "high".to_string(),
                "-level:v".to_string(),
                level.to_string(),
                "-pix_fmt".to_string(),
                "yuv420p".to_string(),
                "-b:v".to_string(),
                format!("{}k", request.video_kbps),
                "-maxrate".to_string(),
                format!("{maxrate}k"),
                "-bufsize".to_string(),
                format!("{bufsize}k"),
                "-g".to_string(),
                keyint.to_string(),
                "-bf".to_string(),
                if hardware == kinodisc_toolchain::HardwareEncoder::VaapiH264 {
                    "0".to_string()
                } else {
                    "3".to_string()
                },
            ]);
        } else {
            // bluray-compat caps B-frames at 3 (strict pyramid), refs at 6
            // and weighted P at 1; level 4.1 holds 4 reference frames at
            // 1080 lines.
            let mut x264 = format!(
                "bluray-compat=1:open-gop=0:slices=4:nal-hrd=vbr:vbv-maxrate={maxrate}:vbv-bufsize={bufsize}:aud=1:colorprim={colour}:transfer={colour}:colormatrix={colour}:keyint={keyint}:min-keyint={keyint}:scenecut=0:b-pyramid=strict"
            );
            if spec.height >= 1080 {
                x264.push_str(":ref=4");
            }
            if interlaced {
                x264.push_str(":tff=1");
            } else if psf {
                x264.push_str(":fake-interlaced=1:pic-struct=1");
            }
            if pass != EncodePass::Single {
                if let Some(passlog) = &request.passlog {
                    args.extend([
                        "-pass".to_string(),
                        if pass == EncodePass::First { "1" } else { "2" }.to_string(),
                        "-passlogfile".to_string(),
                        passlog.display().to_string(),
                    ]);
                }
            }
            if let Some(tune) = request.content.x264_tune() {
                args.extend(["-tune".to_string(), tune.to_string()]);
            }
            args.extend([
                "-preset".to_string(),
                request.x264_preset.to_string(),
                "-profile:v".to_string(),
                "high".to_string(),
                "-level:v".to_string(),
                level.to_string(),
                "-pix_fmt".to_string(),
                "yuv420p".to_string(),
                "-b:v".to_string(),
                format!("{}k", request.video_kbps),
                "-maxrate".to_string(),
                format!("{maxrate}k"),
                "-bufsize".to_string(),
                format!("{bufsize}k"),
                "-g".to_string(),
                keyint.to_string(),
                "-bf".to_string(),
                "3".to_string(),
                "-x264-params".to_string(),
                x264,
            ]);
        }
        // The disc's matrix and limited range: FFmpeg converts in the chain.
        args.extend(disc.output_args());
        if interlaced {
            args.extend(["-flags".to_string(), "+ildct+ilme".to_string()]);
        }
        if pass == EncodePass::First {
            // Statistics only: no audio, subtitles or file.
            args.extend([
                "-map".to_string(),
                "0:v:0".to_string(),
                "-an".to_string(),
                "-sn".to_string(),
                "-dn".to_string(),
                "-progress".to_string(),
                "pipe:1".to_string(),
                "-nostats".to_string(),
                "-f".to_string(),
                "null".to_string(),
                if cfg!(windows) { "NUL" } else { "/dev/null" }.to_string(),
            ]);
            return args;
        }
    }
    args.extend(["-map".to_string(), "0:v:0".to_string()]);
    if request.audios.is_empty() {
        args.extend(["-map".to_string(), "1:a:0".to_string()]);
    } else {
        for audio in &request.audios {
            let source = if audio.silent {
                "1:a:0".to_string()
            } else {
                format!("0:{}", audio.source_stream)
            };
            args.extend(["-map".to_string(), source]);
        }
    }
    let mut external_cursor = 0usize;
    for subtitle in &request.subtitles {
        let source = if let Some(stream) = subtitle.source_stream {
            format!("0:{stream}")
        } else {
            let input = external_inputs.get(external_cursor).copied().unwrap_or(1);
            external_cursor += 1;
            format!("{input}:s:0")
        };
        args.extend(["-map".to_string(), source]);
    }
    if request.audios.is_empty() {
        args.extend([
            "-c:a".to_string(),
            "ac3".to_string(),
            "-b:a".to_string(),
            format!("{AVCHD_DEFAULT_AUDIO_KBPS}k"),
            "-ar".to_string(),
            "48000".to_string(),
            "-ac".to_string(),
            "2".to_string(),
        ]);
    } else {
        for (index, audio) in request.audios.iter().enumerate() {
            if audio.copy {
                args.extend([format!("-c:a:{index}"), "copy".to_string()]);
            } else {
                args.extend([
                    format!("-c:a:{index}"),
                    "ac3".to_string(),
                    format!("-b:a:{index}"),
                    format!("{}k", audio.bitrate_kbps),
                    format!("-ar:a:{index}"),
                    audio.sample_rate.to_string(),
                    format!("-ac:a:{index}"),
                    audio.channels.to_string(),
                ]);
                if let Some(filters) = audio_filter_chain(audio) {
                    args.extend([format!("-filter:a:{index}"), filters]);
                }
            }
            args.extend([
                format!("-metadata:s:a:{index}"),
                format!("language={}", normalize_language(&audio.language)),
            ]);
            if audio.default {
                args.extend([format!("-disposition:a:{index}"), "default".to_string()]);
            }
            if audio.commentary {
                args.extend([format!("-disposition:a:{index}"), "comment".to_string()]);
            }
        }
    }
    if request.subtitles.is_empty() {
        args.push("-sn".to_string());
    } else {
        args.push("-c:s".to_string());
        args.push("copy".to_string());
        for (index, subtitle) in request.subtitles.iter().enumerate() {
            args.push(format!("-metadata:s:s:{index}"));
            args.push(format!(
                "language={}",
                normalize_language(&subtitle.language)
            ));
            if subtitle.forced || subtitle.default {
                let mut disposition = Vec::new();
                if subtitle.default {
                    disposition.push("default");
                }
                if subtitle.forced {
                    disposition.push("forced");
                }
                args.push(format!("-disposition:s:{index}"));
                args.push(disposition.join("+"));
            }
        }
    }
    args.extend([
        "-dn".to_string(),
        "-progress".to_string(),
        "pipe:1".to_string(),
        "-nostats".to_string(),
        "-f".to_string(),
        "matroska".to_string(),
        request.stream_mkv().display().to_string(),
    ]);
    args
}

pub const AVCHD_DEFAULT_AUDIO_KBPS: u16 = 192;

/// Colour description: BT.601 for SD (525 lines SMPTE 170M, 625 lines
/// BT.470BG), BT.709 for HD.
pub fn disc_colour(resolution: AvchdResolution) -> kinodisc_toolchain::DiscColour {
    use kinodisc_toolchain::DiscColour;
    match resolution {
        AvchdResolution::Sd480i => DiscColour::Ntsc601,
        AvchdResolution::Sd576i => DiscColour::Pal601,
        _ => DiscColour::Hd709,
    }
}

/// Whether a stream copied to a disc of `resolution` has the disc's colour
/// matrix already. `tag` is FFprobe's `color_space`. A stream that names no
/// matrix is read as the disc's, like every source of that frame size; one
/// that names another has to be re-encoded to be converted. A copied stream
/// is not retagged.
pub fn copy_keeps_disc_matrix(tag: Option<&str>, resolution: AvchdResolution) -> bool {
    use kinodisc_toolchain::DiscColour;
    if !kinodisc_toolchain::has_matrix_tag(tag) {
        return true;
    }
    let tag = tag.unwrap_or_default().trim();
    match disc_colour(resolution) {
        DiscColour::Hd709 => tag == "bt709",
        // The 525-line and 625-line names share one matrix.
        DiscColour::Ntsc601 | DiscColour::Pal601 => matches!(tag, "smpte170m" | "bt470bg"),
    }
}

/// The scale chain for an episode. Fitting (letterbox, crop) happens on the
/// display canvas (854x480 for 16:9 480i, 1024x576 for 16:9 576i; the frame
/// itself for HD), then the picture is squeezed to the stored frame and
/// tagged with its sample aspect ratio, like the DVD path. QA 1.2.4: SD used
/// to fit on the square-pixel 720x480 frame (a 16:9 film got 36-pixel bars
/// and was stretched by 32/27) and wrote `setsar=32:27`, which FFmpeg reads
/// as `sar=32` with `max=27`, so the stream said 27:1.
pub fn build_filter(request: &AvchdEncodeRequest, spec: ResolutionSpec) -> String {
    use kinodisc_toolchain::{fit_filter, FrameFit};
    let interlaced = codes_interlaced(request);
    let sar = sar_for(request.config.resolution, request.aspect);
    let (width, height) = (spec.width, spec.height);
    let fps = format!(
        "fps={}/{}",
        request.config.frame_rate.num, request.config.frame_rate.den
    );
    let fit = match request.scale_mode {
        VideoScaleMode::Letterbox | VideoScaleMode::Contain => FrameFit::Contain,
        VideoScaleMode::CropFill | VideoScaleMode::Cover => FrameFit::Cover,
        VideoScaleMode::Stretch => FrameFit::Stretch,
    };
    // The stored frame as it is shown: 720x480 with 32:27 samples is 16:9.
    // One resample, straight to the stored frame (see `fit_filter`).
    let scale = fit_filter(
        width,
        height,
        (width * sar.0, height * sar.1),
        fit,
        interlaced,
    );
    let tag = disc_colour(request.config.resolution).tag_filter();
    format!(
        "{scale},setsar={}/{},{fps},format=yuv420p,{tag}",
        sar.0, sar.1
    )
}

fn format_millis(millis: u64) -> String {
    format!("{:.3}", millis as f64 / 1000.0)
}

/// Sample aspect ratio of an AVCHD/Blu-ray picture (square pixels in HD).
pub fn sample_aspect(resolution: AvchdResolution, aspect: AspectRatio) -> (u32, u32) {
    sar_for(resolution, aspect)
}

fn sar_for(resolution: AvchdResolution, aspect: AspectRatio) -> (u32, u32) {
    match resolution {
        AvchdResolution::R1080p | AvchdResolution::R1080i | AvchdResolution::R720p => (1, 1),
        AvchdResolution::Sd480i => {
            if aspect == AspectRatio::SixteenNine {
                (32, 27)
            } else {
                (8, 9)
            }
        }
        AvchdResolution::Sd576i => {
            if aspect == AspectRatio::SixteenNine {
                (64, 45)
            } else {
                (16, 15)
            }
        }
    }
}

fn format_seconds(frames: u64, fps: FrameRate) -> String {
    format!("{:.6}", frames as f64 / fps.as_f64())
}

async fn run_stage(
    ffmpeg: &Path,
    cpu: CpuUsage,
    args: &[String],
    stage: &'static str,
    progress: Option<&EncodeProgressSink>,
) -> Result<(), AvchdError> {
    let args = kinodisc_toolchain::with_thread_limit(args, cpu.encoder_threads());
    let mut child = Command::from(kinodisc_toolchain::encoder_command(ffmpeg, cpu))
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AvchdError::InvalidConfig("no se pudo capturar stdout".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AvchdError::InvalidConfig("no se pudo capturar stderr".to_string()))?;
    let (progress_task, drain_task) = if let Some(sink) = progress {
        let sink = sink.clone();
        (
            Some(tokio::spawn(read_encode_progress(stdout, stage, sink))),
            None,
        )
    } else {
        (None, Some(tokio::spawn(drain_stdout(stdout))))
    };
    let stderr_task = tokio::spawn(read_stderr(stderr));
    let status = match timeout(AVCHD_STAGE_TIMEOUT, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            let _ = child.kill().await;
            if let Some(task) = &progress_task {
                task.abort();
            }
            if let Some(task) = &drain_task {
                task.abort();
            }
            stderr_task.abort();
            return Err(AvchdError::Timeout);
        }
    };
    if let Some(task) = progress_task {
        let _ = task.await;
    }
    if let Some(task) = drain_task {
        let _ = task.await;
    }
    let stderr_log = stderr_task.await.unwrap_or_default();
    if status.success() {
        Ok(())
    } else {
        Err(AvchdError::Ffmpeg {
            stage,
            code: status.code().unwrap_or(-1),
            stderr: tail(&stderr_log, 4_000),
        })
    }
}

async fn read_encode_progress(stdout: ChildStdout, stage: &'static str, sink: EncodeProgressSink) {
    let mut lines = BufReader::new(stdout).lines();
    let mut out_time_us = 0u64;
    let mut speed_x = None;
    let mut fps = None;
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(value) = line.strip_prefix("out_time_us=") {
            out_time_us = value.trim().parse().unwrap_or(out_time_us);
        } else if let Some(value) = line.strip_prefix("speed=") {
            speed_x = value.trim().trim_end_matches('x').parse::<f32>().ok();
        } else if let Some(value) = line.strip_prefix("fps=") {
            fps = value.trim().parse::<f32>().ok();
        } else if line.starts_with("progress=") {
            let _ = sink
                .send(EncodeProgress {
                    stage,
                    out_time_us,
                    speed_x,
                    fps,
                })
                .await;
        }
    }
}

async fn drain_stdout(mut stdout: ChildStdout) {
    let mut buffer = [0u8; 4096];
    while let Ok(read) = stdout.read(&mut buffer).await {
        if read == 0 {
            break;
        }
    }
}

async fn read_stderr(mut stderr: ChildStderr) -> String {
    let mut buffer = Vec::new();
    let _ = stderr.read_to_end(&mut buffer).await;
    String::from_utf8_lossy(&buffer).into_owned()
}

fn tail(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        value.to_string()
    } else {
        value.chars().skip(count - max_chars).collect()
    }
}

/// Concatenates intermediate Matroska files with a stream copy. The combined
/// file keeps continuous timestamps for the muxer.
pub async fn concat_mkv(
    ffmpeg: &Path,
    inputs: &[PathBuf],
    output: &Path,
    cpu: CpuUsage,
) -> Result<(), AvchdError> {
    if inputs.is_empty() {
        return Err(AvchdError::InvalidConfig(
            "no hay streams para concatenar".to_string(),
        ));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if inputs.len() == 1 {
        if inputs[0].as_path() == output {
            return Ok(());
        }
        // The muxer only reads the stream: hard-link it instead of copying many
        // GB; fall back to a copy across volumes.
        if output.exists() {
            std::fs::remove_file(output)?;
        }
        if std::fs::hard_link(&inputs[0], output).is_err() {
            std::fs::copy(&inputs[0], output)?;
        }
        return Ok(());
    }
    let list_path = output.with_extension("concat.txt");
    let list = inputs
        .iter()
        .map(|path| {
            let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            format!(
                "file '{}'
",
                absolute.to_string_lossy().replace('\'', "'\\''")
            )
        })
        .collect::<String>();
    std::fs::write(&list_path, list)?;
    let mut args = vec![
        "-hide_banner".to_string(),
        "-y".to_string(),
        "-f".to_string(),
        "concat".to_string(),
        "-safe".to_string(),
        "0".to_string(),
    ];
    push_file_input(&mut args, &list_path);
    args.extend([
        "-map".to_string(),
        "0".to_string(),
        "-c".to_string(),
        "copy".to_string(),
        "-f".to_string(),
        "matroska".to_string(),
        output.display().to_string(),
    ]);
    run_stage(ffmpeg, cpu, &args, "matroska concat", None).await?;
    let _ = std::fs::remove_file(&list_path);
    if !output.is_file() || output.metadata()?.len() == 0 {
        return Err(AvchdError::MissingOutput(output.display().to_string()));
    }
    Ok(())
}

fn normalize_language(language: &str) -> String {
    kinodisc_domain::LanguageCode::from_code(language)
        .unwrap_or(kinodisc_domain::LanguageCode::Undetermined)
        .iso_639_2()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m2ts_overhead_matches_a_measured_low_bitrate_episode() {
        // 480i, 1174.84 s, video 52 970 602 B (361 kbps), AC-3 14 098 176 B
        // (96 kbps): FFmpeg wrote a 79 104 000 B M2TS.
        let overhead =
            ts_overhead_bytes_for(52_970_602, 14_098_176, 1_174_840, FrameRate::FPS_29_97);
        let estimate = 52_970_602 + 14_098_176 + overhead;
        assert!(estimate >= 79_104_000, "{estimate}");
        assert!(estimate <= 79_104_000 * 102 / 100, "{estimate}");
        // At Blu-ray bitrates the fixed part is small next to the 5 %.
        let hd = ts_overhead_bytes_for(2_250_000_000, 96_000_000, 1_200_000, FrameRate::FPS_29_97);
        let proportional = ts_overhead_bytes(2_346_000_000);
        // 15 Mbps: about 0.55 % more than the proportional 5 %.
        assert!(hd > proportional && hd < proportional * 12 / 10);
    }

    fn config() -> AvchdVideoConfig {
        AvchdVideoConfig {
            resolution: AvchdResolution::R720p,
            frame_rate: FrameRate::FPS_23_976,
            ..AvchdVideoConfig::default()
        }
    }

    #[test]
    fn standard_low_ac3_bitrates_are_accepted() {
        for kbps in [64, 96, 128, 160, 192, 448] {
            let mut req = request();
            // AVCHD on DVD media: 18 Mb/s at most (1.3).
            req.maxrate_kbps = AVCHD_MAX_VIDEO_KBPS;
            req.vbv_bufsize_kbps = AVCHD_MAX_VIDEO_KBPS;
            req.audios[0].bitrate_kbps = kbps;
            req.audios[0].channels = 2;
            assert!(req.validate().is_ok(), "{kbps} kbps");
        }
        let mut req = request();
        req.maxrate_kbps = AVCHD_MAX_VIDEO_KBPS;
        req.vbv_bufsize_kbps = AVCHD_MAX_VIDEO_KBPS;
        req.audios[0].bitrate_kbps = 100;
        assert!(req.validate().is_err());
        req.audios[0].bitrate_kbps = 96;
        req.audios[0].channels = 6;
        assert!(req.validate().is_err(), "5.1 at 96 kbps");
    }

    fn request() -> AvchdEncodeRequest {
        AvchdEncodeRequest {
            input: PathBuf::from("input.mp4"),
            output_dir: PathBuf::from("out"),
            stem: "episode-01".to_string(),
            config: config(),
            disc_capacity: DiscCapacity::Dvd5,
            aspect: AspectRatio::SixteenNine,
            video_kbps: 8_000,
            maxrate_kbps: 24_000,
            vbv_bufsize_kbps: 24_000,
            audios: vec![AvchdAudioConfig {
                source_stream: 1,
                bitrate_kbps: 192,
                channels: 2,
                sample_rate: 48_000,
                pre_filter: None,
                gain_db: 0.0,
                delay_ms: 0,
                duration_ms: 0,
                language: "eng".to_string(),
                copy: false,
                codec: AvchdAudioCodec::Ac3,
                default: true,
                commentary: false,
                silent: false,
            }],
            subtitles: Vec::new(),
            trim: None,
            trim_start_ms: None,
            trim_end_ms: None,
            scale_mode: VideoScaleMode::Letterbox,
            pre_filter: None,
            fps: FrameRate::NTSC,
            duration_ms: 20_000,
            hardware_encoder: None,
            x264_preset: "medium",
            video_copy: false,
            passlog: None,
            progressive_source: false,
            content: kinodisc_toolchain::ContentType::Film,
        }
    }

    #[test]
    fn video_args_include_required_bluray_flags() {
        let args = build_encode_args(&request()).join(" ");
        assert!(args.contains("bluray-compat=1"));
        assert!(args.contains("open-gop=0"));
        assert!(args.contains("slices=4"));
        assert!(args.contains("nal-hrd=vbr"));
        assert!(args.contains("vbv-maxrate=24000"));
        assert!(args.contains("vbv-bufsize=24000"));
        assert!(args.contains("aud=1"));
        assert!(args.contains("colorprim=bt709"));
        // B-frames are safe on hardware players (a PS4 plays them); the
        // pyramid is the strict one Blu-ray allows.
        assert!(args.contains("-bf 3"));
        assert!(args.contains("b-pyramid=strict"));
        assert!(args.contains("-profile:v high"));
        assert!(args.contains("-level:v 4.1"));
        assert!(args.contains("-f matroska"));
        assert!(args.contains("-c:a:0 ac3"));
        // One-second GOP at 23.976.
        assert!(args.contains("keyint=24:min-keyint=24"));
        // Film keeps the x264 defaults (aq-mode=3 measured worse).
        assert!(!args.contains("aq-mode"));
        assert!(!args.contains("-tune"));
    }

    #[test]
    fn two_passes_analyse_the_video_then_write_the_disc_stream() {
        let mut request = request();
        assert!(!two_pass(&request), "no stats file: single pass");
        request.passlog = Some(PathBuf::from("/tmp/episode-01-x264"));
        assert!(two_pass(&request));
        let first = build_encode_args_for(&request, EncodePass::First).join(" ");
        assert!(
            first.contains("-pass 1 -passlogfile /tmp/episode-01-x264"),
            "{first}"
        );
        assert!(first.contains("-map 0:v:0 -an -sn -dn"), "{first}");
        assert!(first.contains("-f null"), "{first}");
        assert!(!first.contains("-c:a"), "{first}");
        assert!(!first.contains("matroska"), "{first}");
        let second = build_encode_args_for(&request, EncodePass::Second).join(" ");
        assert!(
            second.contains("-pass 2 -passlogfile /tmp/episode-01-x264"),
            "{second}"
        );
        assert!(second.contains("-c:a:0 ac3"), "{second}");
        assert!(second.contains("-f matroska"), "{second}");
        // Hardware encoders and stream copy stay single-pass.
        request.hardware_encoder = Some(kinodisc_toolchain::HardwareEncoder::NvencH264);
        assert!(!two_pass(&request));
        request.hardware_encoder = None;
        request.video_copy = true;
        assert!(!two_pass(&request));
    }

    #[test]
    fn a_copy_keeps_the_disc_matrix_unless_the_stream_names_another() {
        for tag in [None, Some("unknown"), Some("bt709")] {
            assert!(
                copy_keeps_disc_matrix(tag, AvchdResolution::R1080p),
                "{tag:?}"
            );
        }
        for tag in ["smpte170m", "bt470bg", "bt2020nc", "smpte240m"] {
            assert!(
                !copy_keeps_disc_matrix(Some(tag), AvchdResolution::R1080p),
                "{tag}"
            );
        }
        for resolution in [AvchdResolution::Sd480i, AvchdResolution::Sd576i] {
            for tag in [None, Some("smpte170m"), Some("bt470bg")] {
                assert!(copy_keeps_disc_matrix(tag, resolution), "{tag:?}");
            }
            assert!(!copy_keeps_disc_matrix(Some("bt709"), resolution));
        }
    }

    #[test]
    fn progressive_sources_on_480i_are_coded_progressive_and_flagged_interlaced() {
        let mut request = request();
        request.config.resolution = AvchdResolution::Sd480i;
        request.config.frame_rate = FrameRate::FPS_29_97;
        request.progressive_source = true;
        let args = build_encode_args(&request).join(" ");
        // MBAFF (tff/+ildct) would ignore fake-interlaced and cost bits.
        assert!(args.contains("fake-interlaced=1:pic-struct=1"), "{args}");
        assert!(!args.contains("tff=1"), "{args}");
        assert!(!args.contains("+ildct"), "{args}");
        assert!(!args.contains("setfield=tff"), "{args}");
        assert!(!args.contains("interl=1"), "{args}");
        assert!(args.contains("keyint=30:min-keyint=30"), "{args}");
        // SD is BT.601.
        assert!(args.contains("colorprim=smpte170m"), "{args}");
        assert!(args.contains("-color_trc smpte170m"), "{args}");
        // A truly interlaced source keeps MBAFF.
        request.progressive_source = false;
        let args = build_encode_args(&request).join(" ");
        assert!(
            args.contains("tff=1") && args.contains("+ildct+ilme"),
            "{args}"
        );
        request.config.resolution = AvchdResolution::Sd576i;
        request.config.frame_rate = FrameRate::PAL;
        let args = build_encode_args(&request).join(" ");
        assert!(args.contains("colorprim=bt470bg"), "{args}");
        assert!(args.contains("-color_trc gamma28"), "{args}");
    }

    #[test]
    fn content_type_tunes_x264_without_denoising() {
        let mut request = request();
        request.content = kinodisc_toolchain::ContentType::Animation;
        let args = build_encode_args(&request).join(" ");
        assert!(args.contains("-tune animation"), "{args}");
        // The denoise measured worse (1.3), so the picture is left alone.
        assert!(!args.contains("hqdn3d"), "{args}");
        request.content = kinodisc_toolchain::ContentType::Grain;
        let args = build_encode_args(&request).join(" ");
        assert!(args.contains("-tune grain"), "{args}");
        assert!(!args.contains("hqdn3d"), "{args}");
    }

    #[test]
    fn longer_gops_put_a_key_frame_every_two_or_three_seconds() {
        let mut request = request();
        request.config.gop_seconds = 2;
        let args = build_encode_args(&request).join(" ");
        assert!(args.contains("keyint=48:min-keyint=48"), "{args}");
        request.config.gop_seconds = 3;
        let args = build_encode_args(&request).join(" ");
        assert!(args.contains("keyint=72:min-keyint=72"), "{args}");
    }

    #[test]
    fn full_hd_keeps_the_level_41_reference_limit() {
        let mut request = request();
        request.config.resolution = AvchdResolution::R1080p;
        let args = build_encode_args(&request).join(" ");
        assert!(args.contains(":ref=4"), "{args}");
        request.config.resolution = AvchdResolution::R720p;
        let args = build_encode_args(&request).join(" ");
        assert!(!args.contains(":ref=4"), "{args}");
    }

    #[test]
    fn interlaced_video_never_goes_to_a_hardware_encoder() {
        // QA 1.1.3: a 1080i 25 disc came out as progressive 1080p25 with AMF.
        let mut request = request();
        request.config.resolution = AvchdResolution::R1080i;
        request.config.frame_rate = FrameRate::PAL;
        request.hardware_encoder = Some(kinodisc_toolchain::HardwareEncoder::AmfH264);
        let args = build_encode_args(&request).join(" ");
        assert!(args.contains("libx264"), "{args}");
        assert!(!args.contains("h264_amf"), "{args}");
        assert!(args.contains("+ildct+ilme"), "{args}");
    }

    #[test]
    fn interlaced_configs_add_tff_flags() {
        let mut request = request();
        request.config.resolution = AvchdResolution::R1080i;
        request.config.frame_rate = FrameRate::FPS_29_97;
        let args = build_encode_args(&request).join(" ");
        assert!(args.contains("tff=1"));
        assert!(args.contains("+ildct+ilme"));
        // `-top` no longer exists in FFmpeg 9; the filter sets field order.
        assert!(!args.contains("-top"));
        assert!(args.contains(",setfield=tff"));
    }

    #[test]
    fn hardware_encoder_switches_codec_and_keeps_bluray_vbv_args() {
        let mut request = request();
        request.hardware_encoder = Some(kinodisc_toolchain::HardwareEncoder::NvencH264);
        let args = build_encode_args(&request).join(" ");
        assert!(args.contains("-c:v h264_nvenc"));
        assert!(!args.contains("libx264"));
        assert!(args.contains("-profile:v high"));
        assert!(args.contains("-maxrate 24000k"));
        assert!(args.contains("-bufsize 24000k"));
        assert!(args.contains("-g 24"));
        assert!(args.contains("-bf 3"));
    }

    #[test]
    fn pgs_tracks_are_mapped_and_copied_into_the_mkv() {
        let mut request = request();
        request.subtitles = vec![AvchdSubtitleConfig {
            source_stream: Some(3),
            source_path: None,
            language: "spa".to_string(),
            forced: false,
            default: true,
            delay_ms: 0,
        }];
        let args = build_encode_args(&request).join(" ");
        assert!(args.contains("-map 0:3"), "{args}");
        assert!(args.contains("-c:s copy"), "{args}");
        assert!(!args.contains("-sn"), "{args}");
        assert!(args.contains("-metadata:s:s:0 language=spa"), "{args}");
        assert!(args.contains("-disposition:s:0 default"), "{args}");
    }

    #[test]
    fn silent_tracks_come_from_generated_silence() {
        let mut request = request();
        for (index, language) in ["spa", "eng"].iter().enumerate() {
            let mut track = request.audios[0].clone();
            track.silent = true;
            track.language = language.to_string();
            track.default = index == 0;
            if index == 0 {
                request.audios[0] = track;
            } else {
                request.audios.push(track);
            }
        }
        let args = build_encode_args(&request).join(" ");
        assert!(
            args.contains("-f lavfi -i anullsrc=r=48000:cl=stereo"),
            "{args}"
        );
        assert_eq!(args.matches("-map 1:a:0").count(), 2, "{args}");
        assert!(!args.contains("-map 0:1"), "{args}");
        assert!(args.contains("-c:a:1 ac3"), "{args}");
        assert!(args.contains("language=eng"), "{args}");
    }

    #[test]
    fn multiple_audio_tracks_are_mapped_and_copied_per_stream() {
        let mut request = request();
        request.audios.push(AvchdAudioConfig {
            source_stream: 2,
            bitrate_kbps: 768,
            channels: 6,
            sample_rate: 48_000,
            pre_filter: None,
            gain_db: 0.0,
            delay_ms: 0,
            duration_ms: 0,
            language: "spa".to_string(),
            copy: true,
            codec: AvchdAudioCodec::Dts,
            default: false,
            commentary: false,
            silent: false,
        });
        let args = build_encode_args(&request).join(" ");
        assert!(args.contains("-map 0:1"), "{args}");
        assert!(args.contains("-map 0:2"), "{args}");
        assert!(args.contains("-c:a:0 ac3"), "{args}");
        assert!(args.contains("-c:a:1 copy"), "{args}");
        assert!(args.contains("-metadata:s:a:1 language=spa"), "{args}");
    }

    #[test]
    fn audio_gain_and_delay_are_applied() {
        let mut request = request();
        if let Some(audio) = request.audios.first_mut() {
            audio.gain_db = 3.0;
            audio.delay_ms = 250;
        }
        let args = build_encode_args(&request).join(" ");
        assert!(
            args.contains("-filter:a:0 volume=3.000dB,adelay=250:all=1"),
            "{args}"
        );
    }

    #[test]
    fn language_codes_become_iso_639_2_terminologic() {
        for (input, expected) in [
            ("es", "spa"),
            ("eng", "eng"),
            ("fr", "fra"),
            ("fre", "fra"),
            ("de", "deu"),
            ("ger", "deu"),
            ("ja", "jpn"),
            ("zh", "zho"),
            ("chi", "zho"),
            ("eu", "eus"),
            ("baq", "eus"),
            ("und", "und"),
            ("unknown", "und"),
        ] {
            assert_eq!(normalize_language(input), expected, "input {input}");
        }
    }

    #[test]
    fn blu_ray_encoder_uses_38_mbps_vbv() {
        let mut request = request();
        request.disc_capacity = DiscCapacity::Bd25;
        request.video_kbps = 30_000;
        request.maxrate_kbps = 38_000;
        request.vbv_bufsize_kbps = 35_000;
        let args = build_encode_args(&request).join(" ");
        assert!(args.contains("vbv-maxrate=38000"));
        assert!(args.contains("vbv-bufsize=35000"));
        assert!(args.contains("-maxrate 38000k"));
        assert!(args.contains("-bufsize 35000k"));
    }

    #[test]
    fn resolution_specs_match_avchd_standards() {
        assert_eq!(
            ResolutionSpec::from_resolution(AvchdResolution::R1080p),
            ResolutionSpec {
                width: 1920,
                height: 1080,
                interlaced: false,
                psf: false,
            }
        );
        assert_eq!(
            ResolutionSpec::from_resolution(AvchdResolution::Sd576i),
            ResolutionSpec {
                width: 720,
                height: 576,
                interlaced: true,
                psf: false,
            }
        );
    }

    #[test]
    fn stream_copy_skips_filters_and_encoder_options() {
        let mut req = request();
        req.video_copy = true;
        let args = build_encode_args(&req).join(" ");
        assert!(args.contains("-c:v copy"));
        assert!(!args.contains("-vf"));
        assert!(!args.contains("libx264"));
        assert!(!args.contains("-x264-params"));
        // Audio is still processed independently.
        assert!(args.contains("ac3"));
    }

    #[test]
    fn psf_1080p25_uses_fake_interlaced_cpu_encode() {
        let mut req = request();
        req.config.resolution = AvchdResolution::R1080p;
        req.config.frame_rate = FrameRate::PAL;
        req.hardware_encoder = Some(kinodisc_toolchain::HardwareEncoder::NvencH264);
        let args = build_encode_args(&req).join(" ");
        assert!(args.contains("fake-interlaced=1:pic-struct=1"));
        assert!(args.contains("libx264"));
        assert!(!args.contains("+ildct"));
    }

    #[test]
    fn hfr_1080p60_uses_level_42() {
        let mut req = request();
        req.config.resolution = AvchdResolution::R1080p;
        req.config.frame_rate = FrameRate::FPS_59_94;
        let args = build_encode_args(&req).join(" ");
        assert!(args.contains("-level:v 4.2"));
        assert!(args.contains("fps=60000/1001"));
    }

    #[test]
    fn trim_ms_args_seek_before_input_and_use_effective_duration() {
        let mut req = request();
        req.trim_start_ms = Some(2_000);
        req.trim_end_ms = Some(12_000);
        let args = build_encode_args(&req);
        let seek = args.iter().position(|arg| arg == "-ss").expect("seek");
        let input = args.iter().position(|arg| arg == "-i").expect("input");
        assert!(seek < input);
        assert_eq!(args[seek + 1], "2.000");
        assert!(args.windows(2).any(|pair| pair == ["-t", "10.000"]));
    }

    #[test]
    fn frame_range_limits_duration_when_ms_bounds_are_missing() {
        let mut req = request();
        req.duration_ms = 60_000;
        req.fps = FrameRate::NTSC;
        req.trim = Some(FrameRange {
            start: 300,
            end: 780,
        });
        let args = build_encode_args(&req);
        assert!(args.windows(2).any(|pair| pair == ["-t", "16.016"]));
    }

    #[test]
    fn sd_avchd_is_resampled_once_and_tags_the_aspect_with_a_slash() {
        let mut sd = request();
        sd.config.resolution = AvchdResolution::Sd480i;
        sd.config.frame_rate = FrameRate::FPS_29_97;
        sd.aspect = AspectRatio::SixteenNine;
        let spec = ResolutionSpec::from_config(&sd.config);
        let filter = build_filter(&sd, spec);
        // One resample, straight to the stored frame (QA 1.3.2).
        assert_eq!(filter.matches("scale=").count(), 1, "{filter}");
        assert!(filter.contains("scale=w='if(gte(dar*9,16),720,"));
        assert!(filter.contains("pad=720:480:"));
        assert!(filter.contains(",setsar=32/27,"));
        assert!(!filter.contains("854"));
        assert!(!filter.contains("setsar=32:27"));
        sd.config.resolution = AvchdResolution::Sd576i;
        sd.config.frame_rate = FrameRate { num: 25, den: 1 };
        sd.aspect = AspectRatio::FourThree;
        let spec = ResolutionSpec::from_config(&sd.config);
        let filter = build_filter(&sd, spec);
        assert!(filter.contains("scale=w='if(gte(dar*3,4),720,"));
        assert!(filter.contains("pad=720:576:"));
        assert!(filter.contains("setsar=16/15"));
        // HD keeps square pixels and needs no squeeze.
        let hd = build_filter(&request(), ResolutionSpec::from_config(&config()));
        assert!(hd.contains("setsar=1/1"));
        assert_eq!(hd.matches("scale=").count(), 1);
        assert!(hd.contains("pad=1280:720:"));
    }

    #[test]
    fn scale_modes_map_to_avchd_filters() {
        let spec = ResolutionSpec::from_config(&config());
        let letterbox = build_filter(&request(), spec);
        assert!(letterbox.contains("if(gte(dar*9,16),1280,"));
        assert!(letterbox.contains("pad=1280:720"));

        let mut crop = request();
        crop.scale_mode = VideoScaleMode::CropFill;
        let crop_filter = build_filter(&crop, spec);
        assert!(crop_filter.contains("2*ceil(1280*dar*9/16/2)"));
        assert!(crop_filter.contains("crop=1280:720"));

        let mut stretch = request();
        stretch.scale_mode = VideoScaleMode::Stretch;
        let stretch_filter = build_filter(&stretch, spec);
        assert!(stretch_filter.contains("scale=1280:720:flags=lanczos"));
        assert!(!stretch_filter.contains("dar"));
    }

    #[test]
    fn invalid_resolution_fps_pair_is_rejected() {
        let mut config = config();
        config.resolution = AvchdResolution::R720p;
        config.frame_rate = FrameRate::FPS_29_97;
        let errors = config_errors(&config);
        assert!(errors
            .iter()
            .any(|error| error.code == "E_AVCHD_FPS_INCOMPATIBLE"));
    }
}
