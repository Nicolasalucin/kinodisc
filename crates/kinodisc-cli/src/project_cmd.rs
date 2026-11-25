//! `kinodisc-cli project …`: build a whole disc project without the app
//! (new project, episodes, menu template, background, music, intro, idle
//! action). Uses the same code as the editor: `kinodisc-project::new`,
//! `kinodisc-project::import` and `kinodisc-menu::templates`. Every write
//! validates first and never saves a project with errors.

use kinodisc_domain::{
    AspectRatio, AuthoringMode, AvchdResolution, DiscCapacity, IdleAction, MediaKind,
    MenuBackground, MenuItem, Project, TvStandard, VideoConfig,
};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub(crate) const USAGE: &str = "\
Uso:
  kinodisc-cli project new <proyecto.json> <vídeo|carpeta> [...] [--name <nombre>]
        [--mode dvd|avchd] [--standard ntsc|pal] [--aspect 16:9|4:3]
        [--resolution 480i|576i|720p|1080i|1080p] [--capacity dvd5|dvd9|bd25|bd50]
        [--template <plantilla>]   (por defecto cinema-dark)
  kinodisc-cli project add <proyecto.json> <vídeo|carpeta> [...]
        (una carpeta agrega sus vídeos en orden natural: «2» antes que «10»;
         la plantilla se vuelve a aplicar con los episodios nuevos)
  kinodisc-cli project menu <proyecto.json> [--template <plantilla>]
        [--background <imagen|vídeo>] [--music <audio>] [--title <texto>]
        [--idle wait|play-all|intro] [--idle-seconds <N>]
        [--animate on|off]   (miniaturas de los episodios en movimiento)
        (las plantillas: kinodisc-cli template --list; la de 24 episodios por
         página es compact-index)
  kinodisc-cli project intro <proyecto.json> <vídeo> [--no-skip] [--idle-only]
  kinodisc-cli project intro <proyecto.json> --remove
  kinodisc-cli project subtitles <proyecto.json> <archivo.srt|carpeta> [...]
        [--episode <N>] [--lang <código>]
        (.srt/.ass/.ssa/.vtt; cada archivo va al episodio cuyo vídeo tiene el
         mismo nombre: «Ep 01.mkv» con «Ep 01.spa.srt»; el idioma sale del
         nombre o de --lang; --episode lo fuerza para un solo archivo)
  kinodisc-cli project episodes <proyecto.json> --after next|menu|stop
        (al terminar cada episodio: el siguiente, volver al menú o parar;
         los episodios nuevos siguen con el próximo)
  kinodisc-cli project gop <proyecto.json> 1|2|3
        (AVCHD/Blu-ray: una imagen completa cada 1, 2 o 3 segundos; 2 da más
         calidad a bitrate bajo y se probó en PS4; 3 es muy experimental)
  kinodisc-cli project passes <proyecto.json> fast|full
        (DVD: siempre dos pasadas; full analiza el vídeo con más detalle en
         la primera y tarda bastante más)
  kinodisc-cli project show <proyecto.json>
Después: kinodisc-cli render <proyecto.json> -o <carpeta>";

const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "mov", "m4v", "avi", "mpg", "mpeg", "ts", "m2ts", "mts", "webm", "wmv", "vob",
    "flv",
];

pub async fn run(args: &[String]) -> ExitCode {
    let result = match args.first().map(String::as_str) {
        Some("new") => new(&args[1..]).await,
        Some("add") => add(&args[1..]).await,
        Some("menu") => menu(&args[1..]).await,
        Some("intro") => intro(&args[1..]).await,
        Some("episodes") => episodes(&args[1..]),
        Some("subtitles") => subtitles(&args[1..]),
        Some("gop") => gop(&args[1..]),
        Some("passes") => passes(&args[1..]),
        Some("show") => show(&args[1..]),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn now() -> String {
    // RFC 3339 in UTC without a date crate: seconds since the epoch.
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let days = seconds / 86_400;
    let (hour, minute, second) = (seconds / 3600 % 24, seconds / 60 % 60, seconds % 60);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Positional values and `--option value` pairs.
type Parsed = (Vec<String>, Vec<(String, String)>);

/// Splits `args` into positional values and `--option value` pairs; flags
/// listed in `switches` take no value.
fn parse(args: &[String], switches: &[&str]) -> Result<Parsed, String> {
    let mut positional = Vec::new();
    let mut options = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(name) = arg.strip_prefix("--") {
            if switches.contains(&name) {
                options.push((name.to_string(), String::new()));
            } else {
                index += 1;
                let value = args
                    .get(index)
                    .filter(|value| !value.starts_with("--"))
                    .ok_or_else(|| format!("Falta el valor de --{name}.\n\n{USAGE}"))?;
                options.push((name.to_string(), value.clone()));
            }
        } else {
            positional.push(arg.clone());
        }
        index += 1;
    }
    Ok((positional, options))
}

fn option<'a>(options: &'a [(String, String)], name: &str) -> Option<&'a str> {
    options
        .iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn reject_unknown(options: &[(String, String)], known: &[&str]) -> Result<(), String> {
    match options
        .iter()
        .find(|(key, _)| !known.contains(&key.as_str()))
    {
        Some((key, _)) => Err(format!("Opción desconocida: --{key}\n\n{USAGE}")),
        None => Ok(()),
    }
}

fn load(path: &str) -> Result<Project, String> {
    kinodisc_project::load_for_editing(path)
        .map_err(|error| format!("No se pudo abrir el proyecto {path}: {error}"))
}

/// Validates and writes; a project with errors is never saved.
fn save(project: &Project, path: &str) -> Result<(), String> {
    let errors: Vec<_> = project
        .validate()
        .into_iter()
        .filter(|diagnostic| diagnostic.is_error())
        .collect();
    if !errors.is_empty() {
        let list = errors
            .iter()
            .map(|error| format!("Error {}: {}", error.code, error.message))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("{list}\nNo se guardó el proyecto."));
    }
    kinodisc_project::save_project(project, path)
        .map_err(|error| format!("No se pudo escribir {path}: {error}"))
}

fn absolute(path: &str) -> Result<PathBuf, String> {
    std::fs::canonicalize(path)
        .map(|path| {
            // `canonicalize` adds the `\\?\` prefix on Windows; the app
            // stores plain paths.
            let text = path.display().to_string();
            PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
        })
        .map_err(|error| format!("No se encuentra {path}: {error}"))
}

async fn new(args: &[String]) -> Result<(), String> {
    let (positional, options) = parse(args, &[])?;
    reject_unknown(
        &options,
        &[
            "name",
            "mode",
            "standard",
            "aspect",
            "resolution",
            "capacity",
            "template",
        ],
    )?;
    // A project without episodes is not valid (E_NO_EPISODES), so the
    // videos come with `new`.
    let [path, inputs @ ..] = positional.as_slice() else {
        return Err(USAGE.to_string());
    };
    if inputs.is_empty() {
        return Err(format!(
            "Faltan los vídeos del disco.

{USAGE}"
        ));
    }
    let template = template_id(option(&options, "template").unwrap_or("cinema-dark"))?;
    if Path::new(path).exists() {
        return Err(format!("{path} ya existe; no se sobrescribe."));
    }
    let mode = match option(&options, "mode").unwrap_or("dvd") {
        "dvd" | "dvd-video" => AuthoringMode::DvdVideo,
        "avchd" | "bluray" | "blu-ray" | "hd" => AuthoringMode::Avchd,
        other => return Err(format!("Modo desconocido: {other}. Usa dvd o avchd.")),
    };
    let standard = match option(&options, "standard").unwrap_or("ntsc") {
        "ntsc" => TvStandard::Ntsc,
        "pal" => TvStandard::Pal,
        other => return Err(format!("Norma desconocida: {other}. Usa ntsc o pal.")),
    };
    let aspect = match option(&options, "aspect").unwrap_or("16:9") {
        "16:9" | "16x9" | "wide" => AspectRatio::SixteenNine,
        "4:3" | "4x3" => AspectRatio::FourThree,
        other => return Err(format!("Aspecto desconocido: {other}. Usa 16:9 o 4:3.")),
    };
    let capacity = match option(&options, "capacity").unwrap_or("dvd5") {
        "dvd5" | "dvd" => DiscCapacity::Dvd5,
        "dvd9" => DiscCapacity::Dvd9,
        "bd25" => DiscCapacity::Bd25,
        "bd50" => DiscCapacity::Bd50,
        other => {
            return Err(format!(
                "Capacidad desconocida: {other}. Usa dvd5, dvd9, bd25 o bd50."
            ))
        }
    };
    if capacity.is_blu_ray() && mode == AuthoringMode::DvdVideo {
        return Err("Un Blu-ray se crea con --mode avchd.".to_string());
    }
    let resolution = match option(&options, "resolution") {
        None if capacity.is_blu_ray() => Some(AvchdResolution::R1080p),
        None => None,
        Some("480i") => Some(AvchdResolution::Sd480i),
        Some("576i") => Some(AvchdResolution::Sd576i),
        Some("720p") => Some(AvchdResolution::R720p),
        Some("1080i") => Some(AvchdResolution::R1080i),
        Some("1080p") => Some(AvchdResolution::R1080p),
        Some(other) => return Err(format!("Resolución desconocida: {other}.")),
    };
    let name = option(&options, "name")
        .map(str::to_string)
        .unwrap_or_else(|| {
            Path::new(path)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Kinodisc".to_string())
        });
    let mut project = kinodisc_project::new::new_project(
        name,
        standard,
        aspect,
        mode,
        resolution,
        None,
        &now(),
        env!("CARGO_PKG_VERSION"),
    )?;
    project.disc.capacity = capacity;
    // The rates the editor sets for each medium (AVCHD on DVD: 18 Mb/s).
    if let VideoConfig::Avchd(config) = &mut project.disc.video {
        let (maxrate, bufsize) = if capacity.is_blu_ray() {
            (38_000, 35_000)
        } else {
            (18_000, 18_000)
        };
        config.vbv_maxrate_kbps = maxrate;
        config.vbv_bufsize_kbps = bufsize;
        config.level = config.effective_level();
        project.budget.max_video_kbps = maxrate;
    }
    let added = add_videos(&mut project, inputs).await?;
    apply_template_keeping_choices(&mut project, template)?;
    save(&project, path)?;
    println!(
        "Proyecto creado: {path} ({}, {}, {added} episodio(s), {} página(s) de menú)",
        if mode == AuthoringMode::DvdVideo {
            "DVD-Video"
        } else {
            "AVCHD/Blu-ray"
        },
        capacity.label(),
        project.menus.len()
    );
    Ok(())
}

fn template_id(name: &str) -> Result<kinodisc_domain::MenuTemplateId, String> {
    crate::template_cmd::parse_template(name).ok_or_else(|| {
        format!("No existe la plantilla «{name}». Usa `kinodisc-cli template --list`.")
    })
}

/// Applies a template (all episodes, as many pages as needed) and keeps
/// what was chosen on the old root menu: a background picked by the user,
/// the music, the title and the idle action.
fn apply_template_keeping_choices(
    project: &mut Project,
    id: kinodisc_domain::MenuTemplateId,
) -> Result<(), String> {
    let old_root = project.menu(project.root_menu).cloned();
    let result = kinodisc_menu::templates::apply_template(project, id)
        .map_err(|error| format!("No se pudo aplicar la plantilla: {error}"))?;
    for diagnostic in &result.diagnostics {
        println!("Aviso {}: {}", diagnostic.code, diagnostic.message);
    }
    project.menus = result.menus;
    project.root_menu = result.root_menu;
    if let Some(old) = old_root.filter(|old| old.template.is_some()) {
        // Without `tpl:bg` the background was the user's own picture.
        let own_background = !old
            .items
            .iter()
            .any(|item| matches!(item, MenuItem::Shape(shape) if shape.id == "tpl:bg"));
        let title = old.items.iter().find_map(|item| match item {
            MenuItem::Text(text) if text.id == "tpl:title" => Some(text.text.clone()),
            _ => None,
        });
        for menu in &mut project.menus {
            if own_background {
                menu.background = old.background.clone();
                menu.items
                    .retain(|item| !matches!(item, MenuItem::Shape(shape) if shape.id == "tpl:bg"));
            }
            if old.audio.is_some() {
                menu.audio = old.audio.clone();
            }
        }
        let root_id = project.root_menu;
        if let Some(root) = project.menus.iter_mut().find(|menu| menu.id == root_id) {
            root.timing = old.timing;
            if let Some(title) = title {
                for item in &mut root.items {
                    if let MenuItem::Text(text) = item {
                        if text.id == "tpl:title" {
                            text.text = title.clone();
                        }
                    }
                }
            }
        }
    }
    if let Some(intro) = &mut project.first_play {
        intro.target_menu = project.root_menu;
    }
    kinodisc_project::import::sync_play_all(project);
    Ok(())
}

/// Probes and adds every video (folders in natural order); returns how many.
async fn add_videos(project: &mut Project, inputs: &[String]) -> Result<usize, String> {
    let mut files = Vec::new();
    for input in inputs {
        let path = absolute(input)?;
        if path.is_dir() {
            let found = folder_videos(&path)?;
            if found.is_empty() {
                return Err(format!("{} no tiene vídeos.", path.display()));
            }
            files.extend(found);
        } else {
            files.push(path);
        }
    }
    let stamp = now();
    for file in &files {
        let probe = probe_kind(file, MediaKind::Video).await?;
        let asset =
            kinodisc_project::import::add_asset(project, file, probe, MediaKind::Video, &stamp);
        let id = kinodisc_project::import::add_episode(project, asset)?;
        let title = project
            .episode(id)
            .map(|episode| episode.title.clone())
            .unwrap_or_default();
        println!("+ {title}");
    }
    Ok(files.len())
}

/// Videos in a folder, in natural order ("Ep 2" before "Ep 10").
fn folder_videos(folder: &Path) -> Result<Vec<PathBuf>, String> {
    let mut videos: Vec<PathBuf> = std::fs::read_dir(folder)
        .map_err(|error| format!("No se pudo leer {}: {error}", folder.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    VIDEO_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
                })
        })
        .collect();
    videos.sort_by_key(|path| natural_key(path));
    Ok(videos)
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Chunk {
    Number(u128, usize),
    Text(String),
}

fn natural_key(path: &Path) -> Vec<Chunk> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut chunks = Vec::new();
    let mut digits = String::new();
    let mut text = String::new();
    for character in name.chars() {
        if character.is_ascii_digit() {
            if !text.is_empty() {
                chunks.push(Chunk::Text(std::mem::take(&mut text)));
            }
            digits.push(character);
        } else {
            if !digits.is_empty() {
                let value = digits.parse().unwrap_or(u128::MAX);
                chunks.push(Chunk::Number(value, digits.len()));
                digits.clear();
            }
            text.push(character);
        }
    }
    if !digits.is_empty() {
        chunks.push(Chunk::Number(
            digits.parse().unwrap_or(u128::MAX),
            digits.len(),
        ));
    }
    if !text.is_empty() {
        chunks.push(Chunk::Text(text));
    }
    chunks
}

async fn probe_kind(path: &Path, kind: MediaKind) -> Result<kinodisc_domain::MediaProbe, String> {
    let probe = kinodisc_media::probe(path)
        .await
        .map_err(|error| format!("No se pudo analizar {}: {error}", path.display()))?;
    match kind {
        MediaKind::Video if probe.video.is_none() => {
            Err(format!("{} no tiene vídeo.", path.display()))
        }
        MediaKind::Audio if probe.audio.is_empty() => {
            Err(format!("{} no tiene audio.", path.display()))
        }
        // A file that only has the name of a picture has no size.
        MediaKind::Image
            if !probe
                .video
                .as_ref()
                .is_some_and(|picture| picture.width > 0 && picture.height > 0) =>
        {
            Err(format!("{} no es una imagen legible.", path.display()))
        }
        _ => Ok(probe),
    }
}

async fn add(args: &[String]) -> Result<(), String> {
    let (positional, options) = parse(args, &[])?;
    reject_unknown(&options, &[])?;
    let [project_path, inputs @ ..] = positional.as_slice() else {
        return Err(USAGE.to_string());
    };
    if inputs.is_empty() {
        return Err(USAGE.to_string());
    }
    let mut project = load(project_path)?;
    let added = add_videos(&mut project, inputs).await?;
    // The menus get the new episodes: the template is applied again.
    let template = project
        .menu(project.root_menu)
        .and_then(|root| root.template)
        .map(|page| page.template);
    if let Some(template) = template {
        apply_template_keeping_choices(&mut project, template)?;
    }
    save(&project, project_path)?;
    println!(
        "{added} episodio(s) agregado(s); el proyecto tiene {} y {} página(s) de menú.",
        project.episodes.len(),
        project.menus.len()
    );
    if template.is_none() {
        println!("El menú no usa plantilla: aplica una con `kinodisc-cli project menu {project_path} --template <plantilla>`.");
    }
    Ok(())
}

/// External text subtitles, matched to episodes by file name.
fn subtitles(args: &[String]) -> Result<(), String> {
    use kinodisc_project::import::{
        add_subtitle_file, subtitle_language, subtitle_matches_episode, SUBTITLE_EXTENSIONS,
    };
    let (positional, options) = parse(args, &[])?;
    reject_unknown(&options, &["episode", "lang"])?;
    let [project_path, inputs @ ..] = positional.as_slice() else {
        return Err(USAGE.to_string());
    };
    if inputs.is_empty() {
        return Err(USAGE.to_string());
    }
    let is_subtitle = |path: &Path| {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                SUBTITLE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
            })
    };
    let mut files = Vec::new();
    for input in inputs {
        let path = absolute(input)?;
        if path.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|file| file.is_file() && is_subtitle(file))
                .collect();
            found.sort();
            files.extend(found);
        } else if is_subtitle(&path) && path.is_file() {
            files.push(path);
        } else {
            return Err(format!(
                "{} no es un subtítulo de texto (.srt, .ass, .ssa, .vtt).",
                path.display()
            ));
        }
    }
    let mut project = load(project_path)?;
    let forced = match option(&options, "episode") {
        Some(number) => {
            let number: u32 = number
                .parse()
                .map_err(|_| format!("--episode espera un número, no «{number}»."))?;
            if files.len() != 1 {
                return Err("--episode va con un solo archivo.".to_string());
            }
            Some(
                project
                    .episodes
                    .iter()
                    .find(|episode| u32::from(episode.number) == number)
                    .map(|episode| episode.id)
                    .ok_or_else(|| format!("No hay episodio {number}."))?,
            )
        }
        None => None,
    };
    let mut added = 0;
    let mut unmatched = Vec::new();
    for file in &files {
        let episode = forced.or_else(|| {
            project
                .episodes
                .iter()
                .find(|episode| subtitle_matches_episode(&project, episode, file))
                .map(|episode| episode.id)
        });
        let Some(episode) = episode else {
            unmatched.push(file.display().to_string());
            continue;
        };
        let language = subtitle_language(file)
            .or_else(|| option(&options, "lang").map(str::to_string))
            .unwrap_or_else(|| "und".to_string());
        add_subtitle_file(&mut project, episode, file, &language)?;
        let number = project.episode(episode).map_or(0, |episode| episode.number);
        println!("+ episodio {number}: {} ({language})", file.display());
        added += 1;
    }
    if added == 0 {
        return Err(format!(
            "Ningún subtítulo coincide con un episodio por nombre: {}",
            unmatched.join(", ")
        ));
    }
    save(&project, project_path)?;
    for file in &unmatched {
        println!("Sin episodio con ese nombre, no se agregó: {file}");
    }
    println!("{added} subtítulo(s) agregado(s).");
    // AVCHD/Blu-ray carry subtitles only when every episode has the same
    // tracks (W_PGS_LAYOUT_MISMATCH at render time).
    if matches!(project.disc.video, VideoConfig::Avchd(_)) {
        let layouts: std::collections::BTreeSet<usize> = project
            .episodes
            .iter()
            .map(|episode| episode.subtitles.len())
            .collect();
        if layouts.len() > 1 {
            let with = project
                .episodes
                .iter()
                .filter(|episode| !episode.subtitles.is_empty())
                .count();
            println!(
                "Aviso: en AVCHD y Blu-ray los subtítulos solo se graban si todos los episodios tienen las mismas pistas; ahora {with} de {} episodios tienen subtítulos, así que el disco saldrá sin ellos.",
                project.episodes.len()
            );
        }
    }
    Ok(())
}

/// Key frame interval of an AVCHD/Blu-ray project.
fn gop(args: &[String]) -> Result<(), String> {
    let [project_path, seconds] = args else {
        return Err(USAGE.to_string());
    };
    let value: u8 = match seconds.as_str() {
        "1" => 1,
        "2" => 2,
        "3" => 3,
        _ => return Err(format!("El GOP es 1, 2 o 3 segundos, no «{seconds}».")),
    };
    let mut project = load(project_path)?;
    let VideoConfig::Avchd(config) = &mut project.disc.video else {
        return Err("El GOP solo se elige en proyectos AVCHD o Blu-ray.".to_string());
    };
    config.gop_seconds = value;
    save(&project, project_path)?;
    println!(
        "GOP de {seconds} s.{}",
        if value == 3 {
            " Muy experimental: no se probó en ningún reproductor."
        } else {
            ""
        }
    );
    Ok(())
}

/// The first pass of a DVD project: fast (default) or full (slower).
fn passes(args: &[String]) -> Result<(), String> {
    let [project_path, kind] = args else {
        return Err(USAGE.to_string());
    };
    let full = match kind.as_str() {
        "fast" => false,
        "full" => true,
        _ => return Err(format!("La primera pasada es fast o full, no «{kind}».")),
    };
    let mut project = load(project_path)?;
    let VideoConfig::Dvd(config) = &mut project.disc.video else {
        return Err("La primera pasada solo se elige en proyectos de DVD.".to_string());
    };
    config.full_first_pass = full;
    save(&project, project_path)?;
    println!(
        "Dos pasadas, la primera {}.",
        if full {
            "completa (más lenta)"
        } else {
            "rápida"
        }
    );
    Ok(())
}

/// What every episode does when it ends.
fn episodes(args: &[String]) -> Result<(), String> {
    let (positional, options) = parse(args, &[])?;
    reject_unknown(&options, &["after"])?;
    let [project_path] = positional.as_slice() else {
        return Err(USAGE.to_string());
    };
    let after = match option(&options, "after") {
        Some("next") => kinodisc_domain::PostAction::NextEpisode,
        Some("menu") => kinodisc_domain::PostAction::ReturnToMenu,
        Some("stop") => kinodisc_domain::PostAction::Stop,
        Some(other) => return Err(format!("--after acepta next, menu o stop, no «{other}».")),
        None => return Err(USAGE.to_string()),
    };
    let mut project = load(project_path)?;
    for episode in &mut project.episodes {
        episode.post_action = after;
    }
    save(&project, project_path)?;
    println!(
        "{} episodio(s): al terminar, {}.",
        project.episodes.len(),
        match after {
            kinodisc_domain::PostAction::NextEpisode => "el siguiente",
            kinodisc_domain::PostAction::ReturnToMenu => "vuelven al menú",
            kinodisc_domain::PostAction::Stop => "se detienen",
        }
    );
    Ok(())
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "bmp"
            )
        })
}

async fn menu(args: &[String]) -> Result<(), String> {
    let (positional, options) = parse(args, &[])?;
    reject_unknown(
        &options,
        &[
            "template",
            "background",
            "music",
            "title",
            "idle",
            "idle-seconds",
            "animate",
        ],
    )?;
    let [project_path] = positional.as_slice() else {
        return Err(USAGE.to_string());
    };
    if options.is_empty() {
        return Err(format!("No se pidió ningún cambio.\n\n{USAGE}"));
    }
    let mut project = load(project_path)?;
    let stamp = now();
    if let Some(name) = option(&options, "template") {
        apply_template_keeping_choices(&mut project, template_id(name)?)?;
        println!(
            "Plantilla {name}: {} página(s) de menú.",
            project.menus.len()
        );
    }
    if let Some(background) = option(&options, "background") {
        let path = absolute(background)?;
        let (kind, image) = if is_image(&path) {
            (MediaKind::Image, true)
        } else {
            (MediaKind::Video, false)
        };
        let probe = probe_kind(&path, kind).await?;
        let asset = kinodisc_project::import::add_asset(&mut project, &path, probe, kind, &stamp);
        for menu in &mut project.menus {
            menu.background = if image {
                MenuBackground::Static { asset }
            } else {
                MenuBackground::VideoLoop {
                    asset,
                    in_frame: 0,
                    out_frame: 0,
                    loop_forever: true,
                    mute_source: true,
                }
            };
            // The template's own background colour gives way to the picture.
            menu.items
                .retain(|item| !matches!(item, MenuItem::Shape(shape) if shape.id == "tpl:bg"));
        }
        println!(
            "Fondo en {} menú(s): {}",
            project.menus.len(),
            path.display()
        );
    }
    if let Some(music) = option(&options, "music") {
        let path = absolute(music)?;
        let probe = probe_kind(&path, MediaKind::Audio).await?;
        let asset = kinodisc_project::import::add_asset(
            &mut project,
            &path,
            probe,
            MediaKind::Audio,
            &stamp,
        );
        for menu in &mut project.menus {
            menu.audio = Some(kinodisc_domain::MenuAudio {
                asset,
                gain_db: 0.0,
                fade_in_ms: 0,
                fade_out_ms: 0,
                loop_audio: true,
            });
        }
        println!(
            "Música en {} menú(s): {}",
            project.menus.len(),
            path.display()
        );
    }
    let root_id = project.root_menu;
    let root = project
        .menus
        .iter_mut()
        .find(|menu| menu.id == root_id)
        .ok_or("El proyecto no tiene menú principal.")?;
    if let Some(title) = option(&options, "title") {
        let mut found = false;
        for item in &mut root.items {
            if let MenuItem::Text(text) = item {
                if text.id == "tpl:title" {
                    text.text = title.to_string();
                    found = true;
                }
            }
        }
        if !found {
            return Err("El menú principal no tiene título de plantilla: aplica una plantilla primero (--template).".to_string());
        }
        println!("Título: {title}");
    }
    if let Some(idle) = option(&options, "idle") {
        root.timing.auto_play = false;
        root.timing.idle_action = match idle {
            "wait" | "none" => IdleAction::Wait,
            "play-all" | "playall" => IdleAction::PlayAll,
            "intro" => IdleAction::Intro,
            other => {
                return Err(format!(
                    "Acción desconocida: {other}. Usa wait, play-all o intro."
                ))
            }
        };
    }
    if let Some(seconds) = option(&options, "idle-seconds") {
        let seconds: u64 = seconds
            .parse()
            .ok()
            .filter(|seconds| (10..=600).contains(seconds))
            .ok_or("--idle-seconds va de 10 a 600.")?;
        root.timing.duration_ms = seconds * 1000;
    }
    if option(&options, "idle").is_some() || option(&options, "idle-seconds").is_some() {
        println!(
            "Si nadie elige nada en {} s: {:?}",
            root.timing.duration_ms / 1000,
            root.timing.idle()
        );
    }
    if let Some(animate) = option(&options, "animate") {
        let enabled = match animate {
            "on" => true,
            "off" => false,
            other => return Err(format!("--animate acepta on u off, no «{other}».")),
        };
        let changed = kinodisc_menu::set_all_thumbnail_motion(&mut project, enabled, 10_000);
        println!(
            "{changed} miniatura(s) {}.",
            if enabled { "en movimiento" } else { "quietas" }
        );
        if enabled && changed == 0 {
            println!("Ninguna página tiene miniaturas: usa una plantilla con miniaturas (cinema-dark, multi-page-tv-grid…).");
        }
    }
    let root = project
        .menu(project.root_menu)
        .ok_or("El proyecto no tiene menú raíz.")?;
    if root.timing.idle() == IdleAction::Intro && project.first_play.is_none() {
        println!("Aviso: no hay intro; agrégala con `kinodisc-cli project intro {project_path} <vídeo>`.");
    }
    save(&project, project_path)
}

async fn intro(args: &[String]) -> Result<(), String> {
    let flags = ["no-skip", "remove", "idle-only"];
    let (positional, options) = parse(args, &flags)?;
    reject_unknown(&options, &flags)?;
    let mut project = load(positional.first().ok_or(USAGE)?)?;
    let project_path = positional[0].clone();
    if option(&options, "remove").is_some() {
        project.first_play = None;
        save(&project, &project_path)?;
        println!("Intro quitada.");
        return Ok(());
    }
    let [_, video] = positional.as_slice() else {
        return Err(USAGE.to_string());
    };
    let path = absolute(video)?;
    let probe = probe_kind(&path, MediaKind::Video).await?;
    let asset =
        kinodisc_project::import::add_asset(&mut project, &path, probe, MediaKind::Video, &now());
    project.first_play = Some(
        serde_json::from_value(serde_json::json!({
            "asset": asset,
            "targetMenu": project.root_menu,
            "skippable": option(&options, "no-skip").is_none(),
            "atStart": option(&options, "idle-only").is_none(),
        }))
        .map_err(|error| error.to_string())?,
    );
    save(&project, &project_path)?;
    println!(
        "Intro: {} ({}{}).",
        path.display(),
        if option(&options, "idle-only").is_some() {
            "solo si nadie elige nada en el menú: usa `project menu --idle intro`"
        } else {
            "se reproduce al poner el disco"
        },
        if option(&options, "no-skip").is_some() {
            "; no se puede saltar"
        } else {
            "; OK la salta"
        }
    );
    Ok(())
}

fn show(args: &[String]) -> Result<(), String> {
    let [project_path] = args else {
        return Err(USAGE.to_string());
    };
    let project = load(project_path)?;
    let root = project.menu(project.root_menu);
    let summary = serde_json::json!({
        "name": project.name,
        "mode": project.disc.mode,
        "capacity": project.disc.capacity,
        "standard": project.disc.standard,
        "aspect": project.disc.aspect,
        "video": project.disc.video,
        "episodes": project.episodes.iter().map(|episode| serde_json::json!({
            "number": episode.number,
            "title": episode.title,
            "audioTracks": episode.audio.len(),
            "subtitles": episode.subtitles.len(),
            "chapters": episode.chapters.len(),
        })).collect::<Vec<_>>(),
        "menus": project.menus.iter().map(|menu| serde_json::json!({
            "name": menu.name,
            "buttons": menu.buttons.len(),
            "template": menu.template.map(|page| page.template),
            "background": match &menu.background {
                MenuBackground::Static { .. } => "image",
                MenuBackground::VideoLoop { .. } => "video",
            },
            "music": menu.audio.is_some(),
        })).collect::<Vec<_>>(),
        "intro": project.first_play.as_ref().and_then(|intro| project.asset(intro.asset)).map(|asset| asset.display_name.clone()),
        "idle": root.map(|menu| menu.timing.idle()),
        "idleSeconds": root.map(|menu| menu.timing.duration_ms / 1000),
        "diagnostics": project.validate().iter().map(|diagnostic| format!("{} {}", diagnostic.code, diagnostic.message)).collect::<Vec<_>>(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).map_err(|error| error.to_string())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_sort_naturally() {
        let mut names = vec!["Ep 10.mkv", "Ep 2.mkv", "ep 1.mkv", "Ep 02b.mkv"];
        names.sort_by_key(|name| natural_key(Path::new(name)));
        assert_eq!(
            names,
            vec!["ep 1.mkv", "Ep 2.mkv", "Ep 02b.mkv", "Ep 10.mkv"]
        );
    }

    #[test]
    fn options_need_values_and_unknown_ones_are_refused() {
        let args: Vec<String> = ["p.json", "--template", "cinema-dark", "--no-skip"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        let (positional, options) = parse(&args, &["no-skip"]).unwrap();
        assert_eq!(positional, vec!["p.json".to_string()]);
        assert_eq!(option(&options, "template"), Some("cinema-dark"));
        assert!(option(&options, "no-skip").is_some());
        assert!(reject_unknown(&options, &["template"]).is_err());
        let missing: Vec<String> = vec!["p.json".into(), "--title".into()];
        assert!(parse(&missing, &[]).is_err());
    }

    #[test]
    fn timestamps_are_rfc3339() {
        let stamp = now();
        assert_eq!(stamp.len(), 20, "{stamp}");
        assert!(
            stamp.ends_with('Z') && stamp.as_bytes()[10] == b'T',
            "{stamp}"
        );
    }
}
