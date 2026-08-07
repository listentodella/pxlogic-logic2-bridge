#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant},
};
#[cfg(target_os = "linux")]
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{
    menu::MenuBuilder,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, WindowEvent,
};
use tauri_plugin_dialog::DialogExt;

const MAX_LOG_LINES: usize = 500;
const NODE_PROBE_MARKER: &str = "PXLOGIC_NODE_OK:";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientSettings {
    logic_app_path: String,
    port_mode: String,
    preferred_port: u16,
    screen_quadrant: u8,
    #[serde(default = "default_maximize_logic_window")]
    maximize_logic_window: bool,
}

fn default_maximize_logic_window() -> bool {
    true
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            logic_app_path: String::new(),
            port_mode: "auto".to_string(),
            preferred_port: 12472,
            screen_quadrant: 3,
            maximize_logic_window: true,
        }
    }
}

impl ClientSettings {
    fn normalized(mut self) -> Self {
        self.logic_app_path = self.logic_app_path.trim().to_string();
        if self.port_mode != "fixed" {
            self.port_mode = "auto".to_string();
        }
        if self.screen_quadrant < 1 || self.screen_quadrant > 4 {
            self.screen_quadrant = 3;
        }
        self
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogicInspection {
    path: String,
    version: Option<String>,
    supported: bool,
    runnable: bool,
    error: Option<String>,
    node_version: Option<String>,
    electron_version: Option<String>,
    profile_id: Option<String>,
    graph_path: Option<String>,
    graph_format: Option<String>,
    graph_identity_kind: Option<String>,
    graph_identity: Option<String>,
    graph_sha256: Option<String>,
    hook_status: Option<String>,
}

impl LogicInspection {
    fn failure(path: &Path, version: Option<String>, error: impl Into<String>) -> Self {
        Self {
            path: path.display().to_string(),
            version,
            supported: false,
            runnable: false,
            error: Some(error.into()),
            node_version: None,
            electron_version: None,
            profile_id: None,
            graph_path: None,
            graph_format: None,
            graph_identity_kind: None,
            graph_identity: None,
            graph_sha256: None,
            hook_status: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompatibilityManifest {
    schema_version: u32,
    profiles: Vec<CompatibilityProfile>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompatibilityProfile {
    id: String,
    platform: String,
    architecture: String,
    graph: CompatibilityGraph,
    hook: CompatibilityHook,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompatibilityGraph {
    identity_kind: String,
    identity: String,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
struct CompatibilityHook {
    status: String,
    validation: String,
}

struct GraphInspection {
    path: PathBuf,
    format: String,
    identity_kind: String,
    identity: String,
    sha256: String,
    profile: Option<CompatibilityProfile>,
}

#[derive(Clone, Debug, Deserialize)]
struct NodeVersions {
    node: String,
    electron: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeState {
    phase: String,
    actual_port: Option<u16>,
    message: String,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
            phase: "stopped".to_string(),
            actual_port: None,
            message: "待机".to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InitialState {
    settings: ClientSettings,
    applications: Vec<LogicInspection>,
    bridge_state: BridgeState,
    logs: Vec<String>,
}

struct ManagedChild {
    token: u64,
    pid: u32,
    child: Child,
}

#[derive(Default)]
struct RuntimeState {
    child: Option<ManagedChild>,
    starting: bool,
}

struct AppState {
    runtime: Mutex<RuntimeState>,
    bridge_state: Mutex<BridgeState>,
    logs: Mutex<VecDeque<String>>,
    next_token: AtomicU64,
    quitting: AtomicBool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            runtime: Mutex::new(RuntimeState::default()),
            bridge_state: Mutex::new(BridgeState::default()),
            logs: Mutex::new(VecDeque::new()),
            next_token: AtomicU64::new(1),
            quitting: AtomicBool::new(false),
        }
    }
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|directory| directory.join("settings.json"))
        .map_err(|error| format!("无法确定配置目录: {error}"))
}

fn legacy_settings_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library/Application Support/pxlogic-logic2-bridge-client/settings.json")
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

fn load_settings(app: &AppHandle) -> ClientSettings {
    let primary = settings_path(app).ok();
    let source = primary
        .as_ref()
        .filter(|path| path.is_file())
        .cloned()
        .or_else(|| legacy_settings_path().filter(|path| path.is_file()));
    source
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str::<ClientSettings>(&contents).ok())
        .unwrap_or_default()
        .normalized()
}

fn store_settings(app: &AppHandle, settings: ClientSettings) -> Result<ClientSettings, String> {
    let settings = settings.normalized();
    let path = settings_path(app)?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("配置路径无父目录: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("无法创建配置目录: {error}"))?;
    let temporary = path.with_extension("json.tmp");
    let contents = serde_json::to_string_pretty(&settings)
        .map_err(|error| format!("无法序列化配置: {error}"))?;
    fs::write(&temporary, format!("{contents}\n"))
        .map_err(|error| format!("无法写入配置: {error}"))?;
    fs::rename(&temporary, &path).map_err(|error| format!("无法保存配置: {error}"))?;
    Ok(settings)
}

#[cfg(target_os = "macos")]
fn logic_executable(app_path: &Path) -> PathBuf {
    app_path.join("Contents/MacOS/Logic")
}

#[cfg(target_os = "linux")]
fn is_appimage(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("appimage"))
}

#[cfg(target_os = "linux")]
fn appimage_cache_root() -> Result<PathBuf, String> {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache"))
        })
        .ok_or_else(|| "无法确定 Linux 缓存目录".to_string())?;
    Ok(root.join("pxlogic/logic2-bridge/appimages"))
}

#[cfg(target_os = "linux")]
fn resolve_logic_installation(app_path: &Path) -> Result<PathBuf, String> {
    if !is_appimage(app_path) {
        return Ok(app_path.to_path_buf());
    }
    let metadata = fs::metadata(app_path)
        .map_err(|error| format!("无法读取 AppImage {}: {error}", app_path.display()))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let key = format!(
        "{:x}",
        Sha256::digest(format!("{}:{}:{}", app_path.display(), metadata.len(), modified))
    );
    let cache_root = appimage_cache_root()?;
    let cached = cache_root.join(format!("{key}-{}-{modified}", metadata.len()));
    let app_dir = cached.join("squashfs-root");
    if app_dir.is_dir() {
        return Ok(app_dir);
    }
    fs::create_dir_all(&cache_root)
        .map_err(|error| format!("无法创建 AppImage 缓存目录: {error}"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temporary = cache_root.join(format!(".extract-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&temporary)
        .map_err(|error| format!("无法创建 AppImage 临时目录: {error}"))?;
    let result = Command::new(app_path)
        .arg("--appimage-extract")
        .current_dir(&temporary)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("无法执行 AppImage 提取: {error}"))?;
    if !result.status.success() {
        let _ = fs::remove_dir_all(&temporary);
        return Err(format!(
            "AppImage 提取失败（{}）: {}",
            result.status,
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    let extracted = temporary.join("squashfs-root");
    if !extracted.is_dir() {
        let _ = fs::remove_dir_all(&temporary);
        return Err("AppImage 提取结果缺少 squashfs-root".to_string());
    }
    if cached.exists() {
        fs::remove_dir_all(&temporary)
            .map_err(|error| format!("无法清理重复 AppImage 提取结果: {error}"))?;
        return Ok(app_dir);
    }
    fs::rename(&temporary, &cached)
        .map_err(|error| format!("无法保存 AppImage 缓存: {error}"))?;
    Ok(app_dir)
}

#[cfg(not(target_os = "linux"))]
fn resolve_logic_installation(app_path: &Path) -> Result<PathBuf, String> {
    Ok(app_path.to_path_buf())
}

#[cfg(not(target_os = "macos"))]
fn logic_executable(app_path: &Path) -> PathBuf {
    if app_path.is_file() {
        return app_path.to_path_buf();
    }
    #[cfg(target_os = "windows")]
    let candidates = [app_path.join("Logic.exe"), app_path.join("Logic")];
    #[cfg(target_os = "linux")]
    let candidates = [
        app_path.join("usr/lib/logic/Logic"),
        app_path.join("usr/lib/logic/Logic.bin"),
        app_path.join("Logic"),
    ];
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| app_path.to_path_buf())
}

#[cfg(target_os = "macos")]
fn read_logic_version(app_path: &Path) -> Result<String, String> {
    let plist_path = app_path.join("Contents/Info.plist");
    let plist = plist::Value::from_file(&plist_path)
        .map_err(|error| format!("无法读取 {}: {error}", plist_path.display()))?;
    plist
        .as_dictionary()
        .and_then(|dictionary| dictionary.get("CFBundleShortVersionString"))
        .and_then(plist::Value::as_string)
        .map(str::to_string)
        .ok_or_else(|| format!("{} 缺少 Logic 版本信息", plist_path.display()))
}

#[cfg(not(target_os = "macos"))]
fn read_logic_version(app_path: &Path) -> Result<String, String> {
    for asar_path in find_asar_paths(app_path) {
        if let Ok(Some(package_json)) = read_asar_entry(&asar_path, "package.json") {
            if let Ok(package) = serde_json::from_slice::<serde_json::Value>(&package_json) {
                if let Some(version) = package.get("version").and_then(|value| value.as_str()) {
                    if !version.trim().is_empty() {
                        return Ok(version.trim().to_string());
                    }
                }
            }
        }
    }
    Err(format!(
        "无法从 {} 的 app.asar/package.json 读取 Logic 版本",
        app_path.display()
    ))
}

fn compatibility_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        platform => platform,
    }
}

fn compatibility_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        architecture => architecture,
    }
}

fn profile_runnable(status: &str, platform: &str, architecture: &str) -> bool {
    status == "verified"
        || (status == "pending-live-validation"
            && platform == "win32"
            && architecture == "x64")
}

fn installation_roots(installation_path: &Path) -> Vec<PathBuf> {
    let root = if installation_path.is_file() {
        installation_path
            .parent()
            .unwrap_or(installation_path)
            .to_path_buf()
    } else {
        installation_path.to_path_buf()
    };
    let mut roots = Vec::new();
    let mut current = root;
    for _ in 0..4 {
        roots.push(current.clone());
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    roots
}

#[cfg(not(target_os = "macos"))]
fn find_asar_paths(installation_path: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for root in installation_roots(installation_path).into_iter().take(2) {
        paths.extend([
            root.join("Contents/Resources/app.asar"),
            root.join("resources/app.asar"),
            root.join("usr/lib/logic/resources/app.asar"),
            root.join("app.asar"),
        ]);
    }
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(not(target_os = "macos"))]
fn read_asar_entry(asar_path: &Path, entry_path: &str) -> Result<Option<Vec<u8>>, String> {
    let data = fs::read(asar_path)
        .map_err(|error| format!("无法读取 ASAR {}: {error}", asar_path.display()))?;
    if data.len() < 16 {
        return Err(format!("ASAR 头部过短: {}", asar_path.display()));
    }
    let header_size = usize::try_from(
        read_u32_le(&data, 4).ok_or_else(|| format!("ASAR 头部无效: {}", asar_path.display()))?,
    )
    .map_err(|_| format!("ASAR 头部过大: {}", asar_path.display()))?;
    let json_size = usize::try_from(
        read_u32_le(&data, 12)
            .ok_or_else(|| format!("ASAR JSON 头部无效: {}", asar_path.display()))?,
    )
    .map_err(|_| format!("ASAR JSON 头部过大: {}", asar_path.display()))?;
    let json_start = 16usize;
    let json_end = json_start
        .checked_add(json_size)
        .ok_or_else(|| format!("ASAR JSON 边界溢出: {}", asar_path.display()))?;
    if json_end > data.len() || 8usize.saturating_add(header_size) > data.len() {
        return Err(format!("ASAR JSON 边界无效: {}", asar_path.display()));
    }
    let header: serde_json::Value = serde_json::from_slice(&data[json_start..json_end])
        .map_err(|error| format!("ASAR JSON 无效 {}: {error}", asar_path.display()))?;
    let mut node = &header;
    for segment in entry_path.split('/').filter(|segment| !segment.is_empty()) {
        node = match node.get("files").and_then(|files| files.get(segment)) {
            Some(value) => value,
            None => return Ok(None),
        };
    }
    let Some(size) = node.get("size").and_then(serde_json::Value::as_u64) else {
        return Ok(None);
    };
    let Some(offset) = node
        .get("offset")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return Ok(None);
    };
    let size =
        usize::try_from(size).map_err(|_| format!("ASAR 文件过大: {}", asar_path.display()))?;
    let start = 8usize
        .checked_add(header_size)
        .and_then(|value| value.checked_add(offset))
        .ok_or_else(|| format!("ASAR 文件偏移溢出: {}", asar_path.display()))?;
    let end = start
        .checked_add(size)
        .ok_or_else(|| format!("ASAR 文件边界溢出: {}", asar_path.display()))?;
    if end > data.len() {
        return Err(format!("ASAR 文件边界无效: {}", asar_path.display()));
    }
    Ok(Some(data[start..end].to_vec()))
}

fn graph_server_names() -> [&'static str; 4] {
    [
        "libgraph_server_shared.dylib",
        "libgraph_server_shared.so",
        "libgraph_server_shared.dll",
        "graph_server_shared.dll",
    ]
}

fn find_graph_binary(installation_path: &Path) -> Result<PathBuf, String> {
    let relative_paths = [
        "Contents/Resources/macos-arm64/libgraph_server_shared.dylib",
        "Contents/Resources/macos-x64/libgraph_server_shared.dylib",
        "resources/linux-x64/libgraph_server_shared.so",
        "usr/lib/logic/resources/linux-x64/libgraph_server_shared.so",
        "resources/win32-x64/libgraph_server_shared.dll",
        "resources/windows-x64/libgraph_server_shared.dll",
        "resources/win-x64/libgraph_server_shared.dll",
        "resources/win32-x64/graph_server_shared.dll",
        "resources/windows-x64/graph_server_shared.dll",
        "resources/win-x64/graph_server_shared.dll",
    ];
    let roots = installation_roots(installation_path);
    for root in &roots {
        if graph_server_names().contains(
            &root
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default(),
        ) {
            return Ok(root.clone());
        }
        for relative in relative_paths {
            let candidate = root.join(relative);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    let root = roots
        .first()
        .ok_or_else(|| format!("无法确定安装目录: {}", installation_path.display()))?;
    let mut queue = vec![(root.clone(), 0usize)];
    let mut visited = HashSet::new();
    while let Some((directory, depth)) = queue.pop() {
        let canonical = fs::canonicalize(&directory).unwrap_or(directory.clone());
        if !visited.insert(canonical) {
            continue;
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let candidate = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_file()
                && graph_server_names().contains(&entry.file_name().to_str().unwrap_or_default())
            {
                return Ok(candidate);
            }
            if file_type.is_dir() && depth < 8 {
                let name = entry.file_name();
                if !matches!(
                    name.to_str(),
                    Some(
                        "Analyzers"
                            | "app.asar.unpacked"
                            | "node_modules"
                            | "pythonlibs"
                            | "locales"
                    )
                ) {
                    queue.push((candidate, depth + 1));
                }
            }
        }
    }
    Err(format!(
        "未找到 GraphServer 二进制: {}",
        installation_path.display()
    ))
}

fn compatibility_manifest() -> Result<CompatibilityManifest, String> {
    let manifest: CompatibilityManifest =
        serde_json::from_str(include_str!("../../../compatibility/profiles.json"))
            .map_err(|error| format!("兼容 profile 清单无效: {error}"))?;
    if manifest.schema_version != 1 {
        return Err(format!(
            "不支持的兼容 profile 清单版本: {}",
            manifest.schema_version
        ));
    }
    Ok(manifest)
}

fn read_u16_le(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32_le(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64_le(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn format_uuid(bytes: &[u8]) -> Option<String> {
    if bytes.len() != 16 {
        return None;
    }
    Some(format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    ))
}

fn parse_macho_uuid(data: &[u8]) -> Option<String> {
    if read_u32_le(data, 0)? != 0xfeedfacf {
        return None;
    }
    let command_count = read_u32_le(data, 16)? as usize;
    let command_bytes = read_u32_le(data, 20)? as usize;
    let mut offset = 32usize;
    let end = offset.checked_add(command_bytes)?.min(data.len());
    for _ in 0..command_count {
        let command = read_u32_le(data, offset)?;
        let size = read_u32_le(data, offset + 4)? as usize;
        if size < 8 || offset.checked_add(size)? > end {
            return None;
        }
        if command == 0x1b && size >= 24 {
            return format_uuid(data.get(offset + 8..offset + 24)?);
        }
        offset += size;
    }
    None
}

fn align4(value: usize) -> Option<usize> {
    value.checked_add(3).map(|value| value & !3)
}

fn parse_elf_build_id(data: &[u8]) -> Option<String> {
    if data.get(0..6)? != [0x7f, b'E', b'L', b'F', 2, 1] {
        return None;
    }
    let program_offset = usize::try_from(read_u64_le(data, 32)?).ok()?;
    let entry_size = read_u16_le(data, 54)? as usize;
    let entry_count = read_u16_le(data, 56)? as usize;
    for index in 0..entry_count {
        let header = program_offset.checked_add(index.checked_mul(entry_size)?)?;
        if read_u32_le(data, header)? != 4 {
            continue;
        }
        let mut note = usize::try_from(read_u64_le(data, header + 8)?).ok()?;
        let note_size = usize::try_from(read_u64_le(data, header + 32)?).ok()?;
        let end = note.checked_add(note_size)?.min(data.len());
        while note.checked_add(12)? <= end {
            let name_size = read_u32_le(data, note)? as usize;
            let description_size = read_u32_le(data, note + 4)? as usize;
            let note_type = read_u32_le(data, note + 8)?;
            let name_offset = note + 12;
            let description_offset = name_offset.checked_add(align4(name_size)?)?;
            let next = description_offset.checked_add(align4(description_size)?)?;
            if next > end {
                break;
            }
            let name = data.get(name_offset..name_offset + name_size)?;
            if note_type == 3 && name.starts_with(b"GNU") {
                return Some(
                    data.get(description_offset..description_offset + description_size)?
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                );
            }
            note = next;
        }
    }
    None
}

fn pe_rva_to_offset(
    data: &[u8],
    section_offset: usize,
    section_count: usize,
    rva: u32,
) -> Option<usize> {
    for index in 0..section_count {
        let offset = section_offset.checked_add(index.checked_mul(40)?)?;
        let virtual_size = read_u32_le(data, offset + 8)?;
        let virtual_address = read_u32_le(data, offset + 12)?;
        let raw_size = read_u32_le(data, offset + 16)?;
        let raw_offset = read_u32_le(data, offset + 20)?;
        let span = virtual_size.max(raw_size);
        if rva >= virtual_address && rva < virtual_address.saturating_add(span) {
            return usize::try_from(raw_offset.checked_add(rva - virtual_address)?).ok();
        }
    }
    None
}

fn format_pe_guid(data: &[u8]) -> Option<String> {
    if data.len() < 16 {
        return None;
    }
    Some(
        format!(
            "{:08x}-{:04x}-{:04x}-{}-{}",
            u32::from_le_bytes(data[0..4].try_into().ok()?),
            u16::from_le_bytes(data[4..6].try_into().ok()?),
            u16::from_le_bytes(data[6..8].try_into().ok()?),
            data[8..10]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            data[10..16]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        )
        .to_uppercase(),
    )
}

fn parse_pe_identity(data: &[u8]) -> Option<(String, String)> {
    if data.get(0..2)? != b"MZ" {
        return None;
    }
    let pe_offset = usize::try_from(read_u32_le(data, 0x3c)?).ok()?;
    if data.get(pe_offset..pe_offset + 4)? != b"PE\0\0" {
        return None;
    }
    let machine = read_u16_le(data, pe_offset + 4)?;
    let section_count = read_u16_le(data, pe_offset + 6)? as usize;
    let timestamp = read_u32_le(data, pe_offset + 8)?;
    let optional_size = read_u16_le(data, pe_offset + 20)? as usize;
    let optional_offset = pe_offset.checked_add(24)?;
    let section_offset = optional_offset.checked_add(optional_size)?;
    let magic = read_u16_le(data, optional_offset)?;
    let directories_offset = optional_offset.checked_add(if magic == 0x20b { 112 } else { 96 })?;
    let debug_directory = directories_offset.checked_add(6 * 8)?;
    if let (Some(debug_rva), Some(debug_size)) = (
        read_u32_le(data, debug_directory),
        read_u32_le(data, debug_directory + 4),
    ) {
        if let Some(debug_offset) = pe_rva_to_offset(data, section_offset, section_count, debug_rva)
        {
            let debug_end = debug_offset
                .saturating_add(debug_size as usize)
                .min(data.len());
            let mut offset = debug_offset;
            while offset.checked_add(28)? <= debug_end {
                if read_u32_le(data, offset + 12)? == 2 {
                    let data_size = read_u32_le(data, offset + 16)? as usize;
                    let data_offset = usize::try_from(read_u32_le(data, offset + 24)?).ok()?;
                    if data_size >= 24
                        && data_offset.checked_add(data_size)? <= data.len()
                        && data.get(data_offset..data_offset + 4)? == b"RSDS"
                    {
                        let guid = format_pe_guid(data.get(data_offset + 4..data_offset + 20)?)?;
                        let age = read_u32_le(data, data_offset + 20)?;
                        return Some(("pe-codeview-guid-age".to_string(), format!("{guid}-{age}")));
                    }
                }
                offset += 28;
            }
        }
    }
    let _architecture = machine;
    Some(("pe-timestamp".to_string(), format!("{timestamp:08x}")))
}

fn graph_fingerprint(data: &[u8], path: PathBuf) -> GraphInspection {
    let (format, identity_kind, identity) = if parse_macho_uuid(data).is_some() {
        (
            "mach-o".to_string(),
            "macho-lc-uuid".to_string(),
            parse_macho_uuid(data).unwrap_or_else(|| "unknown".to_string()),
        )
    } else if parse_elf_build_id(data).is_some() {
        (
            "elf".to_string(),
            "elf-gnu-build-id".to_string(),
            parse_elf_build_id(data).unwrap_or_else(|| "unknown".to_string()),
        )
    } else if let Some((identity_kind, identity)) = parse_pe_identity(data) {
        ("pe".to_string(), identity_kind, identity)
    } else {
        (
            "unknown".to_string(),
            "unknown".to_string(),
            "unknown".to_string(),
        )
    };
    GraphInspection {
        path,
        format,
        identity_kind,
        identity,
        sha256: format!("{:x}", Sha256::digest(data)),
        profile: None,
    }
}

fn inspect_graph_compatibility(
    app_path: &Path,
    _logic_version: &str,
) -> Result<GraphInspection, String> {
    let graph_path = find_graph_binary(app_path)?;
    let data = fs::read(&graph_path)
        .map_err(|error| format!("无法读取 GraphServer {}: {error}", graph_path.display()))?;
    let mut inspection = graph_fingerprint(&data, graph_path);
    let manifest = compatibility_manifest()?;
    let candidates: Vec<_> = manifest
        .profiles
        .into_iter()
        .filter(|profile| {
            profile.platform == compatibility_platform()
                && profile.architecture == compatibility_architecture()
        })
        .collect();
    inspection.profile = candidates.into_iter().find(|profile| {
        profile.graph.identity_kind == inspection.identity_kind
            && profile
                .graph
                .identity
                .eq_ignore_ascii_case(&inspection.identity)
            && profile
                .graph
                .sha256
                .eq_ignore_ascii_case(&inspection.sha256)
    });
    Ok(inspection)
}

fn collect_child_output(child: &mut Child) -> (String, String) {
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut stream) = child.stdout.take() {
        let _ = stream.read_to_string(&mut stdout);
    }
    if let Some(mut stream) = child.stderr.take() {
        let _ = stream.read_to_string(&mut stderr);
    }
    (stdout, stderr)
}

fn probe_logic_node(executable: &Path, timeout: Duration) -> Result<NodeVersions, String> {
    let script = concat!(
        "const v={node:process.versions.node,electron:process.versions.electron};",
        "require('node:net');require('node:child_process');",
        "console.log('PXLOGIC_NODE_OK:'+JSON.stringify(v));"
    );
    let mut child = Command::new(executable)
        .args(["-e", script])
        .env("ELECTRON_RUN_AS_NODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动 Logic 内置 Node: {error}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let (stdout, stderr) = collect_child_output(&mut child);
                if !status.success() {
                    return Err(format!(
                        "Logic 内置 Node 探测失败（{status}）: {}",
                        stderr.trim()
                    ));
                }
                let payload = stdout
                    .lines()
                    .find_map(|line| line.strip_prefix(NODE_PROBE_MARKER))
                    .ok_or_else(|| "Logic 未提供可用的 Electron RunAsNode 环境".to_string())?;
                let versions: NodeVersions = serde_json::from_str(payload)
                    .map_err(|error| format!("Logic Node 版本响应无效: {error}"))?;
                let node_major = versions
                    .node
                    .split('.')
                    .next()
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(0);
                if node_major < 18 || versions.electron.is_empty() {
                    return Err(format!(
                        "Logic 内置运行时过旧: Node {}, Electron {}",
                        versions.node, versions.electron
                    ));
                }
                return Ok(versions);
            }
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Logic 内置 Node 探测超时，RunAsNode 可能已被禁用".to_string());
            }
            Err(error) => {
                let _ = child.kill();
                return Err(format!("Logic 内置 Node 探测失败: {error}"));
            }
        }
    }
}

fn inspect_logic_path(app_path: &Path) -> LogicInspection {
    let runtime_path = match resolve_logic_installation(app_path) {
        Ok(path) => path,
        Err(error) => return LogicInspection::failure(app_path, None, error),
    };
    let version = match read_logic_version(&runtime_path) {
        Ok(version) => version,
        Err(error) => return LogicInspection::failure(app_path, None, error),
    };
    let graph = match inspect_graph_compatibility(&runtime_path, &version) {
        Ok(graph) => graph,
        Err(error) => return LogicInspection::failure(app_path, Some(version), error),
    };
    let Some(profile) = graph.profile.as_ref() else {
        return LogicInspection {
            path: app_path.display().to_string(),
            version: Some(version),
            supported: false,
            runnable: false,
            error: Some(format!(
                "Logic GraphServer 尚无匹配 profile: {} {} (sha256 {})",
                graph.identity_kind, graph.identity, graph.sha256
            )),
            node_version: None,
            electron_version: None,
            profile_id: None,
            graph_path: Some(graph.path.display().to_string()),
            graph_format: Some(graph.format),
            graph_identity_kind: Some(graph.identity_kind),
            graph_identity: Some(graph.identity),
            graph_sha256: Some(graph.sha256),
            hook_status: None,
        };
    };
    let supported = profile.hook.status == "verified";
    let runnable = profile_runnable(
        &profile.hook.status,
        compatibility_platform(),
        compatibility_architecture(),
    );
    if !runnable {
        return LogicInspection {
            path: app_path.display().to_string(),
            version: Some(version),
            supported: false,
            runnable: false,
            error: Some(format!(
                "GraphServer profile {} 尚未完成真机验证: {}",
                profile.id, profile.hook.validation
            )),
            node_version: None,
            electron_version: None,
            profile_id: Some(profile.id.clone()),
            graph_path: Some(graph.path.display().to_string()),
            graph_format: Some(graph.format),
            graph_identity_kind: Some(graph.identity_kind),
            graph_identity: Some(graph.identity),
            graph_sha256: Some(graph.sha256),
            hook_status: Some(profile.hook.status.clone()),
        };
    }
    let executable = logic_executable(&runtime_path);
    if !executable.is_file() {
        return LogicInspection {
            path: app_path.display().to_string(),
            version: Some(version),
            supported: false,
            runnable: false,
            error: Some(format!("Logic 可执行文件不存在: {}", executable.display())),
            node_version: None,
            electron_version: None,
            profile_id: graph.profile.as_ref().map(|profile| profile.id.clone()),
            graph_path: Some(graph.path.display().to_string()),
            graph_format: Some(graph.format),
            graph_identity_kind: Some(graph.identity_kind),
            graph_identity: Some(graph.identity),
            graph_sha256: Some(graph.sha256),
            hook_status: graph
                .profile
                .as_ref()
                .map(|profile| profile.hook.status.clone()),
        };
    }
    let validation_notice = (!supported).then(|| {
        format!(
            "实验性 Windows profile，等待本机 PXLogic 捕获验证: {}",
            profile.hook.validation
        )
    });
    match probe_logic_node(&executable, Duration::from_secs(4)) {
        Ok(versions) => LogicInspection {
            path: app_path.display().to_string(),
            version: Some(version),
            supported,
            runnable,
            error: validation_notice,
            node_version: Some(versions.node),
            electron_version: Some(versions.electron),
            profile_id: Some(profile.id.clone()),
            graph_path: Some(graph.path.display().to_string()),
            graph_format: Some(graph.format),
            graph_identity_kind: Some(graph.identity_kind),
            graph_identity: Some(graph.identity),
            graph_sha256: Some(graph.sha256),
            hook_status: Some(profile.hook.status.clone()),
        },
        Err(error) => LogicInspection {
            path: app_path.display().to_string(),
            version: Some(version),
            supported: false,
            runnable: false,
            error: Some(error),
            node_version: None,
            electron_version: None,
            profile_id: Some(profile.id.clone()),
            graph_path: Some(graph.path.display().to_string()),
            graph_format: Some(graph.format),
            graph_identity_kind: Some(graph.identity_kind),
            graph_identity: Some(graph.identity),
            graph_sha256: Some(graph.sha256),
            hook_status: Some(profile.hook.status.clone()),
        },
    }
}

#[cfg(target_os = "macos")]
fn installed_logic_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let names = ["Saleae Logic.app", "Logic 2.app", "Logic.app"];
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    for root in roots {
        for name in names {
            candidates.push(root.join(name));
        }
    }
    if let Ok(output) = Command::new("mdfind")
        .arg("kMDItemCFBundleIdentifier == \"com.saleae.saleae\"")
        .output()
    {
        if output.status.success() {
            candidates.extend(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(PathBuf::from),
            );
        }
    }
    candidates
}

#[cfg(target_os = "windows")]
fn installed_logic_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(program_files));
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local_app_data));
    }
    roots
        .into_iter()
        .flat_map(|root| {
            ["Saleae Logic", "Logic 2", "Logic"]
                .into_iter()
                .map(move |name| root.join(name))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn installed_logic_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/opt/Saleae Logic"),
        PathBuf::from("/opt/Logic"),
        PathBuf::from("/usr/lib/logic"),
        PathBuf::from("/usr/local/lib/logic"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.extend([
            home.join(".local/share/Logic"),
            home.join(".local/share/Saleae Logic"),
        ]);
    }
    candidates
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn installed_logic_candidates() -> Vec<PathBuf> {
    Vec::new()
}

fn scan_logic_paths(saved_path: Option<&str>) -> Vec<LogicInspection> {
    let mut candidates = installed_logic_candidates();
    if let Some(path) = saved_path.filter(|path| !path.trim().is_empty()) {
        candidates.insert(0, PathBuf::from(path.trim()));
    }
    let mut seen = HashSet::new();
    let mut applications = Vec::new();
    for candidate in candidates {
        let key = candidate.to_string_lossy().into_owned();
        if !seen.insert(key) || !candidate.exists() {
            continue;
        }
        applications.push(inspect_logic_path(&candidate));
    }
    applications.sort_by_key(|inspection| !inspection.runnable);
    applications
}

fn bridge_root(app: &AppHandle) -> Result<PathBuf, String> {
    if let Ok(resource_dir) = app.path().resource_dir() {
        let packaged = resource_dir.join("tools/logic2-bridge");
        if packaged.join("index.cjs").is_file() {
            return Ok(packaged);
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let development = manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| "无法确定 bridge 开发目录".to_string())?;
    if development.join("index.cjs").is_file() {
        Ok(development)
    } else {
        Err(format!("Bridge 入口不存在: {}", development.display()))
    }
}

struct BridgePayload {
    bridge_root: PathBuf,
    helper: PathBuf,
    bitstreams: PathBuf,
    firmware: PathBuf,
}

fn validate_bridge_payload(app: &AppHandle) -> Result<BridgePayload, String> {
    let root = bridge_root(app)?;
    let payload_root = root
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("Bridge 资源目录结构无效: {}", root.display()))?;
    let native_host = if cfg!(target_os = "windows") {
        root.join("build/graph-host.exe")
    } else {
        root.join("build/graph-host")
    };
    let helper = payload_root
        .join("target/release")
        .join(if cfg!(target_os = "windows") {
            "usb_smoke.exe"
        } else {
            "usb_smoke"
        });
    let bitstreams = payload_root.join("resources/bitstreams");
    let firmware = payload_root.join("resources/firmware/SCI_LOGIC.bin");
    let required = [
        root.join("index.cjs"),
        root.join("lib/capture-controller.cjs"),
        root.join("lib/compatibility.cjs"),
        root.join("lib/logic-format.cjs"),
        root.join("lib/websocket-proxy.cjs"),
        root.join("compatibility/profiles.json"),
        native_host,
        helper.clone(),
        bitstreams.join("hspi_ddr.bin"),
        bitstreams.join("hspi_ddr_RST.bin"),
        firmware.clone(),
    ];
    for path in required {
        if !path.is_file() {
            return Err(format!(
                "Bridge 便携包不完整，缺少: {}。请解压并运行完整的平台便携包，不能直接运行 target\\...\\release 下的构建中间文件。",
                path.display()
            ));
        }
    }
    Ok(BridgePayload {
        bridge_root: root,
        helper,
        bitstreams,
        firmware,
    })
}

fn update_bridge_state(app: &AppHandle, next: BridgeState) {
    if let Ok(mut current) = app.state::<AppState>().bridge_state.lock() {
        *current = next.clone();
    }
    let _ = app.emit("bridge-state", next);
}

fn append_log(app: &AppHandle, source: &str, line: &str) {
    if line.is_empty() {
        return;
    }
    let entry = format!("[{source}] {line}");
    if let Ok(mut logs) = app.state::<AppState>().logs.lock() {
        logs.push_back(entry.clone());
        while logs.len() > MAX_LOG_LINES {
            logs.pop_front();
        }
    }
    let _ = app.emit("bridge-log", &entry);
    if let Some(port) = parse_ready_port(line) {
        update_bridge_state(
            app,
            BridgeState {
                phase: "running".to_string(),
                actual_port: Some(port),
                message: "已连接".to_string(),
            },
        );
    }
}

fn parse_ready_port(line: &str) -> Option<u16> {
    let prefix = "Graph WebSocket ready: ws://127.0.0.1:";
    let value = line.split_once(prefix)?.1.split('/').next()?;
    value.parse().ok()
}

fn read_process_lines(app: AppHandle, source: &'static str, stream: impl Read + Send + 'static) {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            match line {
                Ok(line) => append_log(&app, source, &line),
                Err(error) => {
                    append_log(&app, "error", &format!("读取 {source} 日志失败: {error}"));
                    break;
                }
            }
        }
    });
}

fn monitor_bridge(app: AppHandle, token: u64) {
    thread::spawn(move || loop {
        let result = {
            let state = app.state::<AppState>();
            let mut runtime = match state.runtime.lock() {
                Ok(runtime) => runtime,
                Err(_) => return,
            };
            let Some(managed) = runtime.child.as_mut() else {
                return;
            };
            if managed.token != token {
                return;
            }
            match managed.child.try_wait() {
                Ok(Some(status)) => {
                    runtime.child.take();
                    Some(Ok(status))
                }
                Ok(None) => None,
                Err(error) => {
                    runtime.child.take();
                    Some(Err(error.to_string()))
                }
            }
        };
        match result {
            Some(result) => {
                let quitting = app.state::<AppState>().quitting.load(Ordering::Acquire);
                match result {
                    Ok(status) if status.success() || quitting => {
                        update_bridge_state(&app, BridgeState::default())
                    }
                    Ok(status) => update_bridge_state(
                        &app,
                        BridgeState {
                            phase: "error".to_string(),
                            actual_port: None,
                            message: format!("Bridge 已退出（{}）", describe_status(status)),
                        },
                    ),
                    Err(error) => update_bridge_state(
                        &app,
                        BridgeState {
                            phase: "error".to_string(),
                            actual_port: None,
                            message: format!("Bridge 状态读取失败: {error}"),
                        },
                    ),
                }
                return;
            }
            None => thread::sleep(Duration::from_millis(150)),
        }
    });
}

fn describe_status(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| format!("代码 {code}"))
        .unwrap_or_else(|| status.to_string())
}

#[cfg(unix)]
fn configure_child_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(any(unix, target_os = "windows")))]
fn configure_child_process(_command: &mut Command) {}

#[cfg(target_os = "windows")]
fn configure_child_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    // The Bridge is launched through Logic's Electron binary in Node mode.
    // It has piped stdout/stderr for the in-app log panel, so a separate
    // Windows console is neither useful nor expected.
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

#[cfg(unix)]
fn terminate_process(pid: u32, force: bool) -> Result<(), String> {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    let result = unsafe { libc::kill(-(pid as i32), signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

#[cfg(target_os = "windows")]
fn terminate_process(pid: u32, force: bool) -> Result<(), String> {
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/T"]);
    if force {
        command.arg("/F");
    }
    let output = command
        .output()
        .map_err(|error| format!("无法启动 taskkill: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(not(any(unix, target_os = "windows")))]
fn terminate_process(_pid: u32, _force: bool) -> Result<(), String> {
    Err("当前平台尚未实现 bridge 进程组终止".to_string())
}

fn request_stop(app: &AppHandle) -> Result<BridgeState, String> {
    let child = {
        let state = app.state::<AppState>();
        let runtime = state
            .runtime
            .lock()
            .map_err(|_| "Bridge 进程状态已损坏".to_string())?;
        runtime
            .child
            .as_ref()
            .map(|managed| (managed.token, managed.pid))
    };
    let Some((token, pid)) = child else {
        return Ok(app
            .state::<AppState>()
            .bridge_state
            .lock()
            .map_err(|_| "Bridge 状态已损坏".to_string())?
            .clone());
    };
    let next = BridgeState {
        phase: "stopping".to_string(),
        actual_port: None,
        message: "正在停止".to_string(),
    };
    update_bridge_state(app, next.clone());
    terminate_process(pid, false)
        .map_err(|error| format!("无法停止 Bridge 进程 {pid}: {error}"))?;

    let app_for_timeout = app.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(3));
        let still_running = app_for_timeout
            .state::<AppState>()
            .runtime
            .lock()
            .ok()
            .and_then(|runtime| {
                runtime
                    .child
                    .as_ref()
                    .filter(|managed| managed.token == token)
                    .map(|managed| managed.pid)
            });
        if let Some(pid) = still_running {
            let _ = terminate_process(pid, true);
        }
    });
    Ok(next)
}

fn show_main_window(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn hide_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
}

#[tauri::command]
async fn client_initial_state(app: AppHandle) -> Result<InitialState, String> {
    let worker_app = app.clone();
    let (mut settings, applications) = tauri::async_runtime::spawn_blocking(move || {
        let settings = load_settings(&worker_app);
        let applications = scan_logic_paths(Some(&settings.logic_app_path));
        (settings, applications)
    })
    .await
    .map_err(|error| format!("Logic 扫描任务失败: {error}"))?;
    if settings.logic_app_path.is_empty() {
        if let Some(preferred) = applications
            .iter()
            .find(|application| application.runnable)
            .or_else(|| applications.first())
        {
            settings.logic_app_path = preferred.path.clone();
            settings = store_settings(&app, settings)?;
        }
    }
    let state = app.state::<AppState>();
    let bridge_state = state
        .bridge_state
        .lock()
        .map_err(|_| "Bridge 状态已损坏".to_string())?
        .clone();
    let logs = state
        .logs
        .lock()
        .map_err(|_| "Bridge 日志状态已损坏".to_string())?
        .iter()
        .cloned()
        .collect();
    Ok(InitialState {
        settings,
        applications,
        bridge_state,
        logs,
    })
}

#[tauri::command]
fn client_save_settings(
    app: AppHandle,
    settings: ClientSettings,
) -> Result<ClientSettings, String> {
    store_settings(&app, settings)
}

#[tauri::command]
async fn logic_scan(saved_path: String) -> Result<Vec<LogicInspection>, String> {
    tauri::async_runtime::spawn_blocking(move || scan_logic_paths(Some(&saved_path)))
        .await
        .map_err(|error| format!("Logic 扫描任务失败: {error}"))
}

#[tauri::command]
async fn logic_inspect(app_path: String) -> Result<LogicInspection, String> {
    tauri::async_runtime::spawn_blocking(move || inspect_logic_path(Path::new(app_path.trim())))
        .await
        .map_err(|error| format!("Logic 检查任务失败: {error}"))
}

#[tauri::command]
async fn logic_browse(app: AppHandle) -> Result<Option<LogicInspection>, String> {
    let selected = tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "linux")]
        {
            return app
                .dialog()
                .file()
                .set_title("选择 Saleae Logic 2 AppImage")
                .add_filter("Linux AppImage", &["AppImage", "appimage"])
                .blocking_pick_file();
        }
        #[cfg(not(target_os = "linux"))]
        app.dialog()
            .file()
            .set_title("选择 Saleae Logic 2")
            .blocking_pick_folder()
    })
    .await
    .map_err(|error| format!("路径选择任务失败: {error}"))?;
    let Some(path) = selected else {
        return Ok(None);
    };
    let path = path
        .into_path()
        .map_err(|error| format!("选择的路径无效: {error}"))?;
    Ok(Some(inspect_logic_path(&path)))
}

#[tauri::command]
async fn bridge_start(app: AppHandle, settings: ClientSettings) -> Result<BridgeState, String> {
    {
        let state = app.state::<AppState>();
        let mut runtime = state
            .runtime
            .lock()
            .map_err(|_| "Bridge 进程状态已损坏".to_string())?;
        if runtime.starting || runtime.child.is_some() {
            return Err("Bridge 已在运行".to_string());
        }
        runtime.starting = true;
    }
    update_bridge_state(
        &app,
        BridgeState {
            phase: "starting".to_string(),
            actual_port: None,
            message: "正在启动".to_string(),
        },
    );

    let result = start_bridge_inner(&app, settings);
    if result.is_err() {
        if let Ok(mut runtime) = app.state::<AppState>().runtime.lock() {
            runtime.starting = false;
        }
        let message = result.as_ref().err().cloned().unwrap_or_default();
        update_bridge_state(
            &app,
            BridgeState {
                phase: "error".to_string(),
                actual_port: None,
                message,
            },
        );
    }
    result
}

fn start_bridge_inner(app: &AppHandle, settings: ClientSettings) -> Result<BridgeState, String> {
    let settings = store_settings(app, settings)?;
    let selected_app_path = PathBuf::from(&settings.logic_app_path);
    let app_path = resolve_logic_installation(&selected_app_path)?;
    let inspection = inspect_logic_path(&selected_app_path);
    if !inspection.runnable {
        return Err(inspection
            .error
            .unwrap_or_else(|| "Logic 2 安装无效".to_string()));
    }
    let payload = validate_bridge_payload(app)?;
    let executable = logic_executable(&app_path);
    let runtime_app_path = app_path.to_string_lossy().into_owned();
    let helper = payload.helper.to_string_lossy().into_owned();
    let bitstreams = payload.bitstreams.to_string_lossy().into_owned();
    let firmware = payload.firmware.to_string_lossy().into_owned();
    let port = if settings.port_mode == "fixed" {
        settings.preferred_port.to_string()
    } else {
        "auto".to_string()
    };
    let mut command = Command::new(&executable);
    // Electron's Windows RunAsNode command-line handling can split an
    // absolute `C:\\...` script argument at the drive colon and then ask
    // Node to load `C:`.  The working directory is the validated bridge
    // root, so a relative entry is equivalent and avoids that parser path.
    command
        .arg("index.cjs")
        .args(["--app", &runtime_app_path])
        .args(["--port", &port])
        .args(["--pxlogic-helper", &helper])
        .args(["--bitstreams", &bitstreams])
        .args(["--firmware", &firmware])
        .current_dir(&payload.bridge_root)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if settings.maximize_logic_window {
        command.arg("--maximize-window");
    } else {
        command.args(["--screen-quadrant", &settings.screen_quadrant.to_string()]);
    }
    if inspection.hook_status.as_deref() == Some("pending-live-validation") {
        command.arg("--allow-pending-profile");
    }
    configure_child_process(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法通过 Logic 内置 Node 启动 Bridge: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Bridge stdout 不可用".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Bridge stderr 不可用".to_string())?;
    let pid = child.id();
    let token = app
        .state::<AppState>()
        .next_token
        .fetch_add(1, Ordering::AcqRel);
    {
        let state = app.state::<AppState>();
        let mut runtime = state
            .runtime
            .lock()
            .map_err(|_| "Bridge 进程状态已损坏".to_string())?;
        runtime.starting = false;
        runtime.child = Some(ManagedChild { token, pid, child });
    }
    if let Ok(mut logs) = app.state::<AppState>().logs.lock() {
        logs.clear();
    }
    append_log(
        app,
        "client",
        &format!(
            "使用 Logic {} 内置 Node 启动 Bridge",
            inspection.version.unwrap_or_default()
        ),
    );
    read_process_lines(app.clone(), "bridge", stdout);
    read_process_lines(app.clone(), "runtime", stderr);
    monitor_bridge(app.clone(), token);
    Ok(app
        .state::<AppState>()
        .bridge_state
        .lock()
        .map_err(|_| "Bridge 状态已损坏".to_string())?
        .clone())
}

#[tauri::command]
fn bridge_stop(app: AppHandle) -> Result<BridgeState, String> {
    request_stop(&app)
}

fn bridge_log_directory() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").ok_or_else(|| "无法确定用户目录".to_string())?;
        Ok(PathBuf::from(home).join("Library/Application Support/PXLogic/logic2-bridge"))
    }
    #[cfg(target_os = "windows")]
    {
        let root = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| "无法确定 LOCALAPPDATA".to_string())?;
        Ok(root.join("PXLogic/logic2-bridge"))
    }
    #[cfg(target_os = "linux")]
    {
        let root = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .ok_or_else(|| "无法确定 Linux 状态目录".to_string())?;
        Ok(root.join("pxlogic/logic2-bridge"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Err("当前平台尚未实现日志目录定位".to_string())
    }
}

#[tauri::command]
fn logs_open() -> Result<(), String> {
    let directory = bridge_log_directory()?;
    fs::create_dir_all(&directory).map_err(|error| format!("无法创建日志目录: {error}"))?;
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = Command::new("explorer");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");
    command
        .arg(&directory)
        .spawn()
        .map_err(|error| format!("无法打开日志目录: {error}"))?;
    Ok(())
}

fn setup_tray(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let menu = MenuBuilder::new(app)
        .text("show", "显示 PXLogic Bridge")
        .separator()
        .text("stop", "停止 Bridge")
        .separator()
        .text("quit", "退出")
        .build()?;
    let mut builder = TrayIconBuilder::with_id("main-tray")
        .menu(&menu)
        .tooltip("PXLogic Bridge")
        .show_menu_on_left_click(false)
        .icon_as_template(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main_window(app),
            "stop" => {
                let _ = request_stop(app);
            }
            "quit" => {
                app.state::<AppState>()
                    .quitting
                    .store(true, Ordering::Release);
                let _ = request_stop(app);
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }
    builder.build(app)?;
    Ok(())
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            client_initial_state,
            client_save_settings,
            logic_scan,
            logic_inspect,
            logic_browse,
            bridge_start,
            bridge_stop,
            logs_open,
        ])
        .setup(setup_tray)
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if !window
                    .app_handle()
                    .state::<AppState>()
                    .quitting
                    .load(Ordering::Acquire)
                {
                    api.prevent_close();
                    hide_main_window(window.app_handle());
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build PXLogic Bridge");

    app.run(|app, event| match event {
        tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit => {
            app.state::<AppState>()
                .quitting
                .store(true, Ordering::Release);
            let _ = request_stop(app);
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_client_settings() {
        let settings = ClientSettings {
            logic_app_path: "  /Applications/Saleae Logic.app  ".to_string(),
            port_mode: "unexpected".to_string(),
            preferred_port: 43210,
            screen_quadrant: 9,
            maximize_logic_window: true,
        }
        .normalized();
        assert_eq!(settings.logic_app_path, "/Applications/Saleae Logic.app");
        assert_eq!(settings.port_mode, "auto");
        assert_eq!(settings.preferred_port, 43210);
        assert_eq!(settings.screen_quadrant, 3);
        assert!(settings.maximize_logic_window);
    }

    #[test]
    fn migrates_legacy_settings_to_maximized_window() {
        let settings: ClientSettings = serde_json::from_str(
            r#"{"logicAppPath":"","portMode":"auto","preferredPort":12472,"screenQuadrant":3}"#,
        )
        .unwrap();
        assert!(settings.maximize_logic_window);
    }

    #[test]
    fn parses_graph_websocket_ready_line() {
        assert_eq!(
            parse_ready_port("[logic2-bridge] Graph WebSocket ready: ws://127.0.0.1:54102/saleae"),
            Some(54102)
        );
        assert_eq!(parse_ready_port("other output"), None);
    }

    #[test]
    fn parses_macho_graph_uuid() {
        let mut data = vec![0u8; 64];
        data[0..4].copy_from_slice(&0xfeedfacfu32.to_le_bytes());
        data[16..20].copy_from_slice(&1u32.to_le_bytes());
        data[20..24].copy_from_slice(&24u32.to_le_bytes());
        data[32..36].copy_from_slice(&0x1bu32.to_le_bytes());
        data[36..40].copy_from_slice(&24u32.to_le_bytes());
        data[40..56].copy_from_slice(&[
            0x0d, 0xf1, 0x76, 0x31, 0x8e, 0x04, 0x35, 0x01, 0xa7, 0xb5, 0x49, 0xa6, 0x2e, 0x23,
            0x3f, 0xb0,
        ]);
        assert_eq!(
            parse_macho_uuid(&data).as_deref(),
            Some("0DF17631-8E04-3501-A7B5-49A62E233FB0")
        );
    }

    #[test]
    fn parses_pe_codeview_identity() {
        let mut data = vec![0u8; 0x280];
        data[0..2].copy_from_slice(b"MZ");
        data[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        data[0x40..0x44].copy_from_slice(b"PE\0\0");
        data[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes());
        data[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        data[0x54..0x56].copy_from_slice(&0xf0u16.to_le_bytes());
        data[0x58..0x5a].copy_from_slice(&0x20bu16.to_le_bytes());
        let directory = 0x58 + 112 + 6 * 8;
        data[directory..directory + 4].copy_from_slice(&0x1000u32.to_le_bytes());
        data[directory + 4..directory + 8].copy_from_slice(&28u32.to_le_bytes());
        let section = 0x58 + 0xf0;
        data[section + 12..section + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        data[section + 16..section + 20].copy_from_slice(&0x1000u32.to_le_bytes());
        data[section + 20..section + 24].copy_from_slice(&0x200u32.to_le_bytes());
        let debug = 0x200;
        data[debug + 12..debug + 16].copy_from_slice(&2u32.to_le_bytes());
        data[debug + 16..debug + 20].copy_from_slice(&24u32.to_le_bytes());
        data[debug + 24..debug + 28].copy_from_slice(&0x240u32.to_le_bytes());
        data[0x240..0x244].copy_from_slice(b"RSDS");
        data[0x244..0x254].copy_from_slice(&[
            0x78, 0x56, 0x34, 0x12, 0xbc, 0x9a, 0xef, 0xbe, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
            0x66, 0x77,
        ]);
        data[0x254..0x258].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(
            parse_pe_identity(&data),
            Some((
                "pe-codeview-guid-age".to_string(),
                "12345678-9ABC-BEEF-0011-223344556677-7".to_string()
            ))
        );
    }

    #[test]
    fn loads_versioned_compatibility_profiles() {
        let manifest = compatibility_manifest().unwrap();
        assert_eq!(manifest.schema_version, 1);
        assert!(manifest
            .profiles
            .iter()
            .any(|profile| profile.id == "logic-2.4.46-macos-arm64-0df17631"));
    }

    #[test]
    fn only_allows_pending_profile_for_the_windows_validation_target() {
        assert!(profile_runnable("verified", "darwin", "arm64"));
        assert!(profile_runnable("pending-live-validation", "win32", "x64"));
        assert!(!profile_runnable("pending-live-validation", "linux", "x64"));
        assert!(!profile_runnable("pending-live-validation", "win32", "arm64"));
    }
}
