//! NewAPI 进程管理
//!
//! 当 Codex++ 或其管理工具以 NEW API 作为供应商运行时，自动以无终端窗口的
//! 后台静默方式唤起 newapi 进程；当两个入口均关闭时，自动终止 newapi 进程。
//!
//! 由于管理工具与静默启动器是彼此独立的进程，这里用应用状态目录下的一个
//! 租约状态文件做跨进程引用计数：每个希望 newapi 运行的入口都会登记一个
//! owner（"manager" / "launcher"），租约清零时才真正结束进程。

use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::install::newapi_binary_path;
use crate::paths::default_app_state_dir;

const NEWAPI_STATE_FILE: &str = "newapi-state.json";
const NEWAPI_DEFAULT_PORT: u16 = 3000;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct NewApiState {
    /// 由本工具拉起的 newapi 进程 PID（仅当我们拥有时才会在租约清零时结束它）
    pid: Option<u32>,
    /// 当前希望 newapi 运行的入口集合
    owners: Vec<String>,
}

/// 进程内持有的子进程句柄，便于在所属进程退出时回收。
static LOCAL_CHILD: Mutex<Option<std::process::Child>> = Mutex::new(None);

fn state_path() -> PathBuf {
    default_app_state_dir().join(NEWAPI_STATE_FILE)
}

fn load_state() -> NewApiState {
    let path = state_path();
    if let Ok(mut file) = std::fs::File::open(&path) {
        let mut content = String::new();
        if file.read_to_string(&mut content).is_ok() {
            if let Ok(state) = serde_json::from_str::<NewApiState>(&content) {
                return state;
            }
        }
    }
    NewApiState::default()
}

fn save_state(state: &NewApiState) {
    let dir = default_app_state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = state_path();
    if let Ok(content) = serde_json::to_string(state) {
        let _ = std::fs::write(&path, content);
    }
}

/// 探测 newapi 是否已在默认端口监听（说明进程已经在运行）
pub fn is_newapi_running() -> bool {
    port_listening(NEWAPI_DEFAULT_PORT)
}

fn port_listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(400),
    )
    .is_ok()
}

/// 判断当前设置是否使用 NEWAPI 作为供应商。
/// 优先匹配供应商 id 为 "newapi"；对于旧配置，也兼容 baseUrl 指向 localhost:3000/v1 的情况。
pub fn active_profile_uses_newapi(settings: &crate::settings::BackendSettings) -> bool {
    if settings.active_relay_id == "newapi" {
        return true;
    }
    let profile = settings.active_relay_profile();
    profile.base_url == "http://localhost:3000/v1"
        || profile.upstream_base_url == "http://localhost:3000/v1"
}

/// newapi 可执行文件是否存在于安装目录
pub fn newapi_is_available() -> bool {
    newapi_binary_path().exists()
}

/// 第三方 new-api 仓库（QuantumNous/new-api）的最新 release 下载镜像。
/// 用于运行时/安装时拉取 newapi 二进制，避免随签名包分发未签名的第三方二进制。
const NEWAPI_DOWNLOAD_MIRRORS: [&str; 2] = [
    "https://gh-proxy.com/https://github.com",
    "https://xt.ljili.dpdns.org/https://github.com",
];
const NEWAPI_REPO: &str = "QuantumNous/new-api";

/// 在需要时下载 newapi 二进制到目标目录；若目标目录已存在 newapi，则跳过下载。
///
/// 该函数在首次真正需要启动 newapi 时调用（安装/首次启动阶段）。下载失败不会
/// 阻断启动流程——后续 `spawn_newapi` 发现二进制缺失会正常返回 None 并提示用户。
fn download_newapi_if_missing() {
    let binary = newapi_binary_path();
    if binary.exists() {
        // 目标目录已存在 newapi，跳过下载。
        return;
    }

    let (tag, asset) = match resolve_newapi_asset() {
        Some(pair) => pair,
        None => {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "newapi.resolve_failed",
                serde_json::json!({ "path": binary.to_string_lossy() }),
            );
            return;
        }
    };

    let client = match reqwest::blocking::Client::builder().build() {
        Ok(client) => client,
        Err(_) => return,
    };

    for mirror in NEWAPI_DOWNLOAD_MIRRORS {
        let url = format!(
            "{mirror}/{NEWAPI_REPO}/releases/download/{tag}/{asset}"
        );
        match client.get(&url).send() {
            Ok(resp) if resp.status().is_success() => match resp.bytes() {
                Ok(bytes) => {
                    // 校验下载内容确为本平台可执行文件；否则可能是镜像返回了错误页面
                    // 或错误资产，跳过该镜像继续尝试下一个。
                    if !looks_like_executable(&bytes) {
                        let _ = crate::diagnostic_log::append_diagnostic_log(
                            "newapi.download_not_executable",
                            serde_json::json!({ "url": url, "size": bytes.len() }),
                        );
                        continue;
                    }
                    // 先写入临时文件再原子替换，避免半截文件被误判为「已存在」。
                    let tmp = binary.with_extension("tmp");
                    if std::fs::write(&tmp, &bytes).is_ok() {
                        let _ = std::fs::rename(&tmp, &binary);
                        set_executable(&binary);
                        let _ = crate::diagnostic_log::append_diagnostic_log(
                            "newapi.downloaded",
                            serde_json::json!({ "url": url, "size": bytes.len() }),
                        );
                        return;
                    }
                    let _ = std::fs::remove_file(&tmp);
                }
                Err(_) => continue,
            },
            _ => continue,
        }
    }

    let _ = crate::diagnostic_log::append_diagnostic_log(
        "newapi.download_failed",
        serde_json::json!({ "tag": tag, "asset": asset }),
    );
}

/// 解析匹配当前平台/架构的最新 new-api 资产名与版本 tag。
fn resolve_newapi_asset() -> Option<(String, String)> {
    let api_url = format!("https://api.github.com/repos/{NEWAPI_REPO}/releases/latest");
    let endpoints = [
        api_url.clone(),
        format!("https://gh-proxy.com/{api_url}"),
        format!("https://xt.ljili.dpdns.org/{api_url}"),
    ];

    let client = reqwest::blocking::Client::builder().build().ok()?;
    for endpoint in endpoints {
        let resp = client
            .get(&endpoint)
            .header("User-Agent", "codex-plus-plus")
            .send()
            .ok()?;
        if !resp.status().is_success() {
            continue;
        }
        let json: serde_json::Value = match resp.json() {
            Ok(value) => value,
            Err(_) => continue,
        };
        let tag = json.get("tag_name")?.as_str()?.to_string();
        let assets = json.get("assets")?.as_array()?;
        for asset in assets {
            if let Some(name) = asset.get("name").and_then(|n| n.as_str()) {
                if asset_matches_newapi(name) {
                    return Some((tag, name.to_string()));
                }
            }
        }
    }
    None
}

/// 判断给定资产名是否匹配当前平台/架构。
///
/// 必须精确到平台后缀，否则 Windows 端 `contains("new-api")` 会先命中
/// `new-api-arm64-...` / `new-api-macos-...` / `new-api-linux-...` 等其它平台资产，
/// 下载到的是 Mach-O / ELF 文件，Windows 运行时会报「不支持的 16 位应用程序」。
fn asset_matches_newapi(name: &str) -> bool {
    if cfg!(target_os = "windows") {
        name.ends_with(".exe") && name.contains("new-api")
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            name.contains("new-api-arm64")
        } else {
            name.contains("new-api-macos")
        }
    } else {
        name.contains("new-api")
            && !name.contains("macos")
            && !name.contains("arm64")
            && !name.ends_with(".exe")
    }
}

/// 粗略校验下载内容是否像本平台可执行文件，避免把错误页面/损坏数据当成二进制。
fn looks_like_executable(bytes: &[u8]) -> bool {
    if bytes.len() < 4 {
        return false;
    }
    if cfg!(target_os = "windows") {
        // PE 文件以 "MZ" 魔术字开头
        bytes[0] == 0x4D && bytes[1] == 0x5A
    } else if cfg!(target_os = "macos") {
        // Mach-O：thin(0xFEEDFAC3/0xFEEDFACF) 或 fat(0xCAFEBABE/0xBEBAFECA) 魔术字
        matches!(
            &bytes[0..4],
            [0xFE, 0xED, 0xFA, 0xCE]
                | [0xFE, 0xED, 0xFA, 0xCF]
                | [0xCA, 0xFE, 0xBA, 0xBE]
                | [0xBE, 0xBA, 0xFE, 0xCA]
        )
    } else {
        // ELF 文件以 0x7F 'E' 'L' 'F' 开头
        bytes[0] == 0x7F && bytes[1] == 0x45 && bytes[2] == 0x4C && bytes[3] == 0x46
    }
}

/// 在非 Windows 平台为下载得到的二进制加上可执行权限。
fn set_executable(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
}

/// 尝试唤起 newapi 进程（后台静默）。返回是否成功启动。
fn spawn_newapi() -> Option<u32> {
    let binary = newapi_binary_path();
    // 安装/首次启动阶段按需下载；目标目录已存在 newapi 时跳过下载。
    download_newapi_if_missing();
    if !binary.exists() {
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "newapi.binary_missing",
            serde_json::json!({ "path": binary.to_string_lossy() }),
        );
        return None;
    }

    let mut command = std::process::Command::new(&binary);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(crate::windows_create_no_window());
    }
    #[cfg(unix)]
    {
        // 脱离终端，避免被 SIGHUP 影响；stdio 已重定向到 /dev/null
        use std::os::unix::process::CommandExt;
        let _ = command.process_group(0);
    }

    match command.spawn() {
        Ok(child) => {
            let pid = child.id();
            // 仅在本进程内保存句柄，便于退出时回收
            if let Ok(mut guard) = LOCAL_CHILD.lock() {
                *guard = Some(child);
            }
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "newapi.spawned",
                serde_json::json!({ "pid": pid, "path": binary.to_string_lossy() }),
            );
            Some(pid)
        }
        Err(error) => {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "newapi.spawn_failed",
                serde_json::json!({ "error": error.to_string(), "path": binary.to_string_lossy() }),
            );
            None
        }
    }
}

/// 结束由本工具拉起的 newapi 进程
fn kill_newapi(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(crate::windows_create_no_window())
            .output();
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
    }
    if let Ok(mut guard) = LOCAL_CHILD.lock() {
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "newapi.killed",
        serde_json::json!({ "pid": pid }),
    );
}

/// 等待 newapi 在指定端口开始监听，返回是否成功。
fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if port_listening(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// 确保 newapi 处于运行状态，并登记 owner 租约。
/// 当 newapi 已在运行时（端口监听）不会重复拉起；启动失败时不会登记 owner，
/// 避免状态文件被无效 owner 污染导致后续无法重试。
pub fn ensure_newapi_running(owner: &str) {
    let mut state = load_state();

    // 如果端口已经在监听，仅补充 owner 登记即可，不重复拉起。
    if is_newapi_running() {
        if !state.owners.iter().any(|o| o == owner) {
            state.owners.push(owner.to_string());
            save_state(&state);
        }
        return;
    }

    // 端口未监听，尝试唤起 newapi。
    if let Some(pid) = spawn_newapi() {
        state.pid = Some(pid);
    } else {
        // 启动失败时不登记 owner，否则下次调用会误以为已经启动。
        return;
    }

    // 等待进程真正开始监听，避免返回时端口尚未就绪。
    // newapi 为 Go 二进制，冷启动可能需 1~3s，放宽等待以减少误报「未启动」。
    let _ = wait_for_port(NEWAPI_DEFAULT_PORT, Duration::from_secs(3));

    if !state.owners.iter().any(|o| o == owner) {
        state.owners.push(owner.to_string());
    }
    save_state(&state);
}

/// 释放 owner 租约；当没有入口再需要 newapi 时终止进程。
pub fn release_newapi(owner: &str) {
    let mut state = load_state();
    state.owners.retain(|o| o != owner);
    if state.owners.is_empty() {
        if let Some(pid) = state.pid.take() {
            kill_newapi(pid);
        }
        // 清空状态文件
        let _ = std::fs::remove_file(state_path());
    } else {
        save_state(&state);
    }
}
