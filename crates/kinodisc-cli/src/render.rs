use kinodisc_domain::{AuthoringMode, JobId};
use kinodisc_jobs::JobEventKind;
use kinodisc_plan::calculate_bitrate_plan;
use kinodisc_project::load_and_validate;
use kinodisc_render::{render_project_with_options, RenderError, RenderOptions};
use kinodisc_toolchain::{ContentType, CpuUsage, EncoderPreference};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::support::{collect_probes, print_plan_summary};

#[derive(Debug)]
pub(crate) struct RenderArgs {
    pub(crate) project: PathBuf,
    pub(crate) output: PathBuf,
    pub(crate) no_probe: bool,
    pub(crate) mode: Option<AuthoringMode>,
    pub(crate) avchd_folder: bool,
    /// H.264 encoder for AVCHD/Blu-ray (DVD-Video always uses MPEG-2).
    pub(crate) encoder: EncoderPreference,
    /// How much of the processor the encoders may take.
    pub(crate) cpu: CpuUsage,
    /// What the videos are, to tune the encoders.
    pub(crate) content: ContentType,
}

/// `--content` values: `film` (default), `animation` or `grain`.
pub(crate) fn parse_content(value: &str) -> Option<ContentType> {
    Some(match value.to_ascii_lowercase().as_str() {
        "film" | "movie" | "pelicula" => ContentType::Film,
        "animation" | "anime" | "cartoon" | "animacion" => ContentType::Animation,
        "grain" | "grainy" | "grano" => ContentType::Grain,
        _ => return None,
    })
}

/// `--cpu` values: `background`, `balanced` (default) or `full`.
pub(crate) fn parse_cpu(value: &str) -> Option<CpuUsage> {
    Some(match value.to_ascii_lowercase().as_str() {
        "background" | "low" => CpuUsage::Background,
        "balanced" | "normal" => CpuUsage::Balanced,
        "full" | "max" | "100" => CpuUsage::Full,
        _ => return None,
    })
}

/// `--encoder` values: `auto`, `cpu-fast`, `cpu-medium`, `cpu-slow`,
/// `cpu-slower`, or a
/// hardware encoder (`nvenc`, `qsv`, `amf`, `vaapi`, `videotoolbox`).
pub(crate) fn parse_encoder(value: &str) -> Option<EncoderPreference> {
    use kinodisc_toolchain::HardwareEncoder;
    Some(match value.to_ascii_lowercase().as_str() {
        "auto" => EncoderPreference::Auto,
        "cpu-fast" | "fast" => EncoderPreference::CpuFast,
        "cpu" | "cpu-medium" | "medium" => EncoderPreference::CpuOnly,
        "cpu-slow" | "slow" => EncoderPreference::CpuSlow,
        "cpu-slower" | "slower" => EncoderPreference::CpuSlower,
        "nvenc" => EncoderPreference::Prefer(HardwareEncoder::NvencH264),
        "qsv" => EncoderPreference::Prefer(HardwareEncoder::QsvH264),
        "amf" => EncoderPreference::Prefer(HardwareEncoder::AmfH264),
        "vaapi" => EncoderPreference::Prefer(HardwareEncoder::VaapiH264),
        "videotoolbox" => EncoderPreference::Prefer(HardwareEncoder::VideoToolboxH264),
        _ => return None,
    })
}

pub async fn run(args: &[String]) -> ExitCode {
    let args = match parse_render_args(args) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    match render(&args).await {
        Ok(iso) => {
            println!();
            println!("Render completado: {}", iso.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("Render falló: {error}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) async fn render(args: &RenderArgs) -> Result<PathBuf, String> {
    let project_path = args
        .project
        .canonicalize()
        .map_err(|error| format!("No se pudo resolver {}: {error}", args.project.display()))?;
    let mut project = load_and_validate(&project_path).map_err(|error| error.to_string())?;
    let project_dir = project_path
        .parent()
        .ok_or_else(|| "El proyecto no tiene directorio padre".to_string())?
        .to_path_buf();
    let output = absolute_output(&args.output)?;
    if let Some(requested) = args.mode {
        if requested != project.disc.mode {
            return Err(format!(
                "El proyecto declara modo {:?}, pero se solicitó {:?}.",
                project.disc.mode, requested
            ));
        }
    }

    let mut missing = kinodisc_toolchain::provision::needs_install();
    // AVCHD and Blu-ray never run DVDAuthor or Spumux (the app's rule too).
    if project.disc.mode == AuthoringMode::Avchd {
        missing.retain(|download| download.id != "dvdauthor");
    }
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|download| download.id).collect();
        return Err(format!(
            "Faltan herramientas o hay que reinstalarlas tras actualizar: {}. Ejecuta `kinodisc-cli toolchain install all` (descarga verificada con SHA-256).",
            names.join(", ")
        ));
    }

    // Keep the CLI plan summary, but delegate every authoring step to the
    // unified renderer so DVD, AVCHD, Blu-ray, motion menus, HDMV IG menus,
    // subtitles and region masks behave exactly like the desktop app.
    let (probes, probe_warnings) = collect_probes(&project, &project_dir, !args.no_probe).await;
    // Hand these probes to the renderer so it neither re-runs FFprobe over
    // every source nor plans with different metadata than printed here.
    for asset in &mut project.assets {
        if let Some(probe) = probes.get(&asset.id) {
            asset.probe = probe.clone();
        }
    }
    let mut plan = calculate_bitrate_plan(&project, &probes).map_err(|error| error.to_string())?;
    plan.warnings.extend(probe_warnings);
    print_plan_summary(&plan);
    if !plan.feasible {
        return Err("El plan de bitrate no es viable.".to_string());
    }

    let mode_label = match project.disc.mode {
        AuthoringMode::DvdVideo => "dvd",
        AuthoringMode::Avchd if project.disc.capacity.is_blu_ray() => "bluray",
        AuthoringMode::Avchd => "avchd",
    };
    let region_mask =
        matches!(project.disc.mode, AuthoringMode::DvdVideo).then_some(project.disc.region_mask);
    println!();
    println!(
        "Salida: {} ({} · {})",
        output.display(),
        mode_label,
        project.disc.capacity.label()
    );

    let (sender, mut receiver) = mpsc::channel(256);
    let drain = tokio::spawn(async move {
        while let Some(kind) = receiver.recv().await {
            match kind {
                JobEventKind::StageStarted { label, .. } => println!("{label}..."),
                JobEventKind::Warning { code, message } => {
                    eprintln!("Aviso {code}: {message}");
                }
                JobEventKind::Completed { summary } => println!("{summary}"),
                JobEventKind::Failed { message, .. } => eprintln!("Error: {message}"),
                JobEventKind::Cancelled { .. } => eprintln!("Generación cancelada."),
                // Resume/copy notices are useful on the command line too.
                JobEventKind::Log { level, message }
                    if level != kinodisc_jobs::LogLevel::Debug
                        && (message.contains("se reutiliza")
                            || message.starts_with("Reanudación")
                            || message.contains("sin recodificar")
                            || message.starts_with("Menú animado:")
                            || message.starts_with("Intro HD")
                            || message.starts_with("Menú raíz sin elección")
                            || message.starts_with("Episodios: ")) =>
                {
                    println!("  {message}");
                }
                _ => {}
            }
        }
    });

    // Cancel like the desktop app: raise the flag (libmkiso polls it) and drop
    // the render future (kill_on_drop children, TempIso removes the .part).
    let cancel = Arc::new(AtomicBool::new(false));
    let render = Box::pin(render_project_with_options(
        project,
        project_dir,
        output.clone(),
        JobId::new(),
        RenderOptions {
            probe_assets: false,
            composited_backgrounds: None,
            cancel: Some(Arc::clone(&cancel)),
            hardware_encoding: args.encoder,
            mux_backend: kinodisc_render::MuxBackend::Auto,
            cpu_usage: args.cpu,
            content: args.content,
        },
        sender,
    ));
    let result = tokio::select! {
        result = render => result,
        _ = tokio::signal::ctrl_c() => {
            cancel.store(true, Ordering::SeqCst);
            eprintln!("\nCancelando render...");
            Err(RenderError::Cancelled)
        }
    };
    // Flush every queued warning/error before propagating the result.
    let _ = drain.await;
    let outcome = result.map_err(|error| error.to_string())?;

    let iso_path = PathBuf::from(&outcome.iso_path);
    let iso_bytes = std::fs::metadata(&iso_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let usage = outcome.usage;
    println!(
        "Ocupación real: {:.2} GB de {:.2} GB ({:.1}%); el plan preveía hasta {:.2} GB",
        usage.image_bytes as f64 / 1e9,
        usage.capacity_bytes as f64 / 1e9,
        usage.image_bytes as f64 * 100.0 / usage.capacity_bytes.max(1) as f64,
        usage.planned_bytes as f64 / 1e9
    );
    // Folder output is extracted from the final image, so it is
    // byte-identical to the disc (menus, IG streams and all).
    let folder_path = if args.avchd_folder {
        let folder = output.join("disc");
        let report = kinodisc_verify::extract::extract_iso(&iso_path, &folder, &|| false)
            .map_err(|error| format!("No se pudo crear la carpeta del disco: {error}"))?;
        println!(
            "Carpeta del disco generada: {} ({} archivos)",
            folder.display(),
            report.files
        );
        Some(folder)
    } else {
        None
    };
    let summary = serde_json::json!({
        "mode": mode_label,
        "iso": outcome.iso_path,
        "isoBytes": iso_bytes,
        "plannedBytes": usage.planned_bytes,
        "capacityBytes": usage.capacity_bytes,
        "elapsedMs": outcome.elapsed_ms,
        "regionMask": region_mask,
        "folder": folder_path.as_ref().map(|path| path.display().to_string()),
        "warnings": outcome.warnings.iter().map(|warning| warning.code.clone()).collect::<Vec<_>>(),
    });
    let summary_path = output.join("render.json");
    std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary)
            .map_err(|error| format!("No se pudo serializar el resumen: {error}"))?,
    )
    .map_err(|error| format!("No se pudo escribir {}: {error}", summary_path.display()))?;
    println!("Resumen: {}", summary_path.display());
    Ok(iso_path)
}

fn parse_render_args(args: &[String]) -> Result<RenderArgs, String> {
    let mut project = None;
    let mut output = PathBuf::from("build/out");
    let mut no_probe = false;
    let mut mode = None;
    let mut avchd_folder = false;
    let mut encoder = EncoderPreference::Auto;
    let mut cpu = CpuUsage::default();
    let mut content = ContentType::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-o" | "--output" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("Falta el valor de -o/--output.".to_string());
                };
                output = PathBuf::from(value);
            }
            "--no-probe" => no_probe = true,
            "--folder" | "--avchd-folder" => avchd_folder = true,
            "--encoder" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("Falta el valor de --encoder.".to_string());
                };
                encoder = parse_encoder(value).ok_or_else(|| {
                    format!(
                        "Codificador desconocido: {value}. Usa auto, cpu-fast, cpu-medium, cpu-slow, cpu-slower, nvenc, qsv, amf, vaapi o videotoolbox."
                    )
                })?;
            }
            "--cpu" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("Falta el valor de --cpu.".to_string());
                };
                cpu = parse_cpu(value).ok_or_else(|| {
                    format!("Uso de CPU desconocido: {value}. Usa background, balanced o full.")
                })?;
            }
            "--content" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("Falta el valor de --content.".to_string());
                };
                content = parse_content(value).ok_or_else(|| {
                    format!("Tipo de contenido desconocido: {value}. Usa film, animation o grain.")
                })?;
            }
            "--mode" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("Falta el valor de --mode.".to_string());
                };
                mode = Some(match value.to_ascii_lowercase().as_str() {
                    "dvd" | "dvdvideo" | "dvd-video" => AuthoringMode::DvdVideo,
                    "avchd" => AuthoringMode::Avchd,
                    _ => {
                        return Err(format!("Modo desconocido: {value}. Usa dvd o avchd."));
                    }
                });
            }
            value if value.starts_with('-') => {
                return Err(format!("Opción desconocida para render: {value}"));
            }
            value => {
                if project.is_some() {
                    return Err("Solo se admite un proyecto por invocación.".to_string());
                }
                project = Some(PathBuf::from(value));
            }
        }
        index += 1;
    }
    Ok(RenderArgs {
        project: project.ok_or_else(|| {
            "Uso: kinodisc-cli render <project.json> [-o <dir>] [--mode dvd|avchd] [--encoder <tipo>] [--cpu <uso>] [--content film|animation|grain] [--folder] [--no-probe]"
                .to_string()
        })?,
        output,
        no_probe,
        mode,
        avchd_folder,
        encoder,
        cpu,
        content,
    })
}

fn absolute_output(output: &Path) -> Result<PathBuf, String> {
    if output.is_absolute() {
        return Ok(output.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(output))
        .map_err(|error| format!("No se pudo resolver el directorio de salida: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_option_parses_every_choice() {
        let parse = |extra: &[&str]| {
            let mut args = vec!["p.json".to_string()];
            args.extend(extra.iter().map(|value| value.to_string()));
            parse_render_args(&args)
        };
        assert_eq!(parse(&[]).unwrap().encoder, EncoderPreference::Auto);
        assert_eq!(
            parse(&["--encoder", "cpu-slow"]).unwrap().encoder,
            EncoderPreference::CpuSlow
        );
        assert_eq!(
            parse(&["--encoder", "CPU-MEDIUM"]).unwrap().encoder,
            EncoderPreference::CpuOnly
        );
        assert_eq!(
            parse(&["--encoder", "amf"]).unwrap().encoder,
            EncoderPreference::Prefer(kinodisc_toolchain::HardwareEncoder::AmfH264)
        );
        assert!(parse(&["--encoder", "turbo"]).is_err());
        assert!(parse(&["--encoder"]).is_err());
    }

    #[test]
    fn cpu_option_parses_every_choice() {
        let parse = |extra: &[&str]| {
            let mut args = vec!["p.json".to_string()];
            args.extend(extra.iter().map(|value| value.to_string()));
            parse_render_args(&args)
        };
        assert_eq!(parse(&[]).unwrap().cpu, CpuUsage::Balanced);
        assert_eq!(
            parse(&["--cpu", "background"]).unwrap().cpu,
            CpuUsage::Background
        );
        assert_eq!(parse(&["--cpu", "FULL"]).unwrap().cpu, CpuUsage::Full);
        assert_eq!(
            parse(&["--cpu", "balanced"]).unwrap().cpu,
            CpuUsage::Balanced
        );
        assert!(parse(&["--cpu", "turbo"]).is_err());
        assert!(parse(&["--cpu"]).is_err());
    }
}
