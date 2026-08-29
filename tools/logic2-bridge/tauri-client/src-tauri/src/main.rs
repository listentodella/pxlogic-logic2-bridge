#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod mcp_proxy;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs,
    future::Future,
    io::{BufRead, BufReader, Read, Write},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    pin::Pin,
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
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
    /// Selected entry from `resources/firmware/releases.json`. An empty or
    /// unrecognised value is normalised back to the latest image, so the default
    /// is always the newest firmware.
    #[serde(default = "default_pxlogic_firmware_id")]
    pxlogic_firmware_id: String,
    #[serde(default)]
    guidance: GuidanceSettings,
    #[serde(default)]
    status_panel: StatusPanelSettings,
    #[serde(default)]
    mcp: McpSettings,
    // This one-shot UI authorization is bound to the inspected GraphServer
    // fingerprint and must never be persisted with the user's settings.
    #[serde(default, skip_serializing)]
    pending_profile_fingerprint: Option<String>,
}

/// Bump to replay the first-run walkthrough once after a major UI change. A
/// plain boolean would make that impossible without resetting other settings.
const ONBOARDING_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GuidanceSettings {
    /// 0 means the walkthrough has never been completed.
    #[serde(default)]
    onboarding_completed_version: u32,
    /// The always-on-top panel introduces itself the first time it appears on
    /// its own, so an automatic reveal never looks like an unexplained popup.
    #[serde(default)]
    status_panel_intro_seen: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PanelPosition {
    x: i32,
    y: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusPanelSettings {
    /// Physical pixels in virtual-desktop space. `None` places the panel at the
    /// primary work area's top-right corner rather than wherever the OS decides,
    /// which is the difference between a usable monitor and one hidden behind
    /// the Logic 2 window.
    #[serde(default)]
    position: Option<PanelPosition>,
    #[serde(default)]
    collapsed: bool,
    #[serde(default = "default_status_panel_auto_show")]
    auto_show: bool,
}

fn default_status_panel_auto_show() -> bool {
    true
}

impl Default for StatusPanelSettings {
    fn default() -> Self {
        Self {
            position: None,
            collapsed: false,
            auto_show: default_status_panel_auto_show(),
        }
    }
}

/// Ports for the MCP proxy that sits in front of Logic 2's own MCP server.
///
/// Backend-owned: the main window does not render them, and a renderer save must not
/// reset a port an agent has already been registered against.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct McpSettings {
    /// What the proxy tries to listen on. Fixed by default so a registration written
    /// once keeps working; the bound port can still differ if it was taken.
    #[serde(default = "default_mcp_listen_port")]
    listen_port: u16,
    /// Logic 2's MCP server, configurable in its Settings > Automation panel.
    #[serde(default = "default_mcp_upstream_port")]
    upstream_port: u16,
    /// The window reveals itself for an approval regardless; this governs only whether it
    /// opens on its own when the proxy first sees an agent.
    #[serde(default = "default_mcp_auto_show")]
    auto_show: bool,
    /// Physical desktop pixels, independent of the capture status panel position.
    #[serde(default)]
    position: Option<PanelPosition>,
}

fn default_mcp_listen_port() -> u16 {
    mcp_proxy::DEFAULT_LISTEN_PORT
}

fn default_mcp_upstream_port() -> u16 {
    mcp_proxy::DEFAULT_UPSTREAM_PORT
}

fn default_mcp_auto_show() -> bool {
    true
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            listen_port: default_mcp_listen_port(),
            upstream_port: default_mcp_upstream_port(),
            auto_show: default_mcp_auto_show(),
            position: None,
        }
    }
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
            pxlogic_firmware_id: default_pxlogic_firmware_id(),
            guidance: GuidanceSettings::default(),
            status_panel: StatusPanelSettings::default(),
            mcp: McpSettings::default(),
            pending_profile_fingerprint: None,
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
        self.pxlogic_firmware_id = self.pxlogic_firmware_id.trim().to_string();
        if find_mcu_firmware_release(&self.pxlogic_firmware_id).is_none() {
            self.pxlogic_firmware_id = default_pxlogic_firmware_id();
        }
        // A future build must never be treated as already onboarded.
        if self.guidance.onboarding_completed_version > ONBOARDING_VERSION {
            self.guidance.onboarding_completed_version = ONBOARDING_VERSION;
        }
        if self.mcp.listen_port == 0 {
            self.mcp.listen_port = default_mcp_listen_port();
        }
        if self.mcp.upstream_port == 0 {
            self.mcp.upstream_port = default_mcp_upstream_port();
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
    #[serde(default)]
    analyzer_version: Option<u32>,
    profiles: Vec<CompatibilityProfile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McuFirmwareManifest {
    schema_version: u32,
    default: String,
    releases: Vec<McuFirmwareRelease>,
}

/// One selectable PXLogic CH569 MCU firmware image. `firmware_version` is the
/// value the image reports through the device firmware-version register, so it is
/// what the capture helper compares against before deciding to reprogram.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct McuFirmwareRelease {
    id: String,
    label: String,
    firmware_version: String,
    file_name: String,
    byte_length: u64,
    sha256: String,
    pxview_commit: String,
    released: String,
    latest: bool,
    #[serde(default)]
    notes: Option<String>,
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
    logic_sample_rate_hz: Option<u64>,
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
    #[serde(default)]
    pxlogic_usb_speed: Option<String>,
    #[serde(default)]
    pxlogic_logic_mode: Option<u32>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    mode_physical_channels: Option<u8>,
    #[serde(default)]
    effective_sample_rate_hz: Option<u64>,
    #[serde(default)]
    mode_max_sample_rate_hz: Option<u64>,
    #[serde(default)]
    supported: Option<bool>,
    #[serde(default)]
    reason: Option<String>,
    /// Correlates a `timing-marker-result` with the request that asked for it. The
    /// renderer channel is request/response over the same stdin/stderr pair every
    /// other event uses, so the id is what keeps concurrent marker calls apart.
    ///
    /// The rest of a marker result is deliberately not modelled here. It is handed to
    /// the waiting call as the raw JSON object, because its shape belongs to the tool
    /// being served rather than to this event type -- a new marker field should not
    /// need a new field here to survive the trip.
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    ok: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureTelemetry {
    status: String,
    logic_sample_rate_hz: Option<u64>,
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
    pxlogic_usb_speed: Option<String>,
    pxlogic_logic_mode: Option<u32>,
    pxlogic_mode: Option<String>,
    pxlogic_mode_physical_channels: Option<u8>,
    pxlogic_effective_sample_rate_hz: Option<u64>,
    pxlogic_mode_max_sample_rate_hz: Option<u64>,
    pxlogic_supported: Option<bool>,
    pxlogic_reason: Option<String>,
}

impl Default for CaptureTelemetry {
    fn default() -> Self {
        Self {
            status: "idle".to_string(),
            logic_sample_rate_hz: None,
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
            pxlogic_usb_speed: None,
            pxlogic_logic_mode: None,
            pxlogic_mode: None,
            pxlogic_mode_physical_channels: None,
            pxlogic_effective_sample_rate_hz: None,
            pxlogic_mode_max_sample_rate_hz: None,
            pxlogic_supported: None,
            pxlogic_reason: None,
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
    /// Selectable MCU firmware images, newest first. Static for the life of the
    /// build, so the renderer only needs it once.
    firmware_releases: Vec<McuFirmwareRelease>,
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
    /// Firmware the Bridge would program for the current selection. `None` when
    /// the payload could not be validated.
    #[serde(skip_serializing_if = "Option::is_none")]
    firmware_release: Option<McuFirmwareRelease>,
    error: Option<String>,
}

impl PxlogicHardwareState {
    fn failure(error: impl Into<String>) -> Self {
        Self {
            devices: Vec::new(),
            selected_device_id: None,
            firmware_resource_ready: false,
            bitstream_resources_ready: false,
            firmware_release: None,
            error: Some(error.into()),
        }
    }
}

struct ManagedChild {
    token: u64,
    pid: u32,
    child: Child,
    /// Newline-delimited JSON commands to the running session. The Bridge was
    /// launch-arguments-only, which meant a setting like the comparator threshold
    /// could not be changed without restarting Logic 2 and losing the capture.
    control: Option<ChildStdin>,
}

#[derive(Default)]
struct RuntimeState {
    child: Option<ManagedChild>,
    starting: bool,
    /// Set once a stop has been asked for, so the exit that follows is understood as
    /// the answer to that request rather than reported as a crash. Shutting the
    /// session down means closing Logic 2 and the capture helper with it, which does
    /// not always finish inside the grace period, and the kill that ends it looks
    /// exactly like a fault from the outside.
    stop_requested: bool,
}

/// Marker requests waiting for the session to answer them.
///
/// The command goes out on the session's stdin and the answer comes back as a
/// `timing-marker-result` on its stderr, so the two halves meet here rather than in a
/// call stack. A dropped sender means the session died mid-request, which the waiting
/// side reports instead of waiting out its timeout.
#[derive(Default)]
struct RendererRequests {
    next_id: u64,
    pending: BTreeMap<String, tokio::sync::oneshot::Sender<serde_json::Value>>,
}

struct AppState {
    runtime: Mutex<RuntimeState>,
    bridge_state: Mutex<BridgeState>,
    hardware: Mutex<PxlogicHardwareState>,
    capture_telemetry: Mutex<CaptureTelemetry>,
    logs: Mutex<VecDeque<String>>,
    previous_session_logs: Mutex<Vec<String>>,
    next_token: AtomicU64,
    quitting: AtomicBool,
    /// Generation guard for the debounced status-panel move handler.
    panel_move_generation: AtomicU64,
    /// Independent debounce generation for the MCP window.
    mcp_move_generation: AtomicU64,
    /// Logical width of the expanded panel, remembered across a collapse so the
    /// chosen width survives the round trip. 0 means never measured.
    expanded_panel_width: AtomicU32,
    /// Logical height the renderer last measured for the readout. Restoring it on
    /// expand is what keeps the panel from resizing again the instant it opens.
    /// 0 means never measured.
    expanded_panel_height: AtomicU32,
    /// Set while the user is dragging the status panel. `None` means no drag is in
    /// progress and stray move requests are ignored.
    panel_drag: Mutex<Option<PanelDragAnchor>>,
    /// Which edge the panel is resting on, so the layout is only re-flipped when it
    /// actually changes: 0 unknown, 1 anywhere else, 2 the bottom of the display.
    panel_dock: AtomicU8,
    /// What the MCP proxy bound, once it has bound. `None` until then, and while it is
    /// unavailable, which the window has to distinguish from "bound but idle".
    mcp: Mutex<Option<McpRuntimeState>>,
    renderer_requests: Mutex<RendererRequests>,
    /// A bounded activity feed plus the tools most recently advertised by Logic 2.
    /// This belongs to the app, not a Bridge capture session, for real Saleae users too.
    mcp_activity: Mutex<McpActivityStore>,
    mcp_approvals: Mutex<McpApprovalStore>,
    /// Prevent normal activity from repeatedly reopening a window the user hid.
    mcp_auto_shown: AtomicBool,
    /// Tool names Logic 2 advertised in its own `tools/list`, before ours were merged in.
    ///
    /// Kept apart from `mcp_activity`'s catalogue, which holds the merged list the window
    /// displays and so cannot answer "is this name Logic 2's?". A local tool yields to an
    /// upstream one of the same name, which is only decidable with this.
    mcp_upstream_tools: Mutex<HashSet<String>>,
}

/// The proxy's live state, as the window needs to see it.
#[derive(Clone, Debug)]
struct McpRuntimeState {
    ports: mcp_proxy::BoundPorts,
}

const MAX_MCP_ACTIVITIES: usize = 200;

type McpPendingKey = (Option<String>, String);

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct McpToolDefinition {
    name: String,
    description: Option<String>,
    input_schema: serde_json::Value,
    /// Preserve fields introduced by newer MCP/Logic 2 versions instead of reducing
    /// the real tool catalogue to the subset this client happens to understand.
    raw: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct McpActivity {
    sequence: u64,
    observed_at_ms: u64,
    session_id: Option<String>,
    request_id: Option<serde_json::Value>,
    direction: String,
    method: String,
    tool: Option<String>,
    arguments: Option<serde_json::Value>,
    params: Option<serde_json::Value>,
    state: String,
    response: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct McpApprovalRequest {
    approval_id: u64,
    session_id: Option<String>,
    request_id: serde_json::Value,
    tool: String,
    arguments: serde_json::Value,
    reason: String,
    created_at_ms: u64,
    expires_at_ms: u64,
}

struct PendingMcpApproval {
    request: McpApprovalRequest,
    decision: tokio::sync::oneshot::Sender<mcp_proxy::Verdict>,
}

#[derive(Default)]
struct McpApprovalStore {
    next_id: u64,
    pending: BTreeMap<u64, PendingMcpApproval>,
    session_allowed: BTreeMap<String, HashSet<String>>,
}

impl McpApprovalStore {
    fn is_session_allowed(&self, context: &mcp_proxy::ObservationContext, tool: &str) -> bool {
        context
            .session_id
            .as_ref()
            .and_then(|session| self.session_allowed.get(session))
            .is_some_and(|tools| tools.contains(tool))
    }

    fn create(
        &mut self,
        context: &mcp_proxy::ObservationContext,
        call: &mcp_proxy::ToolCall,
        reason: String,
        now: u64,
    ) -> (
        McpApprovalRequest,
        tokio::sync::oneshot::Receiver<mcp_proxy::Verdict>,
    ) {
        self.next_id = self.next_id.saturating_add(1);
        let request = McpApprovalRequest {
            approval_id: self.next_id,
            session_id: context.session_id.clone(),
            request_id: call.id.clone(),
            tool: call.tool.clone(),
            arguments: call.arguments.clone(),
            reason,
            created_at_ms: now,
            expires_at_ms: now.saturating_add(30_000),
        };
        let (decision, receiver) = tokio::sync::oneshot::channel();
        self.pending.insert(
            request.approval_id,
            PendingMcpApproval {
                request: request.clone(),
                decision,
            },
        );
        (request, receiver)
    }

    fn resolve(
        &mut self,
        approval_id: u64,
        allow: bool,
        remember: bool,
    ) -> Option<McpApprovalRequest> {
        let pending = self.pending.remove(&approval_id)?;
        if allow && remember {
            if let Some(session) = pending.request.session_id.as_ref() {
                self.session_allowed
                    .entry(session.clone())
                    .or_default()
                    .insert(pending.request.tool.clone());
            }
        }
        let verdict = if allow {
            mcp_proxy::Verdict::Allow
        } else {
            mcp_proxy::Verdict::Deny("用户拒绝了 MCP 工具调用".to_string())
        };
        let _ = pending.decision.send(verdict);
        Some(pending.request)
    }

    fn expire(&mut self, approval_id: u64) -> Option<McpApprovalRequest> {
        self.pending
            .remove(&approval_id)
            .map(|pending| pending.request)
    }

    fn close_session(&mut self, session_id: &str) {
        self.session_allowed.remove(session_id);
        let ids: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.request.session_id.as_deref() == Some(session_id))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(pending) = self.pending.remove(&id) {
                let _ = pending.decision.send(mcp_proxy::Verdict::Deny(
                    "MCP 会话已结束，工具调用已拒绝".to_string(),
                ));
            }
        }
    }

    fn pending_requests(&self) -> Vec<McpApprovalRequest> {
        self.pending
            .values()
            .map(|pending| pending.request.clone())
            .collect()
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct McpApprovalResolution {
    approval_id: u64,
    outcome: String,
}

enum McpToolPolicy {
    Allow,
    Review(String),
}

fn mcp_tool_policy(tool: &str) -> McpToolPolicy {
    // Names below are the tools exposed by the current Logic 2 MCP integration. The
    // catalogue captured from tools/list remains the source of truth shown in the UI;
    // this deliberately small allow-list cannot accidentally bless a new tool.
    match tool {
        // Read-only inspection and export/save operations do not replace the current
        // capture in Logic 2.
        "get_devices"
        | "wait_capture"
        | "export_data_table_csv"
        | "export_raw_data_binary"
        | "export_raw_data_csv"
        | "legacy_export_analyzer"
        | "save_capture" => McpToolPolicy::Allow,
        // Analyzer changes are explicitly permitted configuration operations.
        "add_analyzer"
        | "remove_analyzer"
        | "add_high_level_analyzer"
        | "remove_high_level_analyzer" => McpToolPolicy::Allow,
        // Timing markers are annotations on the capture, not the capture itself. Nothing
        // here can lose sample data, and being asked to confirm every note an agent
        // writes down would make the feature not worth having.
        "add_timing_marker"
        | "add_timing_marker_pair"
        | "list_timing_markers"
        | "set_timing_marker_note"
        | "remove_timing_marker" => McpToolPolicy::Allow,
        "start_capture" => {
            McpToolPolicy::Review("启动新采集可能替换或改变当前采集状态".to_string())
        }
        "load_capture" => {
            McpToolPolicy::Review("加载采集会切换 Logic 2 当前显示的数据".to_string())
        }
        "stop_capture" => McpToolPolicy::Review("停止采集可能截断尚在进行的数据".to_string()),
        "close_capture" => McpToolPolicy::Review("关闭采集可能丢失尚未保存的数据".to_string()),
        _ => McpToolPolicy::Review("这是尚未分类的 Logic 2 MCP 工具，默认需要确认".to_string()),
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct McpActivitySnapshot {
    activities: Vec<McpActivity>,
    tools: Vec<McpToolDefinition>,
    approvals: Vec<McpApprovalRequest>,
}

#[derive(Default)]
struct McpActivityStore {
    next_sequence: u64,
    activities: VecDeque<McpActivity>,
    pending: BTreeMap<McpPendingKey, u64>,
    tools: BTreeMap<String, McpToolDefinition>,
}

struct McpActivityUpdate {
    activity: Option<McpActivity>,
    tools: Option<Vec<McpToolDefinition>>,
}

impl McpActivityStore {
    fn snapshot(&self) -> McpActivitySnapshot {
        McpActivitySnapshot {
            activities: self.activities.iter().cloned().collect(),
            tools: self.tools.values().cloned().collect(),
            approvals: Vec::new(),
        }
    }

    fn next_sequence(&mut self) -> u64 {
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.next_sequence
    }

    fn push(&mut self, activity: McpActivity) {
        self.activities.push_back(activity);
        while self.activities.len() > MAX_MCP_ACTIVITIES {
            if let Some(removed) = self.activities.pop_front() {
                self.pending
                    .retain(|_, sequence| *sequence != removed.sequence);
            }
        }
    }

    fn record_request(
        &mut self,
        context: &mcp_proxy::ObservationContext,
        body: &[u8],
        observed_at_ms: u64,
    ) -> Option<McpActivity> {
        let value: serde_json::Value = serde_json::from_slice(body).ok()?;
        let method = value.get("method")?.as_str()?.to_owned();
        let request_id = value.get("id").filter(|id| !id.is_null()).cloned();
        let params = value.get("params").cloned();
        let tool = (method == "tools/call")
            .then(|| params.as_ref()?.get("name")?.as_str().map(str::to_owned))
            .flatten();
        let arguments = (method == "tools/call")
            .then(|| params.as_ref()?.get("arguments").cloned())
            .flatten();
        let sequence = self.next_sequence();
        let activity = McpActivity {
            sequence,
            observed_at_ms,
            session_id: context.session_id.clone(),
            request_id: request_id.clone(),
            direction: "client".to_string(),
            method,
            tool,
            arguments,
            params,
            state: if request_id.is_some() {
                "pending".to_string()
            } else {
                "notification".to_string()
            },
            response: None,
        };
        if let Some(id) = request_id.as_ref() {
            if let Some(key) = mcp_pending_key(context, id) {
                self.pending.insert(key, sequence);
            }
        }
        self.push(activity.clone());
        Some(activity)
    }

    fn record_response(
        &mut self,
        context: &mcp_proxy::ObservationContext,
        body: &[u8],
        observed_at_ms: u64,
    ) -> McpActivityUpdate {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
            return McpActivityUpdate {
                activity: None,
                tools: None,
            };
        };

        // Server notifications and requests carry a method of their own rather than
        // answering an earlier client id.
        if let Some(method) = value.get("method").and_then(serde_json::Value::as_str) {
            let sequence = self.next_sequence();
            let activity = McpActivity {
                sequence,
                observed_at_ms,
                session_id: context.session_id.clone(),
                request_id: value.get("id").cloned(),
                direction: "server".to_string(),
                method: method.to_owned(),
                tool: None,
                arguments: None,
                params: value.get("params").cloned(),
                state: "notification".to_string(),
                response: None,
            };
            self.push(activity.clone());
            return McpActivityUpdate {
                activity: Some(activity),
                tools: None,
            };
        }

        let Some(id) = value.get("id").filter(|id| !id.is_null()).cloned() else {
            return McpActivityUpdate {
                activity: None,
                tools: None,
            };
        };
        let id_text = serde_json::to_string(&id).ok();
        let exact_key = id_text
            .as_ref()
            .map(|id| (context.session_id.clone(), id.clone()));
        let mut sequence = exact_key.as_ref().and_then(|key| self.pending.remove(key));
        if sequence.is_none() {
            // Initialization can acquire its session id in the response, while its
            // request had none. Pair by id only when that is unambiguous.
            if let Some(id_text) = id_text.as_ref() {
                let matches: Vec<_> = self
                    .pending
                    .keys()
                    .filter(|(_, pending_id)| pending_id == id_text)
                    .cloned()
                    .collect();
                if matches.len() == 1 {
                    sequence = self.pending.remove(&matches[0]);
                }
            }
        }

        let mut tools_update = None;
        if let Some(sequence) = sequence {
            if let Some(activity) = self
                .activities
                .iter_mut()
                .find(|activity| activity.sequence == sequence)
            {
                activity.observed_at_ms = observed_at_ms;
                if activity.session_id.is_none() {
                    activity.session_id.clone_from(&context.session_id);
                }
                activity.state = if value.get("error").is_some() {
                    "error".to_string()
                } else {
                    "completed".to_string()
                };
                activity.response = Some(value.clone());
                if activity.method == "tools/list" {
                    if let Some(listed) = parse_mcp_tools(&value) {
                        let is_first_page = activity
                            .params
                            .as_ref()
                            .and_then(|params| params.get("cursor"))
                            .is_none_or(serde_json::Value::is_null);
                        if is_first_page {
                            self.tools.clear();
                        }
                        for tool in listed {
                            self.tools.insert(tool.name.clone(), tool);
                        }
                        tools_update = Some(self.tools.values().cloned().collect());
                    }
                }
                return McpActivityUpdate {
                    activity: Some(activity.clone()),
                    tools: tools_update,
                };
            }
        }

        let sequence = self.next_sequence();
        let activity = McpActivity {
            sequence,
            observed_at_ms,
            session_id: context.session_id.clone(),
            request_id: Some(id),
            direction: "server".to_string(),
            method: "response".to_string(),
            tool: None,
            arguments: None,
            params: None,
            state: if value.get("error").is_some() {
                "error".to_string()
            } else {
                "completed".to_string()
            },
            response: Some(value),
        };
        self.push(activity.clone());
        McpActivityUpdate {
            activity: Some(activity),
            tools: None,
        }
    }

    fn close_session(&mut self, session_id: &str, observed_at_ms: u64) -> Vec<McpActivity> {
        let sequences: Vec<u64> = self
            .pending
            .iter()
            .filter(|((session, _), _)| session.as_deref() == Some(session_id))
            .map(|(_, sequence)| *sequence)
            .collect();
        self.pending
            .retain(|(session, _), _| session.as_deref() != Some(session_id));
        let mut changed = Vec::new();
        for sequence in sequences {
            if let Some(activity) = self
                .activities
                .iter_mut()
                .find(|activity| activity.sequence == sequence)
            {
                activity.observed_at_ms = observed_at_ms;
                activity.state = "sessionClosed".to_string();
                changed.push(activity.clone());
            }
        }
        changed
    }
}

fn mcp_pending_key(
    context: &mcp_proxy::ObservationContext,
    id: &serde_json::Value,
) -> Option<McpPendingKey> {
    serde_json::to_string(id)
        .ok()
        .map(|id| (context.session_id.clone(), id))
}

fn parse_mcp_tools(response: &serde_json::Value) -> Option<Vec<McpToolDefinition>> {
    let tools = response.get("result")?.get("tools")?.as_array()?;
    Some(
        tools
            .iter()
            .filter_map(|raw| {
                Some(McpToolDefinition {
                    name: raw.get("name")?.as_str()?.to_owned(),
                    description: raw
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    input_schema: raw
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                    raw: raw.clone(),
                })
            })
            .collect(),
    )
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Tells the panel which way round to lay itself out.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusPanelDock {
    bottom: bool,
}

/// Where a panel drag started, in physical desktop pixels.
///
/// The panel is moved by the cursor's own displacement from this point rather
/// than by coordinates handed over from the renderer: the Bridge reads the live
/// cursor on every step, so a coalesced, dropped or out-of-order move event
/// cannot accumulate drift.
#[derive(Clone, Debug)]
struct PanelDragAnchor {
    window_label: String,
    window_x: i32,
    window_y: i32,
    cursor_x: f64,
    cursor_y: f64,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            runtime: Mutex::new(RuntimeState::default()),
            bridge_state: Mutex::new(BridgeState::default()),
            hardware: Mutex::new(PxlogicHardwareState::failure("尚未扫描 PXLogic 设备")),
            capture_telemetry: Mutex::new(CaptureTelemetry::default()),
            logs: Mutex::new(VecDeque::new()),
            previous_session_logs: Mutex::new(Vec::new()),
            next_token: AtomicU64::new(1),
            quitting: AtomicBool::new(false),
            panel_move_generation: AtomicU64::new(0),
            mcp_move_generation: AtomicU64::new(0),
            expanded_panel_width: AtomicU32::new(0),
            expanded_panel_height: AtomicU32::new(0),
            panel_drag: Mutex::new(None),
            panel_dock: AtomicU8::new(0),
            mcp: Mutex::new(None),
            renderer_requests: Mutex::new(RendererRequests::default()),
            mcp_activity: Mutex::new(McpActivityStore::default()),
            mcp_approvals: Mutex::new(McpApprovalStore::default()),
            mcp_auto_shown: AtomicBool::new(false),
            mcp_upstream_tools: Mutex::new(HashSet::new()),
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
    // Every settings write funnels through here, whichever window made it, so this is
    // the one place that can keep the two in step. Without it each window reads the
    // threshold once at load and never hears about the other's change -- and the stale
    // copy in the main window's form would overwrite a panel edit on its next save.
    let _ = app.emit(
        "pxlogic-threshold",
        PxlogicThresholdChange {
            volts: settings.pxlogic_threshold_volts,
        },
    );
    Ok(settings)
}

/// Broadcast so both windows show the comparator threshold actually in force.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PxlogicThresholdChange {
    volts: f64,
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

/// Selectable PXLogic MCU firmware images, embedded from
/// `resources/firmware/releases.json` so the picker can be populated even
/// before the payload on disk has been validated.
fn mcu_firmware_manifest() -> Result<McuFirmwareManifest, String> {
    let manifest: McuFirmwareManifest = serde_json::from_str(include_str!(
        "../../../../../resources/firmware/releases.json"
    ))
    .map_err(|error| format!("PXLogic 固件清单无效: {error}"))?;
    if manifest.schema_version != 1 {
        return Err(format!(
            "不支持的 PXLogic 固件清单版本: {}",
            manifest.schema_version
        ));
    }
    if manifest.releases.is_empty() {
        return Err("PXLogic 固件清单为空".to_string());
    }
    if manifest
        .releases
        .iter()
        .filter(|entry| entry.latest)
        .count()
        != 1
    {
        return Err("PXLogic 固件清单必须恰好标记一个 latest 版本".to_string());
    }
    let default_entry = manifest
        .releases
        .iter()
        .find(|entry| entry.id == manifest.default)
        .ok_or_else(|| format!("PXLogic 固件清单的默认版本不存在: {}", manifest.default))?;
    if !default_entry.latest {
        return Err("PXLogic 固件清单的默认版本必须是 latest 版本".to_string());
    }
    Ok(manifest)
}

fn mcu_firmware_releases() -> Vec<McuFirmwareRelease> {
    mcu_firmware_manifest()
        .map(|manifest| manifest.releases)
        .unwrap_or_default()
}

/// The newest shipped firmware. New installs and any unrecognised selection land
/// here, so a user who never touches the picker always runs the latest image.
fn default_pxlogic_firmware_id() -> String {
    mcu_firmware_manifest()
        .map(|manifest| manifest.default)
        .unwrap_or_default()
}

fn find_mcu_firmware_release(id: &str) -> Option<McuFirmwareRelease> {
    mcu_firmware_manifest()
        .ok()?
        .releases
        .into_iter()
        .find(|entry| entry.id == id)
}

/// Resolves the firmware the Bridge should program, falling back to the latest
/// image when the stored selection is unknown.
fn selected_mcu_firmware(app: &AppHandle) -> Result<McuFirmwareRelease, String> {
    let manifest = mcu_firmware_manifest()?;
    let requested = load_settings(app).pxlogic_firmware_id;
    manifest
        .releases
        .iter()
        .find(|entry| entry.id == requested)
        .or_else(|| {
            manifest
                .releases
                .iter()
                .find(|entry| entry.id == manifest.default)
        })
        .cloned()
        .ok_or_else(|| "PXLogic 固件清单中没有可用版本".to_string())
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

fn requires_pending_profile_authorization(inspection: &LogicInspection) -> bool {
    !inspection.supported
        && inspection.runnable
        && matches!(
            inspection.hook_status.as_deref(),
            Some("pending-live-validation" | "candidate" | "locally-verified")
        )
}

fn has_pending_profile_authorization(
    inspection: &LogicInspection,
    fingerprint: Option<&str>,
) -> bool {
    requires_pending_profile_authorization(inspection)
        && matches!(
            (inspection.graph_sha256.as_deref(), fingerprint),
            (Some(actual), Some(authorized)) if actual.eq_ignore_ascii_case(authorized)
        )
}

fn validate_bridge_start_compatibility(
    app: &AppHandle,
    settings: &ClientSettings,
) -> Result<LogicInspection, String> {
    let inspection = inspect_logic_selection(Some(app), Path::new(&settings.logic_app_path), false);
    if !inspection.runnable {
        return Err(inspection
            .error
            .unwrap_or_else(|| "Logic 2 安装无效".to_string()));
    }
    if requires_pending_profile_authorization(&inspection)
        && !has_pending_profile_authorization(
            &inspection,
            settings.pending_profile_fingerprint.as_deref(),
        )
    {
        return Err(format!(
            "Logic 2 profile {} 仍处于实验验证状态；请在启动前明确确认实验性采集风险",
            inspection.profile_id.as_deref().unwrap_or("unknown")
        ));
    }
    Ok(inspection)
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

fn is_logic_bundle(path: &Path) -> bool {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        if !path.is_dir() {
            return false;
        }
        let plist_path = path.join("Contents/Info.plist");
        let Ok(plist) = plist::Value::from_file(plist_path) else {
            return false;
        };
        plist
            .as_dictionary()
            .and_then(|dictionary| dictionary.get("CFBundleIdentifier"))
            .and_then(plist::Value::as_string)
            .is_some_and(|identifier| identifier == "com.saleae.saleae")
    }
}

fn logic_app_candidates_from_path(path: &Path) -> Vec<PathBuf> {
    if is_logic_bundle(path) {
        return vec![path.to_path_buf()];
    }
    if !path.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };
    let mut candidates = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|child| {
            child.is_dir()
                && child
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
                && is_logic_bundle(child)
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates
}

fn paths_for_logic_scan(path: &Path) -> Vec<PathBuf> {
    let candidates = logic_app_candidates_from_path(path);
    if candidates.is_empty() {
        vec![path.to_path_buf()]
    } else {
        candidates
    }
}

fn inspect_logic_selection(
    app: Option<&AppHandle>,
    selected_path: &Path,
    force_offline_analysis: bool,
) -> LogicInspection {
    let mut inspections = paths_for_logic_scan(selected_path)
        .into_iter()
        .map(|path| inspect_logic_path(app, &path, force_offline_analysis))
        .collect::<Vec<_>>();
    inspections.sort_by_key(|inspection| {
        (
            !inspection.runnable,
            !inspection.supported,
            inspection.path.clone(),
        )
    });
    inspections
        .into_iter()
        .next()
        .unwrap_or_else(|| inspect_logic_path(app, selected_path, force_offline_analysis))
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
        for candidate in paths_for_logic_scan(&candidate) {
            let key = candidate.to_string_lossy().into_owned();
            if !seen.insert(key) || !candidate.exists() {
                continue;
            }
            applications.push(inspect_logic_path(Some(app), &candidate, false));
        }
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
    firmware_release: McuFirmwareRelease,
}

const BRIDGE_NODE_RUNTIME_FILES: &[&str] = &[
    "index.cjs",
    "lib/capture-controller.cjs",
    "lib/compatibility.cjs",
    "lib/diagnostics.cjs",
    "lib/graph-action-guard.cjs",
    "lib/logic-format.cjs",
    "lib/macos-hook-locator.cjs",
    "lib/offline-compatibility.cjs",
    "lib/websocket-proxy.cjs",
    "lib/windows-hook-locator.cjs",
    "compatibility/profiles.json",
];

fn bridge_payload_required_paths(
    root: &Path,
    native_host: &Path,
    helper: &Path,
    bitstreams: &Path,
    firmware_dir: &Path,
) -> Vec<PathBuf> {
    let mut required = BRIDGE_NODE_RUNTIME_FILES
        .iter()
        .map(|relative| root.join(relative))
        .collect::<Vec<_>>();
    required.extend([
        native_host.to_path_buf(),
        helper.to_path_buf(),
        bitstreams.join("hspi_ddr.bin"),
        bitstreams.join("hspi_ddr_RST.bin"),
        firmware_dir.join("releases.json"),
    ]);
    // Every selectable image must ship, otherwise the firmware picker can offer a
    // version the payload cannot actually program.
    required.extend(
        mcu_firmware_releases()
            .iter()
            .map(|release| firmware_dir.join(&release.file_name)),
    );
    required
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
    let firmware_dir = payload_root.join("resources/firmware");
    let required =
        bridge_payload_required_paths(&root, &native_host, &helper, &bitstreams, &firmware_dir);
    for path in required {
        if !path.is_file() {
            return Err(format!(
                "Bridge 便携包不完整，缺少: {}。请解压并运行完整的平台便携包，不能直接运行 target\\...\\release 下的构建中间文件。",
                path.display()
            ));
        }
    }
    let release = selected_mcu_firmware(app)?;
    let firmware = firmware_dir.join(&release.file_name);
    verify_mcu_firmware_image(&firmware, &release)?;
    Ok(BridgePayload {
        bridge_root: root,
        helper,
        bitstreams,
        firmware,
        firmware_release: release,
    })
}

/// Rejects a firmware image that does not match the manifest before it can reach
/// the device. Programming the MCU resets it and is not reversible from the
/// Bridge, so a truncated or substituted image must fail here rather than at
/// flash time.
fn verify_mcu_firmware_image(path: &Path, release: &McuFirmwareRelease) -> Result<(), String> {
    let image = fs::read(path)
        .map_err(|error| format!("无法读取 PXLogic 固件 {}: {error}", path.display()))?;
    if image.len() as u64 != release.byte_length {
        return Err(format!(
            "PXLogic 固件 {} 长度为 {} 字节，清单要求 {} 字节；请重新解压便携包。",
            release.file_name,
            image.len(),
            release.byte_length
        ));
    }
    let digest = format!("{:x}", Sha256::digest(&image));
    if !digest.eq_ignore_ascii_case(&release.sha256) {
        return Err(format!(
            "PXLogic 固件 {} 校验失败：SHA-256 为 {digest}，清单要求 {}；请重新解压便携包。",
            release.file_name, release.sha256
        ));
    }
    Ok(())
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
        firmware_release: Some(payload.firmware_release),
        error: None,
    }
}

fn update_bridge_state(app: &AppHandle, next: BridgeState) {
    if let Ok(mut current) = app.state::<AppState>().bridge_state.lock() {
        *current = next.clone();
    }
    let phase = next.phase.clone();
    let _ = app.emit("bridge-state", next);
    maybe_auto_show_status_panel(app, &phase);
}

/// The panel is worth revealing exactly once per run: when the Bridge is live
/// and the user is about to lose sight of the main window behind Logic 2.
fn should_auto_show_status_panel(phase: &str, auto_show: bool, visible: bool) -> bool {
    phase == "running" && auto_show && !visible
}

fn maybe_auto_show_status_panel(app: &AppHandle, phase: &str) {
    // Cheap guard first: this runs on every state change, while the rest reads
    // the config file and hops to the main thread.
    if phase != "running" {
        return;
    }
    let auto_show = load_settings(app).status_panel.auto_show;
    let phase = phase.to_string();
    let app = app.clone();
    let dispatch = app.clone();
    // update_bridge_state runs on the Bridge stdout reader thread, and window
    // geometry must be touched on the main thread.
    let _ = dispatch.run_on_main_thread(move || {
        let Some(window) = app.get_webview_window("status") else {
            return;
        };
        let visible = window.is_visible().unwrap_or(false);
        if !should_auto_show_status_panel(&phase, auto_show, visible) {
            return;
        }
        show_status_panel_without_activating(&app);
    });
}

fn parse_bridge_runtime_event(line: &str) -> Option<BridgeRuntimeEvent> {
    const PREFIX: &str = "[logic2-bridge:event] ";
    serde_json::from_str(line.strip_prefix(PREFIX)?).ok()
}

/// A renderer call that outlives no session: if the Bridge stops, the wait ends.
const RENDERER_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Hands a marker result to whoever asked for it.
///
/// Unknown ids are dropped rather than logged as faults: a request that already timed
/// out has no receiver left, and the late answer is not news.
fn resolve_renderer_request(app: &AppHandle, request_id: &str, payload: serde_json::Value) {
    let sender = app
        .state::<AppState>()
        .renderer_requests
        .lock()
        .ok()
        .and_then(|mut requests| requests.pending.remove(request_id));
    if let Some(sender) = sender {
        let _ = sender.send(payload);
    }
}

/// Fails every waiting marker request, used when the session goes away.
fn abandon_renderer_requests(app: &AppHandle) {
    if let Ok(mut requests) = app.state::<AppState>().renderer_requests.lock() {
        requests.pending.clear();
    }
}

/// Sends one renderer command and waits for the matching result.
///
/// The timeout is the session's, not the agent's: a Bridge that never answers must not
/// leave a tool call open, because the MCP client behind it has its own patience and a
/// hung tool looks like a hung Logic 2.
async fn call_renderer(
    app: &AppHandle,
    command_type: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let (request_id, receiver) = {
        let state = app.state::<AppState>();
        let mut requests = state
            .renderer_requests
            .lock()
            .map_err(|_| "渲染通道状态不可用".to_string())?;
        requests.next_id += 1;
        let request_id = format!("mk{}", requests.next_id);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        requests.pending.insert(request_id.clone(), sender);
        (request_id, receiver)
    };

    let mut payload = arguments.clone();
    payload.insert(
        "type".to_string(),
        serde_json::Value::String(command_type.to_string()),
    );
    payload.insert(
        "requestId".to_string(),
        serde_json::Value::String(request_id.clone()),
    );
    let line = serde_json::Value::Object(payload).to_string();

    if let Err(error) = send_bridge_control(app, &line) {
        if let Ok(mut requests) = app.state::<AppState>().renderer_requests.lock() {
            requests.pending.remove(&request_id);
        }
        return Err(error);
    }

    match tokio::time::timeout(RENDERER_REQUEST_TIMEOUT, receiver).await {
        Ok(Ok(payload)) => Ok(payload),
        Ok(Err(_)) => Err("Bridge 会话已结束，标记请求未完成".to_string()),
        Err(_) => {
            if let Ok(mut requests) = app.state::<AppState>().renderer_requests.lock() {
                requests.pending.remove(&request_id);
            }
            Err(format!(
                "Bridge 未在 {} 秒内回应标记请求",
                RENDERER_REQUEST_TIMEOUT.as_secs()
            ))
        }
    }
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
        "capture-plan" => {
            if telemetry.status == "idle" {
                telemetry.status = "configured".to_string();
            }
            telemetry.logic_sample_rate_hz = event
                .logic_sample_rate_hz
                .or(telemetry.logic_sample_rate_hz);
            telemetry.sample_rate_hz = event
                .effective_sample_rate_hz
                .or(event.sample_rate_hz)
                .or(telemetry.sample_rate_hz);
            if let Some(channels) = event.enabled_channels.as_ref() {
                telemetry.enabled_channels.clone_from(channels);
            }
            telemetry.channel_span = event.channel_span.or(telemetry.channel_span);
            apply_pxlogic_plan(telemetry, event);
        }
        "capture-starting" => {
            *telemetry = CaptureTelemetry {
                status: "starting".to_string(),
                logic_sample_rate_hz: event.logic_sample_rate_hz.or(event.sample_rate_hz),
                sample_rate_hz: event.effective_sample_rate_hz.or(event.sample_rate_hz),
                enabled_channels: event.enabled_channels.clone().unwrap_or_default(),
                channel_span: event.channel_span,
                threshold_volts: event.threshold_volts,
                trigger_description: event.trigger_description.clone(),
                ..CaptureTelemetry::default()
            };
            apply_pxlogic_plan(telemetry, event);
        }
        "capture-started" => {
            telemetry.status = "streaming".to_string();
            telemetry.logic_sample_rate_hz = event
                .logic_sample_rate_hz
                .or(telemetry.logic_sample_rate_hz);
            telemetry.sample_rate_hz = event
                .effective_sample_rate_hz
                .or(event.sample_rate_hz)
                .or(telemetry.sample_rate_hz);
            if let Some(channels) = event.enabled_channels.as_ref() {
                telemetry.enabled_channels.clone_from(channels);
            }
            telemetry.channel_span = event.channel_span.or(telemetry.channel_span);
            telemetry.threshold_volts = event.threshold_volts.or(telemetry.threshold_volts);
            if let Some(trigger) = event.trigger_description.as_ref() {
                telemetry.trigger_description = Some(trigger.clone());
            }
            apply_pxlogic_plan(telemetry, event);
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
        "hardware-threshold" => {
            telemetry.threshold_volts = event.threshold_volts.or(telemetry.threshold_volts);
        }
        "capture-unavailable" => telemetry.status = "error".to_string(),
        _ => return false,
    }
    true
}

fn apply_pxlogic_plan(telemetry: &mut CaptureTelemetry, event: &BridgeRuntimeEvent) {
    if let Some(value) = event.pxlogic_usb_speed.as_ref() {
        telemetry.pxlogic_usb_speed = Some(value.clone());
    }
    telemetry.pxlogic_logic_mode = event.pxlogic_logic_mode.or(telemetry.pxlogic_logic_mode);
    if let Some(value) = event.mode.as_ref() {
        telemetry.pxlogic_mode = Some(value.clone());
    }
    telemetry.pxlogic_mode_physical_channels = event
        .mode_physical_channels
        .or(telemetry.pxlogic_mode_physical_channels);
    telemetry.pxlogic_effective_sample_rate_hz = event
        .effective_sample_rate_hz
        .or(telemetry.pxlogic_effective_sample_rate_hz);
    telemetry.pxlogic_mode_max_sample_rate_hz = event
        .mode_max_sample_rate_hz
        .or(telemetry.pxlogic_mode_max_sample_rate_hz);
    telemetry.pxlogic_supported = event.supported.or(telemetry.pxlogic_supported);
    // `supported` is what marks an event as carrying a verdict, so the reason that
    // came with it is the current truth even when it is absent. Keeping a reason
    // until another one replaced it left the panel warning about a combination that
    // no longer existed: Logic 2 lowers the sample rate by itself once enough
    // channels are enabled for the hardware to refuse the request, and it sends the
    // channel change first, so the rejected plan is always followed a moment later by
    // one that works.
    if event.supported.is_some() {
        telemetry.pxlogic_reason.clone_from(&event.reason);
    }
}

/// Sends one newline-delimited JSON command to the running session.
///
/// Returns an error when no session is running rather than silently succeeding: the
/// caller is changing a hardware setting and has to know it did not land.
fn send_bridge_control(app: &AppHandle, command: &str) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut runtime = state
        .runtime
        .lock()
        .map_err(|_| "Bridge 运行状态不可用".to_string())?;
    let child = runtime
        .child
        .as_mut()
        .ok_or_else(|| "Bridge 未在运行".to_string())?;
    let control = child
        .control
        .as_mut()
        .ok_or_else(|| "该 Bridge 会话不支持在线修改".to_string())?;
    control
        .write_all(format!("{command}\n").as_bytes())
        .and_then(|()| control.flush())
        .map_err(|error| format!("无法发送指令到 Bridge: {error}"))
}

/// Retunes the hardware comparator without restarting Logic 2.
///
/// The threshold used to be a launch argument only, so correcting it meant closing
/// Logic 2 and losing the capture in it -- for a value that can only really be judged
/// by looking at the decoded result. The helper receives it per capture, so a change
/// between captures needs no register write and takes effect on the next one; that is
/// also why this refuses while a capture is running rather than half-applying.
#[tauri::command]
fn status_panel_set_threshold(app: AppHandle, volts: f64) -> Result<f64, String> {
    if !volts.is_finite() || !(0.0..=MAX_PXLOGIC_THRESHOLD_VOLTS).contains(&volts) {
        return Err(format!(
            "阈值需在 0 至 {MAX_PXLOGIC_THRESHOLD_VOLTS} V 之间"
        ));
    }
    let capturing = app
        .state::<AppState>()
        .capture_telemetry
        .lock()
        .ok()
        .is_some_and(|telemetry| matches!(telemetry.status.as_str(), "starting" | "streaming"));
    if capturing {
        return Err("采集进行中，请先在 Logic 2 里停止采集".to_string());
    }
    send_bridge_control(
        &app,
        &format!(r#"{{"type":"set-hardware-threshold","volts":{volts}}}"#),
    )?;
    let mut settings = load_settings(&app);
    settings.pxlogic_threshold_volts = volts;
    // The stored profile was verified against the old value, so its verification no
    // longer says anything about this one.
    if let Some(profile) = settings
        .pxlogic_threshold_profiles
        .get_mut(&settings.pxlogic_device_id.clone())
    {
        profile.volts = volts;
        profile.verified = false;
    }
    store_settings(&app, settings)?;
    Ok(volts)
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
        // A marker result is an answer, not a state change: it goes to the request that
        // is waiting for it and nothing else looks at it.
        if event.event_type == "timing-marker-result" {
            if let Some(request_id) = event.request_id.clone() {
                let payload = serde_json::from_str::<serde_json::Value>(
                    line.strip_prefix("[logic2-bridge:event] ").unwrap_or("{}"),
                )
                .unwrap_or(serde_json::Value::Null);
                resolve_renderer_request(app, &request_id, payload);
            }
            return;
        }
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
                // The renderer channel died with the session, so anything still waiting
                // on it is answered now rather than at its own timeout.
                abandon_renderer_requests(&app);
                let quitting = app.state::<AppState>().quitting.load(Ordering::Acquire)
                    || app
                        .state::<AppState>()
                        .runtime
                        .lock()
                        .map(|runtime| runtime.stop_requested)
                        .unwrap_or(false);
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

/// How long a session gets to shut down before it is killed.
///
/// Its own shutdown closes the Logic 2 window, stops the capture helper and ends the
/// native GraphServer host, and three seconds was not enough for that: the kill
/// landed mid-cleanup, which is how orphaned hosts and windows are left behind.
const BRIDGE_STOP_GRACE: Duration = Duration::from_secs(10);

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
    if let Ok(mut runtime) = app.state::<AppState>().runtime.lock() {
        runtime.stop_requested = true;
    }
    terminate_process(pid, false)
        .map_err(|error| format!("无法停止 Bridge 进程 {pid}: {error}"))?;

    let app_for_timeout = app.clone();
    thread::spawn(move || {
        thread::sleep(BRIDGE_STOP_GRACE);
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

/// Inset used when the panel has no remembered position, and the distance at
/// which a dragged panel snaps to a work-area edge.
const STATUS_PANEL_EDGE_MARGIN: i32 = 24;
const STATUS_PANEL_SNAP_THRESHOLD: i32 = 16;
/// How much of the panel must stay on some work area for a remembered position
/// to be considered reachable.
const STATUS_PANEL_MIN_VISIBLE: i32 = 32;
/// How long the panel must hold still before a drag is treated as finished.
const STATUS_PANEL_SETTLE_MS: u64 = 150;
/// Logical inner sizes for the two panel shapes. The window config floor is the
/// collapsed size, so the expanded minimum has to be re-applied in code whenever
/// the panel expands.
const STATUS_PANEL_MIN_WIDTH: f64 = 300.0;
const STATUS_PANEL_COLLAPSED_WIDTH: f64 = 168.0;
const STATUS_PANEL_COLLAPSED_HEIGHT: f64 = 44.0;
const STATUS_PANEL_EXPANDED_WIDTH: f64 = 340.0;
/// Only the height the panel opens at before the renderer has measured itself.
/// `status_panel_fit_height` replaces it with the real content height as soon as the
/// readout has laid out, so this just needs to be close enough that the first frame
/// does not visibly jump.
const STATUS_PANEL_EXPANDED_HEIGHT: f64 = 358.0;
const STATUS_PANEL_EXPANDED_MIN_HEIGHT: f64 = 260.0;
/// Wide enough to never constrain the user, but finite: locking the panel's height
/// to its content needs a maximum size, and that call takes both axes.
const STATUS_PANEL_MAX_WIDTH: f64 = 4096.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PanelRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl PanelRect {
    fn right(&self) -> i32 {
        self.x + self.width
    }

    fn bottom(&self) -> i32 {
        self.y + self.height
    }
}

fn overlap_span(a_start: i32, a_end: i32, b_start: i32, b_end: i32) -> i32 {
    (a_end.min(b_end) - a_start.max(b_start)).max(0)
}

fn clamp_axis(start: i32, length: i32, area_start: i32, area_length: i32) -> i32 {
    if length >= area_length {
        return area_start;
    }
    start.clamp(area_start, area_start + area_length - length)
}

/// Keeps a remembered panel position reachable. A saved position can point at a
/// display that is no longer attached, or leave the panel almost entirely
/// off-screen; both must resolve to somewhere the user can still grab it.
/// Positions that are already visible enough are returned untouched so the
/// panel never drifts on its own. `work_areas` must be ordered with the
/// preferred fallback display first.
fn clamp_panel_position(panel: PanelRect, work_areas: &[PanelRect]) -> (i32, i32) {
    if work_areas.is_empty() {
        return (panel.x, panel.y);
    }
    let visible = |area: &PanelRect| {
        (
            overlap_span(panel.x, panel.right(), area.x, area.right()),
            overlap_span(panel.y, panel.bottom(), area.y, area.bottom()),
        )
    };
    let required_horizontal = STATUS_PANEL_MIN_VISIBLE.min(panel.width);
    let required_vertical = STATUS_PANEL_MIN_VISIBLE.min(panel.height);
    let reachable = work_areas.iter().any(|area| {
        let (horizontal, vertical) = visible(area);
        horizontal >= required_horizontal && vertical >= required_vertical
    });
    if reachable {
        return (panel.x, panel.y);
    }
    // Fall back to the work area the panel overlaps most. When it overlaps none,
    // ties keep the first entry, which the caller orders as the primary display
    // because that is where the user is looking.
    let mut target = work_areas[0];
    let mut best = -1i64;
    for area in work_areas {
        let (horizontal, vertical) = visible(area);
        let score = i64::from(horizontal) * i64::from(vertical);
        if score > best {
            best = score;
            target = *area;
        }
    }
    (
        clamp_axis(panel.x, panel.width, target.x, target.width),
        clamp_axis(panel.y, panel.height, target.y, target.height),
    )
}

/// Top-right of the work area, inset by a margin. An always-on-top monitor is
/// useless where the OS would otherwise put it: under the Logic 2 window the
/// user is about to focus.
fn default_panel_position(panel_width: i32, work_area: PanelRect) -> (i32, i32) {
    (
        work_area.x + (work_area.width - panel_width - STATUS_PANEL_EDGE_MARGIN).max(0),
        work_area.y + STATUS_PANEL_EDGE_MARGIN,
    )
}

fn monitor_work_area(monitor: &tauri::Monitor) -> PanelRect {
    let area = monitor.work_area();
    PanelRect {
        x: area.position.x,
        y: area.position.y,
        width: area.size.width as i32,
        height: area.size.height as i32,
    }
}

/// Work areas of every attached display, primary first. `available_monitors()`
/// makes no ordering promise, and `clamp_panel_position` treats the first entry
/// as the preferred fallback, so the primary display is hoisted explicitly.
fn panel_work_areas(window: &tauri::WebviewWindow) -> Vec<PanelRect> {
    let mut areas: Vec<PanelRect> = window
        .available_monitors()
        .map(|monitors| monitors.iter().map(monitor_work_area).collect())
        .unwrap_or_default();
    if let Some(primary) = window
        .primary_monitor()
        .ok()
        .flatten()
        .map(|monitor| monitor_work_area(&monitor))
    {
        areas.retain(|area| *area != primary);
        areas.insert(0, primary);
    }
    areas
}

fn panel_rect(window: &tauri::WebviewWindow) -> Option<PanelRect> {
    let position = window.outer_position().ok()?;
    let size = window.outer_size().ok()?;
    Some(PanelRect {
        x: position.x,
        y: position.y,
        width: size.width as i32,
        height: size.height as i32,
    })
}

/// Places the panel before it becomes visible, so it never flashes at the
/// position the OS picked.
fn restore_status_panel_position(window: &tauri::WebviewWindow, saved: Option<PanelPosition>) {
    let Some(rect) = panel_rect(window) else {
        return;
    };
    let work_areas = panel_work_areas(window);
    let (x, y) = match saved {
        Some(position) => clamp_panel_position(
            PanelRect {
                x: position.x,
                y: position.y,
                ..rect
            },
            &work_areas,
        ),
        None => match work_areas.first() {
            Some(area) => default_panel_position(rect.width, *area),
            None => return,
        },
    };
    let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
}

/// Switches the panel between the full readout and a compact floating button.
///
/// The window carries no native decorations in either shape: a titlebar would
/// duplicate the panel's own collapse and hide controls, and on a 44 px chip it
/// would dwarf the chip entirely. Only the inner size changes here.
/// The expanded shape to restore: the width the user last chose and the height the
/// renderer last measured for its content. The constants are only reached before the
/// panel has been expanded once, and restoring the measured height is what stops the
/// panel resizing a second time the instant it opens.
fn expanded_panel_size(app: &AppHandle) -> (f64, f64) {
    let state = app.state::<AppState>();
    let width = match state.expanded_panel_width.load(Ordering::Acquire) {
        0 => STATUS_PANEL_EXPANDED_WIDTH,
        remembered => f64::from(remembered).max(STATUS_PANEL_MIN_WIDTH),
    };
    let height = match state.expanded_panel_height.load(Ordering::Acquire) {
        0 => STATUS_PANEL_EXPANDED_HEIGHT,
        remembered => f64::from(remembered).max(STATUS_PANEL_COLLAPSED_HEIGHT),
    };
    (width, height)
}

/// Applies a shape change as a single main-thread turn.
///
/// Each window mutation posted on its own can be committed as its own frame, and the
/// panel is then visibly drawn at the new size in the old place before it jumps.
/// Batching them lets AppKit coalesce the whole change into one visible update.
///
/// The height lock is released first because it is set to whatever the content
/// measured last time, and it would otherwise refuse both a 44 px chip and any new
/// content height outright.
fn apply_panel_frame(
    window: &tauri::WebviewWindow,
    min_size: tauri::LogicalSize<f64>,
    size: tauri::LogicalSize<f64>,
    position: tauri::PhysicalPosition<i32>,
) {
    let target = window.clone();
    let apply = move || {
        let _ = target.set_max_size(None::<tauri::LogicalSize<f64>>);
        let _ = target.set_min_size(Some(min_size));
        let _ = target.set_size(size);
        let _ = target.set_position(position);
    };
    if window.app_handle().run_on_main_thread(apply).is_ok() {
        return;
    }
    let _ = window.set_max_size(None::<tauri::LogicalSize<f64>>);
    let _ = window.set_min_size(Some(min_size));
    let _ = window.set_size(size);
    let _ = window.set_position(position);
}

fn apply_status_panel_collapsed(app: &AppHandle, window: &tauri::WebviewWindow, collapsed: bool) {
    let scale = window.scale_factor().unwrap_or(1.0);
    // Cocoa anchors a resize at the bottom-left, so resizing on its own slides the
    // panel's visible top edge down the screen and leaves the user hunting for the
    // chip. The pre-resize rectangle is captured here and the panel is re-placed
    // against it once the new shape is known.
    let Some(anchor) = panel_rect(window) else {
        return;
    };
    let work_areas = panel_work_areas(window);
    let (width, height) = if collapsed {
        if let Ok(size) = window.inner_size() {
            let logical = (f64::from(size.width) / scale).round();
            if logical >= STATUS_PANEL_MIN_WIDTH {
                app.state::<AppState>()
                    .expanded_panel_width
                    .store(logical as u32, Ordering::Release);
            }
        }
        (STATUS_PANEL_COLLAPSED_WIDTH, STATUS_PANEL_COLLAPSED_HEIGHT)
    } else {
        expanded_panel_size(app)
    };
    // The new size comes from what is about to be requested instead of being read
    // back: the change is posted to the main thread and has not necessarily landed,
    // and a stale read would place the panel using the shape it is leaving. The
    // window is undecorated, so a logical inner size scales straight to a physical
    // outer one.
    let resized = PanelRect {
        width: (width * scale).round() as i32,
        height: (height * scale).round() as i32,
        ..anchor
    };
    let (x, y) = place_resized_panel(anchor, resized, &work_areas);
    let target = PanelRect { x, y, ..resized };
    // Orientation before geometry. The renderer has to know which end the header
    // belongs on before the panel is painted at full size, or it paints one frame
    // the wrong way round and the header visibly jumps across the panel.
    publish_dock_for_panel(app, scale, target, &work_areas);
    let floor = if collapsed {
        tauri::LogicalSize::new(STATUS_PANEL_COLLAPSED_WIDTH, STATUS_PANEL_COLLAPSED_HEIGHT)
    } else {
        tauri::LogicalSize::new(
            STATUS_PANEL_MIN_WIDTH,
            STATUS_PANEL_EXPANDED_MIN_HEIGHT.min(height),
        )
    };
    apply_panel_frame(
        window,
        floor,
        tauri::LogicalSize::new(width, height),
        tauri::PhysicalPosition::new(x, y),
    );
}

fn snap_axis(start: i32, length: i32, area_start: i32, area_length: i32, threshold: i32) -> i32 {
    if length >= area_length {
        return start;
    }
    let area_end = area_start + area_length;
    if (start - area_start).abs() <= threshold {
        return area_start;
    }
    if (start + length - area_end).abs() <= threshold {
        return area_end - length;
    }
    start
}

/// Snaps a panel to the work-area edges it came to rest near. Only ever applied
/// after a drag settles, never while the pointer is moving, so it cannot fight
/// the user mid-gesture.
fn snap_to_work_area(panel: PanelRect, work_area: PanelRect, threshold: i32) -> (i32, i32) {
    (
        snap_axis(
            panel.x,
            panel.width,
            work_area.x,
            work_area.width,
            threshold,
        ),
        snap_axis(
            panel.y,
            panel.height,
            work_area.y,
            work_area.height,
            threshold,
        ),
    )
}

/// The work area the panel sits on, chosen by overlap so a panel straddling two
/// displays snaps to the one showing most of it.
fn work_area_under_panel(panel: PanelRect, work_areas: &[PanelRect]) -> Option<PanelRect> {
    let mut best: Option<(i64, PanelRect)> = None;
    for area in work_areas {
        let score = i64::from(overlap_span(panel.x, panel.right(), area.x, area.right()))
            * i64::from(overlap_span(panel.y, panel.bottom(), area.y, area.bottom()));
        if score <= 0 {
            continue;
        }
        if best.is_none_or(|(current, _)| score > current) {
            best = Some((score, *area));
        }
    }
    best.map(|(_, area)| area)
}

/// Where one edge of the panel lands when the panel changes shape.
///
/// A panel resting against a work-area edge has to keep resting against it: an
/// expand has to grow inwards and the matching collapse has to shrink back
/// outwards, or every toggle walks the panel further from the edge it was parked
/// against.
///
/// The closing clamp is what keeps an expanded panel readable. The chip can be
/// dropped anywhere, so it is routinely parked at an edge, and growing it there
/// pushes the readout past the display by the difference between the two shapes.
/// Nothing else recovers that: `snap_axis` ignores an overshoot far larger than
/// its threshold, and `clamp_panel_position` settles for
/// `STATUS_PANEL_MIN_VISIBLE`, which is the right answer for a remembered
/// position and the wrong one for a panel the user just asked to read.
fn anchored_axis(
    start: i32,
    old_length: i32,
    new_length: i32,
    area_start: i32,
    area_length: i32,
    threshold: i32,
) -> i32 {
    let area_end = area_start + area_length;
    let holds_start = (start - area_start).abs() <= threshold;
    let holds_end = (start + old_length - area_end).abs() <= threshold;
    // A panel spanning the whole axis sits at both edges at once; holding the
    // start is then the only choice that does not make it jump.
    let candidate = if holds_end && !holds_start {
        area_end - new_length
    } else {
        start
    };
    clamp_axis(candidate, new_length, area_start, area_length)
}

/// Places a panel that just changed shape. `anchor` is the rectangle it occupied
/// beforehand and `resized` carries the new size at the old origin.
///
/// The display is chosen from where the panel already was rather than from the
/// grown rectangle, so expanding beside a second monitor cannot hop onto it just
/// because the larger shape happens to overlap it more.
fn place_resized_panel(
    anchor: PanelRect,
    resized: PanelRect,
    work_areas: &[PanelRect],
) -> (i32, i32) {
    let Some(area) =
        work_area_under_panel(anchor, work_areas).or_else(|| work_areas.first().copied())
    else {
        return (resized.x, resized.y);
    };
    (
        anchored_axis(
            anchor.x,
            anchor.width,
            resized.width,
            area.x,
            area.width,
            STATUS_PANEL_SNAP_THRESHOLD,
        ),
        anchored_axis(
            anchor.y,
            anchor.height,
            resized.height,
            area.y,
            area.height,
            STATUS_PANEL_SNAP_THRESHOLD,
        ),
    )
}

/// Keeps a panel that is being dragged wholly on one display.
///
/// The display is chosen by the cursor rather than by the panel, so the panel can
/// still be dragged onto a second monitor: it follows the pointer across the seam
/// instead of sticking against the edge of the display it started on. A cursor
/// that is outside every work area -- in the menu bar, say -- falls back to the
/// display showing most of the panel.
///
/// Clamping is the point of the function. The reason to drag an always-on-top
/// readout into a corner is to keep reading it, so the window manager's usual
/// freedom to push most of it off the screen is not worth having here.
fn confine_dragged_panel(
    panel: PanelRect,
    cursor: (i32, i32),
    work_areas: &[PanelRect],
) -> (i32, i32) {
    let holds_cursor = |area: &&PanelRect| {
        (area.x..area.right()).contains(&cursor.0) && (area.y..area.bottom()).contains(&cursor.1)
    };
    let Some(area) = work_areas
        .iter()
        .find(holds_cursor)
        .copied()
        .or_else(|| work_area_under_panel(panel, work_areas))
        .or_else(|| work_areas.first().copied())
    else {
        return (panel.x, panel.y);
    };
    (
        clamp_axis(panel.x, panel.width, area.x, area.width),
        clamp_axis(panel.y, panel.height, area.y, area.height),
    )
}

/// Whether the panel is resting on the bottom edge of its display.
///
/// Shares `STATUS_PANEL_SNAP_THRESHOLD` with the snap and the resize anchor so all
/// three agree: a panel close enough to the bottom to be snapped there is also
/// close enough to hold a collapsing chip there, and to mirror its own layout.
/// Continuous confinement during a drag pins a docked panel at exactly zero
/// distance, so the tolerance mostly acts as stickiness once docked rather than as
/// a band to hover in.
fn panel_docked_at_bottom(panel: PanelRect, work_areas: &[PanelRect]) -> bool {
    work_area_under_panel(panel, work_areas)
        .or_else(|| work_areas.first().copied())
        .is_some_and(|area| (panel.bottom() - area.bottom()).abs() <= STATUS_PANEL_SNAP_THRESHOLD)
}

/// Where a collapsed chip will land once it is expanded, computed through the same
/// placement the expand itself uses so the two cannot disagree.
fn project_expanded_panel(
    chip: PanelRect,
    expanded: (i32, i32),
    work_areas: &[PanelRect],
) -> PanelRect {
    let resized = PanelRect {
        width: expanded.0,
        height: expanded.1,
        ..chip
    };
    let (x, y) = place_resized_panel(chip, resized, work_areas);
    PanelRect { x, y, ..resized }
}

/// Where the panel will sit once expanded, given the rectangle it occupies now.
///
/// A collapsed chip is projected forward. The orientation only shows in the expanded
/// layout, and judging it by the chip gets it wrong exactly where it matters: a chip
/// resting 36 px above the bottom edge is not docked, but expanding it is clamped
/// flush to that edge, so the panel would open with its header at the wrong end and
/// reflow after it had already been painted. That reads as the header jumping across
/// the panel.
fn projected_expanded_panel(
    app: &AppHandle,
    scale: f64,
    panel: PanelRect,
    work_areas: &[PanelRect],
) -> PanelRect {
    // Already showing the readout, so its own rectangle decides the layout.
    if f64::from(panel.height) / scale > STATUS_PANEL_COLLAPSED_HEIGHT + 1.0 {
        return panel;
    }
    let (width, height) = expanded_panel_size(app);
    let expanded = (
        (width * scale).round() as i32,
        (height * scale).round() as i32,
    );
    project_expanded_panel(panel, expanded, work_areas)
}

/// Tells the panel to mirror itself when it comes to rest on the bottom edge, and
/// only when that changes.
///
/// The header is the drag handle and carries the collapse and hide controls, and a
/// collapsed chip rests on the same edge the panel was docked against. Leaving the
/// header at the top of a bottom-docked panel would move the grab point the height
/// of the whole readout every time the shape changed.
///
/// Takes the rectangle rather than reading the window: geometry changes are posted to
/// the main thread and a read-back can still report the shape being left behind.
fn publish_dock_for_panel(app: &AppHandle, scale: f64, panel: PanelRect, work_areas: &[PanelRect]) {
    let bottom = panel_docked_at_bottom(
        projected_expanded_panel(app, scale, panel, work_areas),
        work_areas,
    );
    let encoded = if bottom { 2 } else { 1 };
    if app
        .state::<AppState>()
        .panel_dock
        .swap(encoded, Ordering::AcqRel)
        == encoded
    {
        return;
    }
    let _ = app.emit_to("status", "status-panel-dock", StatusPanelDock { bottom });
}

fn sync_status_panel_dock(app: &AppHandle, window: &tauri::WebviewWindow) {
    let Some(rect) = panel_rect(window) else {
        return;
    };
    publish_dock_for_panel(
        app,
        window.scale_factor().unwrap_or(1.0),
        rect,
        &panel_work_areas(window),
    );
}

fn persist_status_panel_position(app: &AppHandle, position: PanelPosition) {
    let mut settings = load_settings(app);
    if settings.status_panel.position == Some(position) {
        return;
    }
    settings.status_panel.position = Some(position);
    let _ = store_settings(app, settings);
}

fn settle_status_panel(app: &AppHandle, window: &tauri::WebviewWindow) {
    let Some(rect) = panel_rect(window) else {
        return;
    };
    let work_areas = panel_work_areas(window);
    let (x, y) = match work_area_under_panel(rect, &work_areas) {
        Some(area) => snap_to_work_area(rect, area, STATUS_PANEL_SNAP_THRESHOLD),
        // Off every display: getting it back matters more than snapping.
        None => clamp_panel_position(rect, &work_areas),
    };
    // Repositioning emits another `Moved`, but the next settle computes the same
    // coordinates and stops there, so this converges after one idle pass.
    if (x, y) != (rect.x, rect.y) {
        let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
    }
    persist_status_panel_position(app, PanelPosition { x, y });
    // A snap can be what finally brings the panel onto the edge, so the layout is
    // resolved after the position, not before it.
    sync_status_panel_dock(app, window);
}

/// `Moved` fires continuously while the user drags. Persisting on every event
/// would thrash the config file, so the work runs once the panel has held still
/// briefly.
fn schedule_status_panel_settle(window: &tauri::WebviewWindow) {
    let app = window.app_handle().clone();
    let generation = app
        .state::<AppState>()
        .panel_move_generation
        .fetch_add(1, Ordering::AcqRel)
        + 1;
    let window = window.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(STATUS_PANEL_SETTLE_MS));
        if app
            .state::<AppState>()
            .panel_move_generation
            .load(Ordering::Acquire)
            != generation
        {
            return;
        }
        let settle_app = app.clone();
        let _ = app.run_on_main_thread(move || settle_status_panel(&settle_app, &window));
    });
}

fn persist_mcp_window_position(app: &AppHandle, position: PanelPosition) {
    let mut settings = load_settings(app);
    if settings.mcp.position == Some(position) {
        return;
    }
    settings.mcp.position = Some(position);
    let _ = store_settings(app, settings);
}

fn settle_mcp_window(app: &AppHandle, window: &tauri::WebviewWindow) {
    let Some(rect) = panel_rect(window) else {
        return;
    };
    let (x, y) = clamp_panel_position(rect, &panel_work_areas(window));
    if (x, y) != (rect.x, rect.y) {
        let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
    }
    persist_mcp_window_position(app, PanelPosition { x, y });
}

fn schedule_mcp_window_settle(window: &tauri::WebviewWindow) {
    let app = window.app_handle().clone();
    let generation = app
        .state::<AppState>()
        .mcp_move_generation
        .fetch_add(1, Ordering::AcqRel)
        + 1;
    let window = window.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(STATUS_PANEL_SETTLE_MS));
        if app
            .state::<AppState>()
            .mcp_move_generation
            .load(Ordering::Acquire)
            != generation
        {
            return;
        }
        let settle_app = app.clone();
        let _ = app.run_on_main_thread(move || settle_mcp_window(&settle_app, &window));
    });
}

fn show_mcp_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("mcp") {
        restore_status_panel_position(&window, load_settings(app).mcp.position);
        let _ = window.show();
        let _ = window.set_always_on_top(true);
        // Deliberately no set_focus: agent activity and approvals may reveal this over
        // Logic 2, but typing and shortcuts must stay in the app the user is operating.
    }
}

fn hide_mcp_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("mcp") {
        let _ = window.hide();
    }
}

#[tauri::command]
fn mcp_window_show(app: AppHandle) {
    show_mcp_window(&app);
}

#[tauri::command]
fn mcp_window_hide(app: AppHandle) {
    hide_mcp_window(&app);
}

#[tauri::command]
fn mcp_window_begin_move(app: AppHandle) -> Result<(), String> {
    panel_begin_move(&app, "mcp")
}

#[tauri::command]
fn mcp_window_move(app: AppHandle) {
    panel_move(&app, "mcp");
}

#[tauri::command]
fn mcp_window_end_move(app: AppHandle) {
    panel_end_move(&app, "mcp");
}

#[tauri::command]
fn mcp_set_auto_show(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = load_settings(&app);
    if settings.mcp.auto_show == enabled {
        return Ok(());
    }
    settings.mcp.auto_show = enabled;
    store_settings(&app, settings).map(|_| ())
}

fn show_main_window(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn show_status_panel(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window("status") {
        let panel = load_settings(app).status_panel;
        apply_status_panel_collapsed(app, &window, panel.collapsed);
        restore_status_panel_position(&window, panel.position);
        sync_status_panel_dock(app, &window);
        let _ = window.show();
        let _ = window.set_always_on_top(true);
    }
}

/// Reveals the panel without promoting the app to a regular activation policy.
/// The automatic reveal happens exactly when focus is being handed to Logic 2,
/// so activating here would snatch it straight back.
fn show_status_panel_without_activating(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("status") {
        let panel = load_settings(app).status_panel;
        apply_status_panel_collapsed(app, &window, panel.collapsed);
        restore_status_panel_position(&window, panel.position);
        sync_status_panel_dock(app, &window);
        let _ = window.show();
        let _ = window.set_always_on_top(true);
    }
}

fn hide_status_panel(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("status") {
        let _ = window.hide();
    }
}

#[tauri::command]
fn status_panel_show(app: AppHandle) -> Result<(), String> {
    show_status_panel(&app);
    Ok(())
}

#[tauri::command]
fn status_panel_hide(app: AppHandle) -> Result<(), String> {
    hide_status_panel(&app);
    Ok(())
}

#[tauri::command]
fn status_panel_intro_acknowledge(app: AppHandle) -> Result<(), String> {
    let mut settings = load_settings(&app);
    if settings.guidance.status_panel_intro_seen {
        return Ok(());
    }
    settings.guidance.status_panel_intro_seen = true;
    store_settings(&app, settings)?;
    Ok(())
}

#[tauri::command]
fn status_panel_set_auto_show(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = load_settings(&app);
    if settings.status_panel.auto_show == enabled {
        return Ok(());
    }
    settings.status_panel.auto_show = enabled;
    store_settings(&app, settings)?;
    Ok(())
}

#[tauri::command]
fn status_panel_set_collapsed(app: AppHandle, collapsed: bool) -> Result<(), String> {
    let Some(window) = app.get_webview_window("status") else {
        return Err("状态面板窗口不可用".to_string());
    };
    apply_status_panel_collapsed(&app, &window, collapsed);
    // No settle pass here: `apply_status_panel_collapsed` already places the new
    // shape from the size it requested, whereas a settle would read the size back
    // and can still see the shape being left behind.
    let mut settings = load_settings(&app);
    if settings.status_panel.collapsed != collapsed {
        settings.status_panel.collapsed = collapsed;
        store_settings(&app, settings)?;
    }
    Ok(())
}

/// The collapsed chip is one big click target, so it cannot also be a static drag
/// region: the renderer only calls this once the pointer has actually moved.
///
/// Starts a panel drag that the Bridge carries out itself.
///
/// `start_dragging()` hands the gesture to the window manager. On macOS that is
/// what arms the system's edge tiling: dragging the panel against a screen edge
/// offers to zoom or tile it, which is meaningless for a 340 px always-on-top
/// readout and discards the position the user was aiming for. Driving the move
/// from here avoids that entirely, and is also the only way to hold the panel
/// fully on the work area while it is moving.
fn panel_begin_move(app: &AppHandle, window_label: &str) -> Result<(), String> {
    let Some(window) = app.get_webview_window(window_label) else {
        return Err(format!("窗口 {window_label} 不可用"));
    };
    let position = window
        .outer_position()
        .map_err(|error| format!("无法读取窗口位置: {error}"))?;
    let cursor = app
        .cursor_position()
        .map_err(|error| format!("无法读取光标位置: {error}"))?;
    if let Ok(mut drag) = app.state::<AppState>().panel_drag.lock() {
        *drag = Some(PanelDragAnchor {
            window_label: window_label.to_string(),
            window_x: position.x,
            window_y: position.y,
            cursor_x: cursor.x,
            cursor_y: cursor.y,
        });
    }
    Ok(())
}

fn panel_move(app: &AppHandle, window_label: &str) {
    let Some(window) = app.get_webview_window(window_label) else {
        return;
    };
    let Some(anchor) = app
        .state::<AppState>()
        .panel_drag
        .lock()
        .ok()
        .and_then(|drag| drag.clone())
        .filter(|anchor| anchor.window_label == window_label)
    else {
        return;
    };
    let (Ok(cursor), Some(rect)) = (app.cursor_position(), panel_rect(&window)) else {
        return;
    };
    let moved = PanelRect {
        x: anchor.window_x + (cursor.x - anchor.cursor_x).round() as i32,
        y: anchor.window_y + (cursor.y - anchor.cursor_y).round() as i32,
        ..rect
    };
    let cursor = (cursor.x.round() as i32, cursor.y.round() as i32);
    let (x, y) = confine_dragged_panel(moved, cursor, &panel_work_areas(&window));
    if (x, y) != (rect.x, rect.y) {
        let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
    }
    if window_label == "status" {
        sync_status_panel_dock(app, &window);
    }
}

fn panel_end_move(app: &AppHandle, window_label: &str) {
    if let Ok(mut drag) = app.state::<AppState>().panel_drag.lock() {
        if drag
            .as_ref()
            .is_some_and(|anchor| anchor.window_label == window_label)
        {
            *drag = None;
        }
    }
}

#[tauri::command]
fn status_panel_begin_move(app: AppHandle) -> Result<(), String> {
    panel_begin_move(&app, "status")
}

#[tauri::command]
fn status_panel_move(app: AppHandle) {
    panel_move(&app, "status");
}

#[tauri::command]
fn status_panel_end_move(app: AppHandle) {
    panel_end_move(&app, "status");
}

/// Read once as the panel loads. The change event covers everything afterwards,
/// but a reload can happen long after the last move, and the panel must not come
/// back the wrong way round.
#[tauri::command]
fn status_panel_dock_edge(app: AppHandle) -> bool {
    let Some(window) = app.get_webview_window("status") else {
        return false;
    };
    let Some(rect) = panel_rect(&window) else {
        return false;
    };
    panel_docked_at_bottom(rect, &panel_work_areas(&window))
}

/// Sizes the expanded panel to the height the renderer measured for its content.
///
/// The readout is short and can be collapsed into its chip when it is in the way, so
/// scrolling it is never the right answer; the window follows the content instead.
/// Only the renderer can supply the number, because it depends on how the text
/// wrapped: the first-run card and an error line each add a chunk that no constant
/// can anticipate.
///
/// The height is then locked, which is what makes a scrollbar structurally
/// impossible rather than merely hidden. Width stays free so long values can be
/// given more room.
#[tauri::command]
fn status_panel_fit_height(app: AppHandle, height: f64) {
    let Some(window) = app.get_webview_window("status") else {
        return;
    };
    if !height.is_finite() || height <= 0.0 {
        return;
    }
    let scale = window.scale_factor().unwrap_or(1.0);
    let Some(rect) = panel_rect(&window) else {
        return;
    };
    // A chip is 44 px of fixed chrome; fitting it to a readout it is not showing
    // would blow it back up to full size.
    if f64::from(rect.height) / scale <= STATUS_PANEL_COLLAPSED_HEIGHT + 1.0 {
        return;
    }
    let work_areas = panel_work_areas(&window);
    // Never taller than the display: an always-on-top panel covering the screen is a
    // worse outcome than one whose content is clipped.
    let ceiling = work_area_under_panel(rect, &work_areas)
        .or_else(|| work_areas.first().copied())
        .map_or(STATUS_PANEL_EXPANDED_HEIGHT, |area| {
            f64::from(area.height) / scale
        })
        .max(STATUS_PANEL_EXPANDED_MIN_HEIGHT);
    let target = height.clamp(STATUS_PANEL_EXPANDED_MIN_HEIGHT.min(ceiling), ceiling);
    let physical = (target * scale).round() as i32;
    // Remembered even when the size already matches, so the next expand opens at the
    // measured height instead of the constant and needs no second resize.
    app.state::<AppState>()
        .expanded_panel_height
        .store(target.round() as u32, Ordering::Release);
    // Rounding between logical and physical pixels can leave a pixel of
    // disagreement, which would otherwise bounce between here and the renderer's
    // resize observer forever.
    if (physical - rect.height).abs() <= 1 {
        return;
    }
    let width = f64::from(rect.width) / scale;
    // Growing a panel docked on the bottom edge has to grow it upwards, exactly as
    // expanding from a chip there does.
    let resized = PanelRect {
        height: physical,
        ..rect
    };
    let (x, y) = place_resized_panel(rect, resized, &work_areas);
    publish_dock_for_panel(&app, scale, PanelRect { x, y, ..resized }, &work_areas);
    apply_panel_frame(
        &window,
        tauri::LogicalSize::new(STATUS_PANEL_MIN_WIDTH, target),
        tauri::LogicalSize::new(width, target),
        tauri::PhysicalPosition::new(x, y),
    );
    // The lock is what makes a scrollbar impossible rather than merely hidden, and it
    // has to land after the resize that `apply_panel_frame` clears it for.
    let locked = window.clone();
    let _ = window.app_handle().run_on_main_thread(move || {
        let _ = locked.set_max_size(Some(tauri::LogicalSize::new(
            STATUS_PANEL_MAX_WIDTH,
            target,
        )));
    });
}

#[tauri::command]
fn status_panel_toggle(app: AppHandle) -> Result<(), String> {
    let Some(window) = app.get_webview_window("status") else {
        return Err("状态面板窗口不可用".to_string());
    };
    if window.is_visible().unwrap_or(false) {
        hide_status_panel(&app);
    } else {
        show_status_panel(&app);
    }
    Ok(())
}

#[tauri::command]
fn main_window_show(app: AppHandle) -> Result<(), String> {
    show_main_window(&app);
    Ok(())
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
    if let Ok(mut current) = app.state::<AppState>().hardware.lock() {
        current.clone_from(&hardware);
    }
    let _ = app.emit("pxlogic-hardware", &hardware);
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
        firmware_releases: mcu_firmware_releases(),
    })
}

/// Window geometry and guidance progress are owned by the backend; the main
/// window never renders them and its save payload therefore omits them. Merging
/// them back from disk stops an ordinary settings save from resetting the panel
/// position, the collapsed shape, and the walkthrough flags to their defaults.
fn merge_backend_owned_settings(
    mut incoming: ClientSettings,
    current: &ClientSettings,
) -> ClientSettings {
    incoming.guidance = current.guidance.clone();
    incoming.status_panel = current.status_panel.clone();
    incoming.mcp = current.mcp.clone();
    incoming
}

/// The only way renderer-supplied settings may reach disk. Every entry point that
/// accepts a `ClientSettings` from the UI must go through here: the main window
/// neither renders nor returns the backend-owned sections, so a direct
/// `store_settings` would reset the panel geometry and the walkthrough flags to
/// their defaults.
fn store_renderer_settings(
    app: &AppHandle,
    incoming: ClientSettings,
) -> Result<ClientSettings, String> {
    let current = load_settings(app);
    store_settings(app, merge_backend_owned_settings(incoming, &current))
}

#[tauri::command]
fn onboarding_complete(app: AppHandle) -> Result<(), String> {
    let mut settings = load_settings(&app);
    if settings.guidance.onboarding_completed_version == ONBOARDING_VERSION {
        return Ok(());
    }
    settings.guidance.onboarding_completed_version = ONBOARDING_VERSION;
    store_settings(&app, settings)?;
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogicInstance {
    pid: u32,
    /// Present when the window was started by the Bridge with
    /// `--useExistingGraph --graphPort N`. Only used to tell the user which
    /// windows the Bridge is responsible for; every running window has to be
    /// replaced either way, because Logic never rebuilds its device state in a
    /// fresh GraphServer after reconnecting.
    #[serde(skip_serializing_if = "Option::is_none")]
    graph_port: Option<u16>,
}

/// Extracts Logic windows from a `ps -axo pid=,command=` listing.
///
/// Kept free of process access so every discriminator can be tested: the Bridge
/// itself runs through this same binary in Node mode with a `.cjs` entry script,
/// and Chromium spawns its helper processes from it too, so neither may be
/// mistaken for a window that owns a graph connection.
fn parse_logic_instances(listing: &str, executable: &str, self_pid: u32) -> Vec<LogicInstance> {
    let mut instances = Vec::new();
    for line in listing.lines() {
        let trimmed = line.trim_start();
        let Some((raw_pid, rest)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = raw_pid.parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Some(arguments) = rest.trim_start().strip_prefix(executable) else {
            continue;
        };
        // A longer path that merely starts with ours is a different binary.
        if !(arguments.is_empty() || arguments.starts_with(char::is_whitespace)) {
            continue;
        }
        let arguments = arguments.trim_start();
        let first = arguments.split_whitespace().next().unwrap_or_default();
        if first.ends_with(".cjs") || arguments.contains("--type=") {
            continue;
        }
        let graph_port = arguments
            .split_whitespace()
            .skip_while(|argument| *argument != "--graphPort")
            .nth(1)
            .and_then(|value| value.parse::<u16>().ok());
        instances.push(LogicInstance { pid, graph_port });
    }
    instances
}

#[cfg(unix)]
fn running_logic_instances(executable: &Path) -> Vec<LogicInstance> {
    let Ok(output) = Command::new("ps").args(["-axo", "pid=,command="]).output() else {
        return Vec::new();
    };
    parse_logic_instances(
        &String::from_utf8_lossy(&output.stdout),
        &executable.to_string_lossy(),
        std::process::id(),
    )
}

#[cfg(not(unix))]
fn running_logic_instances(_executable: &Path) -> Vec<LogicInstance> {
    Vec::new()
}

#[cfg(unix)]
fn terminate_single_process(pid: u32, force: bool) -> Result<(), String> {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    if unsafe { libc::kill(pid as i32, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error.to_string())
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn logic_executable_for_settings(app: &AppHandle) -> Result<PathBuf, String> {
    let settings = load_settings(app);
    let app_path = PathBuf::from(settings.logic_app_path.trim());
    if app_path.as_os_str().is_empty() {
        return Err("尚未选择 Logic 2 安装".to_string());
    }
    Ok(logic_executable(&resolve_logic_installation(&app_path)?))
}

/// Reports Logic windows the launcher would collide with. Every running window has
/// to be replaced, so the renderer asks before starting and confirms closing them,
/// which is destructive and must never happen silently.
#[tauri::command]
fn logic_running_instances(app: AppHandle) -> Result<Vec<LogicInstance>, String> {
    Ok(running_logic_instances(&logic_executable_for_settings(
        &app,
    )?))
}

/// Closes Logic windows the user explicitly agreed to close. Politely first, then
/// forcibly, because an unresponsive window would otherwise block every later
/// Bridge start.
#[cfg(unix)]
#[tauri::command]
async fn logic_close_instances(app: AppHandle, pids: Vec<u32>) -> Result<(), String> {
    let executable = logic_executable_for_settings(&app)?;
    // Only ever signal a pid that is still one of our Logic windows, so a stale
    // list from the renderer cannot be turned into a kill of an unrelated process.
    let known: HashSet<u32> = running_logic_instances(&executable)
        .into_iter()
        .map(|instance| instance.pid)
        .collect();
    let targets: Vec<u32> = pids.into_iter().filter(|pid| known.contains(pid)).collect();
    for pid in &targets {
        terminate_single_process(*pid, false)?;
    }
    for _ in 0..40 {
        if targets.iter().all(|pid| !process_is_alive(*pid)) {
            return Ok(());
        }
        tauri::async_runtime::spawn_blocking(|| thread::sleep(Duration::from_millis(250)))
            .await
            .map_err(|error| format!("等待 Logic 2 退出失败: {error}"))?;
    }
    for pid in &targets {
        let _ = terminate_single_process(*pid, true);
    }
    for _ in 0..20 {
        if targets.iter().all(|pid| !process_is_alive(*pid)) {
            return Ok(());
        }
        tauri::async_runtime::spawn_blocking(|| thread::sleep(Duration::from_millis(250)))
            .await
            .map_err(|error| format!("等待 Logic 2 退出失败: {error}"))?;
    }
    Err("Logic 2 未能退出，请手动关闭后重试".to_string())
}

#[cfg(not(unix))]
#[tauri::command]
async fn logic_close_instances(_app: AppHandle, _pids: Vec<u32>) -> Result<(), String> {
    Err("当前平台尚未实现关闭 Logic 2 实例".to_string())
}

#[tauri::command]
fn client_save_settings(
    app: AppHandle,
    settings: ClientSettings,
) -> Result<ClientSettings, String> {
    store_renderer_settings(&app, settings)
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
        inspect_logic_selection(Some(&app), Path::new(app_path.trim()), false)
    })
    .await
    .map_err(|error| format!("Logic 检查任务失败: {error}"))
}

#[tauri::command]
async fn logic_analyze(app: AppHandle, app_path: String) -> Result<LogicInspection, String> {
    tauri::async_runtime::spawn_blocking(move || {
        inspect_logic_selection(Some(&app), Path::new(app_path.trim()), true)
    })
    .await
    .map_err(|error| format!("Logic 兼容性分析任务失败: {error}"))
}

#[tauri::command]
async fn pxlogic_scan(
    app: AppHandle,
    preferred_device_id: String,
) -> Result<PxlogicHardwareState, String> {
    let scan_app = app.clone();
    let hardware = tauri::async_runtime::spawn_blocking(move || {
        scan_pxlogic_hardware(&scan_app, preferred_device_id.trim())
    })
    .await
    .map_err(|error| format!("PXLogic 扫描任务失败: {error}"))?;
    if let Ok(mut current) = app.state::<AppState>().hardware.lock() {
        current.clone_from(&hardware);
    }
    let _ = app.emit("pxlogic-hardware", &hardware);
    Ok(hardware)
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
            .set_title("选择 Saleae Logic 2 或其所在文件夹")
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
    Ok(Some(inspect_logic_selection(Some(&app), &path, false)))
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

/// Picks a free loopback port for the renderer channel, or `None` to run without one.
///
/// Asking the OS for port 0 and releasing it immediately is a race in principle: the
/// port could be taken again before Logic 2 binds it. It is the right trade anyway --
/// a fixed port collides for certain when a second Logic 2 starts, and losing the
/// channel costs only the marker tools, which report the channel as unavailable.
fn allocate_renderer_debug_port() -> Option<u16> {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|address| address.port())
}

fn start_bridge_inner(app: &AppHandle, settings: ClientSettings) -> Result<BridgeState, String> {
    // These settings came straight from the renderer, so the backend-owned
    // sections have to be merged back before anything is written.
    let mut settings = store_renderer_settings(app, settings)?;
    let inspection = validate_bridge_start_compatibility(app, &settings)?;
    settings.logic_app_path = inspection.path.clone();
    settings = store_settings(app, settings)?;
    let selected_app_path = PathBuf::from(&settings.logic_app_path);
    let app_path = resolve_logic_installation(&selected_app_path)?;
    let hardware = scan_pxlogic_hardware(app, &settings.pxlogic_device_id);
    if let Some(error) = hardware.error {
        return Err(error);
    }
    let selected_device_id = hardware
        .selected_device_id
        .clone()
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
    if let Ok(mut current) = app.state::<AppState>().hardware.lock() {
        current.clone_from(&hardware);
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
    // Opens the channel the timing-marker tools need. This is a transport only: no
    // DevTools window is opened, `--auto-open-devtools-for-tabs` is never passed, and
    // the proxy never sends `Page.inspect`. Chromium shows nothing for the port being
    // open, and the automation banner belongs to `--enable-automation`, which is not
    // used. The port is taken from the OS so two Logic instances cannot collide.
    let renderer_debug_port = allocate_renderer_debug_port();
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
        .args(
            selected_device
                .serial_number
                .as_deref()
                .map(|serial| ["--pxlogic-serial", serial])
                .into_iter()
                .flatten(),
        )
        .args([
            "--pxlogic-usb-speed",
            selected_device.usb_speed.as_deref().unwrap_or("high"),
        ])
        .args([
            "--pxlogic-logic-mode",
            &selected_device.logic_mode.unwrap_or(0).to_string(),
        ])
        .args([
            "--hardware-threshold-volts",
            &settings.pxlogic_threshold_volts.to_string(),
        ])
        .current_dir(&payload.bridge_root)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(debug_port) = renderer_debug_port {
        command.args(["--remote-debugging-port", &debug_port.to_string()]);
    }
    if settings.maximize_logic_window {
        command.arg("--maximize-window");
    } else {
        command.args(["--screen-quadrant", &settings.screen_quadrant.to_string()]);
    }
    if has_pending_profile_authorization(
        &inspection,
        settings.pending_profile_fingerprint.as_deref(),
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
    let control = child.stdin.take();
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
        runtime.stop_requested = false;
        runtime.child = Some(ManagedChild {
            token,
            pid,
            control,
            child,
        });
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
    show_status_panel(app);
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
    validate_bridge_start_compatibility(&app, &settings)?;
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
    let logic = inspect_logic_selection(Some(app), Path::new(&settings.logic_app_path), false);
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

/// Reports the proxy's address so the window can show it, and so the registration
/// command it offers names a port that is actually listening.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct McpStatus {
    /// `None` until the proxy has bound, and if binding failed outright.
    listen_port: Option<u16>,
    requested_listen_port: u16,
    upstream_port: u16,
    /// True when the preferred port was taken. The window says so out loud, because an
    /// agent registered against the preferred port would quietly fail to connect.
    fell_back: bool,
    /// Whether anything is listening on Logic 2's MCP port right now.
    upstream_reachable: bool,
    auto_show: bool,
}

/// How often the upstream check runs. Logic 2's MCP server can be switched on and off
/// while the app watches, and a stale "not enabled" would send the user hunting.
const MCP_UPSTREAM_POLL: Duration = Duration::from_secs(3);

fn mcp_status_snapshot(app: &AppHandle, upstream_reachable: bool) -> McpStatus {
    let settings = load_settings(app).mcp;
    let bound = app
        .state::<AppState>()
        .mcp
        .lock()
        .ok()
        .and_then(|state| state.as_ref().map(|state| state.ports));
    McpStatus {
        listen_port: bound.map(|ports| ports.listen_port),
        requested_listen_port: bound
            .map_or(settings.listen_port, |ports| ports.requested_listen_port),
        upstream_port: bound.map_or(settings.upstream_port, |ports| ports.upstream_port),
        fell_back: bound.is_some_and(|ports| ports.fell_back()),
        upstream_reachable,
        auto_show: settings.auto_show,
    }
}

/// Observes the proxied traffic on behalf of the app.
///
/// Task 1 keeps it transparent: nothing is reported and nothing is refused. It exists
/// now so the activity feed and the approval gate have a seam to grow into rather than
/// the forwarding path being edited later.
struct TauriProxyObserver {
    app: AppHandle,
}

/// The timing-marker tools this client adds to Logic 2's MCP surface.
///
/// Logic 2 defines its own fifteen tools inside its renderer, reading `rapidDataStore`
/// directly. Timing markers live on that same store with no tool in front of them, so
/// an agent can capture and decode but cannot write down where it found something. That
/// is the gap these five close.
///
/// The descriptions carry the facts an agent cannot discover by trying: that `timeSec` is
/// measured from the start of the capture, that a capture has to exist before a marker
/// can sit on it, and that Logic 2 only annotates a capture once it has finished.
fn marker_tool_definitions() -> Vec<serde_json::Value> {
    // Only names from `DarkColor` render. Logic 2 looks the value up in its colour map and
    // silently drops an unknown one, so an enum listing a colour that does nothing would
    // be worse than a shorter one. The first six are the palette Logic 2 cycles through
    // itself; the plain names are the same map's other entries.
    let colors = serde_json::json!([
        "paleRed",
        "green2",
        "purple2",
        "orange2",
        "fuchsia",
        "lightBlue",
        "red",
        "green",
        "orange",
        "purple",
        "yellow"
    ]);
    // Both add tools state this. An agent that reads it on one and not the other will
    // assume the quieter one is unrestricted.
    let capture_state_note = "Logic 2 only annotates a capture that has finished (or, on MSO, \
                              one that is paused), so this fails while a capture is still running.";
    vec![
        serde_json::json!({
            "name": "add_timing_marker",
            "description": format!("Add a timing marker to the current Logic 2 capture, optionally with a note. \
                            Use this to record where something was found -- a protocol error, an unexpected \
                            edge, the start of a transaction. timeSec is measured in seconds from the start \
                            of the capture. Requires an active capture in Logic 2. {capture_state_note}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "timeSec": {
                        "type": "number",
                        "description": "Position in seconds from the start of the capture.",
                    },
                    "note": {
                        "type": "string",
                        "description": "Free text attached to the marker, shown in Logic 2's Timing Markers sidebar.",
                    },
                    "label": {
                        "type": "string",
                        "description": "Short label drawn on the marker itself. Logic 2 defaults it to the marker's id.",
                    },
                    "color": {
                        "type": "string",
                        "enum": colors,
                        "description": "Marker colour.",
                    },
                },
                "required": ["timeSec"],
            },
        }),
        serde_json::json!({
            "name": "add_timing_marker_pair",
            "description": format!("Add a timing marker pair spanning two times, which is how Logic 2 measures \
                            an interval: the pair reports its own duration. Use this instead of two separate \
                            markers when the question is how long something took -- a transaction, a gap \
                            between edges, a pulse width. Both times are in seconds from the start of the \
                            capture. Requires an active capture in Logic 2. {capture_state_note}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "startSec": {
                        "type": "number",
                        "description": "Interval start, in seconds from the start of the capture.",
                    },
                    "endSec": {
                        "type": "number",
                        "description": "Interval end, in seconds from the start of the capture. Must differ from startSec.",
                    },
                    "note": {
                        "type": "string",
                        "description": "Free text attached to the pair, shown in Logic 2's Timing Markers sidebar.",
                    },
                    "label": {
                        "type": "string",
                        "description": "Short label drawn on the pair. Logic 2 defaults it to the pair's id.",
                    },
                    "color": {
                        "type": "string",
                        "enum": colors,
                        "description": "Pair colour.",
                    },
                },
                "required": ["startSec", "endSec"],
            },
        }),
        serde_json::json!({
            "name": "list_timing_markers",
            "description": "List what is annotating the current Logic 2 capture: single markers under \
                            \"markers\", each with its timeSec, and interval pairs under \"pairs\", each with \
                            startSec, endSec and durationSec. Both carry ids, labels and notes, and share one \
                            id sequence. Requires an active capture in Logic 2.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] },
        }),
        serde_json::json!({
            "name": "set_timing_marker_note",
            "description": "Set or clear the note on an existing timing marker or pair. Omit note to clear it. \
                            Use list_timing_markers to find the id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Marker or pair id from list_timing_markers." },
                    "note": { "type": "string", "description": "New note text; omit to clear." },
                },
                "required": ["id"],
            },
        }),
        serde_json::json!({
            "name": "remove_timing_marker",
            "description": "Remove one timing marker or pair from the current Logic 2 capture by id. \
                            Use list_timing_markers to find the id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Marker or pair id from list_timing_markers." },
                },
                "required": ["id"],
            },
        }),
    ]
}

/// Maps a marker tool name onto the session command that serves it.
fn marker_command_for_tool(tool: &str) -> Option<&'static str> {
    match tool {
        "add_timing_marker" => Some("add-timing-marker"),
        "add_timing_marker_pair" => Some("add-timing-marker-pair"),
        "list_timing_markers" => Some("list-timing-markers"),
        "set_timing_marker_note" => Some("set-timing-marker-note"),
        "remove_timing_marker" => Some("remove-timing-marker"),
        _ => None,
    }
}

/// Renders a marker outcome as an MCP tool result.
///
/// A failure is reported as `isError` with the reason as text rather than as a JSON-RPC
/// error, which is what the protocol asks for: the call reached the tool and the tool
/// has something to say. An agent can then correct itself -- starting a capture first,
/// for instance -- instead of treating it as a broken endpoint.
fn marker_tool_result(payload: &serde_json::Value) -> serde_json::Value {
    let ok = payload
        .get("ok")
        .and_then(|ok| ok.as_bool())
        .unwrap_or(false);
    if !ok {
        let reason = payload
            .get("error")
            .and_then(|error| error.as_str())
            .unwrap_or("时间标记操作失败");
        return serde_json::json!({
            "content": [{ "type": "text", "text": reason }],
            "isError": true,
        });
    }
    // The interesting part of a success is the marker data, so it is returned as text
    // JSON: MCP results are text content, and an agent reads JSON well.
    let mut reported = payload.clone();
    if let Some(object) = reported.as_object_mut() {
        object.remove("ok");
        object.remove("type");
        object.remove("requestId");
    }
    let text = serde_json::to_string(&reported).unwrap_or_else(|_| "{}".to_string());
    serde_json::json!({ "content": [{ "type": "text", "text": text }] })
}

impl mcp_proxy::ProxyObserver for TauriProxyObserver {
    fn observe_request(&self, context: &mcp_proxy::ObservationContext, body: &[u8]) {
        let state = self.app.state::<AppState>();
        if load_settings(&self.app).mcp.auto_show
            && !state.mcp_auto_shown.swap(true, Ordering::AcqRel)
        {
            show_mcp_window(&self.app);
        }
        let activity = state
            .mcp_activity
            .lock()
            .ok()
            .and_then(|mut store| store.record_request(context, body, unix_time_millis()));
        if let Some(activity) = activity {
            let _ = self.app.emit("mcp-activity", activity);
        }
    }

    fn observe_response(&self, context: &mcp_proxy::ObservationContext, body: &[u8]) {
        let update = self
            .app
            .state::<AppState>()
            .mcp_activity
            .lock()
            .ok()
            .map(|mut store| store.record_response(context, body, unix_time_millis()));
        if let Some(update) = update {
            if let Some(activity) = update.activity {
                let _ = self.app.emit("mcp-activity", activity);
            }
            if let Some(tools) = update.tools {
                let _ = self.app.emit("mcp-tools", tools);
            }
        }
    }

    fn local_tools(&self) -> Vec<serde_json::Value> {
        // Advertised whether or not a session is running. A tool that appears only
        // sometimes teaches an agent nothing; one that is always listed and explains it
        // needs a capture is answerable.
        marker_tool_definitions()
    }

    fn observe_upstream_tools(&self, names: &[String]) {
        // Replaced rather than extended, so a tool Logic 2 has dropped stops shadowing.
        if let Ok(mut upstream) = self.app.state::<AppState>().mcp_upstream_tools.lock() {
            *upstream = names.iter().cloned().collect();
        }
    }

    fn call_local_tool<'a>(
        &'a self,
        call: &'a mcp_proxy::ToolCall,
    ) -> Pin<Box<dyn Future<Output = Option<serde_json::Value>> + Send + 'a>> {
        let app = self.app.clone();
        let call = call.clone();
        Box::pin(async move {
            let command = marker_command_for_tool(&call.tool)?;
            // Logic 2 wins a name collision. The merged tool list already suppresses ours
            // in that case, so answering it here anyway would hand the agent an
            // implementation whose schema it was never shown.
            let shadowed_by_upstream = app
                .state::<AppState>()
                .mcp_upstream_tools
                .lock()
                .map(|names| names.contains(&call.tool))
                .unwrap_or(false);
            if shadowed_by_upstream {
                return None;
            }
            let arguments = call
                .arguments
                .as_object()
                .cloned()
                .unwrap_or_else(serde_json::Map::new);
            match call_renderer(&app, command, &arguments).await {
                Ok(payload) => Some(marker_tool_result(&payload)),
                // A transport failure is still this tool's answer, not a reason to fall
                // through to Logic 2 -- which has no such tool and would reject it with
                // a less useful message.
                Err(error) => Some(serde_json::json!({
                    "content": [{ "type": "text", "text": error }],
                    "isError": true,
                })),
            }
        })
    }

    fn review<'a>(
        &'a self,
        context: &'a mcp_proxy::ObservationContext,
        call: &'a mcp_proxy::ToolCall,
    ) -> Pin<Box<dyn Future<Output = mcp_proxy::Verdict> + Send + 'a>> {
        let app = self.app.clone();
        let context = context.clone();
        let call = call.clone();
        Box::pin(async move {
            let reason = match mcp_tool_policy(&call.tool) {
                McpToolPolicy::Allow => return mcp_proxy::Verdict::Allow,
                McpToolPolicy::Review(reason) => reason,
            };
            let state = app.state::<AppState>();
            let already_allowed = state
                .mcp_approvals
                .lock()
                .map(|approvals| approvals.is_session_allowed(&context, &call.tool))
                .unwrap_or(false);
            if already_allowed {
                return mcp_proxy::Verdict::Allow;
            }
            let Some((request, receiver)) =
                state.mcp_approvals.lock().ok().map(|mut approvals| {
                    approvals.create(&context, &call, reason, unix_time_millis())
                })
            else {
                return mcp_proxy::Verdict::Deny("无法建立 MCP 审批请求".to_string());
            };
            show_mcp_window(&app);
            let _ = app.emit("mcp-approval", &request);
            match tokio::time::timeout(Duration::from_secs(30), receiver).await {
                Ok(Ok(verdict)) => verdict,
                _ => {
                    if let Ok(mut approvals) = app.state::<AppState>().mcp_approvals.lock() {
                        approvals.expire(request.approval_id);
                    }
                    let _ = app.emit(
                        "mcp-approval-resolved",
                        McpApprovalResolution {
                            approval_id: request.approval_id,
                            outcome: "timeout".to_string(),
                        },
                    );
                    mcp_proxy::Verdict::Deny(
                        "MCP 工具调用等待确认超时（30 秒），已拒绝".to_string(),
                    )
                }
            }
        })
    }

    fn observe_session_closed(&self, context: &mcp_proxy::ObservationContext) {
        let Some(session_id) = context.session_id.as_deref() else {
            return;
        };
        if let Ok(mut approvals) = self.app.state::<AppState>().mcp_approvals.lock() {
            approvals.close_session(session_id);
        }
        let changed = self
            .app
            .state::<AppState>()
            .mcp_activity
            .lock()
            .map(|mut store| store.close_session(session_id, unix_time_millis()))
            .unwrap_or_default();
        for activity in changed {
            let _ = self.app.emit("mcp-activity", activity);
        }
    }
}

/// Starts the proxy on its own thread with its own runtime.
///
/// A dedicated runtime rather than Tauri's: the proxy needs tokio's IO and timer drivers,
/// and depending on which features Tauri happens to enable for its own runtime would make
/// this fragile for no benefit.
fn start_mcp_proxy(app: &AppHandle) {
    let app = app.clone();
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                append_log(&app, "client", &format!("MCP 代理无法启动运行时: {error}"));
                return;
            }
        };
        runtime.block_on(async move {
            let settings = load_settings(&app).mcp;
            let (listener, listen_port) = match mcp_proxy::bind_listener(settings.listen_port).await
            {
                Ok(bound) => bound,
                Err(error) => {
                    append_log(&app, "client", &format!("MCP 代理无法监听端口: {error}"));
                    return;
                }
            };
            let ports = mcp_proxy::BoundPorts {
                requested_listen_port: settings.listen_port,
                listen_port,
                upstream_port: settings.upstream_port,
            };
            if let Ok(mut state) = app.state::<AppState>().mcp.lock() {
                *state = Some(McpRuntimeState { ports });
            }
            append_log(
                &app,
                "client",
                &format!(
                    "MCP 代理就绪: http://127.0.0.1:{} -> 127.0.0.1:{}{}",
                    ports.listen_port,
                    ports.upstream_port,
                    if ports.fell_back() {
                        format!("（首选端口 {} 被占用）", ports.requested_listen_port)
                    } else {
                        String::new()
                    },
                ),
            );
            let poll_app = app.clone();
            let upstream_port = ports.upstream_port;
            tokio::spawn(async move {
                // Emitted only when the answer changes, so an idle app is silent.
                let mut previous: Option<bool> = None;
                loop {
                    let reachable = mcp_proxy::upstream_reachable(upstream_port).await;
                    if previous != Some(reachable) {
                        previous = Some(reachable);
                        let status = mcp_status_snapshot(&poll_app, reachable);
                        let _ = poll_app.emit("mcp-status", status);
                    }
                    tokio::time::sleep(MCP_UPSTREAM_POLL).await;
                }
            });
            let observer = Arc::new(TauriProxyObserver { app: app.clone() });
            let proxy = Arc::new(mcp_proxy::ProxyRuntime::new(ports.upstream_port, observer));
            mcp_proxy::serve(listener, proxy).await;
        });
    });
}

#[tauri::command]
async fn mcp_status(app: AppHandle) -> McpStatus {
    let upstream_port = load_settings(&app).mcp.upstream_port;
    let reachable = mcp_proxy::upstream_reachable(upstream_port).await;
    mcp_status_snapshot(&app, reachable)
}

#[tauri::command]
fn mcp_activity_snapshot(state: tauri::State<'_, AppState>) -> McpActivitySnapshot {
    let mut snapshot = state
        .mcp_activity
        .lock()
        .map(|store| store.snapshot())
        .unwrap_or(McpActivitySnapshot {
            activities: Vec::new(),
            tools: Vec::new(),
            approvals: Vec::new(),
        });
    snapshot.approvals = state
        .mcp_approvals
        .lock()
        .map(|approvals| approvals.pending_requests())
        .unwrap_or_default();
    snapshot
}

#[tauri::command]
fn mcp_approval_resolve(
    app: AppHandle,
    approval_id: u64,
    allow: bool,
    remember: bool,
) -> Result<(), String> {
    let request = app
        .state::<AppState>()
        .mcp_approvals
        .lock()
        .map_err(|_| "MCP 审批状态不可用".to_string())?
        .resolve(approval_id, allow, remember)
        .ok_or_else(|| "该 MCP 审批已处理或超时".to_string())?;
    let _ = app.emit(
        "mcp-approval-resolved",
        McpApprovalResolution {
            approval_id: request.approval_id,
            outcome: if allow { "allowed" } else { "denied" }.to_string(),
        },
    );
    Ok(())
}

fn setup_app(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    // The proxy is started before the tray so a failure to listen is logged while the
    // app is still coming up, rather than looking like a later fault.
    start_mcp_proxy(&app.handle().clone());
    setup_tray(app)
}

fn setup_tray(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let menu = MenuBuilder::new(app)
        .text("show", "显示 PXLogic Bridge")
        .text("status", "显示状态面板")
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
            "status" => show_status_panel(app),
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
            logic_running_instances,
            logic_close_instances,
            pxlogic_scan,
            bridge_start,
            bridge_restart,
            bridge_stop,
            diagnostics_export,
            logs_open,
            manual_open,
            status_panel_show,
            status_panel_hide,
            status_panel_toggle,
            status_panel_set_collapsed,
            status_panel_begin_move,
            status_panel_move,
            status_panel_end_move,
            status_panel_dock_edge,
            status_panel_fit_height,
            status_panel_set_threshold,
            mcp_status,
            mcp_activity_snapshot,
            mcp_approval_resolve,
            mcp_window_show,
            mcp_window_hide,
            mcp_window_begin_move,
            mcp_window_move,
            mcp_window_end_move,
            mcp_set_auto_show,
            status_panel_intro_acknowledge,
            status_panel_set_auto_show,
            onboarding_complete,
            main_window_show,
        ])
        .setup(setup_app)
        .on_window_event(|window, event| {
            if matches!(event, WindowEvent::Moved(_)) && window.label() == "status" {
                if let Some(status) = window.app_handle().get_webview_window("status") {
                    schedule_status_panel_settle(&status);
                }
                return;
            }
            if matches!(event, WindowEvent::Moved(_)) && window.label() == "mcp" {
                if let Some(mcp) = window.app_handle().get_webview_window("mcp") {
                    schedule_mcp_window_settle(&mcp);
                }
                return;
            }
            if let WindowEvent::CloseRequested { api, .. } = event {
                if !window
                    .app_handle()
                    .state::<AppState>()
                    .quitting
                    .load(Ordering::Acquire)
                {
                    api.prevent_close();
                    if window.label() == "status" {
                        hide_status_panel(window.app_handle());
                    } else if window.label() == "mcp" {
                        hide_mcp_window(window.app_handle());
                    } else {
                        hide_main_window(window.app_handle());
                    }
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
    fn mcp_request_and_response_share_one_activity_and_capture_real_tool_schema() {
        let mut store = McpActivityStore::default();
        let request_context = mcp_proxy::ObservationContext::default();
        let response_context = mcp_proxy::ObservationContext {
            session_id: Some("logic-session".to_string()),
        };
        let pending = store
            .record_request(
                &request_context,
                br#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}"#,
                10,
            )
            .unwrap();
        let update = store.record_response(
            &response_context,
            br#"{"jsonrpc":"2.0","id":7,"result":{"tools":[{"name":"capture_info","description":"Read capture","inputSchema":{"type":"object","properties":{"verbose":{"type":"boolean"}}},"annotations":{"readOnlyHint":true}}]}}"#,
            20,
        );

        let completed = update.activity.unwrap();
        assert_eq!(completed.sequence, pending.sequence);
        assert_eq!(completed.state, "completed");
        assert_eq!(completed.session_id.as_deref(), Some("logic-session"));
        assert_eq!(store.activities.len(), 1);
        assert!(store.pending.is_empty());
        let tools = update.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "capture_info");
        assert_eq!(tools[0].input_schema["type"], "object");
        assert_eq!(tools[0].raw["annotations"]["readOnlyHint"], true);
    }

    #[test]
    fn mcp_tool_policy_gates_data_lifecycle_and_unknown_tools_only() {
        for tool in [
            "get_devices",
            "export_raw_data_csv",
            "add_analyzer",
            "remove_high_level_analyzer",
        ] {
            assert!(
                matches!(mcp_tool_policy(tool), McpToolPolicy::Allow),
                "{tool}"
            );
        }
        for tool in [
            "start_capture",
            "load_capture",
            "stop_capture",
            "close_capture",
            "future_logic_tool",
        ] {
            assert!(
                matches!(mcp_tool_policy(tool), McpToolPolicy::Review(_)),
                "{tool}"
            );
        }
    }

    #[test]
    fn every_marker_tool_is_classified_rather_than_left_unknown() {
        // The unknown branch gates by default, which for an annotation would mean a
        // confirmation dialog per note. Each name has to be recognised on purpose.
        for tool in marker_tool_definitions() {
            let name = tool["name"].as_str().unwrap();
            assert!(
                matches!(mcp_tool_policy(name), McpToolPolicy::Allow),
                "{name} fell through to the gate"
            );
            assert!(marker_command_for_tool(name).is_some(), "{name}");
        }
    }

    #[test]
    fn the_marker_tools_declare_the_facts_an_agent_cannot_guess() {
        let definitions = marker_tool_definitions();
        assert_eq!(definitions.len(), 5);
        let add = definitions
            .iter()
            .find(|tool| tool["name"] == "add_timing_marker")
            .expect("add_timing_marker");
        let description = add["description"].as_str().unwrap();
        // Where zero is, and that a capture must already exist.
        assert!(description.contains("from the start"), "{description}");
        assert!(description.contains("active capture"), "{description}");
        assert_eq!(add["inputSchema"]["required"][0], "timeSec");
        assert!(add["inputSchema"]["properties"]["note"].is_object());

        // Both writing tools have to say that Logic 2 refuses to annotate a running
        // capture. An agent that reads it on one and not the other concludes the quieter
        // one is unrestricted and retries against it.
        for name in ["add_timing_marker", "add_timing_marker_pair"] {
            let tool = definitions
                .iter()
                .find(|tool| tool["name"] == name)
                .expect(name);
            let description = tool["description"].as_str().unwrap();
            assert!(description.contains("finished"), "{name}: {description}");
        }

        // A pair is an interval, so both ends are required and the reply is about duration.
        let pair = definitions
            .iter()
            .find(|tool| tool["name"] == "add_timing_marker_pair")
            .expect("add_timing_marker_pair");
        let required: Vec<&str> = pair["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["startSec", "endSec"]);
        assert!(pair["description"].as_str().unwrap().contains("duration"));

        // Listing has to name both shapes, or an agent reads a capture holding only pairs
        // as an empty one.
        let list = definitions
            .iter()
            .find(|tool| tool["name"] == "list_timing_markers")
            .expect("list_timing_markers");
        let description = list["description"].as_str().unwrap();
        assert!(description.contains("markers"), "{description}");
        assert!(description.contains("pairs"), "{description}");
        assert!(description.contains("durationSec"), "{description}");
    }

    #[test]
    fn every_advertised_marker_colour_is_one_logic_2_can_actually_render() {
        // `MarkerManager.color` is a key into Logic 2's colour map and the sidebar renders
        // `darkColors[color]`. An unknown name resolves to undefined and the colour is
        // silently dropped, so advertising one is advertising a no-op. These names were
        // taken from that map in Logic 2 2.4.46's own sources; the first six are the
        // palette it cycles through for new markers.
        let renderable = [
            "paleRed",
            "green2",
            "purple2",
            "orange2",
            "fuchsia",
            "lightBlue",
            "red",
            "green",
            "orange",
            "purple",
            "yellow",
        ];
        let mut checked = 0;
        for tool in marker_tool_definitions() {
            let Some(color) = tool["inputSchema"]["properties"].get("color") else {
                continue;
            };
            let advertised = color["enum"].as_array().expect("colour enum");
            assert!(!advertised.is_empty());
            for value in advertised {
                let name = value.as_str().unwrap();
                assert!(
                    renderable.contains(&name),
                    "{name} is not a colour Logic 2 renders"
                );
            }
            checked += 1;
        }
        // Both writing tools take a colour; a silent zero here would pass vacuously.
        assert_eq!(checked, 2);
    }

    #[test]
    fn a_marker_failure_is_reported_as_a_tool_error_the_agent_can_act_on() {
        // Not a JSON-RPC error: the call reached the tool, and the tool has something to
        // say. An agent that hears "start a capture first" can do that.
        let payload = serde_json::json!({
            "type": "timing-marker-result",
            "requestId": "mk1",
            "ok": false,
            "error": "Logic 2 has no active capture session; start or load a capture first",
        });
        let result = marker_tool_result(&payload);
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("start or load a capture"));
    }

    #[test]
    fn a_marker_success_reports_the_marker_without_the_transport_fields() {
        let payload = serde_json::json!({
            "type": "timing-marker-result",
            "requestId": "mk2",
            "ok": true,
            "marker": { "id": 4, "timeSec": 1.5, "note": "SPI framing error" },
        });
        let result = marker_tool_result(&payload);
        assert!(result.get("isError").is_none());
        let text = result["content"][0]["text"].as_str().unwrap();
        let reported: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(reported["marker"]["id"], 4);
        assert_eq!(reported["marker"]["note"], "SPI framing error");
        // The correlation id and envelope are ours, not the agent's business.
        assert!(reported.get("requestId").is_none());
        assert!(reported.get("ok").is_none());
        assert!(reported.get("type").is_none());
    }

    #[test]
    fn a_result_without_an_ok_flag_is_treated_as_a_failure() {
        let result = marker_tool_result(&serde_json::json!({}));
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn a_timing_marker_result_carries_the_request_id_it_answers() {
        let event = parse_bridge_runtime_event(
            r#"[logic2-bridge:event] {"type":"timing-marker-result","requestId":"mk7","ok":true,"marker":{"id":2}}"#,
        )
        .expect("event");
        assert_eq!(event.event_type, "timing-marker-result");
        assert_eq!(event.request_id.as_deref(), Some("mk7"));
        assert_eq!(event.ok, Some(true));
    }

    #[test]
    fn mcp_approval_can_allow_a_tool_for_only_its_current_session() {
        let mut approvals = McpApprovalStore::default();
        let context = mcp_proxy::ObservationContext {
            session_id: Some("session-a".to_string()),
        };
        let call = mcp_proxy::ToolCall {
            id: serde_json::json!(4),
            tool: "start_capture".to_string(),
            arguments: serde_json::json!({"duration": 1}),
        };
        let (request, mut receiver) = approvals.create(&context, &call, "risk".to_string(), 100);
        assert_eq!(request.expires_at_ms, 30_100);
        approvals.resolve(request.approval_id, true, true).unwrap();
        assert_eq!(receiver.try_recv().unwrap(), mcp_proxy::Verdict::Allow);
        assert!(approvals.is_session_allowed(&context, "start_capture"));
        assert!(!approvals.is_session_allowed(
            &mcp_proxy::ObservationContext {
                session_id: Some("session-b".to_string())
            },
            "start_capture"
        ));
        approvals.close_session("session-a");
        assert!(!approvals.is_session_allowed(&context, "start_capture"));
    }

    #[test]
    fn mcp_errors_and_session_close_update_the_original_pending_activity() {
        let mut store = McpActivityStore::default();
        let context = mcp_proxy::ObservationContext {
            session_id: Some("session-a".to_string()),
        };
        let failed = store
            .record_request(
                &context,
                br#"{"jsonrpc":"2.0","id":"failed","method":"tools/call","params":{"name":"read","arguments":{"channel":1}}}"#,
                1,
            )
            .unwrap();
        let update = store.record_response(
            &context,
            br#"{"jsonrpc":"2.0","id":"failed","error":{"code":-1,"message":"no"}}"#,
            2,
        );
        assert_eq!(update.activity.unwrap().sequence, failed.sequence);
        assert_eq!(store.activities.back().unwrap().state, "error");

        let pending = store
            .record_request(
                &context,
                br#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"read"}}"#,
                3,
            )
            .unwrap();
        let closed = store.close_session("session-a", 4);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].sequence, pending.sequence);
        assert_eq!(closed[0].state, "sessionClosed");
    }

    #[test]
    fn malformed_observations_are_ignored_and_activity_history_is_bounded() {
        let mut store = McpActivityStore::default();
        let context = mcp_proxy::ObservationContext::default();
        assert!(store.record_request(&context, b"not json", 0).is_none());
        assert!(store
            .record_response(&context, b"not json", 0)
            .activity
            .is_none());
        for index in 0..(MAX_MCP_ACTIVITIES + 5) {
            let body = format!(
                r#"{{"jsonrpc":"2.0","method":"notifications/progress","params":{{"progress":{index}}}}}"#
            );
            store.record_request(&context, body.as_bytes(), index as u64);
        }
        assert_eq!(store.activities.len(), MAX_MCP_ACTIVITIES);
        assert_eq!(store.activities.front().unwrap().sequence, 6);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn discovers_logic_bundle_when_parent_directory_is_selected() {
        let root = std::env::temp_dir().join(format!(
            "pxlogic-logic-bundle-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let logic = root.join("Saleae Logic.app");
        let other = root.join("Other.app");
        fs::create_dir_all(logic.join("Contents")).unwrap();
        fs::create_dir_all(other.join("Contents")).unwrap();
        fs::write(
            logic.join("Contents/Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.saleae.saleae</string></dict></plist>"#,
        )
        .unwrap();
        fs::write(
            other.join("Contents/Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.example.other</string></dict></plist>"#,
        )
        .unwrap();

        let candidates = logic_app_candidates_from_path(&root);
        assert_eq!(candidates, vec![logic.clone()]);
        assert!(is_logic_bundle(&logic));
        assert!(!is_logic_bundle(&other));
        assert_eq!(paths_for_logic_scan(&logic), vec![logic]);

        fs::remove_dir_all(root).unwrap();
    }

    const PRIMARY_WORK_AREA: PanelRect = PanelRect {
        x: 0,
        y: 25,
        width: 1920,
        height: 1055,
    };

    fn status_panel_rect(x: i32, y: i32) -> PanelRect {
        PanelRect {
            x,
            y,
            width: 340,
            height: 390,
        }
    }

    #[test]
    fn keeps_a_remembered_panel_position_that_is_still_reachable() {
        let panel = status_panel_rect(1200, 400);
        assert_eq!(
            clamp_panel_position(panel, &[PRIMARY_WORK_AREA]),
            (1200, 400),
            "a fully visible panel must never be nudged"
        );

        // Hanging off the right edge with 70 px still showing is enough to grab.
        let panel = status_panel_rect(1850, 400);
        assert_eq!(
            clamp_panel_position(panel, &[PRIMARY_WORK_AREA]),
            (1850, 400)
        );

        // 20 px is not, so the panel is pulled back inside.
        let panel = status_panel_rect(1900, 400);
        assert_eq!(
            clamp_panel_position(panel, &[PRIMARY_WORK_AREA]),
            (1580, 400),
            "a sliver of a 340 px panel is not a usable grab target"
        );
    }

    #[test]
    fn pulls_an_unreachable_panel_position_back_onto_a_work_area() {
        // Saved while a second display was attached to the right; that display is
        // gone, so the panel would open outside every work area.
        let panel = status_panel_rect(3000, 500);
        assert_eq!(
            clamp_panel_position(panel, &[PRIMARY_WORK_AREA]),
            (1580, 500),
            "the panel must come back to the right edge of the surviving display"
        );

        // Dragged almost entirely above the menu bar.
        let panel = status_panel_rect(400, -380);
        assert_eq!(clamp_panel_position(panel, &[PRIMARY_WORK_AREA]), (400, 25));
    }

    #[test]
    fn honours_a_secondary_display_left_of_the_primary() {
        let secondary = PanelRect {
            x: -1440,
            y: 0,
            width: 1440,
            height: 900,
        };
        let panel = status_panel_rect(-1200, 300);
        assert_eq!(
            clamp_panel_position(panel, &[PRIMARY_WORK_AREA, secondary]),
            (-1200, 300),
            "negative coordinates are valid on a left-hand display"
        );

        // Beyond the left edge of every display, so the panel returns to the
        // preferred fallback rather than the nearest edge: the primary display is
        // where the user is looking.
        let panel = status_panel_rect(-2000, 300);
        assert_eq!(
            clamp_panel_position(panel, &[PRIMARY_WORK_AREA, secondary]),
            (0, 300)
        );
        // Ordering decides the fallback, which is why panel_work_areas hoists the
        // primary monitor to the front.
        assert_eq!(
            clamp_panel_position(panel, &[secondary, PRIMARY_WORK_AREA]),
            (-1440, 300)
        );
    }

    #[test]
    fn panel_position_survives_a_panel_larger_than_the_work_area() {
        let tiny = PanelRect {
            x: 0,
            y: 0,
            width: 200,
            height: 200,
        };
        let panel = status_panel_rect(900, 900);
        assert_eq!(
            clamp_panel_position(panel, &[tiny]),
            (0, 0),
            "a panel that cannot fit is aligned to the work-area origin"
        );

        // Without monitor information the saved value is the best guess there is.
        assert_eq!(clamp_panel_position(panel, &[]), (900, 900));
    }

    #[test]
    fn defaults_the_panel_to_the_top_right_of_the_work_area() {
        assert_eq!(
            default_panel_position(340, PRIMARY_WORK_AREA),
            (1556, 49),
            "an always-on-top monitor belongs out of the way, not where the OS puts it"
        );
    }

    #[test]
    fn status_panel_settings_default_to_auto_show_at_no_remembered_position() {
        let settings = StatusPanelSettings::default();
        assert!(settings.auto_show);
        assert!(!settings.collapsed);
        assert_eq!(settings.position, None);
    }

    #[test]
    fn status_panel_settings_round_trip_through_the_config_file() {
        let stored = serde_json::to_string(&ClientSettings {
            status_panel: StatusPanelSettings {
                position: Some(PanelPosition { x: 1556, y: 49 }),
                collapsed: true,
                auto_show: false,
            },
            ..ClientSettings::default()
        })
        .unwrap();
        let restored: ClientSettings = serde_json::from_str(&stored).unwrap();
        let restored = restored.normalized();
        assert_eq!(
            restored.status_panel.position,
            Some(PanelPosition { x: 1556, y: 49 })
        );
        assert!(restored.status_panel.collapsed);
        assert!(!restored.status_panel.auto_show);
    }

    #[test]
    fn settings_written_before_the_panel_existed_still_load() {
        // Every field added for the guidance work must be optional, otherwise an
        // upgrade would silently reset the user's whole configuration.
        let legacy = r#"{
            "logicAppPath": "/Applications/Saleae Logic.app",
            "portMode": "auto",
            "preferredPort": 12472,
            "screenQuadrant": 3
        }"#;
        let settings: ClientSettings = serde_json::from_str(legacy).unwrap();
        let settings = settings.normalized();
        assert_eq!(settings.guidance.onboarding_completed_version, 0);
        assert!(!settings.guidance.status_panel_intro_seen);
        assert!(settings.status_panel.auto_show);
        assert_eq!(settings.status_panel.position, None);
    }

    #[test]
    fn a_future_onboarding_version_is_clamped_so_the_walkthrough_can_replay() {
        let settings = ClientSettings {
            guidance: GuidanceSettings {
                onboarding_completed_version: ONBOARDING_VERSION + 7,
                status_panel_intro_seen: true,
            },
            ..ClientSettings::default()
        }
        .normalized();
        assert_eq!(
            settings.guidance.onboarding_completed_version,
            ONBOARDING_VERSION
        );
    }

    #[test]
    fn snaps_a_settled_panel_to_every_work_area_edge() {
        let snap = |x, y| snap_to_work_area(status_panel_rect(x, y), PRIMARY_WORK_AREA, 16);

        // Left, top, right, bottom in turn.
        assert_eq!(snap(9, 400), (0, 400));
        assert_eq!(snap(400, 34), (400, 25));
        assert_eq!(snap(1570, 400), (1580, 400));
        assert_eq!(snap(400, 682), (400, 690));

        // Both axes at once lands the panel in the corner.
        assert_eq!(snap(12, 33), (0, 25));
        assert_eq!(snap(1572, 685), (1580, 690));
    }

    #[test]
    fn leaves_a_panel_alone_when_it_settles_away_from_an_edge() {
        let panel = status_panel_rect(700, 400);
        assert_eq!(snap_to_work_area(panel, PRIMARY_WORK_AREA, 16), (700, 400));

        // Exactly one pixel past the threshold on both axes.
        let panel = status_panel_rect(17, 42);
        assert_eq!(snap_to_work_area(panel, PRIMARY_WORK_AREA, 16), (17, 42));
    }

    #[test]
    fn never_snaps_an_axis_the_panel_cannot_fit_on() {
        let narrow = PanelRect {
            x: 0,
            y: 0,
            width: 200,
            height: 200,
        };
        let panel = status_panel_rect(5, 5);
        assert_eq!(
            snap_to_work_area(panel, narrow, 16),
            (5, 5),
            "snapping a panel wider than the work area has no meaningful edge"
        );
    }

    #[test]
    fn snapping_follows_the_display_showing_most_of_the_panel() {
        let secondary = PanelRect {
            x: -1440,
            y: 0,
            width: 1440,
            height: 900,
        };
        let areas = [PRIMARY_WORK_AREA, secondary];

        // Mostly on the secondary display, near its left edge.
        let panel = status_panel_rect(-1430, 400);
        let area = work_area_under_panel(panel, &areas).unwrap();
        assert_eq!(area, secondary);
        assert_eq!(snap_to_work_area(panel, area, 16), (-1440, 400));

        // Straddling the seam but mostly on the primary.
        let panel = status_panel_rect(-40, 400);
        assert_eq!(
            work_area_under_panel(panel, &areas).unwrap(),
            PRIMARY_WORK_AREA
        );

        // Off every display, so there is nothing to snap to.
        let panel = status_panel_rect(-5000, 400);
        assert_eq!(work_area_under_panel(panel, &areas), None);
    }

    const COLLAPSED_CHIP: (i32, i32) = (168, 44);
    const EXPANDED_PANEL: (i32, i32) = (340, 340);

    fn resize(at: (i32, i32), from: (i32, i32), to: (i32, i32)) -> (PanelRect, PanelRect) {
        (
            PanelRect {
                x: at.0,
                y: at.1,
                width: from.0,
                height: from.1,
            },
            PanelRect {
                x: at.0,
                y: at.1,
                width: to.0,
                height: to.1,
            },
        )
    }

    fn expand(at: (i32, i32), areas: &[PanelRect]) -> (i32, i32) {
        let (anchor, resized) = resize(at, COLLAPSED_CHIP, EXPANDED_PANEL);
        place_resized_panel(anchor, resized, areas)
    }

    #[test]
    fn expanding_a_chip_parked_at_an_edge_grows_inwards() {
        let areas = [PRIMARY_WORK_AREA];

        // Flush against the right edge: the readout has to grow leftwards.
        assert_eq!(expand((1752, 400), &areas), (1580, 400));
        // Flush against the bottom edge: upwards.
        assert_eq!(expand((400, 1036), &areas), (400, 740));
        // A corner does both at once.
        assert_eq!(expand((1752, 1036), &areas), (1580, 740));
        // The top-left corner has room in both directions, so nothing moves.
        assert_eq!(expand((0, 25), &areas), (0, 25));
    }

    #[test]
    fn expanding_near_an_edge_pulls_the_whole_readout_back_on_screen() {
        let areas = [PRIMARY_WORK_AREA];

        // The chip sits well clear of the snap threshold, so it is not treated as
        // parked, but expanding in place would still hang 120 px off the display.
        assert_eq!(expand((1700, 400), &areas), (1580, 400));
        assert_eq!(expand((400, 900), &areas), (400, 740));

        // One pixel of overflow is still overflow.
        assert_eq!(expand((1581, 400), &areas), (1580, 400));
    }

    #[test]
    fn expanding_leaves_a_panel_with_room_exactly_where_it_was() {
        let areas = [PRIMARY_WORK_AREA];
        assert_eq!(expand((700, 400), &areas), (700, 400));
        assert_eq!(expand((1580, 740), &areas), (1580, 740));
    }

    #[test]
    fn collapsing_from_an_edge_keeps_the_chip_against_that_edge() {
        let areas = [PRIMARY_WORK_AREA];
        let collapse = |at| {
            let (anchor, resized) = resize(at, EXPANDED_PANEL, COLLAPSED_CHIP);
            place_resized_panel(anchor, resized, &areas)
        };

        // Expanding from the right edge and collapsing again has to return the chip
        // to the same corner, or every toggle walks it further inwards.
        assert_eq!(collapse((1580, 400)), (1752, 400));
        assert_eq!(collapse((400, 740)), (400, 1036));
        assert_eq!(collapse((1580, 740)), (1752, 1036));

        // Away from any edge the origin is what stays put.
        assert_eq!(collapse((700, 400)), (700, 400));
    }

    #[test]
    fn expanding_beside_another_display_stays_on_the_one_the_chip_is_on() {
        let secondary = PanelRect {
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let areas = [PRIMARY_WORK_AREA, secondary];

        // The chip is flush against the primary's right edge. The expanded shape
        // overlaps the secondary display more than the primary, so choosing the
        // display from the grown rectangle would throw the panel onto the wrong
        // monitor; it is chosen from where the chip actually sits.
        assert_eq!(expand((1752, 400), &areas), (1580, 400));

        // A chip genuinely on the secondary display expands there.
        assert_eq!(expand((3672, 400), &areas), (3500, 400));
    }

    #[test]
    fn a_panel_too_large_for_its_display_starts_at_the_corner() {
        let cramped = PanelRect {
            x: 0,
            y: 25,
            width: 320,
            height: 200,
        };
        // Neither axis can fit, so the panel starts at the corner and the user
        // resizes or moves it from there.
        assert_eq!(expand((100, 100), &[cramped]), (0, 25));
    }

    #[test]
    fn a_resize_with_no_display_to_measure_against_stays_put() {
        let (anchor, resized) = resize((700, 400), COLLAPSED_CHIP, EXPANDED_PANEL);
        assert_eq!(place_resized_panel(anchor, resized, &[]), (700, 400));
    }

    fn drag(to: (i32, i32), cursor: (i32, i32), areas: &[PanelRect]) -> (i32, i32) {
        let panel = PanelRect {
            x: to.0,
            y: to.1,
            width: EXPANDED_PANEL.0,
            height: EXPANDED_PANEL.1,
        };
        confine_dragged_panel(panel, cursor, areas)
    }

    #[test]
    fn dragging_the_panel_at_an_edge_stops_it_going_off_screen() {
        let areas = [PRIMARY_WORK_AREA];

        // Pushed past each edge in turn, the panel comes to rest against it with
        // the whole readout still on screen.
        assert_eq!(drag((1700, 400), (1800, 500), &areas), (1580, 400));
        assert_eq!(drag((-50, 400), (100, 500), &areas), (0, 400));
        assert_eq!(drag((400, 900), (500, 1000), &areas), (400, 740));

        // The macOS menu bar is outside the work area, so a cursor up there
        // matches no display and the panel falls back to the one it overlaps.
        assert_eq!(drag((400, -30), (500, 10), &areas), (400, 25));

        // With room to spare the panel goes exactly where it was dragged.
        assert_eq!(drag((700, 400), (800, 500), &areas), (700, 400));
    }

    #[test]
    fn dragging_across_a_seam_follows_the_cursor_onto_the_other_display() {
        let secondary = PanelRect {
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let areas = [PRIMARY_WORK_AREA, secondary];

        // Still on the primary: confined to it, so the panel cannot spill over the
        // seam and be cut in half.
        assert_eq!(drag((1850, 400), (1900, 450), &areas), (1580, 400));

        // The cursor crosses the seam, so the panel follows it onto the secondary
        // display rather than sticking to the primary's edge.
        assert_eq!(drag((1850, 400), (1960, 450), &areas), (1920, 400));
    }

    #[test]
    fn dragging_with_no_display_to_measure_against_stays_put() {
        assert_eq!(drag((700, 400), (800, 500), &[]), (700, 400));
    }

    #[test]
    fn the_panel_reports_resting_on_the_bottom_edge() {
        let areas = [PRIMARY_WORK_AREA];
        let at = |y| {
            panel_docked_at_bottom(
                PanelRect {
                    x: 700,
                    y,
                    width: EXPANDED_PANEL.0,
                    height: EXPANDED_PANEL.1,
                },
                &areas,
            )
        };

        // Flush against the bottom, which is where confinement leaves a panel that
        // was dragged there.
        assert!(at(740));
        // Within the shared snap tolerance on either side of flush.
        assert!(at(724));
        assert!(at(756));
        // One pixel beyond it is not docked.
        assert!(!at(723));
        // Nowhere near, and hard against the top.
        assert!(!at(400));
        assert!(!at(25));
    }

    #[test]
    fn a_collapsed_chip_on_the_bottom_edge_is_docked_too() {
        let areas = [PRIMARY_WORK_AREA];
        let chip = |y| {
            panel_docked_at_bottom(
                PanelRect {
                    x: 700,
                    y,
                    width: COLLAPSED_CHIP.0,
                    height: COLLAPSED_CHIP.1,
                },
                &areas,
            )
        };

        // Expanding a chip parked on the bottom edge keeps the panel on that edge,
        // so both shapes have to agree about being docked or the layout would flip
        // back and forth as the panel is collapsed and expanded.
        assert!(chip(1036));
        assert!(!chip(400));
    }

    #[test]
    fn orientation_follows_where_the_chip_will_expand_to() {
        let areas = [PRIMARY_WORK_AREA];
        let project = |y| {
            project_expanded_panel(
                PanelRect {
                    x: 700,
                    y,
                    width: COLLAPSED_CHIP.0,
                    height: COLLAPSED_CHIP.1,
                },
                EXPANDED_PANEL,
                &areas,
            )
        };

        // A chip resting 36 px clear of the bottom edge is not itself docked, but
        // expanding it is clamped flush to that edge. Judging the orientation by the
        // chip would open the panel with its header at the top and then move it,
        // which is seen as the header jumping across the panel after it is drawn.
        let chip_at_1000 = PanelRect {
            x: 700,
            y: 1000,
            width: COLLAPSED_CHIP.0,
            height: COLLAPSED_CHIP.1,
        };
        assert!(!panel_docked_at_bottom(chip_at_1000, &areas));
        assert_eq!(project(1000).y, 740);
        assert!(panel_docked_at_bottom(project(1000), &areas));

        // Flush already: the projection lands on the same place rather than moving.
        assert_eq!(project(1036).y, 740);
        assert!(panel_docked_at_bottom(project(1036), &areas));

        // Well clear of the edge, the projection stays put and reports no dock.
        assert_eq!(project(400).y, 400);
        assert!(!panel_docked_at_bottom(project(400), &areas));
    }

    #[test]
    fn tells_an_adoptable_logic_window_from_everything_else_that_shares_its_binary() {
        const LOGIC: &str = "/Applications/Saleae Logic.app/Contents/MacOS/Logic";
        // Real shapes seen on macOS: a window the Bridge started, one the user
        // started, the Bridge itself running through the same binary in Node mode,
        // Chromium's helper children, and an unrelated process.
        let listing = format!(
            "\
  501 {LOGIC} --useExistingGraph --graphPort 63602 --start-maximized
  502 {LOGIC}
  503 {LOGIC} index.cjs --app /Applications/Saleae Logic.app --port auto
  504 {LOGIC} --type=renderer --graphPort 63602
  505 {LOGIC}Helper --type=gpu-process
  506 /usr/bin/something else
  507 {LOGIC} --graphPort notaport
"
        );
        let instances = parse_logic_instances(&listing, LOGIC, 999);

        assert_eq!(
            instances
                .iter()
                .map(|instance| (instance.pid, instance.graph_port))
                .collect::<Vec<_>>(),
            vec![(501, Some(63602)), (502, None), (507, None)],
            "only browser processes count, and only a real port makes one adoptable"
        );

        // The Bridge must never mistake itself for a window to close.
        let instances = parse_logic_instances(&listing, LOGIC, 502);
        assert!(
            !instances.iter().any(|instance| instance.pid == 502),
            "the current process is excluded"
        );
    }

    #[test]
    fn an_ordinary_settings_save_cannot_wipe_backend_owned_state() {
        // The main window never renders the panel geometry or the walkthrough
        // flags, so its save payload omits them and serde fills in defaults.
        // Without the merge, moving the panel and then changing any setting in the
        // main window would silently forget where the panel was.
        let current = ClientSettings {
            guidance: GuidanceSettings {
                onboarding_completed_version: ONBOARDING_VERSION,
                status_panel_intro_seen: true,
            },
            status_panel: StatusPanelSettings {
                position: Some(PanelPosition { x: 1556, y: 49 }),
                collapsed: true,
                auto_show: false,
            },
            ..ClientSettings::default()
        };
        let from_renderer: ClientSettings = serde_json::from_str(
            r#"{
                "logicAppPath": "/Applications/Saleae Logic.app",
                "portMode": "auto",
                "preferredPort": 12472,
                "screenQuadrant": 3
            }"#,
        )
        .unwrap();
        assert_eq!(
            from_renderer.status_panel.position, None,
            "the renderer payload really does omit the panel state"
        );

        let merged = merge_backend_owned_settings(from_renderer, &current).normalized();
        assert_eq!(
            merged.status_panel.position,
            Some(PanelPosition { x: 1556, y: 49 })
        );
        assert!(merged.status_panel.collapsed);
        assert!(!merged.status_panel.auto_show);
        assert_eq!(
            merged.guidance.onboarding_completed_version,
            ONBOARDING_VERSION
        );
        assert!(merged.guidance.status_panel_intro_seen);
        // The renderer still owns everything it does send.
        assert_eq!(merged.logic_app_path, "/Applications/Saleae Logic.app");
    }

    #[test]
    fn reveals_the_panel_only_when_the_bridge_goes_live_and_it_is_hidden() {
        assert!(should_auto_show_status_panel("running", true, false));

        // Already visible: revealing again would fight a user who positioned it.
        assert!(!should_auto_show_status_panel("running", true, true));
        // Opted out.
        assert!(!should_auto_show_status_panel("running", false, false));
        // Every other phase, including the transient ones on the way to running.
        for phase in ["stopped", "starting", "stopping", "recovery", "error"] {
            assert!(
                !should_auto_show_status_panel(phase, true, false),
                "{phase} must not reveal the panel"
            );
        }
    }

    #[test]
    fn the_window_config_floor_allows_the_collapsed_chip() {
        // Collapsing sets the inner size to the chip dimensions. If either
        // configured minimum ever rises above them the panel silently refuses to
        // collapse, which a unit test of the resize code alone cannot see.
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let status = config["app"]["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|window| window["label"] == "status")
            .expect("the status panel window must exist");
        let min_width = status["minWidth"].as_f64().unwrap();
        let min_height = status["minHeight"].as_f64().unwrap();
        assert!(
            min_width <= STATUS_PANEL_COLLAPSED_WIDTH,
            "configured minWidth {min_width} blocks the {STATUS_PANEL_COLLAPSED_WIDTH} px chip"
        );
        assert!(
            min_height <= STATUS_PANEL_COLLAPSED_HEIGHT,
            "configured minHeight {min_height} blocks the {STATUS_PANEL_COLLAPSED_HEIGHT} px chip"
        );
        // The chip only earns its keep by being much smaller than the readout.
        assert!(STATUS_PANEL_COLLAPSED_HEIGHT < STATUS_PANEL_EXPANDED_MIN_HEIGHT);
        assert!(STATUS_PANEL_COLLAPSED_WIDTH < STATUS_PANEL_MIN_WIDTH);
        // Expanding re-applies the real minimum, so the default must satisfy it.
        assert!(STATUS_PANEL_EXPANDED_WIDTH >= STATUS_PANEL_MIN_WIDTH);
        assert!(STATUS_PANEL_EXPANDED_HEIGHT >= STATUS_PANEL_EXPANDED_MIN_HEIGHT);

        // The panel is deliberately shown without focus while Logic 2 owns it, and
        // macOS otherwise swallows the first click on an inactive window just to
        // activate it. Without this the chip needs two clicks to expand, which is
        // the very problem the chip exists to solve.
        assert_eq!(
            status["acceptFirstMouse"], true,
            "the collapsed chip must expand on the first click"
        );
        // A native titlebar would duplicate the panel's own collapse and hide
        // controls and would dwarf the chip, so the panel draws its own chrome.
        assert_eq!(
            status["decorations"], false,
            "the panel owns its window controls"
        );
    }

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
            pxlogic_firmware_id: String::new(),
            guidance: GuidanceSettings::default(),
            status_panel: StatusPanelSettings::default(),
            mcp: McpSettings::default(),
            pending_profile_fingerprint: None,
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
    fn keeps_pxlogic_hardware_plan_for_the_compact_status_panel() {
        let mut telemetry = CaptureTelemetry::default();
        let plan = parse_bridge_runtime_event(
            r#"[logic2-bridge:event] {"type":"capture-plan","logicSampleRateHz":125000000,"sampleRateHz":50000000,"enabledChannels":[0,1,2,3],"channelSpan":4,"pxlogicUsbSpeed":"super","pxlogicLogicMode":0,"mode":"STREAM_LOGIC500x4","modePhysicalChannels":4,"effectiveSampleRateHz":50000000,"modeMaxSampleRateHz":500000000,"supported":true}"#,
        )
        .unwrap();

        assert!(apply_capture_runtime_event(&mut telemetry, &plan));
        assert_eq!(telemetry.status, "configured");
        assert_eq!(telemetry.logic_sample_rate_hz, Some(125_000_000));
        assert_eq!(telemetry.sample_rate_hz, Some(50_000_000));
        assert_eq!(telemetry.pxlogic_usb_speed.as_deref(), Some("super"));
        assert_eq!(telemetry.pxlogic_mode.as_deref(), Some("STREAM_LOGIC500x4"));
        assert_eq!(telemetry.pxlogic_mode_physical_channels, Some(4));
        assert_eq!(telemetry.pxlogic_effective_sample_rate_hz, Some(50_000_000));
        assert_eq!(telemetry.pxlogic_mode_max_sample_rate_hz, Some(500_000_000));
        assert_eq!(telemetry.pxlogic_supported, Some(true));
    }

    #[test]
    fn a_logic_rate_downgrade_replaces_the_plan_it_invalidated() {
        // Logic 2 drops the sample rate on its own once enough channels are enabled
        // for the hardware to refuse the combination, and it sends the channel change
        // first and the new rate about a millisecond later. The middle step is
        // therefore always a plan the hardware cannot serve, and none of it may
        // survive into the plan that follows.
        let plan = |json: &str| parse_bridge_runtime_event(json).unwrap();
        let mut telemetry = CaptureTelemetry::default();

        // Four channels at 500 MHz: the fastest mode, and fine.
        assert!(apply_capture_runtime_event(
            &mut telemetry,
            &plan(
                r#"[logic2-bridge:event] {"type":"capture-plan","logicSampleRateHz":500000000,"enabledChannels":[0,1,2,3],"channelSpan":4,"mode":"STREAM_LOGIC500x4","modePhysicalChannels":4,"effectiveSampleRateHz":500000000,"modeMaxSampleRateHz":500000000,"supported":true,"reason":null}"#
            )
        ));
        assert_eq!(telemetry.pxlogic_mode.as_deref(), Some("STREAM_LOGIC500x4"));
        assert_eq!(telemetry.pxlogic_supported, Some(true));

        // Four more channels arrive while the rate is still 500 MHz.
        assert!(apply_capture_runtime_event(
            &mut telemetry,
            &plan(
                r#"[logic2-bridge:event] {"type":"capture-plan","logicSampleRateHz":500000000,"enabledChannels":[0,1,2,3,4,5,6,7],"channelSpan":8,"mode":"STREAM_LOGIC250x8","modePhysicalChannels":8,"effectiveSampleRateHz":250000000,"modeMaxSampleRateHz":250000000,"supported":false,"reason":"请求采样率超过 STREAM_LOGIC250x8 上限"}"#
            )
        ));
        assert_eq!(telemetry.pxlogic_supported, Some(false));
        assert!(telemetry.pxlogic_reason.is_some());

        // Logic settles on 250 MHz, which the hardware can serve.
        assert!(apply_capture_runtime_event(
            &mut telemetry,
            &plan(
                r#"[logic2-bridge:event] {"type":"capture-plan","logicSampleRateHz":250000000,"enabledChannels":[0,1,2,3,4,5,6,7],"channelSpan":8,"mode":"STREAM_LOGIC250x8","modePhysicalChannels":8,"effectiveSampleRateHz":250000000,"modeMaxSampleRateHz":250000000,"supported":true,"reason":null}"#
            )
        ));
        assert_eq!(telemetry.logic_sample_rate_hz, Some(250_000_000));
        assert_eq!(
            telemetry.pxlogic_effective_sample_rate_hz,
            Some(250_000_000)
        );
        assert_eq!(telemetry.enabled_channels, vec![0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(telemetry.pxlogic_supported, Some(true));
        // The warning belonged to a plan that no longer exists.
        assert_eq!(telemetry.pxlogic_reason, None);
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
    fn pending_profile_authorization_is_explicit_and_transient() {
        let inspection = LogicInspection {
            path: "/Applications/Saleae Logic.app".to_string(),
            version: Some("2.4.46".to_string()),
            supported: false,
            runnable: true,
            error: None,
            node_version: Some("22.0.0".to_string()),
            electron_version: Some("33.0.0".to_string()),
            profile_id: Some("logic-candidate".to_string()),
            graph_path: None,
            graph_format: None,
            graph_identity_kind: Some("macho-lc-uuid".to_string()),
            graph_identity: Some("ABC".to_string()),
            graph_sha256: Some("DEF".to_string()),
            hook_status: Some("candidate".to_string()),
        };
        assert!(requires_pending_profile_authorization(&inspection));

        assert!(has_pending_profile_authorization(&inspection, Some("DEF")));
        assert!(!has_pending_profile_authorization(
            &inspection,
            Some("stale")
        ));
        assert!(!has_pending_profile_authorization(&inspection, None));

        for status in ["pending-live-validation", "locally-verified"] {
            let mut pending = inspection.clone();
            pending.hook_status = Some(status.to_string());
            assert!(requires_pending_profile_authorization(&pending));
            assert!(has_pending_profile_authorization(&pending, Some("def")));
        }
        let mut unsupported = inspection.clone();
        unsupported.runnable = false;
        unsupported.hook_status = Some("unsupported".to_string());
        assert!(!requires_pending_profile_authorization(&unsupported));
        assert!(!has_pending_profile_authorization(
            &unsupported,
            Some("DEF")
        ));
        let mut missing_fingerprint = inspection.clone();
        missing_fingerprint.graph_sha256 = None;
        assert!(!has_pending_profile_authorization(
            &missing_fingerprint,
            Some("DEF")
        ));

        let mut verified = inspection.clone();
        verified.supported = true;
        verified.hook_status = Some("verified".to_string());
        assert!(!requires_pending_profile_authorization(&verified));

        let mut settings = ClientSettings::default();
        settings.pending_profile_fingerprint = Some("DEF".to_string());
        let serialized = serde_json::to_value(settings).unwrap();
        assert!(!serialized
            .as_object()
            .unwrap()
            .contains_key("pendingProfileFingerprint"));
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
    fn bridge_payload_contract_covers_direct_node_dependencies() {
        assert!(BRIDGE_NODE_RUNTIME_FILES.contains(&"lib/diagnostics.cjs"));
        assert!(BRIDGE_NODE_RUNTIME_FILES.contains(&"lib/graph-action-guard.cjs"));

        let bridge_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        for relative in BRIDGE_NODE_RUNTIME_FILES {
            assert!(
                bridge_root.join(relative).is_file(),
                "Bridge payload contract references missing source file: {relative}"
            );
        }
    }

    fn repository_firmware_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../resources/firmware")
    }

    #[test]
    fn firmware_manifest_is_well_formed_and_defaults_to_the_latest_image() {
        let manifest = mcu_firmware_manifest().expect("firmware manifest");
        assert_eq!(manifest.schema_version, 1);

        let latest = manifest
            .releases
            .iter()
            .filter(|release| release.latest)
            .collect::<Vec<_>>();
        assert_eq!(latest.len(), 1, "exactly one image may be marked latest");
        assert_eq!(
            latest[0].id, manifest.default,
            "the default selection must be the latest image"
        );
        assert_eq!(default_pxlogic_firmware_id(), manifest.default);

        let mut ids = manifest
            .releases
            .iter()
            .map(|release| release.id.as_str())
            .collect::<Vec<_>>();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "firmware ids must be unique");

        let mut versions = manifest
            .releases
            .iter()
            .map(|release| release.firmware_version.as_str())
            .collect::<Vec<_>>();
        versions.sort_unstable();
        versions.dedup();
        assert_eq!(
            versions.len(),
            total,
            "firmware versions must be unique so a device register identifies the image"
        );
    }

    #[test]
    fn every_manifest_firmware_image_ships_and_matches_its_digest() {
        let firmware_dir = repository_firmware_dir();
        for release in mcu_firmware_manifest().expect("firmware manifest").releases {
            let path = firmware_dir.join(&release.file_name);
            assert!(
                path.is_file(),
                "firmware manifest references a missing image: {}",
                path.display()
            );
            verify_mcu_firmware_image(&path, &release).unwrap_or_else(|error| {
                panic!("{} does not match the manifest: {error}", release.file_name)
            });
        }
    }

    #[test]
    fn payload_contract_requires_every_selectable_firmware_image() {
        let root = PathBuf::from("/payload/bridge");
        let firmware_dir = PathBuf::from("/payload/resources/firmware");
        let required = bridge_payload_required_paths(
            &root,
            &PathBuf::from("/payload/bridge/build/graph-host"),
            &PathBuf::from("/payload/target/release/usb_smoke"),
            &PathBuf::from("/payload/resources/bitstreams"),
            &firmware_dir,
        );
        assert!(required.contains(&firmware_dir.join("releases.json")));
        for release in mcu_firmware_releases() {
            assert!(
                required.contains(&firmware_dir.join(&release.file_name)),
                "{} must be part of the payload contract",
                release.file_name
            );
        }
    }

    #[test]
    fn unknown_firmware_selection_falls_back_to_the_latest_image() {
        let latest = default_pxlogic_firmware_id();
        for stored in ["", "   ", "pxview-0.0.0", "../../etc/passwd"] {
            let settings = ClientSettings {
                pxlogic_firmware_id: stored.to_string(),
                ..ClientSettings::default()
            }
            .normalized();
            assert_eq!(
                settings.pxlogic_firmware_id, latest,
                "{stored:?} must normalise back to the latest image"
            );
        }
    }

    #[test]
    fn a_known_firmware_selection_is_preserved() {
        let older = mcu_firmware_releases()
            .into_iter()
            .find(|release| !release.latest)
            .expect("at least one older image is offered");
        let settings = ClientSettings {
            pxlogic_firmware_id: older.id.clone(),
            ..ClientSettings::default()
        }
        .normalized();
        assert_eq!(settings.pxlogic_firmware_id, older.id);
        assert_ne!(settings.pxlogic_firmware_id, default_pxlogic_firmware_id());
    }

    #[test]
    fn a_corrupt_firmware_image_is_rejected_before_it_reaches_the_device() {
        let release = mcu_firmware_releases()
            .into_iter()
            .find(|release| release.latest)
            .expect("latest image");
        let directory =
            std::env::temp_dir().join(format!("pxlogic-firmware-verify-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("temp directory");

        let truncated = directory.join("truncated.bin");
        fs::write(&truncated, vec![0u8; release.byte_length as usize - 1]).expect("write");
        let error = verify_mcu_firmware_image(&truncated, &release).expect_err("length mismatch");
        assert!(error.contains("长度"), "{error}");

        let substituted = directory.join("substituted.bin");
        fs::write(&substituted, vec![0u8; release.byte_length as usize]).expect("write");
        let error = verify_mcu_firmware_image(&substituted, &release).expect_err("digest mismatch");
        assert!(error.contains("SHA-256"), "{error}");

        let genuine = repository_firmware_dir().join(&release.file_name);
        assert!(verify_mcu_firmware_image(&genuine, &release).is_ok());

        let _ = fs::remove_dir_all(&directory);
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
