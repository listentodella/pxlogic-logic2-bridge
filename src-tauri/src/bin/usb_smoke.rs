use std::{
    cell::{Cell, RefCell},
    env,
    error::Error,
    fs,
    io::{self, BufRead, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use pxlogic_core::{
    capture::{decode_cross_data_with_map, read_sample_word},
    enabled_channel_mask, resolve_enabled_channels, CaptureBackend, CaptureMode, CaptureProgress,
    CaptureSettings, CaptureTriggerKind, CaptureTriggerMetadata, DeviceInfo, DiagnosticEvent,
    DiagnosticLevel, ExternalTriggerMode, PwmSettings, RusbBackend,
};
use pxlogic_graph::{GraphFrameRequest, GraphSession};
use pxlogic_waveform::WaveformRequest;

#[derive(Debug)]
struct Options {
    device_id: Option<String>,
    sample_rate_hz: u64,
    duration_ms: u64,
    channel_count: u8,
    enabled_channels: Option<Vec<u8>>,
    decode_cross: bool,
    compare_mappings: bool,
    threshold_volts: f64,
    external_trigger_mode: ExternalTriggerMode,
    clock_negedge: bool,
    trigger_out_enabled: bool,
    mode: CaptureMode,
    buffer_size_mb: u64,
    list_only: bool,
    list_json: bool,
    prepare_only: bool,
    skip_prepare: bool,
    flash_mcu: bool,
    output: Option<PathBuf>,
    trigger_channel: Option<u8>,
    trigger_kind: CaptureTriggerKind,
    trigger_high_mask: u32,
    trigger_low_mask: u32,
    glitch_filter_enabled: bool,
    cancel_after_ms: Option<u64>,
    graph_smoke: bool,
    pwm_configure: bool,
    pwm_enabled: bool,
    pwm_frequency_hz: f64,
    pwm_duty_percent: f64,
    pwm_only: bool,
    /// Emit line-delimited JSON events while the USB capture is still running.
    ///
    /// This is deliberately kept on the existing `usb_smoke` binary so the
    /// Electron bridge and the Tauri application use precisely the same USB,
    /// FPGA setup, lane-transpose, and decoded sample path.
    live_protocol: bool,
    /// In live mode, emit packed LA_CROSS_DATA events without also serializing
    /// the much larger time-major preview stream.
    live_cross_only: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            device_id: None,
            sample_rate_hz: 25_000_000,
            duration_ms: 10,
            channel_count: 16,
            enabled_channels: None,
            decode_cross: true,
            compare_mappings: false,
            threshold_volts: 2.0,
            external_trigger_mode: ExternalTriggerMode::Close,
            clock_negedge: false,
            trigger_out_enabled: false,
            mode: CaptureMode::Buffer,
            buffer_size_mb: 16,
            list_only: false,
            list_json: false,
            prepare_only: false,
            skip_prepare: false,
            flash_mcu: false,
            output: None,
            trigger_channel: None,
            trigger_kind: CaptureTriggerKind::Rising,
            trigger_high_mask: 0,
            trigger_low_mask: 0,
            glitch_filter_enabled: false,
            cancel_after_ms: None,
            graph_smoke: false,
            pwm_configure: false,
            pwm_enabled: false,
            pwm_frequency_hz: 1_000.0,
            pwm_duty_percent: 50.0,
            pwm_only: false,
            live_protocol: false,
            live_cross_only: false,
        }
    }
}

fn emit_live_event(value: serde_json::Value) {
    // `println!` is not guaranteed to flush when stdout is a pipe. The
    // renderer must receive data while the capture is in progress, so flush
    // after every event instead of waiting for the helper to exit.
    println!("{value}");
    let _ = io::stdout().flush();
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse_options()?;
    let backend = RusbBackend::default();
    let devices = backend.list_devices()?;
    if options.list_json {
        println!("{}", serde_json::to_string(&devices)?);
        return Ok(());
    }
    if devices.is_empty() {
        if options.list_only {
            println!("devices: none");
            return Ok(());
        }
        return Err("no PXLogic USB devices found".into());
    }

    println!("devices:");
    for device in &devices {
        print_device(device);
    }
    if options.list_only {
        return Ok(());
    }

    let device_id = match &options.device_id {
        Some(device_id) => device_id,
        None => {
            &devices
                .iter()
                .find(|device| device.ready)
                .unwrap_or(&devices[0])
                .id
        }
    };
    println!("selected: {device_id}");
    if let Some(selected) = devices.iter().find(|device| device.id == *device_id) {
        if !selected.ready {
            return Err(format!(
                "selected device is not ready: {}",
                selected
                    .probe_error
                    .as_deref()
                    .unwrap_or("PX identity probe did not complete")
            )
            .into());
        }
    }

    let mut trace = |event: DiagnosticEvent| {
        let level = match event.level {
            DiagnosticLevel::Info => "info",
            DiagnosticLevel::Warn => "warn",
            DiagnosticLevel::Error => "error",
        };
        println!("[{}:{level}] {}", event.phase, event.message);
    };

    if options.flash_mcu {
        let firmware = RusbBackend::load_mcu_firmware()?
            .ok_or("missing MCU firmware; expected resources/firmware/SCI_LOGIC.bin")?;
        backend.flash_mcu_firmware_with_trace(device_id, &firmware, &mut trace)?;
        println!("MCU firmware update submitted; rediscover the re-enumerated device, then run capture again");
        return Ok(());
    }

    if options.skip_prepare {
        println!("[prepare:info] reusing FPGA state prepared by the Bridge session");
    } else {
        let bitstreams = RusbBackend::load_bitstreams()?
            .ok_or("missing FPGA bitstreams; expected resources/bitstreams/*.bin")?;
        backend.prepare_device_with_trace(device_id, Some(&bitstreams), &mut trace)?;
    }
    if options.pwm_configure || options.pwm_only {
        let configuration = backend.configure_pwm0_with_trace(
            device_id,
            &PwmSettings {
                enabled: options.pwm_enabled,
                frequency_hz: options.pwm_frequency_hz,
                duty_percent: options.pwm_duty_percent,
            },
            &mut trace,
        )?;
        println!(
            "pwm0: enabled={}, requested={:.3} Hz at {:.3}%, effective={:.3} Hz at {:.3}%, period_ticks={}, high_ticks={}",
            configuration.enabled,
            configuration.requested_frequency_hz,
            configuration.requested_duty_percent,
            configuration.effective_frequency_hz,
            configuration.effective_duty_percent,
            configuration.period_ticks,
            configuration.high_ticks,
        );
    }
    if options.pwm_only {
        println!("PWM0 configuration completed without starting capture");
        return Ok(());
    }
    if options.prepare_only {
        println!("prepare completed");
        return Ok(());
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_timer = options.cancel_after_ms.map(|delay_ms| {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay_ms));
            cancel.store(true, Ordering::Release);
        })
    });
    if options.live_protocol {
        // The renderer writes a single `stop` line to stdin when the user
        // presses Logic's stop button. This sets the same cancellation flag
        // used by the USB read loop, so the FPGA stream is stopped cleanly
        // rather than relying on process termination.
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines().map_while(Result::ok) {
                if line.trim().eq_ignore_ascii_case("stop") {
                    cancel.store(true, Ordering::Release);
                    break;
                }
            }
        });
    }
    let mut progress = |event: CaptureProgress| {
        println!(
            "[progress] bytes={}/{} samples={}",
            event.bytes_read, event.bytes_expected, event.samples_read
        );
        if options.live_protocol {
            emit_live_event(serde_json::json!({
                "type": "progress",
                "bytesRead": event.bytes_read,
                "bytesExpected": event.bytes_expected,
                "samplesRead": event.samples_read,
                "sampleMemoryBytes": event.sample_memory_bytes,
            }));
        }
    };
    let settings = CaptureSettings {
        device_id: device_id.to_string(),
        sample_rate_hz: options.sample_rate_hz,
        channel_count: options.channel_count,
        enabled_channels: options.enabled_channels.clone().unwrap_or_default(),
        duration_ms: options.duration_ms,
        buffer_size_mb: options.buffer_size_mb,
        decode_cross: options.decode_cross && !options.compare_mappings,
        threshold_volts: options.threshold_volts,
        external_trigger_mode: options.external_trigger_mode,
        mode: options.mode,
        trigger_enabled: options.trigger_channel.is_some(),
        trigger_channel: options.trigger_channel.unwrap_or(0),
        trigger_kind: options.trigger_kind,
        trigger_high_mask: options.trigger_high_mask,
        trigger_low_mask: options.trigger_low_mask,
        glitch_filter_enabled: options.glitch_filter_enabled,
        clock_edge: options.clock_negedge,
        trigger_out_enabled: options.trigger_out_enabled,
        ..CaptureSettings::default()
    };
    let requested_enabled_channels =
        resolve_enabled_channels(settings.channel_count, &settings.enabled_channels)?;
    let requested_channel_mask =
        enabled_channel_mask(settings.channel_count, &requested_enabled_channels)?;
    println!(
        "requested channels: physical_span={}, enabled={requested_enabled_channels:?}, lanes={}, mask=0x{requested_channel_mask:08x}",
        settings.channel_count,
        requested_enabled_channels.len(),
    );
    let graph = options.graph_smoke.then(GraphSession::default);
    let capture = if let Some(graph) = graph.as_ref() {
        let graph_error = RefCell::new(None::<String>);
        let streamed_chunks = Cell::new(0u64);
        let streamed_bytes = Cell::new(0u64);
        let mut started = |metadata: &pxlogic_core::CaptureMetadata| {
            if let Err(error) = graph.begin_capture(metadata.clone()) {
                *graph_error.borrow_mut() = Some(error.to_string());
            }
        };
        let mut samples = |chunk: &[u8]| {
            if graph_error.borrow().is_some() {
                return;
            }
            match graph.append_samples(chunk) {
                Ok(_) => {
                    streamed_chunks.set(streamed_chunks.get().saturating_add(1));
                    streamed_bytes.set(
                        streamed_bytes
                            .get()
                            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX)),
                    );
                }
                Err(error) => *graph_error.borrow_mut() = Some(error.to_string()),
            }
        };
        let mut triggered = |trigger: &CaptureTriggerMetadata| {
            println!(
                "[trigger] D{} {:?} at sample {}",
                trigger.channel, trigger.kind, trigger.sample_index
            );
            if let Err(error) = graph.set_trigger(trigger.clone()) {
                *graph_error.borrow_mut() = Some(error.to_string());
            }
        };
        let capture = backend.capture_with_trace_streaming(
            &settings,
            cancel.as_ref(),
            &mut progress,
            &mut trace,
            &mut started,
            &mut samples,
            &mut |_, _| {},
            &mut triggered,
        )?;
        if let Some(error) = graph_error.into_inner() {
            return Err(format!("graph streaming failed: {error}").into());
        }
        println!(
            "graph stream: {} chunks, {} decoded bytes, {} indexed samples",
            streamed_chunks.get(),
            streamed_bytes.get(),
            graph.sample_count()
        );
        graph.finish_capture(capture.clone())?;
        verify_graph_smoke(graph, &capture)?;
        capture
    } else if options.live_protocol {
        // Stream mode is backed by a finite PXLogic buffer. Keep the USB
        // session alive by re-arming the same stream window until the
        // renderer writes `stop` to stdin. Every event carries a monotonic
        // sample offset, so the Electron bridge can treat the windows as one
        // continuous capture.
        let mut window_index = 0u64;
        let mut total_sample_count = 0u64;
        let mut total_decoded_byte_length = 0u64;
        let mut total_raw_byte_length = 0u64;
        let mut session_metadata = None;
        let mut last_capture = None;

        loop {
            if cancel.load(Ordering::Acquire) && last_capture.is_some() {
                break;
            }

            let sample_offset = total_sample_count;
            let raw_byte_offset = total_raw_byte_length;
            let current_window = window_index;
            let first_window = current_window == 0;
            let mut started = |metadata: &pxlogic_core::CaptureMetadata| {
                let event_type = if first_window { "started" } else { "window" };
                emit_live_event(serde_json::json!({
                    "type": event_type,
                    "windowIndex": current_window,
                    "sampleOffset": sample_offset,
                    "metadata": metadata,
                }));
            };
            let mut samples = |chunk: &[u8]| {
                if options.live_cross_only {
                    return;
                }
                emit_live_event(serde_json::json!({
                    "type": "samples",
                    "windowIndex": current_window,
                    "sampleOffset": sample_offset,
                    "encoding": "base64",
                    "data": BASE64.encode(chunk),
                    "byteLength": chunk.len(),
                    "sampleCount": chunk.len() / 2,
                }));
            };
            let mut cross_lanes = |enabled_channels: &[u8], chunk: &[u8]| {
                emit_live_event(serde_json::json!({
                    "type": "cross",
                    "windowIndex": current_window,
                    "sampleOffset": sample_offset,
                    "rawByteOffset": raw_byte_offset,
                    "encoding": "base64",
                    "enabledChannels": enabled_channels,
                    "data": BASE64.encode(chunk),
                    "byteLength": chunk.len(),
                }));
            };
            let mut live_progress = |event: CaptureProgress| {
                println!(
                    "[progress] window={} bytes={}/{} samples={}",
                    current_window, event.bytes_read, event.bytes_expected, event.samples_read
                );
                emit_live_event(serde_json::json!({
                    "type": "progress",
                    "windowIndex": current_window,
                    "bytesRead": raw_byte_offset.saturating_add(event.bytes_read),
                    "bytesExpected": raw_byte_offset.saturating_add(event.bytes_expected),
                    "samplesRead": sample_offset.saturating_add(event.samples_read),
                    "sampleMemoryBytes": event.sample_memory_bytes,
                }));
            };
            let mut triggered = |trigger: &CaptureTriggerMetadata| {
                let mut global_trigger = trigger.clone();
                global_trigger.sample_index =
                    global_trigger.sample_index.saturating_add(sample_offset);
                emit_live_event(serde_json::json!({
                    "type": "trigger",
                    "windowIndex": current_window,
                    "trigger": global_trigger,
                }));
            };

            let capture = backend.capture_with_trace_streaming(
                &settings,
                cancel.as_ref(),
                &mut live_progress,
                &mut trace,
                &mut started,
                &mut samples,
                &mut cross_lanes,
                &mut triggered,
            )?;

            if session_metadata.is_none() {
                session_metadata = Some(capture.metadata.clone());
            }
            let window_sample_count = capture.metadata.sample_count;
            let window_decoded_bytes = capture.samples.len() as u64;
            let window_raw_bytes = window_sample_count
                .saturating_mul(capture.metadata.enabled_channels.len() as u64)
                / 8;
            total_sample_count = total_sample_count.saturating_add(window_sample_count);
            total_decoded_byte_length =
                total_decoded_byte_length.saturating_add(window_decoded_bytes);
            total_raw_byte_length = total_raw_byte_length.saturating_add(window_raw_bytes);
            last_capture = Some(capture);
            window_index = window_index.saturating_add(1);

            if cancel.load(Ordering::Acquire) {
                break;
            }
        }

        let mut capture =
            last_capture.ok_or("PXLogic live capture stopped before any samples arrived")?;
        if let Some(mut metadata) = session_metadata {
            metadata.sample_count = total_sample_count;
            capture.metadata = metadata;
        }
        emit_live_event(serde_json::json!({
            "type": "session",
            "windowCount": window_index,
            "sampleCount": total_sample_count,
            "decodedByteLength": total_decoded_byte_length,
            "rawByteLength": total_raw_byte_length,
        }));
        capture
    } else {
        backend.capture_with_trace(&settings, cancel.as_ref(), &mut progress, &mut trace)?
    };
    cancel.store(true, Ordering::Release);
    if let Some(timer) = cancel_timer {
        let _ = timer.join();
    }

    println!(
        "capture: {} Hz, physical_span={} channels, enabled={:?}, {} samples, unitsize={}, {} bytes",
        capture.metadata.sample_rate_hz,
        capture.metadata.channel_count,
        capture.metadata.enabled_channels,
        capture.metadata.sample_count,
        capture.metadata.unitsize,
        if options.live_protocol {
            capture
                .metadata
                .sample_count
                .saturating_mul(u64::from(capture.metadata.unitsize))
        } else {
            capture.samples.len() as u64
        }
    );
    if options.live_protocol {
        let finished_metadata = capture.metadata.clone();
        let finished_decoded_byte_length = finished_metadata
            .sample_count
            .saturating_mul(u64::from(finished_metadata.unitsize));
        emit_live_event(serde_json::json!({
            "type": "finished",
            "metadata": finished_metadata,
            "decodedByteLength": finished_decoded_byte_length,
        }));
    }
    if capture.metadata.enabled_channels != requested_enabled_channels {
        return Err(format!(
            "capture enabled-channel mismatch: requested={requested_enabled_channels:?}, metadata={:?}",
            capture.metadata.enabled_channels
        )
        .into());
    }
    if options.compare_mappings {
        compare_cross_mappings(&capture)?;
    } else if !options.live_protocol {
        print_sample_summary(&capture);
    }

    if !options.live_protocol {
        let output = options.output.unwrap_or_else(default_output_path);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        pxlogic_file::save_pxcap(&output, &capture)?;
        println!("saved: {}", output.display());
    }

    Ok(())
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut options = Options::default();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => options.device_id = Some(next_arg(&mut args, "--device")?),
            "--rate" => options.sample_rate_hz = next_arg(&mut args, "--rate")?.parse()?,
            "--ms" => options.duration_ms = next_arg(&mut args, "--ms")?.parse()?,
            "--channels" => options.channel_count = next_arg(&mut args, "--channels")?.parse()?,
            "--enabled-channels" => {
                options.enabled_channels = Some(parse_enabled_channels(&next_arg(
                    &mut args,
                    "--enabled-channels",
                )?)?)
            }
            "--vth" => options.threshold_volts = next_arg(&mut args, "--vth")?.parse()?,
            "--external-trigger" => {
                options.external_trigger_mode =
                    parse_external_trigger_mode(&next_arg(&mut args, "--external-trigger")?)?
            }
            "--clock-negedge" => options.clock_negedge = true,
            "--trigger-out" => options.trigger_out_enabled = true,
            "--mode" => options.mode = parse_mode(&next_arg(&mut args, "--mode")?)?,
            "--buffer-mb" => {
                options.buffer_size_mb = next_arg(&mut args, "--buffer-mb")?.parse()?
            }
            "--trigger-channel" => {
                options.trigger_channel = Some(next_arg(&mut args, "--trigger-channel")?.parse()?)
            }
            "--trigger" => {
                options.trigger_kind = parse_trigger_kind(&next_arg(&mut args, "--trigger")?)?
            }
            "--trigger-high-mask" => {
                options.trigger_high_mask =
                    parse_u32_mask(&next_arg(&mut args, "--trigger-high-mask")?)?
            }
            "--trigger-low-mask" => {
                options.trigger_low_mask =
                    parse_u32_mask(&next_arg(&mut args, "--trigger-low-mask")?)?
            }
            "--glitch-filter" => options.glitch_filter_enabled = true,
            "--cancel-after-ms" => {
                options.cancel_after_ms = Some(next_arg(&mut args, "--cancel-after-ms")?.parse()?)
            }
            "--graph-smoke" => options.graph_smoke = true,
            "--pwm-enable" => {
                options.pwm_configure = true;
                options.pwm_enabled = true;
            }
            "--pwm-frequency" => {
                options.pwm_configure = true;
                options.pwm_frequency_hz = next_arg(&mut args, "--pwm-frequency")?.parse()?;
            }
            "--pwm-duty" => {
                options.pwm_configure = true;
                options.pwm_duty_percent = next_arg(&mut args, "--pwm-duty")?.parse()?;
            }
            "--pwm-only" => options.pwm_only = true,
            "--live" => {
                options.live_protocol = true;
                options.mode = CaptureMode::Stream;
                // A live session is ended by the renderer's `stop` command.
                // This bounded fallback keeps a lost renderer from leaving a
                // USB stream open indefinitely.
                options.duration_ms = 60_000;
            }
            "--live-cross-only" => options.live_cross_only = true,
            "--out" => options.output = Some(PathBuf::from(next_arg(&mut args, "--out")?)),
            "--list-only" => options.list_only = true,
            "--list-json" => options.list_json = true,
            "--prepare-only" => options.prepare_only = true,
            "--skip-prepare" => options.skip_prepare = true,
            "--flash-mcu" => options.flash_mcu = true,
            "--raw-cross" => options.decode_cross = false,
            "--decode-cross" => options.decode_cross = true,
            "--compare-mappings" => options.compare_mappings = true,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    if options.prepare_only && options.skip_prepare {
        return Err("--prepare-only and --skip-prepare cannot be used together".into());
    }
    Ok(options)
}

fn next_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| format!("{flag} requires a value").into())
}

fn parse_mode(value: &str) -> Result<CaptureMode, Box<dyn Error>> {
    match value.to_ascii_lowercase().as_str() {
        "buffer" => Ok(CaptureMode::Buffer),
        "stream" => Ok(CaptureMode::Stream),
        other => Err(format!("unknown capture mode: {other}; expected buffer or stream").into()),
    }
}

fn parse_u32_mask(value: &str) -> Result<u32, Box<dyn Error>> {
    let normalized = value.trim();
    if let Some(hex) = normalized
        .strip_prefix("0x")
        .or_else(|| normalized.strip_prefix("0X"))
    {
        return Ok(u32::from_str_radix(hex, 16)?);
    }
    Ok(normalized.parse()?)
}

fn parse_trigger_kind(value: &str) -> Result<CaptureTriggerKind, Box<dyn Error>> {
    match value.to_ascii_lowercase().as_str() {
        "rising" | "rise" | "r" => Ok(CaptureTriggerKind::Rising),
        "falling" | "fall" | "f" => Ok(CaptureTriggerKind::Falling),
        "high" | "one" | "1" => Ok(CaptureTriggerKind::High),
        "low" | "zero" | "0" => Ok(CaptureTriggerKind::Low),
        other => Err(format!(
            "unknown trigger kind: {other}; expected rising, falling, high, or low"
        )
        .into()),
    }
}

fn parse_external_trigger_mode(value: &str) -> Result<ExternalTriggerMode, Box<dyn Error>> {
    match value.to_ascii_lowercase().as_str() {
        "close" | "closed" | "off" => Ok(ExternalTriggerMode::Close),
        "rising" | "rise" => Ok(ExternalTriggerMode::Rising),
        "one" | "high" | "1" => Ok(ExternalTriggerMode::One),
        "falling" | "fall" => Ok(ExternalTriggerMode::Falling),
        "zero" | "low" | "0" => Ok(ExternalTriggerMode::Zero),
        "edge" | "either" => Ok(ExternalTriggerMode::Edge),
        other => Err(format!(
            "unknown external trigger mode: {other}; expected close, rising, one, falling, zero, or edge"
        )
        .into()),
    }
}

fn parse_enabled_channels(value: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let channels = value
        .split(',')
        .map(str::trim)
        .filter(|channel| !channel.is_empty())
        .map(str::parse::<u8>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if channels.is_empty() {
        return Err("--enabled-channels requires at least one channel index".into());
    }
    Ok(channels)
}

fn print_help() {
    println!(
        "usb_smoke [--list-only|--list-json] [--prepare-only|--skip-prepare] [--flash-mcu] [--device id] [--rate hz] [--ms duration] [--channels n] [--enabled-channels 0,4] [--vth volts] [--external-trigger close|rising|one|falling|zero|edge] [--clock-negedge] [--trigger-out] [--mode buffer|stream] [--buffer-mb mb] [--trigger-channel n] [--trigger rising|falling|high|low] [--trigger-high-mask mask] [--trigger-low-mask mask] [--glitch-filter] [--cancel-after-ms] [--graph-smoke] [--live] [--live-cross-only] [--pwm-frequency hz] [--pwm-duty percent] [--pwm-enable] [--pwm-only] [--raw-cross] [--compare-mappings] [--out path]"
    );
}

fn verify_graph_smoke(
    graph: &GraphSession,
    capture: &pxlogic_core::CaptureData,
) -> Result<(), Box<dyn Error>> {
    let graph_metadata = graph.metadata().ok_or("graph metadata is missing")?;
    if graph_metadata.enabled_channels != capture.metadata.enabled_channels {
        return Err(format!(
            "graph enabled-channel mismatch: graph={:?}, capture={:?}",
            graph_metadata.enabled_channels, capture.metadata.enabled_channels
        )
        .into());
    }
    if graph_metadata.trigger != capture.metadata.trigger {
        return Err(format!(
            "graph trigger mismatch: graph={:?}, capture={:?}",
            graph_metadata.trigger, capture.metadata.trigger
        )
        .into());
    }
    if let Some(trigger) = graph_metadata.trigger {
        println!(
            "graph trigger: D{} {:?} at sample {}",
            trigger.channel, trigger.kind, trigger.sample_index
        );
    }

    if graph.sample_count() != capture.metadata.sample_count {
        return Err(format!(
            "graph sample count mismatch: graph={}, capture={}",
            graph.sample_count(),
            capture.metadata.sample_count
        )
        .into());
    }

    let channels = resolve_enabled_channels(
        capture.metadata.channel_count,
        &capture.metadata.enabled_channels,
    )?;
    let enabled_mask = enabled_channel_mask(capture.metadata.channel_count, &channels)?;
    for sample_index in 0..capture.metadata.sample_count.min(100_000) {
        let word = read_sample_word(&capture.samples, capture.metadata.unitsize, sample_index)
            .ok_or("capture samples ended before metadata sample_count")?;
        if word & !enabled_mask != 0 {
            return Err(format!(
                "sample {sample_index} contains data outside enabled physical mask 0x{enabled_mask:08x}: 0x{word:08x}"
            )
            .into());
        }
    }
    let full_frame = graph.render_frame(&GraphFrameRequest {
        frame_id: 1,
        waveform: WaveformRequest {
            start_sample: 0,
            sample_count: capture.metadata.sample_count.max(1),
            pixels: 1_200,
            channels: channels.clone(),
        },
        analyzer_view: None,
        analyzers: Vec::new(),
    })?;
    if full_frame.sample_count != capture.metadata.sample_count
        || full_frame.tile.sample_count != capture.metadata.sample_count
        || full_frame.tile.channels.len() != channels.len()
    {
        return Err(format!(
            "full graph frame mismatch: frame_samples={}, tile_samples={}, tile_channels={}",
            full_frame.sample_count,
            full_frame.tile.sample_count,
            full_frame.tile.channels.len()
        )
        .into());
    }
    if full_frame.tile.channels.iter().any(|channel| {
        channel
            .packed_bins
            .as_deref()
            .unwrap_or_default()
            .is_empty()
    }) {
        return Err("full graph frame contains an empty packed waveform tile".into());
    }

    let first_edge = full_frame
        .tile
        .channels
        .iter()
        .flat_map(|channel| {
            channel
                .rising_edges
                .iter()
                .chain(&channel.falling_edges)
                .map(move |sample| (channel.channel, *sample))
        })
        .min_by_key(|(_, sample)| *sample);

    if let Some((edge_channel, edge_sample)) = first_edge {
        let exact_start = edge_sample.saturating_sub(512);
        let exact_samples = capture
            .metadata
            .sample_count
            .saturating_sub(exact_start)
            .min(2_048)
            .max(1);
        let exact_frame = graph.render_frame(&GraphFrameRequest {
            frame_id: 2,
            waveform: WaveformRequest {
                start_sample: exact_start,
                sample_count: exact_samples,
                pixels: u32::try_from(exact_samples).unwrap_or(2_048).max(1),
                channels,
            },
            analyzer_view: None,
            analyzers: Vec::new(),
        })?;
        let edge_tile = exact_frame
            .tile
            .channels
            .iter()
            .find(|channel| channel.channel == edge_channel)
            .ok_or("exact graph frame omitted the edge channel")?;
        let edge_is_connected = edge_tile.segments.windows(2).any(|segments| {
            segments[0].end_sample == edge_sample
                && segments[1].start_sample == edge_sample
                && segments[0].high != segments[1].high
        });
        if !edge_is_connected {
            return Err(format!(
                "exact graph frame did not connect the transition at D{edge_channel} sample {edge_sample}"
            )
            .into());
        }
        if edge_tile.segments.windows(2).any(|segments| {
            segments[0].end_sample != segments[1].start_sample
                || segments[0].high == segments[1].high
        }) {
            return Err(format!(
                "exact graph frame contains disconnected or redundant segments on D{edge_channel}"
            )
            .into());
        }
        println!(
            "graph exact: D{edge_channel} edge at sample {edge_sample}, {} connected segments",
            edge_tile.segments.len()
        );
    } else {
        println!("graph exact: capture is static; edge connectivity check skipped");
    }

    println!(
        "graph frame: {} samples, {} channels, {} pixels",
        full_frame.sample_count,
        full_frame.tile.channels.len(),
        full_frame.tile.pixels
    );
    Ok(())
}

fn print_device(device: &DeviceInfo) {
    println!(
        "  {} [{}] vid={:04x} pid={:04x} bus={:?} addr={:?}",
        device.id,
        if device.ready { "ready" } else { "found" },
        device.vid,
        device.pid,
        device.bus,
        device.address
    );
    println!("    label: {}", device.label);
    if let Some(speed) = device.usb_speed.as_deref() {
        println!("    usb_speed: {speed}");
    }
    if let Some(profile) = device.profile_model.as_deref() {
        println!("    pxview_profile: {profile}");
    }
    if let Some(logic_mode) = device.logic_mode {
        println!("    logic_mode: 0x{logic_mode:08x}");
    }
    if let Some(manufacturer) = device.manufacturer.as_deref() {
        println!("    manufacturer: {manufacturer}");
    }
    if let Some(product) = device.product.as_deref() {
        println!("    product: {product}");
    }
    if let Some(serial) = device.serial_number.as_deref() {
        println!("    serial: {serial}");
    }
    if let Some(error) = device.probe_error.as_deref() {
        println!("    probe: {error}");
    }
}

fn default_output_path() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let file_name = format!("usb-smoke-{timestamp}.pxcap");
    env::current_dir()
        .map(|dir| dir.join("debug-captures").join(&file_name))
        .unwrap_or_else(|_| env::temp_dir().join(file_name))
}

fn print_sample_summary(capture: &pxlogic_core::CaptureData) {
    print_decoded_summary(
        &capture.samples,
        capture.metadata.unitsize,
        capture.metadata.channel_count,
        capture.metadata.sample_count,
    );
}

fn print_decoded_summary(samples: &[u8], unitsize: u8, channel_count: u8, sample_count: u64) {
    let inspect_samples = sample_count.min(100_000);
    let mut previous = None;
    let mut highs = vec![0u64; channel_count as usize];
    let mut transitions = vec![0u64; channel_count as usize];
    let mut first_edges = vec![Vec::<u64>::new(); channel_count as usize];

    for sample_index in 0..inspect_samples {
        let Some(word) = read_sample_word(samples, unitsize, sample_index) else {
            break;
        };
        for channel in 0..channel_count {
            if (word & (1u32 << channel)) != 0 {
                highs[channel as usize] += 1;
            }
        }
        if let Some(prev) = previous {
            let changed = prev ^ word;
            for channel in 0..channel_count {
                if (changed & (1u32 << channel)) != 0 {
                    transitions[channel as usize] += 1;
                    let edges = &mut first_edges[channel as usize];
                    if edges.len() < 8 {
                        edges.push(sample_index);
                    }
                }
            }
        }
        previous = Some(word);
    }

    println!("summary over first {inspect_samples} samples:");
    for channel in 0..channel_count {
        let high = highs[channel as usize];
        let edges = transitions[channel as usize];
        if high != 0 || edges != 0 {
            let ratio = if inspect_samples == 0 {
                0.0
            } else {
                high as f64 * 100.0 / inspect_samples as f64
            };
            println!(
                "  D{channel}: high={ratio:6.2}% ({high}), transitions={edges}, first_edges={:?}",
                first_edges[channel as usize]
            );
        }
    }
}

#[derive(Debug)]
struct MappingCandidate {
    name: String,
    map: Vec<u8>,
    reverse_bits: bool,
}

fn compare_cross_mappings(capture: &pxlogic_core::CaptureData) -> Result<(), Box<dyn Error>> {
    let channel_count = capture.metadata.channel_count;
    let enabled_channels = resolve_enabled_channels(
        capture.metadata.channel_count,
        &capture.metadata.enabled_channels,
    )?;
    if enabled_channels.len() != usize::from(channel_count) {
        return Err(
            "cross mapping comparison requires a contiguous all-channel raw capture".into(),
        );
    }
    let channels = usize::from(channel_count);
    if !matches!(channel_count, 16 | 32) {
        return Err(format!(
            "cross mapping comparison only supports 16/32 channels, got {channel_count}"
        )
        .into());
    }

    let stripe_bytes = channels * 8;
    let aligned_len = capture.samples.len() / stripe_bytes * stripe_bytes;
    if aligned_len == 0 {
        return Err("not enough raw cross data for one complete stripe".into());
    }
    let raw = &capture.samples[..aligned_len];
    let decoded_sample_count = (aligned_len / stripe_bytes * 64) as u64;

    println!(
        "raw cross analysis: {} bytes, {} complete stripes, {} decoded samples",
        aligned_len,
        aligned_len / stripe_bytes,
        decoded_sample_count
    );
    print_raw_lane_activity(raw, channel_count);

    for candidate in mapping_candidates(channel_count) {
        println!();
        println!("mapping: {}", candidate.name);
        print_mapping_preview(&candidate.map);
        let decoded = decode_cross_data_with_map(
            channel_count,
            raw,
            Some(&candidate.map),
            candidate.reverse_bits,
        )?;
        print_decoded_summary(
            &decoded,
            capture.metadata.unitsize,
            channel_count,
            decoded_sample_count,
        );
    }

    Ok(())
}

fn print_mapping_preview(map: &[u8]) {
    let preview: Vec<String> = map
        .iter()
        .take(16)
        .enumerate()
        .map(|(logical, physical)| format!("D{logical}<-raw{physical}"))
        .collect();
    println!("  {}", preview.join(", "));
}

fn print_raw_lane_activity(raw: &[u8], channel_count: u8) {
    let channels = usize::from(channel_count);
    let stripe_bytes = channels * 8;
    let stripes = raw.len() / stripe_bytes;
    let mut highs = vec![0u64; channels];
    let mut transitions = vec![0u64; channels];
    let mut previous = vec![None::<u64>; channels];

    for stripe in 0..stripes {
        let stripe_offset = stripe * stripe_bytes;
        for channel in 0..channels {
            let offset = stripe_offset + channel * 8;
            let word = u64::from_le_bytes(raw[offset..offset + 8].try_into().expect("fixed lane"));
            highs[channel] += u64::from(word.count_ones());
            if let Some(prev) = previous[channel] {
                transitions[channel] += u64::from((prev ^ word).count_ones());
            }
            previous[channel] = Some(word);
        }
    }

    let mut lanes: Vec<_> = (0..channels)
        .map(|channel| (channel, highs[channel], transitions[channel]))
        .collect();
    lanes.sort_by_key(|&(_channel, high, edges)| std::cmp::Reverse((edges, high)));

    println!("most active raw lanes:");
    for (channel, high, edges) in lanes.into_iter().take(12) {
        if high == 0 && edges == 0 {
            continue;
        }
        let samples = (stripes * 64) as f64;
        let ratio = if samples == 0.0 {
            0.0
        } else {
            high as f64 * 100.0 / samples
        };
        println!("  raw{channel}: high={ratio:6.2}% ({high}), transitions={edges}");
    }
}

fn mapping_candidates(channel_count: u8) -> Vec<MappingCandidate> {
    let channels = channel_count;
    let mut candidates = Vec::new();
    push_candidate(
        &mut candidates,
        "identity",
        channels,
        |logical, _| logical,
        false,
    );
    push_candidate(
        &mut candidates,
        "identity + reverse time bits inside each 64-sample lane word",
        channels,
        |logical, _| logical,
        true,
    );
    push_candidate(
        &mut candidates,
        "reverse all channels",
        channels,
        |logical, channels| channels - 1 - logical,
        false,
    );
    push_candidate(
        &mut candidates,
        "reverse channels inside each 8-channel group",
        channels,
        |logical, _| (logical / 8) * 8 + (7 - (logical % 8)),
        false,
    );
    push_candidate(
        &mut candidates,
        "reverse 8-channel group order",
        channels,
        |logical, channels| {
            let groups = channels / 8;
            let group = logical / 8;
            let lane = logical % 8;
            (groups - 1 - group) * 8 + lane
        },
        false,
    );
    if channels == 32 {
        push_candidate(
            &mut candidates,
            "swap 16-channel halves",
            channels,
            |logical, _| (logical + 16) % 32,
            false,
        );
        push_candidate(
            &mut candidates,
            "even lanes first, odd lanes second",
            channels,
            |logical, _| {
                if logical < 16 {
                    logical * 2
                } else {
                    (logical - 16) * 2 + 1
                }
            },
            false,
        );
        push_candidate(
            &mut candidates,
            "odd lanes first, even lanes second",
            channels,
            |logical, _| {
                if logical < 16 {
                    logical * 2 + 1
                } else {
                    (logical - 16) * 2
                }
            },
            false,
        );
    }
    candidates
}

fn push_candidate(
    candidates: &mut Vec<MappingCandidate>,
    name: &str,
    channel_count: u8,
    map_fn: impl Fn(u8, u8) -> u8,
    reverse_bits: bool,
) {
    let map = (0..channel_count)
        .map(|logical| map_fn(logical, channel_count))
        .collect();
    candidates.push(MappingCandidate {
        name: name.to_string(),
        map,
        reverse_bits,
    });
}
