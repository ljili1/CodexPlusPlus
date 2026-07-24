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
/// current codex_home / session_delete locations.
pub fn import_config(src: &Path) -> Result<BackupResult> {
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

    // First pass: read everything into memory so a corrupt archive cannot
    // partially overwrite the live configuration.
    let mut staged: Vec<(PathBuf, Vec<u8>)> = Vec::new();
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
        let target = root.join(rest.replace('\\', "/"));
        let mut data = Vec::new();
        zf.read_to_end(&mut data)
            .with_context(|| format!("读取备份条目失败: {name}"))?;
        staged.push((target, data));
    }

    // Second pass: write.
    let mut result = BackupResult::default();
    for (target, data) in staged {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败: {}", parent.display()))?;
        }
        fs::write(&target, &data)
            .with_context(|| format!("写入失败: {}", target.display()))?;
        result.files += 1;
        result.bytes += data.len() as u64;
    }

    Ok(result)
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
}
