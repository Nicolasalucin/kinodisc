//! Project persistence and validation.
//!
//! Phase 0 intentionally keeps the format as a versioned JSON document. SQLite
//! will be used later for job history and caches, not as the project format.

pub mod bundle;
pub mod import;
pub mod new;

use kinodisc_domain::{Diagnostic, Project, Severity};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("no se pudo leer o escribir el proyecto: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON de proyecto inválido: {0}")]
    Json(#[from] serde_json::Error),
    #[error("versión de esquema {found} no soportada; se esperaba 1")]
    UnsupportedSchema { found: u32 },
    /// Lists every problem in the same form as the planner's error, so the
    /// app shows them in the interface language (QA 1.2.3: "1 errores",
    /// Spanish only, without saying which).
    #[error("el proyecto tiene errores: {}", joined(.errors))]
    Invalid {
        count: usize,
        errors: Vec<Diagnostic>,
    },
}

fn joined(errors: &[Diagnostic]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Loads a project without running semantic validation.
pub fn load_project(path: impl AsRef<Path>) -> Result<Project, ProjectError> {
    let text = std::fs::read_to_string(path)?;
    let project: Project = serde_json::from_str(&text)?;
    if project.schema_version != CURRENT_SCHEMA_VERSION {
        return Err(ProjectError::UnsupportedSchema {
            found: project.schema_version,
        });
    }
    Ok(project)
}

/// Loads a project and rejects it when there are validation errors.
pub fn load_and_validate(path: impl AsRef<Path>) -> Result<Project, ProjectError> {
    let project = load_project(path)?;
    ensure_valid(&project)?;
    Ok(project)
}

/// Errors the editor can fix by itself: a project that only has these
/// still opens in the app (the render keeps refusing it until it is fixed).
pub const EDITABLE_ERRORS: &[&str] = &["E_AC3_KBPS"];

/// Loads a project for the editor: like [`load_and_validate`], but errors in
/// [`EDITABLE_ERRORS`] do not block opening it.
pub fn load_for_editing(path: impl AsRef<Path>) -> Result<Project, ProjectError> {
    let project = load_project(path)?;
    let errors: Vec<Diagnostic> = project
        .validate()
        .into_iter()
        .filter(|diagnostic| {
            diagnostic.severity == Severity::Error
                && !EDITABLE_ERRORS.contains(&diagnostic.code.as_str())
        })
        .collect();
    if !errors.is_empty() {
        return Err(ProjectError::Invalid {
            count: errors.len(),
            errors,
        });
    }
    Ok(project)
}

/// Serializes a project with stable, human-readable formatting.
pub fn project_to_json(project: &Project) -> Result<String, ProjectError> {
    Ok(serde_json::to_string_pretty(project)?)
}

pub fn save_project(project: &Project, path: impl AsRef<Path>) -> Result<(), ProjectError> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let path = path.as_ref();
    let json = project_to_json(project)?;
    // `fs::write` truncates the project before writing: a crash or a full disk
    // left an empty/partial file. Write a synced sibling and rename it over
    // the target so the previous version survives any failure.
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project.json");
    let temp = parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

pub fn ensure_valid(project: &Project) -> Result<(), ProjectError> {
    let diagnostics = project.validate();
    let errors: Vec<Diagnostic> = diagnostics
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ProjectError::Invalid {
            count: errors.len(),
            errors,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/projects/minimal.json")
    }

    #[test]
    fn a_bad_audio_bitrate_opens_in_the_editor_but_not_for_rendering() {
        let text = std::fs::read_to_string(fixture_path()).expect("fixture");
        let bad = text.replacen("\"ac3Kbps\": 192", "\"ac3Kbps\": 100", 1);
        assert_ne!(bad, text, "the fixture has an AC-3 bitrate to change");
        let dir = std::env::temp_dir().join(format!("kinodisc-ac3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("bad-ac3.json");
        std::fs::write(&path, bad).expect("write");
        assert!(load_for_editing(&path).is_ok());
        assert!(matches!(
            load_and_validate(&path),
            Err(ProjectError::Invalid { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn minimal_fixture_loads_and_validates() {
        let project = load_and_validate(fixture_path()).expect("fixture must be valid");
        assert_eq!(project.schema_version, 1);
        assert_eq!(project.name, "Minimal DVD");
        assert_eq!(project.episodes.len(), 1);
        assert_eq!(project.menus.len(), 2);
    }

    #[test]
    fn avchd_fixture_loads_without_menus() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/projects/avchd-dvd5.json");
        let project = load_and_validate(path).expect("avchd fixture must be valid");
        assert_eq!(project.disc.mode, kinodisc_domain::AuthoringMode::Avchd);
        assert!(project.menus.is_empty());
        assert_eq!(project.episodes.len(), 2);
        assert!(matches!(
            project.disc.video,
            kinodisc_domain::VideoConfig::Avchd(_)
        ));
    }

    #[test]
    fn round_trip_preserves_project() {
        let project = load_project(fixture_path()).expect("fixture must load");
        let json = project_to_json(&project).expect("serialize");
        let parsed: Project = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed.id, project.id);
        assert_eq!(parsed.root_menu, project.root_menu);
    }

    #[test]
    fn schema_is_valid_json() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/project.schema.json");
        let text = std::fs::read_to_string(path).expect("schema must exist");
        let value: serde_json::Value = serde_json::from_str(&text).expect("schema must be JSON");
        assert_eq!(value["title"], "Kinodisc Project");
    }
}
