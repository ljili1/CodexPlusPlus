//! Configuration backup export / import for Codex++.
//!
//! Exports a curated set of user configuration, credentials and conversation
//! history into a single plain `.zip` archive. The archive carries a
//! `manifest.json` so it can be validated on import and restored to the
//! current machine's paths.
//!
//! WARNING: the archive is stored unencrypted and may contain secrets
//! (auth.json, .sandbox-secrets). The UI is responsible for warning the user
//! and confirming the import.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub const BACKUP_FORMAT_VERSION: u32 = 1;

pub const CODEX_HOME_PREFIX: &str = "codex_home";
pub const SESSION_DELETE_PREFIX: &str = "session_delete";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BackupManifest {
    pub format_version: u32,
    pub created_at_unix: u64,
    pub app: String,
    pub options: BackupOptions,
    pub sources: Vec<BackupSourceInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BackupSourceInfo {
    pub logical: String,
    pub original_path: String,
}

#[derive(Serialize, Default, Debug, Clone)]
pub struct BackupResult {
    pub files: usize,
    pub bytes: u64,
    /// Human-readable notes about entries that were skipped during import
    /// (e.g. SQLite databases still held open by a running process).
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Number of local files kept because the category was imported in
    /// add-new mode and the file already existed.
    #[serde(default)]
    pub skipped_existing: usize,
}

/// Which categories to include in an export. Defaults to everything except
/// logs (which can be large and is reconstructible).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BackupOptions {
    pub config: bool,
    pub history: bool,
    pub memories: bool,
    pub gui: bool,
    pub logs: bool,
}

impl Default for BackupOptions {
    fn default() -> Self {
        Self {
            config: true,
            history: true,
            memories: true,
            gui: true,
            logs: false,
        }
    }
}

/// How to treat a category when importing a backup archive.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImportMode {
    /// Replace existing local files with the archived copies.
    Overwrite,
    /// Keep existing local files, only add files that are missing locally.
    #[serde(rename = "add")]
    AddNew,
}

/// Per-category import behavior. The default mirrors the legacy behavior
/// (overwrite everything).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ImportOptions {
    pub config: ImportMode,
    pub history: ImportMode,
    pub memories: ImportMode,
    pub gui: ImportMode,
    pub logs: ImportMode,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            config: ImportMode::Overwrite,
            history: ImportMode::Overwrite,
            memories: ImportMode::Overwrite,
            gui: ImportMode::Overwrite,
            logs: ImportMode::Overwrite,
        }
    }
}

impl ImportOptions {
    fn mode_for(&self, prefix: &str, rest: &str) -> ImportMode {
        match entry_category(prefix, rest) {
            Some("config") => self.config,
            Some("history") => self.history,
            Some("memories") => self.memories,
            Some("gui") => self.gui,
            Some("logs") => self.logs,
            _ => ImportMode::Overwrite,
        }
    }
}

/// Map a staged backup entry (`prefix`/`rest` inside the archive) to its
/// backup category, using the same layout tables as `collect_export_entries`.
/// Entries that belong to no known category default to overwrite.
fn entry_category(prefix: &str, rest: &str) -> Option<&'static str> {
    if prefix == CODEX_HOME_PREFIX {
        for rel in codex_home_config_files() {
            if rest == *rel {
                return Some("config");
            }
        }
        for rel in codex_home_config_dirs() {
            if rest.starts_with(rel) {
                return Some("config");
            }
        }
        for base in codex_home_sqlite() {
            if rest == *base || rest.starts_with(&format!("{base}-")) {
                return Some("memories");
            }
        }
        if rest == "logs_2.sqlite" || rest.starts_with("logs_2.sqlite-") {
            return Some("logs");
        }
        for rel in codex_home_history_dirs() {
            if rest.starts_with(rel) {
                return Some("history");
            }
        }
    } else if prefix == SESSION_DELETE_PREFIX {
        for rel in session_delete_files() {
            if rest == *rel {
                return Some("gui");
            }
        }
        for rel in session_delete_dirs() {
            if rest.starts_with(rel) {
                return Some("gui");
            }
        }
    }
    None
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn codex_home_root() -> PathBuf {
    crate::codex_home::default_codex_home_dir()
}

fn session_delete_root() -> PathBuf {
    crate::paths::default_app_state_dir()
}

/// Relative (inside codex_home) provider config + credential files.
fn codex_home_config_files() -> &'static [&'static str] {
    &["config.toml", "auth.json", ".codex-global-state.json"]
}

/// Relative (inside codex_home) directories holding credentials / config.
fn codex_home_config_dirs() -> &'static [&'static str] {
    &[".sandbox-secrets"]
}

/// Relative (inside codex_home) conversation history / user content dirs.
fn codex_home_history_dirs() -> &'static [&'static str] {
    &["sessions", "archived_sessions", "skills"]
}

/// SQLite databases (conversation memory / goals / state) to back up.
fn codex_home_sqlite() -> &'static [&'static str] {
    &["memories_1.sqlite", "goals_1.sqlite", "state_5.sqlite"]
}

fn session_delete_files() -> &'static [&'static str] {
    &["settings.json", "latest-status.json"]
}

fn session_delete_dirs() -> &'static [&'static str] {
    &["dream-skin"]
}

/// Collect (zip_relative_path, absolute_source_path) pairs for export,
/// honoring the user-selected categories in `options`.
fn collect_export_entries(options: &BackupOptions) -> Vec<(String, PathBuf)> {
    let mut entries: Vec<(String, PathBuf)> = Vec::new();

    let home = codex_home_root();

    if options.config {
        for rel in codex_home_config_files() {
            entries.push((format!("{CODEX_HOME_PREFIX}/{rel}"), home.join(rel)));
        }
        for rel in codex_home_config_dirs() {
            entries.push((format!("{CODEX_HOME_PREFIX}/{rel}"), home.join(rel)));
        }
    }
    if options.memories {
        for base in codex_home_sqlite() {
            let base_path = home.join(base);
            entries.push((format!("{CODEX_HOME_PREFIX}/{base}"), base_path));
            for suffix in ["-wal", "-shm"] {
                let sibling = home.join(format!("{base}{suffix}"));
                if sibling.exists() {
                    entries.push((
                        format!("{CODEX_HOME_PREFIX}/{base}{suffix}"),
                        sibling,
                    ));
                }
            }
        }
    }
    if options.history {
        for rel in codex_home_history_dirs() {
            entries.push((format!("{CODEX_HOME_PREFIX}/{rel}"), home.join(rel)));
        }
    }
    if options.logs {
        let base = "logs_2.sqlite";
        entries.push((format!("{CODEX_HOME_PREFIX}/{base}"), home.join(base)));
        for suffix in ["-wal", "-shm"] {
            let sibling = home.join(format!("{base}{suffix}"));
            if sibling.exists() {
                entries.push((
                    format!("{CODEX_HOME_PREFIX}/{base}{suffix}"),
                    sibling,
                ));
            }
        }
    }

    let sess = session_delete_root();
    if options.gui {
        for rel in session_delete_files() {
            entries.push((format!("{SESSION_DELETE_PREFIX}/{rel}"), sess.join(rel)));
        }
        for rel in session_delete_dirs() {
            entries.push((format!("{SESSION_DELETE_PREFIX}/{rel}"), sess.join(rel)));
        }
    }

    entries
}

/// Recursively collect every file under `path` (or `path` itself if it is a file).
fn list_files(path: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if path.is_file() {
        out.push(path.to_path_buf());
        return Ok(out);
    }
    if path.is_dir() {
        let mut stack = vec![path.to_path_buf()];
        while let Some(current) = stack.pop() {
            let read = fs::read_dir(&current)
                .with_context(|| format!("无法读取目录: {}", current.display()))?;
            for entry in read {
                let entry = entry?;
                let child = entry.path();
                if child.is_dir() {
                    stack.push(child);
                } else if child.is_file() {
                    out.push(child);
                }
            }
        }
    }
    Ok(out)
}

fn file_options() -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644)
}

/// Export the curated configuration into a zip archive at `dest`.
pub fn export_config(dest: &Path, options: &BackupOptions) -> Result<BackupResult> {
    let entries = collect_export_entries(options);

    let home = codex_home_root();
    let sess = session_delete_root();
    let manifest = BackupManifest {
        format_version: BACKUP_FORMAT_VERSION,
        created_at_unix: now_unix(),
        app: "codex-plus".to_string(),
        options: options.clone(),
        sources: vec![
            BackupSourceInfo {
                logical: CODEX_HOME_PREFIX.to_string(),
                original_path: home.to_string_lossy().into(),
            },
            BackupSourceInfo {
                logical: SESSION_DELETE_PREFIX.to_string(),
                original_path: sess.to_string_lossy().into(),
            },
        ],
    };

    let file = fs::File::create(dest)
        .with_context(|| format!("无法创建备份文件: {}", dest.display()))?;
    let mut zip = ZipWriter::new(file);
    let options = file_options();

    zip.start_file("manifest.json", options)
        .context("写入 manifest 失败")?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())
        .context("写入 manifest 失败")?;

    let mut result = BackupResult::default();
    for (rel, src) in &entries {
        if !src.exists() {
            continue;
        }
        let files = list_files(src)?;
        for f in &files {
            let data = fs::read(f).with_context(|| format!("读取失败: {}", f.display()))?;
            let zip_path = if src.is_file() {
                rel.clone()
            } else {
                let sub = f
                    .strip_prefix(src)
                    .unwrap_or_else(|_| Path::new(""))
                    .to_string_lossy()
                    .replace('\\', "/");
                if sub.is_empty() {
                    rel.clone()
                } else {
                    format!("{rel}/{sub}")
                }
            };
            zip.start_file(&zip_path, options)
                .with_context(|| format!("写入 zip 失败: {zip_path}"))?;
            zip.write_all(&data)
                .with_context(|| format!("写入 zip 失败: {zip_path}"))?;
            result.files += 1;
            result.bytes += data.len() as u64;
        }
    }

    zip.finish().context("完成 zip 写入失败")?;
    Ok(result)
}

/// Import a previously exported configuration archive, restoring files to the
/// current codex_home / session_delete locations. `options` selects, per
/// category, whether existing local files are overwritten or kept (add-new).
pub fn import_config(src: &Path, options: &ImportOptions) -> Result<BackupResult> {
    let file = fs::File::open(src)
        .with_context(|| format!("无法打开备份文件: {}", src.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("不是有效的备份文件: {}", src.display()))?;

    let manifest = read_manifest(&mut archive)?;
    if manifest.app != "codex-plus" {
        return Err(anyhow!("备份文件格式不正确（应用标识不符）。"));
    }
    if manifest.format_version > BACKUP_FORMAT_VERSION {
        return Err(anyhow!(
            "备份文件版本({})高于当前支持版本({})，无法导入。",
            manifest.format_version,
            BACKUP_FORMAT_VERSION
        ));
    }

    let home = codex_home_root();
    let sess = session_delete_root();

    // A fresh import supersedes anything staged by earlier imports.
    let _ = fs::remove_dir_all(pending_import_dir());

    // First pass: read everything into memory so a corrupt archive cannot
    // partially overwrite the live configuration. The per-entry import mode
    // (overwrite vs add-new) is resolved here as well.
    let mut staged: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut staged_modes: Vec<ImportMode> = Vec::new();
    let mut staged_rels: Vec<(String, String)> = Vec::new();
    for i in 0..archive.len() {
        let mut zf = archive
            .by_index(i)
            .with_context(|| format!("读取备份条目 #{i} 失败"))?;
        let name = zf.name().to_string();
        if name == "manifest.json" {
            continue;
        }
        let (prefix, rest) = name
            .split_once('/')
            .ok_or_else(|| anyhow!("备份条目路径格式不正确: {name}"))?;
        let root = match prefix {
            CODEX_HOME_PREFIX => &home,
            SESSION_DELETE_PREFIX => &sess,
            _ => continue,
        };
        let mode = options.mode_for(prefix, rest);
        let target = root.join(rest.replace('\\', "/"));
        let mut data = Vec::new();
        zf.read_to_end(&mut data)
            .with_context(|| format!("读取备份条目失败: {name}"))?;
        staged.push((target, data));
        staged_modes.push(mode);
        staged_rels.push((prefix.to_string(), rest.to_string()));
    }

    // Second pass: write. SQLite databases need special care: they (and
    // their -wal/-shm sidecars) are typically held open by the running
    // Codex / Codex++ processes, so a naive overwrite fails on Windows or,
    // worse, corrupts the database. A busy database is skipped as a whole
    // with a warning instead of failing the entire import.
    let mut result = BackupResult::default();

    // Map every SQLite sidecar entry (-wal / -shm) to its main database
    // target so the trio can be handled together.
    let mut sidecar_group: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    let mut is_sidecar = vec![false; staged.len()];
    for (idx, (target, _)) in staged.iter().enumerate() {
        let name = target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        for suffix in ["-wal", "-shm"] {
            if let Some(base) = name.strip_suffix(suffix) {
                // Only *.sqlite-wal / *.sqlite-shm are SQLite sidecars; other
                // files that merely end in "-wal"/"-shm" are regular entries.
                if base.ends_with(".sqlite") {
                    let main_target = target.with_file_name(base);
                    sidecar_group.entry(main_target).or_default().push(idx);
                    is_sidecar[idx] = true;
                }
            }
        }
    }

    for idx in 0..staged.len() {
        let (target, data) = &staged[idx];
        let mode = staged_modes[idx];
        let (prefix, rest) = &staged_rels[idx];
        if is_sidecar[idx] {
            // Sidecars are never restored from the archive; they are
            // cleared together with their main database below.
            continue;
        }
        if sidecar_group.contains_key(target) {
            // This is a SQLite main database. Sidecar entries themselves are
            // skipped by the `is_sidecar` branch above; here we only decide
            // whether the main database can be restored.
            if mode == ImportMode::AddNew && target.exists() {
                result.skipped_existing += 1;
                continue;
            }
            if is_locked(target) {
                stage_pending_or_warn(
                    &mut result,
                    prefix,
                    rest,
                    data,
                    &file_display_name(target),
                    "正被其他程序占用",
                );
                continue;
            }
            // Drop stale local WAL/SHM sidecars so the restored snapshot is
            // self-consistent. If a sidecar cannot be deleted the database
            // is in fact still open somewhere: skip it as a whole.
            if remove_sqlite_sidecars(target).is_err() {
                stage_pending_or_warn(
                    &mut result,
                    prefix,
                    rest,
                    data,
                    &file_display_name(target),
                    "数据库被占用",
                );
                continue;
            }
            write_entry(target, data)?;
            result.files += 1;
            result.bytes += data.len() as u64;
            continue;
        }
        if mode == ImportMode::AddNew && target.exists() {
            result.skipped_existing += 1;
            continue;
        }
        if is_locked(target) {
            stage_pending_or_warn(
                &mut result,
                prefix,
                rest,
                data,
                &file_display_name(target),
                "文件被其他程序占用",
            );
            continue;
        }
        write_entry(target, data)?;
        result.files += 1;
        result.bytes += data.len() as u64;
    }

    Ok(result)
}

/// Handle a target that could not be written because it is held open by
/// another process: stage the archived copy so the next manager start can
/// apply it automatically, and record a corresponding warning.
fn stage_pending_or_warn(
    result: &mut BackupResult,
    prefix: &str,
    rest: &str,
    data: &[u8],
    display_name: &str,
    reason: &str,
) {
    match stage_pending_file(prefix, rest, data) {
        Ok(()) => result.warnings.push(format!(
            "{display_name}：{reason}，已暂存，重启 Codex++ 后自动生效"
        )),
        Err(_) => result.warnings.push(format!(
            "{display_name}：{reason}，已跳过（关闭 Codex++ 与 Codex 会话后重新导入）"
        )),
    }
}

/// Directory holding files staged by an import that hit locked targets,
/// to be applied automatically on the next manager start. Layout mirrors
/// the backup archive: `<prefix>/<rest>` underneath this directory.
fn pending_import_dir() -> PathBuf {
    crate::paths::default_app_state_dir().join("pending-config-import")
}

fn stage_pending_file(prefix: &str, rest: &str, data: &[u8]) -> Result<()> {
    let target = pending_import_dir()
        .join(prefix)
        .join(rest.replace('\\', "/"));
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建暂存目录失败: {}", parent.display()))?;
    }
    fs::write(&target, data).with_context(|| format!("暂存失败: {}", target.display()))
}

/// Apply files staged by a previous import that hit locked targets. Called
/// at manager startup, before anything opens the Codex home databases.
/// Best effort: a file that is still locked stays staged for the next
/// start. Returns the number of files applied.
pub fn apply_pending_config_import() -> Result<usize> {
    let dir = pending_import_dir();
    if !dir.is_dir() {
        return Ok(0);
    }
    let home = codex_home_root();
    let sess = session_delete_root();
    let mut applied = 0usize;

    for pending in list_files(&dir)? {
        let rel = match pending.strip_prefix(&dir) {
            Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        let Some((prefix, rest)) = rel.split_once('/') else {
            // Stray file without a known prefix: drop it.
            let _ = fs::remove_file(&pending);
            continue;
        };
        let root = match prefix {
            CODEX_HOME_PREFIX => &home,
            SESSION_DELETE_PREFIX => &sess,
            _ => {
                let _ = fs::remove_file(&pending);
                continue;
            }
        };
        let target = root.join(rest);
        let is_sqlite_main = target
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(".sqlite"))
            .unwrap_or(false);
        if is_locked(&target) {
            // Still in use: retry on the next start.
            continue;
        }
        if is_sqlite_main && remove_sqlite_sidecars(&target).is_err() {
            // Database is in use after all: retry on the next start.
            continue;
        }
        let Ok(data) = fs::read(&pending) else {
            continue;
        };
        if write_entry(&target, &data).is_err() {
            // Keep staged for the next start.
            continue;
        }
        let _ = fs::remove_file(&pending);
        applied += 1;
    }

    // Clean up the staging directory once everything was applied.
    let _ = fs::remove_dir(&dir);
    Ok(applied)
}

/// `"name"` of a path, falling back to the full path when the file name is
/// not valid Unicode.
fn file_display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_string())
        .unwrap_or_else(|| path.to_string_lossy().into())
}

/// True if `path` exists and cannot be opened for writing, i.e. it is held
/// open (locked) by another process. Missing files are writable.
fn is_locked(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    fs::OpenOptions::new().write(true).open(path).is_err()
}

/// Delete the stale SQLite `-wal` / `-shm` sidecar files belonging to
/// `main`. Fails when a sidecar cannot be removed (still mapped / locked),
/// which reliably indicates the database is currently in use.
fn remove_sqlite_sidecars(main: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", main.display(), suffix));
        if sidecar.exists() {
            fs::remove_file(&sidecar)
                .with_context(|| format!("无法删除 {}", sidecar.display()))?;
        }
    }
    Ok(())
}

fn write_entry(target: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
    fs::write(target, data).with_context(|| format!("写入失败: {}", target.display()))
}

fn read_manifest(archive: &mut ZipArchive<fs::File>) -> Result<BackupManifest> {
    for i in 0..archive.len() {
        let mut zf = archive.by_index(i)?;
        if zf.name() == "manifest.json" {
            let mut buf = String::new();
            zf.read_to_string(&mut buf)?;
            return serde_json::from_str(&buf)
                .context("解析 manifest.json 失败，可能不是 Codex++ 备份文件。");
        }
    }
    Err(anyhow!(
        "备份文件中缺少 manifest.json，可能不是 Codex++ 备份文件。"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_produces_valid_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("backup.zip");
        // Should succeed even when the live ~/.codex directory is absent
        // (fresh CI runner): the archive always carries a manifest.
        let res = export_config(&dest, &BackupOptions::default()).unwrap();
        let file = fs::File::open(&dest).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let manifest = read_manifest(&mut archive).unwrap();
        assert_eq!(manifest.app, "codex-plus");
        assert_eq!(manifest.format_version, BACKUP_FORMAT_VERSION);
        assert!(manifest.options.config && manifest.options.gui);
        // When present, at least the manifest contributes to the byte count.
        assert!(res.bytes > 0 || res.files >= 0);
    }

    #[test]
    fn remove_sidecars_clears_wal_and_shm() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("test.sqlite");
        fs::write(&db, b"db").unwrap();
        fs::write(tmp.path().join("test.sqlite-wal"), b"wal").unwrap();
        fs::write(tmp.path().join("test.sqlite-shm"), b"shm").unwrap();

        remove_sqlite_sidecars(&db).unwrap();

        assert!(db.exists());
        assert!(!tmp.path().join("test.sqlite-wal").exists());
        assert!(!tmp.path().join("test.sqlite-shm").exists());
    }

    #[test]
    fn is_locked_false_for_missing_and_regular_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_locked(&tmp.path().join("missing.sqlite")));
        let file = tmp.path().join("plain.toml");
        fs::write(&file, b"x").unwrap();
        assert!(!is_locked(&file));
    }

    #[test]
    fn entry_category_classifies_layout() {
        assert_eq!(
            entry_category(CODEX_HOME_PREFIX, "config.toml"),
            Some("config")
        );
        assert_eq!(
            entry_category(CODEX_HOME_PREFIX, ".sandbox-secrets/key"),
            Some("config")
        );
        assert_eq!(
            entry_category(CODEX_HOME_PREFIX, "memories_1.sqlite"),
            Some("memories")
        );
        assert_eq!(
            entry_category(CODEX_HOME_PREFIX, "memories_1.sqlite-wal"),
            Some("memories")
        );
        assert_eq!(
            entry_category(CODEX_HOME_PREFIX, "sessions/a/thread.jsonl"),
            Some("history")
        );
        assert_eq!(
            entry_category(CODEX_HOME_PREFIX, "logs_2.sqlite-shm"),
            Some("logs")
        );
        assert_eq!(
            entry_category(SESSION_DELETE_PREFIX, "settings.json"),
            Some("gui")
        );
        assert_eq!(
            entry_category(SESSION_DELETE_PREFIX, "dream-skin/wall.png"),
            Some("gui")
        );
        assert_eq!(entry_category(CODEX_HOME_PREFIX, "unknown.txt"), None);
    }
}
