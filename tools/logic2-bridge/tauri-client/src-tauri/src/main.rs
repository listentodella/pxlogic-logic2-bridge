#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{
    menu::MenuBuilder,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, WindowEvent,
};
use tauri_plugin_dialog::DialogExt;

const MAX_LOG_LINES: usize = 500;
const NODE_PROBE_MARKER: &str = "PXLOGIC_NODE_OK:";
const DEFAULT_PXLOGIC_THRESHOLD_VOLTS: f64 = 1.8;
const MAX_PXLOGIC_THRESHOLD_VOLTS: f64 = 6.668;
const OFFLINE_ANALYSIS_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientSettings {
    logic_app_path: String,
    port_mode: String,
    preferred_port: u16,
    screen_quadrant: u8,
    #[serde(default = "default_maximize_logic_window")]
    maximize_logic_window: bool,
    #[serde(default)]
    pxlogic_device_id: String,
    #[serde(default = "default_pxlogic_threshold_volts")]
    pxlogic_threshold_volts: f64,
    #[serde(default, rename = "pxlogicComparatorThresholdVolts", skip_serializing)]
    temporary_comparator_threshold_volts: Option<f64>,
    #[serde(default)]
    pxlogic_threshold_profiles: BTreeMap<String, ThresholdProfile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThresholdProfile {
    volts: f64,
    #[serde(default)]
    verified: bool,
    #[serde(default)]
    reference: String,
}

fn default_maximize_logic_window() -> bool {
    true
}

fn default_pxlogic_threshold_volts() -> f64 {
    DEFAULT_PXLOGIC_THRESHOLD_VOLTS
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            logic_app_path: String::new(),
            port_mode: "auto".to_string(),
            preferred_port: 12472,
            screen_quadrant: 3,
            maximize_logic_window: true,
            pxlogic_device_id: String::new(),
            pxlogic_threshold_volts: default_pxlogic_threshold_volts(),
            temporary_comparator_threshold_volts: None,
            pxlogic_threshold_profiles: BTreeMap::new(),
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
        self.pxlogic_device_id = self.pxlogic_device_id.trim().to_string();
        if let Some(temporary_threshold) = self.temporary_comparator_threshold_volts.take() {
            self.pxlogic_threshold_volts = temporary_threshold;
        }
        if !self.pxlogic_threshold_volts.is_finite()
            || !(0.0..=MAX_PXLOGIC_THRESHOLD_VOLTS).contains(&self.pxlogic_threshold_volts)
        {
            self.pxlogic_threshold_volts = default_pxlogic_threshold_volts();
        }
        self.pxlogic_threshold_profiles
            .retain(|device_id, profile| {
                profile.reference = profile.reference.trim().to_string();
                !device_id.trim().is_empty()
                    && profile.volts.is_finite()
                    && (0.0..=MAX_PXLOGIC_THRESHOLD_VOLTS).contains(&profile.volts)
            });
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
    #[serde(default)]
    analyzer_version: Option<u32>,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OfflineAnalysisResult {
    status: String,
    #[serde(default)]
    cached: bool,
    reason: String,
    profile: Option<CompatibilityProfile>,
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
    error_code: Option<String>,
    recovery_action: Option<String>,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
            phase: "stopped".to_string(),
            actual_port: None,
            message: "待机".to_string(),
            error_code: None,
            recovery_action: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeRuntimeEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    recovery_action: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    failed: Option<bool>,
    #[serde(default)]
    sample_rate_hz: Option<u64>,
    #[serde(default)]
    enabled_channels: Option<Vec<u8>>,
    #[serde(default)]
    channel_span: Option<u64>,
    #[serde(default)]
    threshold_volts: Option<f64>,
    #[serde(default)]
    trigger_description: Option<String>,
    #[serde(default)]
    cross_chunks: Option<u64>,
    #[serde(default)]
    converted_bytes: Option<u64>,
    #[serde(default)]
    window_count: Option<u64>,
    #[serde(default)]
    sample_count: Option<u64>,
    #[serde(default)]
    callback_count: Option<u64>,
    #[serde(default)]
    queued_bytes: Option<u64>,
    #[serde(default)]
    injected_bytes: Option<u64>,
    #[serde(default)]
    underflows: Option<u64>,
    #[serde(default)]
    dropped_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureTelemetry {
    status: String,
    sample_rate_hz: Option<u64>,
    enabled_channels: Vec<u8>,
    channel_span: Option<u64>,
    threshold_volts: Option<f64>,
    trigger_description: Option<String>,
    cross_chunks: u64,
    converted_bytes: u64,
    window_count: Option<u64>,
    sample_count: Option<u64>,
    callback_count: Option<u64>,
    queued_bytes: Option<u64>,
    injected_bytes: Option<u64>,
    underflows: Option<u64>,
    dropped_bytes: Option<u64>,
}

impl Default for CaptureTelemetry {
    fn default() -> Self {
        Self {
            status: "idle".to_string(),
            sample_rate_hz: None,
            enabled_channels: Vec::new(),
            channel_span: None,
            threshold_volts: None,
            trigger_description: None,
            cross_chunks: 0,
            converted_bytes: 0,
            window_count: None,
            sample_count: None,
            callback_count: None,
            queued_bytes: None,
            injected_bytes: None,
            underflows: None,
            dropped_bytes: None,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticsReport {
    schema_version: u32,
    generated_at_unix_seconds: u64,
    client_version: String,
    platform: String,
    architecture: String,
    settings: ClientSettings,
    logic: LogicInspection,
    bridge_state: BridgeState,
    capture_telemetry: CaptureTelemetry,
    recent_logs: Vec<String>,
    previous_session_logs: Vec<String>,
    graph_log_tail: Option<String>,
    graph_host_crash_reports: Vec<CrashReportSnapshot>,
    local_compatibility_manifest: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CrashReportSnapshot {
    path: String,
    modified_at_unix_seconds: u64,
    size_bytes: u64,
    header: Option<serde_json::Value>,
    report_tail: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InitialState {
    settings: ClientSettings,
    applications: Vec<LogicInspection>,
    hardware: PxlogicHardwareState,
    bridge_state: BridgeState,
    capture_telemetry: CaptureTelemetry,
    logs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
struct PxlogicDeviceInfo {
    id: String,
    vid: u16,
    pid: u16,
    bus: Option<u8>,
    address: Option<u8>,
    label: String,
    ready: bool,
    manufacturer: Option<String>,
    product: Option<String>,
    serial_number: Option<String>,
    usb_speed: Option<String>,
    logic_mode: Option<u32>,
    profile_model: Option<String>,
    probe_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PxlogicHardwareState {
    devices: Vec<PxlogicDeviceInfo>,
    selected_device_id: Option<String>,
    firmware_resource_ready: bool,
    bitstream_resources_ready: bool,
    error: Option<String>,
}

impl PxlogicHardwareState {
    fn failure(error: impl Into<String>) -> Self {
        Self {
            devices: Vec::new(),
            selected_device_id: None,
            firmware_resource_ready: false,
            bitstream_resources_ready: false,
            error: Some(error.into()),
        }
    }
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
    capture_telemetry: Mutex<CaptureTelemetry>,
    logs: Mutex<VecDeque<String>>,
    previous_session_logs: Mutex<Vec<String>>,
    next_token: AtomicU64,
    quitting: AtomicBool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            runtime: Mutex::new(RuntimeState::default()),
            bridge_state: Mutex::new(BridgeState::default()),
            capture_telemetry: Mutex::new(CaptureTelemetry::default()),
            logs: Mutex::new(VecDeque::new()),
            previous_session_logs: Mutex::new(Vec::new()),
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

fn compatibility_cache_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|directory| directory.join("compatibility-analysis.json"))
        .map_err(|error| format!("无法确定兼容性缓存目录: {error}"))
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
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
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
        Sha256::digest(format!(
            "{}:{}:{}",
            app_path.display(),
            metadata.len(),
            modified
        ))
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
    fs::rename(&temporary, &cached).map_err(|error| format!("无法保存 AppImage 缓存: {error}"))?;
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
        || (status == "pending-live-validation" && platform == "win32" && architecture == "x64")
        || (matches!(status, "candidate" | "locally-verified")
            && matches!(
                (platform, architecture),
                ("darwin", "arm64") | ("win32", "x64") | ("linux", "x64")
            ))
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

fn local_compatibility_profiles(
    cache_path: Option<&Path>,
    analyzer_version: u32,
) -> Result<Vec<CompatibilityProfile>, String> {
    let Some(cache_path) = cache_path.filter(|path| path.is_file()) else {
        return Ok(Vec::new());
    };
    let contents = fs::read_to_string(cache_path)
        .map_err(|error| format!("无法读取本地兼容性缓存 {}: {error}", cache_path.display()))?;
    let manifest: CompatibilityManifest = serde_json::from_str(&contents)
        .map_err(|error| format!("本地兼容性缓存无效 {}: {error}", cache_path.display()))?;
    validate_local_compatibility_manifest(manifest, analyzer_version)
}

fn validate_local_compatibility_manifest(
    manifest: CompatibilityManifest,
    analyzer_version: u32,
) -> Result<Vec<CompatibilityProfile>, String> {
    if manifest.schema_version != 1 {
        return Err(format!(
            "不支持的本地兼容性缓存版本: {}",
            manifest.schema_version
        ));
    }
    if manifest.analyzer_version != Some(analyzer_version) {
        return Ok(Vec::new());
    }
    Ok(manifest
        .profiles
        .into_iter()
        .filter(|profile| {
            matches!(
                profile.hook.status.as_str(),
                "candidate" | "locally-verified"
            )
        })
        .collect())
}

fn inspect_graph_compatibility(
    app_path: &Path,
    cache_path: Option<&Path>,
) -> Result<GraphInspection, String> {
    let graph_path = find_graph_binary(app_path)?;
    let data = fs::read(&graph_path)
        .map_err(|error| format!("无法读取 GraphServer {}: {error}", graph_path.display()))?;
    let mut inspection = graph_fingerprint(&data, graph_path);
    let built_in = compatibility_manifest()?;
    let analyzer_version = built_in
        .analyzer_version
        .ok_or_else(|| "内置兼容 profile 清单缺少 analyzerVersion".to_string())?;
    let mut profiles = built_in.profiles;
    profiles.extend(local_compatibility_profiles(cache_path, analyzer_version)?);
    let candidates: Vec<_> = profiles
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

fn run_offline_compatibility_analysis(
    app: &AppHandle,
    runtime_path: &Path,
    graph_path: &Path,
    logic_version: &str,
    force: bool,
) -> Result<OfflineAnalysisResult, String> {
    let bridge = bridge_root(app)?;
    let analyzer = bridge.join("lib/offline-compatibility.cjs");
    if !analyzer.is_file() {
        return Err(format!("离线兼容性分析器不存在: {}", analyzer.display()));
    }
    let executable = logic_executable(runtime_path);
    if !executable.is_file() {
        return Err(format!("Logic 可执行文件不存在: {}", executable.display()));
    }
    let cache = compatibility_cache_path(app)?;
    let mut command = Command::new(executable);
    command
        // Keep the script relative to cwd for Electron RunAsNode on Windows.
        .arg("lib/offline-compatibility.cjs")
        .arg(graph_path)
        .args(["--logic-version", logic_version])
        .args(["--platform", compatibility_platform()])
        .args(["--architecture", compatibility_architecture()])
        .arg("--cache")
        .arg(&cache)
        .current_dir(&bridge)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if force {
        command.arg("--force");
    }
    configure_child_process(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动离线兼容性分析: {error}"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < OFFLINE_ANALYSIS_TIMEOUT => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("离线兼容性分析超时".to_string());
            }
            Err(error) => {
                let _ = child.kill();
                return Err(format!("离线兼容性分析进程失败: {error}"));
            }
        }
    };
    let (stdout, stderr) = collect_child_output(&mut child);
    if !status.success() {
        return Err(format!("离线兼容性分析失败（{status}）: {}", stderr.trim()));
    }
    serde_json::from_str(stdout.trim()).map_err(|error| format!("离线兼容性分析响应无效: {error}"))
}

fn profile_matches_graph(profile: &CompatibilityProfile, graph: &GraphInspection) -> bool {
    profile.platform == compatibility_platform()
        && profile.architecture == compatibility_architecture()
        && profile.graph.identity_kind == graph.identity_kind
        && profile.graph.identity.eq_ignore_ascii_case(&graph.identity)
        && profile.graph.sha256.eq_ignore_ascii_case(&graph.sha256)
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

fn inspect_logic_path(
    app: Option<&AppHandle>,
    app_path: &Path,
    force_offline_analysis: bool,
) -> LogicInspection {
    let runtime_path = match resolve_logic_installation(app_path) {
        Ok(path) => path,
        Err(error) => return LogicInspection::failure(app_path, None, error),
    };
    let version = match read_logic_version(&runtime_path) {
        Ok(version) => version,
        Err(error) => return LogicInspection::failure(app_path, None, error),
    };
    let cache_path = app.and_then(|app| compatibility_cache_path(app).ok());
    let mut graph = match inspect_graph_compatibility(&runtime_path, cache_path.as_deref()) {
        Ok(graph) => graph,
        Err(error) => return LogicInspection::failure(app_path, Some(version), error),
    };
    let should_analyze = graph.profile.is_none()
        || (force_offline_analysis
            && graph.profile.as_ref().is_some_and(|profile| {
                matches!(
                    profile.hook.status.as_str(),
                    "candidate" | "locally-verified"
                )
            }));
    if should_analyze {
        if let Some(app) = app {
            match run_offline_compatibility_analysis(
                app,
                &runtime_path,
                &graph.path,
                &version,
                force_offline_analysis,
            ) {
                Ok(analysis) if analysis.status == "candidate" => {
                    let Some(profile) = analysis.profile else {
                        return LogicInspection::failure(
                            app_path,
                            Some(version),
                            "离线分析返回 candidate 但没有 profile",
                        );
                    };
                    if !profile_matches_graph(&profile, &graph) {
                        return LogicInspection::failure(
                            app_path,
                            Some(version),
                            "离线分析 profile 与当前 GraphServer 指纹不一致",
                        );
                    }
                    graph.profile = Some(profile);
                }
                Ok(analysis) if analysis.status == "unsupported" => {
                    return LogicInspection {
                        path: app_path.display().to_string(),
                        version: Some(version),
                        supported: false,
                        runnable: false,
                        error: Some(format!(
                            "离线兼容性分析{}未通过: {}。请参考手工 profile 分析文档",
                            if analysis.cached { "（缓存）" } else { "" },
                            analysis.reason
                        )),
                        node_version: None,
                        electron_version: None,
                        profile_id: None,
                        graph_path: Some(graph.path.display().to_string()),
                        graph_format: Some(graph.format),
                        graph_identity_kind: Some(graph.identity_kind),
                        graph_identity: Some(graph.identity),
                        graph_sha256: Some(graph.sha256),
                        hook_status: Some("unsupported".to_string()),
                    };
                }
                Ok(analysis) => {
                    return LogicInspection::failure(
                        app_path,
                        Some(version),
                        format!("离线兼容性分析返回未知状态: {}", analysis.status),
                    );
                }
                Err(error) => {
                    return LogicInspection {
                        path: app_path.display().to_string(),
                        version: Some(version),
                        supported: false,
                        runnable: false,
                        error: Some(format!("无法完成离线兼容性分析: {error}")),
                        node_version: None,
                        electron_version: None,
                        profile_id: None,
                        graph_path: Some(graph.path.display().to_string()),
                        graph_format: Some(graph.format),
                        graph_identity_kind: Some(graph.identity_kind),
                        graph_identity: Some(graph.identity),
                        graph_sha256: Some(graph.sha256),
                        hook_status: Some("unsupported".to_string()),
                    };
                }
            }
        }
    }
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
            hook_status: Some("unknown".to_string()),
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
            "实验性离线 profile，等待 ABI 与本机 PXLogic 捕获验证: {}",
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

fn scan_logic_paths(app: &AppHandle, saved_path: Option<&str>) -> Vec<LogicInspection> {
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
        applications.push(inspect_logic_path(Some(app), &candidate, false));
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
        root.join("lib/macos-hook-locator.cjs"),
        root.join("lib/offline-compatibility.cjs"),
        root.join("lib/websocket-proxy.cjs"),
        root.join("lib/windows-hook-locator.cjs"),
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

fn scan_pxlogic_hardware(app: &AppHandle, preferred_device_id: &str) -> PxlogicHardwareState {
    let payload = match validate_bridge_payload(app) {
        Ok(payload) => payload,
        Err(error) => return PxlogicHardwareState::failure(error),
    };
    let mut command = Command::new(&payload.helper);
    command
        .arg("--list-json")
        .current_dir(payload.helper.parent().unwrap_or_else(|| Path::new(".")))
        .env("PXLOGIC_BITSTREAM_DIR", &payload.bitstreams)
        .env("PXLOGIC_MCU_FIRMWARE", &payload.firmware)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_child_process(&mut command);
    let output = match command.output() {
        Ok(output) => output,
        Err(error) => {
            return PxlogicHardwareState::failure(format!("无法启动 PXLogic 设备扫描: {error}"))
        }
    };
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        return PxlogicHardwareState::failure(format!(
            "PXLogic 设备扫描失败（{}）: {}",
            output.status,
            diagnostic.trim()
        ));
    }
    let devices = match serde_json::from_slice::<Vec<PxlogicDeviceInfo>>(&output.stdout) {
        Ok(devices) => devices,
        Err(error) => {
            return PxlogicHardwareState::failure(format!("PXLogic 设备扫描响应无效: {error}"))
        }
    };
    let selected_device_id = devices
        .iter()
        .find(|device| device.id == preferred_device_id)
        .or_else(|| devices.iter().find(|device| device.ready))
        .or_else(|| devices.first())
        .map(|device| device.id.clone());
    PxlogicHardwareState {
        devices,
        selected_device_id,
        firmware_resource_ready: payload.firmware.is_file(),
        bitstream_resources_ready: payload.bitstreams.join("hspi_ddr.bin").is_file()
            && payload.bitstreams.join("hspi_ddr_RST.bin").is_file(),
        error: None,
    }
}

fn update_bridge_state(app: &AppHandle, next: BridgeState) {
    if let Ok(mut current) = app.state::<AppState>().bridge_state.lock() {
        *current = next.clone();
    }
    let _ = app.emit("bridge-state", next);
}

fn parse_bridge_runtime_event(line: &str) -> Option<BridgeRuntimeEvent> {
    const PREFIX: &str = "[logic2-bridge:event] ";
    serde_json::from_str(line.strip_prefix(PREFIX)?).ok()
}

fn capture_failure_message(code: &str) -> &'static str {
    match code {
        "PXLOGIC_RATE_MISMATCH" => "实际采样率与 Logic 2 设置不一致",
        "PXLOGIC_CHANNEL_MISMATCH" | "PXLOGIC_CHANNEL_MAPPING_CHANGED" => {
            "实际通道映射与 Logic 2 设置不一致"
        }
        "PXLOGIC_CONVERSION_FAILED" => "PXLogic 数据转换失败",
        "PXLOGIC_HELPER_START_FAILED" => "PXLogic 采集进程无法启动",
        "PXLOGIC_HELPER_EXITED" => "PXLogic 采集进程异常退出",
        "PXLOGIC_USB_REENUMERATED" => "检测到 PXLogic 的 USB 地址发生变化，常见于电脑 USB 控制器、Hub 或设备重置。采集已安全停止，设备通常未损坏。请重新扫描并初始化 Bridge。",
        "GRAPH_ANALYZER_CLEANUP_CRASH" => "Logic 2 图形服务在采集中清理协议分析器时异常退出。PXLogic 设备通常未损坏；请重新初始化 Bridge，诊断信息已保留。",
        _ => "PXLogic 采集失败",
    }
}

fn is_graph_analyzer_cleanup_crash(line: &str) -> bool {
    line.contains("simulation_provider.cpp:45")
        && line.contains("removing an analyzer during a simulation")
}

fn graph_analyzer_cleanup_failure(actual_port: Option<u16>) -> BridgeState {
    BridgeState {
        phase: "recovery".to_string(),
        actual_port,
        message: "Logic 2 图形服务在采集中清理协议分析器时异常退出。PXLogic 设备通常未损坏；请重新初始化 Bridge，诊断信息已保留。".to_string(),
        error_code: Some("GRAPH_ANALYZER_CLEANUP_CRASH".to_string()),
        recovery_action: Some("restart-bridge".to_string()),
    }
}

fn classify_start_failure(message: &str) -> (&'static str, &'static str) {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("graphserver")
        || normalized.contains("logic 2")
        || normalized.contains("profile")
    {
        ("LOGIC_COMPATIBILITY", "review-logic")
    } else if normalized.contains("pxlogic")
        || normalized.contains("firmware")
        || normalized.contains("bitstream")
        || normalized.contains("fpga")
        || message.contains("设备")
    {
        ("PXLOGIC_NOT_READY", "rescan-hardware")
    } else {
        ("BRIDGE_START_FAILED", "export-diagnostics")
    }
}

fn apply_capture_runtime_event(
    telemetry: &mut CaptureTelemetry,
    event: &BridgeRuntimeEvent,
) -> bool {
    match event.event_type.as_str() {
        "capture-starting" => {
            *telemetry = CaptureTelemetry {
                status: "starting".to_string(),
                sample_rate_hz: event.sample_rate_hz,
                enabled_channels: event.enabled_channels.clone().unwrap_or_default(),
                channel_span: event.channel_span,
                threshold_volts: event.threshold_volts,
                trigger_description: event.trigger_description.clone(),
                ..CaptureTelemetry::default()
            };
        }
        "capture-started" => {
            telemetry.status = "streaming".to_string();
            telemetry.sample_rate_hz = event.sample_rate_hz.or(telemetry.sample_rate_hz);
            if let Some(channels) = event.enabled_channels.as_ref() {
                telemetry.enabled_channels.clone_from(channels);
            }
            telemetry.channel_span = event.channel_span.or(telemetry.channel_span);
            telemetry.threshold_volts = event.threshold_volts.or(telemetry.threshold_volts);
            if let Some(trigger) = event.trigger_description.as_ref() {
                telemetry.trigger_description = Some(trigger.clone());
            }
        }
        "capture-progress" => {
            if let Some(value) = event.cross_chunks {
                telemetry.cross_chunks = value;
            }
            if let Some(value) = event.converted_bytes {
                telemetry.converted_bytes = value;
            }
            telemetry.window_count = event.window_count.or(telemetry.window_count);
            telemetry.sample_count = event.sample_count.or(telemetry.sample_count);
        }
        "injection-progress" => {
            telemetry.callback_count = event.callback_count.or(telemetry.callback_count);
            telemetry.queued_bytes = event.queued_bytes.or(telemetry.queued_bytes);
            telemetry.injected_bytes = event.injected_bytes.or(telemetry.injected_bytes);
            telemetry.underflows = event.underflows.or(telemetry.underflows);
            telemetry.dropped_bytes = event.dropped_bytes.or(telemetry.dropped_bytes);
        }
        "capture-ended" => {
            telemetry.status = event.status.clone().unwrap_or_else(|| {
                if event.failed.unwrap_or(false) {
                    "error".to_string()
                } else {
                    "stopped".to_string()
                }
            });
            if let Some(value) = event.cross_chunks {
                telemetry.cross_chunks = value;
            }
            if let Some(value) = event.converted_bytes {
                telemetry.converted_bytes = value;
            }
        }
        "capture-unavailable" => telemetry.status = "error".to_string(),
        _ => return false,
    }
    true
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
    if let Some(event) = parse_bridge_runtime_event(line) {
        let capture_telemetry = app
            .state::<AppState>()
            .capture_telemetry
            .lock()
            .ok()
            .and_then(|mut telemetry| {
                apply_capture_runtime_event(&mut telemetry, &event).then(|| telemetry.clone())
            });
        if let Some(telemetry) = capture_telemetry {
            let _ = app.emit("capture-telemetry", telemetry);
        }
        if event.event_type == "capture-unavailable" {
            let code = event
                .code
                .clone()
                .unwrap_or_else(|| "PXLOGIC_CAPTURE_FAILED".to_string());
            let actual_port = app
                .state::<AppState>()
                .bridge_state
                .lock()
                .ok()
                .and_then(|state| state.actual_port);
            if let Some(detail) = event.detail.as_deref() {
                let detail_entry = format!("[diagnostic] {code}: {detail}");
                if let Ok(mut logs) = app.state::<AppState>().logs.lock() {
                    logs.push_back(detail_entry);
                    while logs.len() > MAX_LOG_LINES {
                        logs.pop_front();
                    }
                }
            }
            update_bridge_state(
                app,
                BridgeState {
                    phase: "recovery".to_string(),
                    actual_port,
                    message: capture_failure_message(&code).to_string(),
                    error_code: Some(code),
                    recovery_action: event
                        .recovery_action
                        .clone()
                        .or_else(|| Some("restart-bridge".to_string())),
                },
            );
        }
        if event.event_type == "graphserver-failure" {
            let code = event
                .code
                .clone()
                .unwrap_or_else(|| "GRAPH_ANALYZER_CLEANUP_CRASH".to_string());
            let actual_port = app
                .state::<AppState>()
                .bridge_state
                .lock()
                .ok()
                .and_then(|state| state.actual_port);
            if let Some(detail) = event.detail.as_deref() {
                let detail_entry = format!("[diagnostic] {code}: {detail}");
                if let Ok(mut logs) = app.state::<AppState>().logs.lock() {
                    logs.push_back(detail_entry);
                    while logs.len() > MAX_LOG_LINES {
                        logs.pop_front();
                    }
                }
            }
            update_bridge_state(
                app,
                BridgeState {
                    phase: "recovery".to_string(),
                    actual_port,
                    message: capture_failure_message(&code).to_string(),
                    error_code: Some(code),
                    recovery_action: event
                        .recovery_action
                        .or_else(|| Some("restart-bridge".to_string())),
                },
            );
        }
    }
    if let Some(port) = parse_ready_port(line) {
        update_bridge_state(
            app,
            BridgeState {
                phase: "running".to_string(),
                actual_port: Some(port),
                message: "已连接".to_string(),
                error_code: None,
                recovery_action: None,
            },
        );
    }
    if is_graph_analyzer_cleanup_crash(line) {
        let actual_port = app
            .state::<AppState>()
            .bridge_state
            .lock()
            .ok()
            .and_then(|state| state.actual_port);
        update_bridge_state(app, graph_analyzer_cleanup_failure(actual_port));
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
                let graph_analyzer_cleanup_crash = app
                    .state::<AppState>()
                    .bridge_state
                    .lock()
                    .map(|state| {
                        state.error_code.as_deref() == Some("GRAPH_ANALYZER_CLEANUP_CRASH")
                    })
                    .unwrap_or(false)
                    || app
                        .state::<AppState>()
                        .logs
                        .lock()
                        .map(|logs| {
                            logs.iter()
                                .any(|line| is_graph_analyzer_cleanup_crash(line))
                        })
                        .unwrap_or(false);
                match result {
                    _ if graph_analyzer_cleanup_crash && !quitting => {
                        update_bridge_state(&app, graph_analyzer_cleanup_failure(None))
                    }
                    Ok(status) if status.success() || quitting => {
                        update_bridge_state(&app, BridgeState::default())
                    }
                    Ok(status) => update_bridge_state(
                        &app,
                        BridgeState {
                            phase: "error".to_string(),
                            actual_port: None,
                            message: format!("Bridge 已退出（{}）", describe_status(status)),
                            error_code: Some("BRIDGE_PROCESS_EXITED".to_string()),
                            recovery_action: Some("export-diagnostics".to_string()),
                        },
                    ),
                    Err(error) => update_bridge_state(
                        &app,
                        BridgeState {
                            phase: "error".to_string(),
                            actual_port: None,
                            message: format!("Bridge 状态读取失败: {error}"),
                            error_code: Some("BRIDGE_STATUS_FAILED".to_string()),
                            recovery_action: Some("export-diagnostics".to_string()),
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
        error_code: None,
        recovery_action: None,
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
    let (mut settings, applications, hardware) = tauri::async_runtime::spawn_blocking(move || {
        let settings = load_settings(&worker_app);
        let applications = scan_logic_paths(&worker_app, Some(&settings.logic_app_path));
        let hardware = scan_pxlogic_hardware(&worker_app, &settings.pxlogic_device_id);
        (settings, applications, hardware)
    })
    .await
    .map_err(|error| format!("启动检查任务失败: {error}"))?;
    let mut settings_changed = false;
    if settings.logic_app_path.is_empty() {
        if let Some(preferred) = applications
            .iter()
            .find(|application| application.runnable)
            .or_else(|| applications.first())
        {
            settings.logic_app_path = preferred.path.clone();
            settings_changed = true;
        }
    }
    if let Some(selected_device_id) = hardware.selected_device_id.as_deref() {
        if settings.pxlogic_device_id != selected_device_id {
            settings.pxlogic_device_id = selected_device_id.to_string();
            settings_changed = true;
        }
    }
    if settings_changed {
        settings = store_settings(&app, settings)?;
    }
    let state = app.state::<AppState>();
    let bridge_state = state
        .bridge_state
        .lock()
        .map_err(|_| "Bridge 状态已损坏".to_string())?
        .clone();
    let capture_telemetry = state
        .capture_telemetry
        .lock()
        .map_err(|_| "采集状态已损坏".to_string())?
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
        hardware,
        bridge_state,
        capture_telemetry,
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
async fn logic_scan(app: AppHandle, saved_path: String) -> Result<Vec<LogicInspection>, String> {
    tauri::async_runtime::spawn_blocking(move || scan_logic_paths(&app, Some(&saved_path)))
        .await
        .map_err(|error| format!("Logic 扫描任务失败: {error}"))
}

#[tauri::command]
async fn logic_inspect(app: AppHandle, app_path: String) -> Result<LogicInspection, String> {
    tauri::async_runtime::spawn_blocking(move || {
        inspect_logic_path(Some(&app), Path::new(app_path.trim()), false)
    })
    .await
    .map_err(|error| format!("Logic 检查任务失败: {error}"))
}

#[tauri::command]
async fn logic_analyze(app: AppHandle, app_path: String) -> Result<LogicInspection, String> {
    tauri::async_runtime::spawn_blocking(move || {
        inspect_logic_path(Some(&app), Path::new(app_path.trim()), true)
    })
    .await
    .map_err(|error| format!("Logic 兼容性分析任务失败: {error}"))
}

#[tauri::command]
async fn pxlogic_scan(
    app: AppHandle,
    preferred_device_id: String,
) -> Result<PxlogicHardwareState, String> {
    tauri::async_runtime::spawn_blocking(move || {
        scan_pxlogic_hardware(&app, preferred_device_id.trim())
    })
    .await
    .map_err(|error| format!("PXLogic 扫描任务失败: {error}"))
}

#[tauri::command]
async fn logic_browse(app: AppHandle) -> Result<Option<LogicInspection>, String> {
    let dialog_app = app.clone();
    let selected = tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "linux")]
        {
            return dialog_app
                .dialog()
                .file()
                .set_title("选择 Saleae Logic 2 AppImage")
                .add_filter("Linux AppImage", &["AppImage", "appimage"])
                .blocking_pick_file();
        }
        #[cfg(not(target_os = "linux"))]
        dialog_app
            .dialog()
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
    Ok(Some(inspect_logic_path(Some(&app), &path, false)))
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
            error_code: None,
            recovery_action: None,
        },
    );
    let capture_telemetry = CaptureTelemetry::default();
    if let Ok(mut current) = app.state::<AppState>().capture_telemetry.lock() {
        *current = capture_telemetry.clone();
    }
    let _ = app.emit("capture-telemetry", capture_telemetry);

    let result = start_bridge_inner(&app, settings);
    if result.is_err() {
        if let Ok(mut runtime) = app.state::<AppState>().runtime.lock() {
            runtime.starting = false;
        }
        let message = result.as_ref().err().cloned().unwrap_or_default();
        let (error_code, recovery_action) = classify_start_failure(&message);
        update_bridge_state(
            &app,
            BridgeState {
                phase: "error".to_string(),
                actual_port: None,
                message,
                error_code: Some(error_code.to_string()),
                recovery_action: Some(recovery_action.to_string()),
            },
        );
    }
    result
}

fn start_bridge_inner(app: &AppHandle, settings: ClientSettings) -> Result<BridgeState, String> {
    let mut settings = store_settings(app, settings)?;
    let selected_app_path = PathBuf::from(&settings.logic_app_path);
    let app_path = resolve_logic_installation(&selected_app_path)?;
    let inspection = inspect_logic_path(Some(app), &selected_app_path, false);
    if !inspection.runnable {
        return Err(inspection
            .error
            .unwrap_or_else(|| "Logic 2 安装无效".to_string()));
    }
    let hardware = scan_pxlogic_hardware(app, &settings.pxlogic_device_id);
    if let Some(error) = hardware.error {
        return Err(error);
    }
    let selected_device_id = hardware
        .selected_device_id
        .ok_or_else(|| "未检测到 PXLogic 设备".to_string())?;
    let selected_device = hardware
        .devices
        .iter()
        .find(|device| device.id == selected_device_id)
        .ok_or_else(|| "所选 PXLogic 设备已断开".to_string())?;
    if !selected_device.ready {
        return Err(selected_device
            .probe_error
            .clone()
            .unwrap_or_else(|| "所选 PXLogic 设备尚未就绪".to_string()));
    }
    settings.pxlogic_device_id = selected_device_id;
    settings = store_settings(app, settings)?;
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
        .args(["--pxlogic-device", &settings.pxlogic_device_id])
        .args([
            "--hardware-threshold-volts",
            &settings.pxlogic_threshold_volts.to_string(),
        ])
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
    if matches!(
        inspection.hook_status.as_deref(),
        Some("pending-live-validation" | "candidate" | "locally-verified")
    ) {
        command.arg("--allow-pending-profile");
    }
    let compatibility_cache = compatibility_cache_path(app)?;
    if compatibility_cache.is_file() {
        command
            .arg("--compatibility-profiles")
            .arg(compatibility_cache);
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
    let previous_session_logs = if let Ok(mut logs) = app.state::<AppState>().logs.lock() {
        let previous = logs.iter().cloned().collect::<Vec<_>>();
        logs.clear();
        previous
    } else {
        Vec::new()
    };
    if !previous_session_logs.is_empty() {
        if let Ok(mut previous) = app.state::<AppState>().previous_session_logs.lock() {
            *previous = previous_session_logs;
        }
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

fn wait_for_bridge_stop(app: &AppHandle, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let process_stopped = app
            .state::<AppState>()
            .runtime
            .lock()
            .map_err(|_| "Bridge 进程状态已损坏".to_string())?
            .child
            .is_none();
        let state_settled = app
            .state::<AppState>()
            .bridge_state
            .lock()
            .map_err(|_| "Bridge 状态已损坏".to_string())?
            .phase
            != "stopping";
        if process_stopped && state_settled {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("等待 Bridge 停止超时，请先退出后再重新启动".to_string());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[tauri::command]
async fn bridge_restart(app: AppHandle, settings: ClientSettings) -> Result<BridgeState, String> {
    request_stop(&app)?;
    let wait_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        wait_for_bridge_stop(&wait_app, Duration::from_secs(6))
    })
    .await
    .map_err(|error| format!("Bridge 恢复任务失败: {error}"))??;
    bridge_start(app, settings).await
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

fn read_text_tail(path: &Path, max_bytes: usize) -> Option<String> {
    let contents = fs::read(path).ok()?;
    let begin = contents.len().saturating_sub(max_bytes);
    Some(String::from_utf8_lossy(&contents[begin..]).into_owned())
}

#[cfg(target_os = "macos")]
fn recent_graph_host_crash_reports() -> Vec<CrashReportSnapshot> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let directory = PathBuf::from(home).join("Library/Logs/DiagnosticReports");
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut reports = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("graph-host-") && name.ends_with(".ips"))
        })
        .filter_map(|entry| {
            let path = entry.path();
            let metadata = entry.metadata().ok()?;
            let modified_at_unix_seconds = metadata
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_secs();
            let report_tail = read_text_tail(&path, 64 * 1024);
            let header = fs::read_to_string(&path)
                .ok()
                .and_then(|contents| contents.lines().next().map(str::to_string))
                .and_then(|line| serde_json::from_str(&line).ok());
            Some(CrashReportSnapshot {
                path: path.to_string_lossy().into_owned(),
                modified_at_unix_seconds,
                size_bytes: metadata.len(),
                header,
                report_tail,
            })
        })
        .collect::<Vec<_>>();
    reports.sort_by_key(|report| std::cmp::Reverse(report.modified_at_unix_seconds));
    reports.truncate(3);
    reports
}

#[cfg(not(target_os = "macos"))]
fn recent_graph_host_crash_reports() -> Vec<CrashReportSnapshot> {
    Vec::new()
}

fn diagnostics_report(app: &AppHandle) -> Result<DiagnosticsReport, String> {
    let settings = load_settings(app);
    let logic = inspect_logic_path(Some(app), Path::new(&settings.logic_app_path), false);
    let bridge_state = app
        .state::<AppState>()
        .bridge_state
        .lock()
        .map_err(|_| "Bridge 状态已损坏".to_string())?
        .clone();
    let capture_telemetry = app
        .state::<AppState>()
        .capture_telemetry
        .lock()
        .map_err(|_| "采集状态已损坏".to_string())?
        .clone();
    let recent_logs = app
        .state::<AppState>()
        .logs
        .lock()
        .map_err(|_| "Bridge 日志状态已损坏".to_string())?
        .iter()
        .cloned()
        .collect();
    let previous_session_logs = app
        .state::<AppState>()
        .previous_session_logs
        .lock()
        .map_err(|_| "上一 Bridge 会话日志状态已损坏".to_string())?
        .clone();
    let graph_log_tail = bridge_log_directory()
        .ok()
        .and_then(|directory| read_text_tail(&directory.join("graphio.log"), 64 * 1024));
    let local_compatibility_manifest = compatibility_cache_path(app)
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok());
    let generated_at_unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(DiagnosticsReport {
        schema_version: 2,
        generated_at_unix_seconds,
        client_version: app.package_info().version.to_string(),
        platform: std::env::consts::OS.to_string(),
        architecture: std::env::consts::ARCH.to_string(),
        settings,
        logic,
        bridge_state,
        capture_telemetry,
        recent_logs,
        previous_session_logs,
        graph_log_tail,
        graph_host_crash_reports: recent_graph_host_crash_reports(),
        local_compatibility_manifest,
    })
}

#[tauri::command]
async fn diagnostics_export(app: AppHandle) -> Result<Option<String>, String> {
    let report = diagnostics_report(&app)?;
    let generated_at = report.generated_at_unix_seconds;
    let dialog_app = app.clone();
    let selected = tauri::async_runtime::spawn_blocking(move || {
        dialog_app
            .dialog()
            .file()
            .set_title("导出 PXLogic Bridge 诊断")
            .set_file_name(format!("pxlogic-bridge-diagnostics-{generated_at}.json"))
            .add_filter("JSON", &["json"])
            .blocking_save_file()
    })
    .await
    .map_err(|error| format!("诊断文件选择任务失败: {error}"))?;
    let Some(path) = selected else {
        return Ok(None);
    };
    let mut path = path
        .into_path()
        .map_err(|error| format!("诊断文件路径无效: {error}"))?;
    if path.extension().is_none() {
        path.set_extension("json");
    }
    let contents = serde_json::to_string_pretty(&report)
        .map_err(|error| format!("无法序列化诊断信息: {error}"))?;
    fs::write(&path, format!("{contents}\n"))
        .map_err(|error| format!("无法写入诊断文件: {error}"))?;
    Ok(Some(path.display().to_string()))
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

fn compatibility_manual_path(app: &AppHandle) -> Result<PathBuf, String> {
    if let Ok(resource_dir) = app.path().resource_dir() {
        let packaged = resource_dir.join("docs/graphserver-profile-manual.md");
        if packaged.is_file() {
            return Ok(packaged);
        }
    }
    let development = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .map(|root| root.join("docs/graphserver-profile-manual.md"))
        .ok_or_else(|| "无法确定手工兼容性指南路径".to_string())?;
    if development.is_file() {
        Ok(development)
    } else {
        Err(format!("手工兼容性指南不存在: {}", development.display()))
    }
}

#[tauri::command]
fn manual_open(app: AppHandle) -> Result<(), String> {
    let manual = compatibility_manual_path(&app)?;
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = Command::new("explorer.exe");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");
    command
        .arg(&manual)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_child_process(&mut command);
    command
        .spawn()
        .map_err(|error| format!("无法打开手工兼容性指南: {error}"))?;
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
            logic_analyze,
            logic_browse,
            pxlogic_scan,
            bridge_start,
            bridge_restart,
            bridge_stop,
            diagnostics_export,
            logs_open,
            manual_open,
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
            pxlogic_device_id: "  usb:1234  ".to_string(),
            pxlogic_threshold_volts: 1.12,
            temporary_comparator_threshold_volts: None,
            pxlogic_threshold_profiles: BTreeMap::from([(
                "usb:1234".to_string(),
                ThresholdProfile {
                    volts: 1.12,
                    verified: true,
                    reference: "custom".to_string(),
                },
            )]),
        }
        .normalized();
        assert_eq!(settings.logic_app_path, "/Applications/Saleae Logic.app");
        assert_eq!(settings.port_mode, "auto");
        assert_eq!(settings.preferred_port, 43210);
        assert_eq!(settings.screen_quadrant, 3);
        assert!(settings.maximize_logic_window);
        assert_eq!(settings.pxlogic_device_id, "usb:1234");
        assert_eq!(settings.pxlogic_threshold_volts, 1.12);
        assert!(settings.pxlogic_threshold_profiles["usb:1234"].verified);
    }

    #[test]
    fn migrates_legacy_settings_to_maximized_window() {
        let settings: ClientSettings = serde_json::from_str(
            r#"{"logicAppPath":"","portMode":"auto","preferredPort":12472,"screenQuadrant":3}"#,
        )
        .unwrap();
        assert!(settings.maximize_logic_window);
        assert_eq!(settings.pxlogic_device_id, "");
        assert_eq!(settings.pxlogic_threshold_volts, 1.8);
        assert!(settings.pxlogic_threshold_profiles.is_empty());
    }

    #[test]
    fn drops_invalid_per_device_threshold_profiles() {
        let settings: ClientSettings = serde_json::from_str(
            r#"{"logicAppPath":"","portMode":"auto","preferredPort":12472,"screenQuadrant":3,"pxlogicThresholdProfiles":{"usb:ready":{"volts":2.2,"verified":true,"reference":" fixture-stm32-spi "},"usb:invalid":{"volts":7.0,"verified":true,"reference":"custom"}}}"#,
        )
        .unwrap();
        let settings = settings.normalized();
        assert_eq!(settings.pxlogic_threshold_profiles.len(), 1);
        let profile = &settings.pxlogic_threshold_profiles["usb:ready"];
        assert_eq!(profile.volts, 2.2);
        assert!(profile.verified);
        assert_eq!(profile.reference, "fixture-stm32-spi");
    }

    #[test]
    fn keeps_existing_pxview_threshold_value() {
        let settings: ClientSettings = serde_json::from_str(
            r#"{"logicAppPath":"","portMode":"auto","preferredPort":12472,"screenQuadrant":3,"pxlogicThresholdVolts":2.5}"#,
        )
        .unwrap();
        let settings = settings.normalized();
        assert_eq!(settings.pxlogic_threshold_volts, 2.5);
        assert!(settings.temporary_comparator_threshold_volts.is_none());
    }

    #[test]
    fn migrates_temporary_comparator_field_without_rescaling() {
        let settings: ClientSettings = serde_json::from_str(
            r#"{"logicAppPath":"","portMode":"auto","preferredPort":12472,"screenQuadrant":3,"pxlogicComparatorThresholdVolts":1.2}"#,
        )
        .unwrap();
        let settings = settings.normalized();
        assert_eq!(settings.pxlogic_threshold_volts, 1.2);
        assert!(settings.temporary_comparator_threshold_volts.is_none());
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
    fn parses_capture_recovery_event() {
        let event = parse_bridge_runtime_event(
            r#"[logic2-bridge:event] {"type":"capture-unavailable","code":"PXLOGIC_RATE_MISMATCH","detail":"rate","recoveryAction":"restart-bridge"}"#,
        )
        .unwrap();
        assert_eq!(event.event_type, "capture-unavailable");
        assert_eq!(event.code.as_deref(), Some("PXLOGIC_RATE_MISMATCH"));
        assert_eq!(event.detail.as_deref(), Some("rate"));
        assert_eq!(event.recovery_action.as_deref(), Some("restart-bridge"));
        assert!(parse_bridge_runtime_event("ordinary log").is_none());
    }

    #[test]
    fn parses_graphserver_failure_event_for_recovery() {
        let event = parse_bridge_runtime_event(
            r#"[logic2-bridge:event] {"type":"graphserver-failure","code":"GRAPH_ANALYZER_CLEANUP_CRASH","detail":"assertion","recoveryAction":"restart-bridge"}"#,
        )
        .unwrap();
        assert_eq!(event.event_type, "graphserver-failure");
        assert_eq!(event.code.as_deref(), Some("GRAPH_ANALYZER_CLEANUP_CRASH"));
        assert_eq!(event.detail.as_deref(), Some("assertion"));
        assert_eq!(event.recovery_action.as_deref(), Some("restart-bridge"));
        assert!(capture_failure_message("GRAPH_ANALYZER_CLEANUP_CRASH")
            .contains("PXLogic 设备通常未损坏"));
    }

    #[test]
    fn aggregates_capture_and_native_injection_telemetry() {
        let mut telemetry = CaptureTelemetry::default();
        let started = parse_bridge_runtime_event(
            r#"[logic2-bridge:event] {"type":"capture-started","sampleRateHz":50000000,"enabledChannels":[0,4],"channelSpan":5,"thresholdVolts":2.2,"triggerDescription":"D4 rising"}"#,
        )
        .unwrap();
        assert!(apply_capture_runtime_event(&mut telemetry, &started));
        assert_eq!(telemetry.status, "streaming");
        assert_eq!(telemetry.sample_rate_hz, Some(50_000_000));
        assert_eq!(telemetry.enabled_channels, vec![0, 4]);
        assert_eq!(telemetry.threshold_volts, Some(2.2));

        let injection = parse_bridge_runtime_event(
            r#"[logic2-bridge:event] {"type":"injection-progress","callbackCount":128,"queuedBytes":131072,"injectedBytes":8388608,"underflows":2,"droppedBytes":0}"#,
        )
        .unwrap();
        assert!(apply_capture_runtime_event(&mut telemetry, &injection));
        assert_eq!(telemetry.callback_count, Some(128));
        assert_eq!(telemetry.injected_bytes, Some(8_388_608));
        assert_eq!(telemetry.underflows, Some(2));
        assert_eq!(telemetry.dropped_bytes, Some(0));
    }

    #[test]
    fn classifies_start_failures_into_user_actions() {
        assert_eq!(
            classify_start_failure("GraphServer profile mismatch"),
            ("LOGIC_COMPATIBILITY", "review-logic")
        );
        assert_eq!(
            classify_start_failure("PXLogic firmware missing"),
            ("PXLOGIC_NOT_READY", "rescan-hardware")
        );
        assert_eq!(
            classify_start_failure("unexpected spawn failure"),
            ("BRIDGE_START_FAILED", "export-diagnostics")
        );
    }

    #[test]
    fn reassures_users_after_confirmed_usb_reenumeration() {
        let message = capture_failure_message("PXLOGIC_USB_REENUMERATED");
        assert!(message.contains("采集已安全停止"));
        assert!(message.contains("设备通常未损坏"));
        assert!(message.contains("USB 控制器、Hub 或设备重置"));
    }

    #[test]
    fn classifies_the_graphserver_analyzer_cleanup_assertion() {
        let line = "[main] [critical] [simulation_provider.cpp:45] Assert: TODO: add support for removing an analyzer during a simulation";
        assert!(is_graph_analyzer_cleanup_crash(line));
        assert!(!is_graph_analyzer_cleanup_crash(
            "Direct message handler exception: Pipe with specified id does not exist"
        ));
        let state = graph_analyzer_cleanup_failure(Some(12472));
        assert_eq!(state.phase, "recovery");
        assert_eq!(state.actual_port, Some(12472));
        assert_eq!(
            state.error_code.as_deref(),
            Some("GRAPH_ANALYZER_CLEANUP_CRASH")
        );
        assert!(state.message.contains("PXLogic 设备通常未损坏"));
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
            .analyzer_version
            .is_some_and(|version| version >= 1));
        assert!(manifest
            .profiles
            .iter()
            .any(|profile| profile.id == "logic-2.4.46-macos-arm64-0df17631"));
    }

    #[test]
    fn rejects_stale_or_invalid_local_compatibility_profiles() {
        let current = compatibility_manifest().unwrap().analyzer_version.unwrap();
        let parse = |analyzer_version: u32, status: &str| {
            serde_json::from_value::<CompatibilityManifest>(serde_json::json!({
                "schemaVersion": 1,
                "analyzerVersion": analyzer_version,
                "profiles": [{
                    "id": "local-test",
                    "platform": "darwin",
                    "architecture": "arm64",
                    "graph": {
                        "identityKind": "macho-lc-uuid",
                        "identity": "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE",
                        "sha256": "00"
                    },
                    "hook": { "status": status, "validation": "test" }
                }]
            }))
            .unwrap()
        };

        assert_eq!(
            validate_local_compatibility_manifest(parse(current, "candidate"), current)
                .unwrap()
                .len(),
            1
        );
        assert!(
            validate_local_compatibility_manifest(parse(current + 1, "candidate"), current)
                .unwrap()
                .is_empty()
        );
        assert!(
            validate_local_compatibility_manifest(parse(current, "verified"), current)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn allows_known_pending_and_exact_offline_candidate_targets() {
        assert!(profile_runnable("verified", "darwin", "arm64"));
        assert!(profile_runnable("pending-live-validation", "win32", "x64"));
        assert!(!profile_runnable("pending-live-validation", "linux", "x64"));
        assert!(!profile_runnable(
            "pending-live-validation",
            "win32",
            "arm64"
        ));
        assert!(profile_runnable("candidate", "darwin", "arm64"));
        assert!(profile_runnable("candidate", "win32", "x64"));
        assert!(profile_runnable("candidate", "linux", "x64"));
        assert!(!profile_runnable("candidate", "darwin", "x64"));
        assert!(!profile_runnable("unsupported", "darwin", "arm64"));
    }
}
