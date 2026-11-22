//! Plain-text record of each production, burn and erase, kept in
//! `<app data>/logs` (Windows: `%LOCALAPPDATA%\org.kinodisc.app\logs`) so a
//! user can attach it to a bug report. Local only: nothing is sent. The
//! user's home folder is written as `~`. A log that cannot be written never
//! fails the job, and only the newest logs are kept.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Logs kept per folder (all kinds together).
const KEEP: usize = 30;
/// The kinds this module writes; pruning never touches other files.
const KINDS: [&str; 3] = ["produccion", "grabacion", "borrado"];

pub fn logs_dir() -> Option<PathBuf> {
    kinodisc_toolchain::legacy::app_data_dir().map(|dir| dir.join("logs"))
}

pub struct JobLog {
    file: Option<std::fs::File>,
    path: Option<PathBuf>,
    redact: Vec<String>,
}

impl JobLog {
    /// Starts `<kind>-<date>-<time>.log`; `kind` is one of [`KINDS`].
    pub fn create(kind: &str) -> Self {
        let Some(dir) = logs_dir() else {
            return Self::disabled();
        };
        Self::create_in(&dir, kind)
    }

    pub fn create_in(dir: &Path, kind: &str) -> Self {
        debug_assert!(KINDS.contains(&kind));
        if std::fs::create_dir_all(dir).is_err() {
            return Self::disabled();
        }
        prune(dir, KEEP.saturating_sub(1));
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let mut opened = None;
        for attempt in 0..10 {
            let name = if attempt == 0 {
                format!("{kind}-{stamp}.log")
            } else {
                format!("{kind}-{stamp}-{attempt}.log")
            };
            let path = dir.join(name);
            if let Ok(file) = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                opened = Some((file, path));
                break;
            }
        }
        let Some((file, path)) = opened else {
            return Self::disabled();
        };
        let mut log = Self {
            file: Some(file),
            path: Some(path),
            redact: home_prefixes(),
        };
        log.line(format!(
            "Kinodisc {} · {} {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        log
    }

    fn disabled() -> Self {
        Self {
            file: None,
            path: None,
            redact: Vec::new(),
        }
    }

    /// Appends one timestamped line (or several, for multi-line text).
    pub fn line(&mut self, text: impl AsRef<str>) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let text = redact(text.as_ref(), &self.redact);
        let time = chrono::Local::now().format("%H:%M:%S");
        for line in text.lines() {
            let _ = writeln!(file, "{time}  {line}");
        }
        let _ = file.flush();
    }

    /// Where the log is, home folder written as `~`.
    pub fn display_path(&self) -> Option<String> {
        self.path
            .as_ref()
            .map(|path| redact(&path.display().to_string(), &self.redact))
    }
}

/// Spellings of the user's home folder: the profile path, with either
/// slash, and the short 8.3 form Windows uses in `TEMP`.
fn home_prefixes() -> Vec<String> {
    let mut prefixes = Vec::new();
    for name in ["USERPROFILE", "HOME"] {
        if let Some(value) = std::env::var_os(name).filter(|value| !value.is_empty()) {
            prefixes.push(
                value
                    .to_string_lossy()
                    .trim_end_matches(['\\', '/'])
                    .to_string(),
            );
        }
    }
    let temp = std::env::temp_dir().display().to_string();
    for marker in ["\\AppData\\", "/AppData/"] {
        if let Some(index) = temp.find(marker) {
            prefixes.push(temp[..index].to_string());
        }
    }
    let forward: Vec<String> = prefixes
        .iter()
        .map(|prefix| prefix.replace('\\', "/"))
        .collect();
    prefixes.extend(forward);
    prefixes.retain(|prefix| prefix.len() > 3);
    // Longest first, so a prefix never leaves part of a longer one behind.
    prefixes.sort_by_key(|prefix| std::cmp::Reverse(prefix.len()));
    prefixes.dedup();
    prefixes
}

fn redact(text: &str, prefixes: &[String]) -> String {
    let mut text = text.to_string();
    for prefix in prefixes {
        text = replace_ignoring_case(&text, prefix, "~");
    }
    text
}

/// Windows paths are case-insensitive; ASCII case folding is enough for the
/// comparison and keeps byte offsets intact.
fn replace_ignoring_case(text: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    let haystack = text.to_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (index, _) in haystack.match_indices(&needle_lower) {
        if index < last {
            continue;
        }
        out.push_str(&text[last..index]);
        out.push_str(with);
        last = index + needle.len();
    }
    out.push_str(&text[last..]);
    out
}

/// Removes the oldest logs of ours so that at most `keep` remain.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let ours = name.ends_with(".log")
                && KINDS
                    .iter()
                    .any(|kind| name.starts_with(&format!("{kind}-")));
            if !ours {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            Some((metadata.modified().ok()?, entry.path()))
        })
        .collect();
    if logs.len() <= keep {
        return;
    }
    logs.sort();
    let excess = logs.len() - keep;
    for (_, path) in logs.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_home_folder_is_written_as_a_tilde_in_any_spelling() {
        let prefixes = vec![
            r"C:\Users\Ana Pérez".to_string(),
            "C:/Users/Ana Pérez".to_string(),
            r"C:\Users\ANAPER~1".to_string(),
        ];
        let text = r"ISO c:\users\ana pérez\Videos\x.iso y C:/Users/Ana Pérez/a, temp C:\Users\ANAPER~1\AppData\Local\Temp";
        assert_eq!(
            redact(text, &prefixes),
            r"ISO ~\Videos\x.iso y ~/a, temp ~\AppData\Local\Temp"
        );
    }

    #[test]
    fn logs_are_written_and_only_the_newest_of_ours_are_kept() {
        let dir = std::env::temp_dir().join(format!("kd-joblog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("otro.txt"), b"no es nuestro").unwrap();
        for index in 0..(KEEP + 5) {
            std::fs::write(dir.join(format!("grabacion-2026010{index:02}.log")), b"x").unwrap();
        }
        let mut log = JobLog::create_in(&dir, "produccion");
        log.line("línea uno\nlínea dos");
        let path = log.path.clone().unwrap();
        drop(log);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.lines().next().unwrap().contains("Kinodisc "), "{text}");
        assert!(
            text.contains("  línea uno\n") && text.contains("  línea dos\n"),
            "{text}"
        );
        let ours = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".log"))
            .count();
        assert_eq!(ours, KEEP);
        assert!(dir.join("otro.txt").is_file(), "other files are left alone");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
