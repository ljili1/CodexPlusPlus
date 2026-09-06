//! 全局记忆：把用户在设置里维护的「全局记忆」内容写入 Codex 的
//! 用户级 `AGENTS.md`，使其对所有 Codex 会话自动生效。
//!
//! Codex CLI 会读取用户目录下的 `AGENTS.md` 作为全局指令。本模块以
//! 带标记的写入方式管理该文件，避免覆盖用户手动维护的 `AGENTS.md`：
//! 仅在启用且内容非空时写入（带 Codex++ 标记），禁用或清空时若文件
//! 由本模块管理则删除。

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

const MARKER: &str = "<!-- Codex++ global memory (managed by Codex++) -->\n";

/// 根据设置将全局记忆应用到 `$CODEX_HOME/AGENTS.md`。
pub fn apply_global_memory(home: &Path, enabled: bool, content: &str) -> Result<()> {
    let path = home.join("AGENTS.md");
    if enabled && !content.trim().is_empty() {
        let body = format!("{MARKER}{content}\n");
        fs::write(&path, body)
            .with_context(|| format!("写入全局记忆失败: {}", path.display()))?;
        return Ok(());
    }
    if path.exists() {
        let existing = fs::read_to_string(&path).unwrap_or_default();
        if existing.starts_with(MARKER) {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_when_enabled_and_clears_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        apply_global_memory(home, true, "记住用 UTF-8。").unwrap();
        let written = fs::read_to_string(home.join("AGENTS.md")).unwrap();
        assert!(written.contains("记住用 UTF-8。"));
        assert!(written.starts_with(MARKER));

        apply_global_memory(home, false, "记住用 UTF-8。").unwrap();
        assert!(!home.join("AGENTS.md").exists());

        // 不覆盖用户手动维护的 AGENTS.md。
        fs::write(home.join("AGENTS.md"), "user content").unwrap();
        apply_global_memory(home, false, "x").unwrap();
        assert_eq!(fs::read_to_string(home.join("AGENTS.md")).unwrap(), "user content");
    }
}
